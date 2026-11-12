// SPDX-FileCopyrightText: Copyright (c) 2001, Frank Warmerdam <warmerdam@pobox.com>
// SPDX-FileCopyrightText: Copyright (c) 2008-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2010, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1999, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     frmts/vrt/vrtsources.cpp
//     gcore/rasterio.cpp
//     gcore/overview.cpp
//     gcore/gdal_misc.cpp

//! GDAL virtual rasters (`.vrt`): reading, writing, and the source windowing they share with
//! [`translate`](super::translate).
//!
//! A VRT is an XML description of a raster whose bands are composed of windows of other rasters
//! ("sources"). terrio writes VRTs GDAL opens, and opens the VRTs GDAL writes for mosaics and
//! subsets: `SimpleSource`, `ComplexSource` (with `NODATA`, `ScaleOffset`, `ScaleRatio` and
//! `LUT`, as `gdaldem color-relief -of VRT` writes them) and `AveragedSource`, with
//! `SrcRect`/`DstRect` windows. [`VirtualRaster`] extends serialized interoperability with
//! lazy warped VRTs, built-in scalar derived bands, kernel sources and validity masks.

mod virtual_raster;
pub(crate) use virtual_raster::{SourceInventoryScope, resolve_reference, source_input_paths, subdataset_parts};
pub use virtual_raster::{VirtualRaster, open_virtual_raster};

use super::{Grid, Resampling, common_type, convert_value, data_type_name, parse_data_type};
use crate::crs::SpatialRef;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// GDAL sourced bands use an integer flag; raw bands use CPLTestBool.
pub(crate) fn relative_reference_flag(value: Option<&str>, raw: bool) -> bool {
    let Some(value) = value else {
        return raw;
    };
    if raw {
        return !matches!(value.to_ascii_lowercase().as_str(), "0" | "no" | "off" | "false");
    }
    let value = value.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let digits = value.strip_prefix('+').or_else(|| value.strip_prefix('-')).unwrap_or(value);
    digits.chars().take_while(char::is_ascii_digit).any(|digit| digit != '0')
}

/// A virtual raster.
#[derive(Debug, Clone, PartialEq)]
pub struct VrtDataset {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// The geotransform, when georeferenced.
    pub geo_transform: Option<[f64; 6]>,
    /// The CRS.
    pub srs: Option<SpatialRef>,
    /// The bands.
    pub bands: Vec<VrtBand>,
}

/// A band of a virtual raster.
#[derive(Debug, Clone, PartialEq)]
pub struct VrtBand {
    /// The data type.
    pub data_type: DataType,
    /// The nodata value.
    pub nodata: Option<f64>,
    /// The color interpretation (`Red`, `Green`, `Blue`, `Alpha`...), when declared.
    pub color_interp: Option<String>,
    /// The sources, drawn in order (later ones on top).
    pub sources: Vec<VrtSource>,
}

/// A window of a band of another raster, placed in a virtual band.
#[derive(Debug, Clone, PartialEq)]
pub struct VrtSource {
    /// The raster's path: absolute, or relative to the VRT's directory when
    /// [`VrtSource::relative_to_vrt`] is set.
    pub filename: String,
    /// Whether [`VrtSource::filename`] is relative to the VRT's directory.
    pub relative_to_vrt: bool,
    /// The band, from 1.
    pub band: usize,
    /// The window read: `[x_off, y_off, x_size, y_size]` in source pixels.
    pub src_rect: [f64; 4],
    /// Where it goes: `[x_off, y_off, x_size, y_size]` in VRT pixels.
    pub dst_rect: [f64; 4],
    /// Whether it is a `ComplexSource` (nodata skipping and scaling) rather than a `SimpleSource`.
    pub complex: bool,
    /// Source pixels with this value are not drawn (`ComplexSource` only).
    pub nodata: Option<f64>,
    /// Linear scaling `value * ratio + offset` (`ComplexSource` only).
    pub scale: Option<(f64, f64)>,
    /// A lookup table `(input, output)` applied after scaling, interpolating linearly between
    /// its inputs and clamping beyond them (`ComplexSource` only).
    pub lut: Option<Vec<(f64, f64)>>,
    /// Resampling when the source and destination windows differ in size.
    pub resampling: Resampling,
    /// The source's size and data type, written to help GDAL open the VRT lazily.
    pub properties: Option<(usize, usize, DataType)>,
}

impl VrtDataset {
    /// Reads a VRT file's description.
    #[cfg(not(target_family = "wasm"))]
    pub fn open(path: &str) -> Result<Self> {
        crate::formats::check_driver_allowed("VRT", path, crate::formats::Kind::Raster)?;
        let dataset = Self::from_xml(&std::fs::read_to_string(path)?)?;
        crate::declared::check(
            dataset.width,
            dataset.height,
            dataset.bands.len(),
            common_type(dataset.bands.iter().map(|b| b.data_type)),
            false,
        )?;
        Ok(dataset)
    }

    /// Parses a VRT description. Under a process-wide raster allow-list, only with the VRT driver
    /// allowed, as GDAL's VRT driver reads one.
    pub fn from_xml(text: &str) -> Result<Self> {
        crate::formats::check_driver_allowed("VRT", "a VRT description", crate::formats::Kind::Raster)?;
        let root = xml::parse(text)?;
        if root.name != "VRTDataset" {
            return Err(Error::InvalidData(format!("not a VRT: root element is <{}>", root.name)));
        }
        if root.attr("subClass").is_some() {
            return Err(Error::Unsupported(format!("VRT subclass {} is not supported", root.attr("subClass").unwrap_or_default())));
        }
        let size = |name: &str| -> Result<usize> {
            root.attr(name).and_then(|v| v.trim().parse().ok()).ok_or_else(|| Error::InvalidData(format!("VRT: missing {name}")))
        };
        let (width, height) = (size("rasterXSize")?, size("rasterYSize")?);
        let geo_transform = match root.child("GeoTransform") {
            Some(g) => {
                let v: Vec<f64> = g
                    .text
                    .split(',')
                    .map(|s| s.trim().parse().map_err(|_| Error::InvalidData("VRT: invalid GeoTransform".into())))
                    .collect::<Result<_>>()?;
                if v.len() != 6 {
                    return Err(Error::InvalidData("VRT: GeoTransform needs 6 values".into()));
                }
                Some([v[0], v[1], v[2], v[3], v[4], v[5]])
            }
            None => None,
        };
        let srs = root.child("SRS").filter(|s| !s.text.trim().is_empty()).map(|s| SpatialRef::parse(s.text.trim())).transpose()?;
        let mut bands = Vec::new();
        for b in root.children.iter().filter(|c| c.name == "VRTRasterBand") {
            if let Some(sub) = b.attr("subClass") {
                return Err(Error::Unsupported(format!("VRT band subclass {sub} is not supported")));
            }
            let data_type = parse_data_type(b.attr("dataType").unwrap_or("Byte"))?;
            let nodata = b.child("NoDataValue").map(|n| parse_number(&n.text)).transpose()?;
            let mut sources = Vec::new();
            for s in &b.children {
                let complex = match s.name.as_str() {
                    "SimpleSource" | "AveragedSource" => false,
                    "ComplexSource" => true,
                    "KernelFilteredSource" => return Err(Error::Unsupported("VRT KernelFilteredSource is not supported".into())),
                    _ => continue,
                };
                let fname = s.child("SourceFilename").ok_or_else(|| Error::InvalidData("VRT: source without SourceFilename".into()))?;
                let band = match s.child("SourceBand") {
                    Some(sb) => sb
                        .text
                        .trim()
                        .parse()
                        .map_err(|_| Error::Unsupported(format!("VRT source band '{}' is not supported", sb.text.trim())))?,
                    None => 1,
                };
                let rect = |name: &str| -> Option<[f64; 4]> {
                    let r = s.child(name)?;
                    let g = |a: &str| r.attr(a).and_then(|v| v.trim().parse::<f64>().ok());
                    Some([g("xOff")?, g("yOff")?, g("xSize")?, g("ySize")?])
                };
                let properties = s.child("SourceProperties").and_then(|p| {
                    let w = p.attr("RasterXSize")?.parse().ok()?;
                    let h = p.attr("RasterYSize")?.parse().ok()?;
                    let dt = parse_data_type(p.attr("DataType")?).ok()?;
                    Some((w, h, dt))
                });
                let src_rect = rect("SrcRect").unwrap_or([f64::NAN; 4]);
                let dst_rect = rect("DstRect").unwrap_or([0.0, 0.0, width as f64, height as f64]);
                let nodata = s.child("NODATA").map(|n| parse_number(&n.text)).transpose()?;
                let offset = s.child("ScaleOffset").map(|n| parse_number(&n.text)).transpose()?;
                let ratio = s.child("ScaleRatio").map(|n| parse_number(&n.text)).transpose()?;
                let scale = (offset.is_some() || ratio.is_some()).then(|| (offset.unwrap_or(0.0), ratio.unwrap_or(1.0)));
                if s.child("Exponent").is_some() {
                    return Err(Error::Unsupported("VRT exponential scaling is not supported".into()));
                }
                let lut = s.child("LUT").map(|l| parse_lut(&l.text)).transpose()?.filter(|l| !l.is_empty());
                let resampling = match s.attr("resampling") {
                    Some(r) => Resampling::parse(r)?,
                    None if s.name == "AveragedSource" => Resampling::Average,
                    None => Resampling::Nearest,
                };
                sources.push(VrtSource {
                    filename: fname.text.clone(),
                    relative_to_vrt: relative_reference_flag(fname.attr("relativeToVRT"), false),
                    band,
                    src_rect,
                    dst_rect,
                    complex,
                    nodata,
                    scale,
                    lut,
                    resampling,
                    properties,
                });
            }
            let color_interp = b.child("ColorInterp").map(|c| c.text.trim().to_string());
            bands.push(VrtBand { data_type, nodata, color_interp, sources });
        }
        if bands.is_empty() {
            return Err(Error::InvalidData("VRT: no bands".into()));
        }
        Ok(Self { width, height, geo_transform, srs, bands })
    }

    /// The VRT's XML description, as GDAL writes it.
    pub fn to_xml(&self) -> String {
        let mut s = format!("<VRTDataset rasterXSize=\"{}\" rasterYSize=\"{}\">\n", self.width, self.height);
        // GDAL reads any CRS definition here; an EPSG (or ESRI) code is the most portable.
        let srs = self.srs.as_ref().and_then(|s| s.authority_code().or_else(|| s.to_wkt()));
        if let Some(def) = srs {
            s += &format!("  <SRS>{}</SRS>\n", xml::escape(&def));
        }
        if let Some(gt) = self.geo_transform {
            let v: Vec<String> = gt.iter().map(|x| format!("{x:.16e}")).collect();
            s += &format!("  <GeoTransform>{}</GeoTransform>\n", v.join(", "));
        }
        for (i, b) in self.bands.iter().enumerate() {
            s += &format!("  <VRTRasterBand dataType=\"{}\" band=\"{}\">\n", data_type_name(b.data_type), i + 1);
            if let Some(nd) = b.nodata {
                s += &format!("    <NoDataValue>{}</NoDataValue>\n", fmt_number(nd));
            }
            if let Some(ci) = &b.color_interp {
                s += &format!("    <ColorInterp>{}</ColorInterp>\n", xml::escape(ci));
            }
            for src in &b.sources {
                let tag = if src.complex { "ComplexSource" } else { "SimpleSource" };
                let res = if src.resampling == Resampling::Nearest {
                    String::new()
                } else {
                    format!(" resampling=\"{}\"", src.resampling.name())
                };
                s += &format!("    <{tag}{res}>\n");
                s += &format!(
                    "      <SourceFilename relativeToVRT=\"{}\">{}</SourceFilename>\n",
                    u8::from(src.relative_to_vrt),
                    xml::escape_filename(&src.filename)
                );
                s += &format!("      <SourceBand>{}</SourceBand>\n", src.band);
                if let Some((w, h, dt)) = src.properties {
                    s += &format!(
                        "      <SourceProperties RasterXSize=\"{w}\" RasterYSize=\"{h}\" DataType=\"{}\" BlockXSize=\"{}\" BlockYSize=\"1\" />\n",
                        data_type_name(dt),
                        w
                    );
                }
                let r = |name: &str, r: &[f64; 4]| {
                    format!(
                        "      <{name} xOff=\"{}\" yOff=\"{}\" xSize=\"{}\" ySize=\"{}\" />\n",
                        fmt_number(r[0]),
                        fmt_number(r[1]),
                        fmt_number(r[2]),
                        fmt_number(r[3])
                    )
                };
                s += &r("SrcRect", &src.src_rect);
                s += &r("DstRect", &src.dst_rect);
                if src.complex {
                    if let Some(nd) = src.nodata {
                        s += &format!("      <NODATA>{}</NODATA>\n", fmt_number(nd));
                    }
                    if let Some((off, ratio)) = src.scale {
                        s += &format!(
                            "      <ScaleOffset>{}</ScaleOffset>\n      <ScaleRatio>{}</ScaleRatio>\n",
                            fmt_number(off),
                            fmt_number(ratio)
                        );
                    }
                    if let Some(lut) = &src.lut {
                        s += &format!("      <LUT>{}</LUT>\n", format_lut(lut));
                    }
                }
                s += &format!("    </{tag}>\n");
            }
            s += "  </VRTRasterBand>\n";
        }
        s += "</VRTDataset>\n";
        s
    }

    /// Writes the VRT file.
    #[cfg(fs)]
    pub fn write(&self, path: &str) -> Result<()> {
        Ok(std::fs::write(path, self.to_xml())?)
    }

    /// Reads the pixels of the virtual raster. `base` is the directory relative source paths are
    /// relative to. Refused, before any source is read, when the VRT declares more than
    /// `TERRIO_MAX_DECLARED_BYTES` or `TERRIO_MAX_BANDS` allow.
    pub fn materialize(&self, base: &Path) -> Result<RasterDataset> {
        let data_type = common_type(self.bands.iter().map(|b| b.data_type));
        // What the VRT declares, before its output is allocated (see `crate::declared`).
        crate::declared::check(self.width, self.height, self.bands.len(), data_type, false)?;
        crate::dataset::fallible::check_raster_buffer("a VRT", self.width, self.height, self.bands.len(), data_type)?;
        let mut cache: HashMap<PathBuf, RasterDataset> = HashMap::new();
        let n = crate::dataset::fallible::len(&[self.width, self.height], "a VRT output")?;
        let samples = crate::dataset::fallible::len(&[n, self.bands.len()], "a VRT output")?;
        let output_bytes = crate::dataset::fallible::len(&[samples, data_type.size_bytes()], "a VRT output")?;
        let output_band_bytes = crate::dataset::fallible::len(&[n, data_type.size_bytes()], "a VRT band")?;
        let mut cached_bytes = 0usize;
        let nodata = self.bands[0].nodata;
        // The output in its type, values converted as they are stored.
        let mut out = crate::dataset::fallible::raster_data(data_type, samples, 0.0, "a VRT output")?;
        let mut srs = self.srs.clone();
        for (bi, band) in self.bands.iter().enumerate() {
            crate::pixel::with_data!(&mut out, v => {
                let fill = convert_value(band.nodata.unwrap_or(0.0), band.data_type);
                v[bi * n..(bi + 1) * n].iter_mut().for_each(|x| *x = fill as _);
            });
            for src in &band.sources {
                let path = if src.relative_to_vrt || Path::new(&src.filename).is_relative() {
                    base.join(&src.filename)
                } else {
                    PathBuf::from(&src.filename)
                };
                if !cache.contains_key(&path) {
                    let ds = crate::open_raster(&path.to_string_lossy())?;
                    cached_bytes = ds
                        .raster
                        .data
                        .len()
                        .checked_mul(ds.raster.data_type.size_bytes())
                        .and_then(|bytes| cached_bytes.checked_add(bytes))
                        .ok_or_else(|| Error::ResourceLimit("VRT cached source buffers exceed the address space".into()))?;
                    cache.insert(path.clone(), ds);
                }
                let ds = &cache[&path];
                if srs.is_none() {
                    let s = ds.crs();
                    srs = (!s.is_unknown()).then_some(s);
                }
                let r = &ds.raster;
                if src.band == 0 || src.band > r.bands {
                    return Err(Error::InvalidData(format!("VRT: '{}' has no band {}", path.display(), src.band)));
                }
                let values = super::BandView::of(r, src.band - 1);
                let skip = if src.complex { src.nodata } else { None };
                let valid_bytes = if skip.is_some() { r.cols.checked_mul(r.rows) } else { Some(0) };
                let peak = output_bytes.checked_add(cached_bytes).and_then(|bytes| valid_bytes.and_then(|mask| bytes.checked_add(mask)));
                crate::dataset::fallible::check_usable(peak, || {
                    "VRT output, cached sources and validity buffers are too large to allocate".into()
                })
                .map_err(Error::ResourceLimit)?;
                let retained = output_bytes
                    .checked_sub(output_band_bytes)
                    .and_then(|bytes| cached_bytes.checked_sub(values.byte_len()).and_then(|cached| bytes.checked_add(cached)))
                    .ok_or_else(|| Error::ResourceLimit("VRT retained buffers exceed the address space".into()))?;
                let valid: Option<Vec<bool>> = skip.map(|nd| values.valid(nd));
                let src_rect = if src.src_rect[0].is_nan() { [0.0, 0.0, r.cols as f64, r.rows as f64] } else { src.src_rect };
                let req = SourceRead {
                    values,
                    valid: valid.as_deref(),
                    width: r.cols,
                    height: r.rows,
                    src_type: r.data_type,
                    src_rect,
                    dst_rect: src.dst_rect,
                    resampling: src.resampling,
                    working_float: src.complex,
                    invalid_value: None,
                    within_rects: true,
                };
                // GDAL's complex sources work in double precision for some types, else in float.
                let double = is_float_working(r.data_type)
                    || matches!(band.data_type, DataType::I32 | DataType::U32 | DataType::I64 | DataType::U64 | DataType::F64);
                let value = |v: f64| {
                    let v = match src.scale {
                        Some((off, ratio)) => {
                            let v = v * ratio + off;
                            if is_float_working(r.data_type) { v } else { v as f32 as f64 }
                        }
                        None => v,
                    };
                    match &src.lut {
                        Some(lut) => {
                            let w = if double { v } else { v as f32 as f64 };
                            let out = lookup(lut, w);
                            if band.data_type == DataType::U8 {
                                // Byte output: rounded in float, half up, and clamped.
                                ((out as f32 + 0.5).clamp(0.0, 255.0) as u8) as f64
                            } else if double {
                                out
                            } else {
                                out as f32 as f64
                            }
                        }
                        None => v,
                    }
                };
                crate::pixel::with_data!(&mut out, v => {
                    req.read_with_retained(self.width, self.height, &mut v[bi * n..(bi + 1) * n], retained, |x| convert_value(value(x), band.data_type) as _)?;
                });
            }
        }
        let grid =
            Grid { width: self.width, height: self.height, geo_transform: self.geo_transform.unwrap_or([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]) };
        let mut ds = super::build_raster_data(grid, out, self.bands.len(), nodata, &srs.unwrap_or_default())?;
        ds.driver = RasterDriver::VRT;
        ds.geotransform_present = Some(self.geo_transform.is_some());
        Ok(ds)
    }
}

/// Whether GDAL's complex sources work in double precision for this source type.
fn is_float_working(dt: DataType) -> bool {
    matches!(dt, DataType::F64 | DataType::I32 | DataType::U32 | DataType::I64 | DataType::U64)
}

/// Opens a VRT file and reads its pixels.
#[cfg(not(target_family = "wasm"))]
pub fn read_vrt(path: &str) -> Result<RasterDataset> {
    open_virtual_raster(path)?.read()
}

/// Opens a VRT file and reads its pixels.
#[cfg(target_family = "wasm")]
pub fn read_vrt(_path: &str) -> Result<RasterDataset> {
    Err(Error::Unsupported("VRT files are not supported in WebAssembly".into()))
}

/// Parses a `LUT` element: `input:output` pairs separated by commas.
fn parse_lut(text: &str) -> Result<Vec<(f64, f64)>> {
    let values: Vec<&str> = text.split([',', ':']).collect();
    if values.len() < 2 {
        return Ok(Vec::new());
    }
    values.as_chunks::<2>().0.iter().map(|[a, b]| Ok((parse_number(a)?, parse_number(b)?))).collect()
}

/// Formats a lookup table as GDAL writes it: `%g`, or `%.17g` where neighbouring inputs would
/// look the same in `%g`.
pub(crate) fn format_lut(lut: &[(f64, f64)]) -> String {
    let g = |v: f64| crate::drivers::common::fmt_g(v, 6);
    let mut parts = Vec::with_capacity(lut.len());
    for (i, (input, output)) in lut.iter().enumerate() {
        let same_as = |j: Option<usize>| j.and_then(|j| lut.get(j)).is_some_and(|(x, _)| g(*x) == g(*input));
        let precise = same_as(i.checked_sub(1)) || same_as(Some(i + 1));
        let input = if precise { crate::drivers::common::fmt_g(*input, 17) } else { g(*input) };
        parts.push(format!("{input}:{}", g(*output)));
    }
    parts.join(",")
}

/// GDAL's `VRTComplexSource::LookupValue()`: linear interpolation between the inputs, the first
/// or last output beyond them, a NaN first input mapping NaN.
fn lookup(lut: &[(f64, f64)], v: f64) -> f64 {
    let mut start = 0;
    if lut[0].0.is_nan() {
        if v.is_nan() || lut.len() == 1 {
            return lut[0].1;
        }
        start = 1;
    }
    let rest = &lut[start..];
    let i = start + rest.partition_point(|(x, _)| *x < v);
    if i == start {
        return lut[start].1;
    }
    if i == lut.len() {
        return lut[lut.len() - 1].1;
    }
    if lut[i].0 == v {
        return lut[i].1;
    }
    let (x0, y0) = lut[i - 1];
    let (x1, y1) = lut[i];
    y0 + (v - x0) * ((y1 - y0) / (x1 - x0))
}

fn parse_number(s: &str) -> Result<f64> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "nan" => Ok(f64::NAN),
        "inf" | "+inf" => Ok(f64::INFINITY),
        "-inf" => Ok(f64::NEG_INFINITY),
        _ => t.parse().map_err(|_| Error::InvalidData(format!("VRT: invalid number '{t}'"))),
    }
}

/// Formats a number as GDAL does: integers without a fraction, others with 17 significant digits.
pub(crate) fn fmt_number(v: f64) -> String {
    if v.is_nan() {
        "nan".into()
    } else if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// ─── Reading a window of a source into a destination window ────────────────────────────────────

/// A read of a source window into a destination window, with GDAL's VRT source semantics
/// (`VRTSimpleSource::GetSrcDstWindow()` followed by `GDALRasterBand::RasterIO()`). A VRT
/// source's read stays within its rectangles ([`Self::within_rects`]).
pub(crate) struct SourceRead<'a> {
    /// The source band's pixels.
    pub values: super::BandView<'a>,
    /// Which source pixels are valid (`None`: all).
    pub valid: Option<&'a [bool]>,
    /// The source's size.
    pub width: usize,
    /// The source's height.
    pub height: usize,
    /// The source's data type (resampled values are rounded to it, as GDAL does).
    /// A `sum` is not: its value goes to the output's type as it is.
    pub src_type: DataType,
    /// The source window.
    pub src_rect: [f64; 4],
    /// The destination window.
    pub dst_rect: [f64; 4],
    /// The resampling method.
    pub resampling: Resampling,
    /// Whether values are resampled in floating point (complex sources) rather than in the
    /// source's type.
    pub working_float: bool,
    /// The value written where resampling finds no valid source pixel (`None`: nothing is
    /// written).
    pub invalid_value: Option<f64>,
    /// Whether the read stays within its rectangles, as a VRT source's does: it uses only the
    /// source pixels that meet `src_rect`, and writes a destination pixel only from the part of
    /// it inside `dst_rect` (a method sampling a point, only when the pixel's centre is inside).
    /// Otherwise, as for a window of one raster, kernels and areas reach the source pixels
    /// around `src_rect`. Either way, a window is copied one for one only as [`Self::copies`]
    /// says, and resampled at its offset otherwise.
    pub within_rects: bool,
}

impl SourceRead<'_> {
    /// Reads into a destination `out` of `dw × dh` pixels, setting each valid destination pixel
    /// the source covers to `f(value)`. Rows are read in parallel.
    pub fn read<T: Send>(&self, dw: usize, dh: usize, out: &mut [T], f: impl Fn(f64) -> T + Sync + Send) -> Result<()> {
        self.read_with_retained(dw, dh, out, 0, f)
    }

    /// Also account for buffers retained by a caller while this source is resampled.
    pub fn read_with_retained<T: Send>(
        &self,
        dw: usize,
        dh: usize,
        out: &mut [T],
        retained: usize,
        f: impl Fn(f64) -> T + Sync + Send,
    ) -> Result<()> {
        let Some(w) = self.window(dw as f64, dh as f64, dw, dh) else { return Ok(()) };
        let (ow, oh) = (w.out_size.0, w.out_size.1);
        if ow == 0 || oh == 0 {
            return Ok(());
        }
        let is_valid = |i: usize| self.valid.is_none_or(|m| m[i]);
        let output_bytes = std::mem::size_of_val(out);
        // The destination rows the source covers.
        let start = (w.out_off.1 * dw).min(out.len());
        let end = ((w.out_off.1 + oh) * dw).min(out.len());
        let rows = &mut out[start..end];
        let x0 = w.out_off.0;
        if self.copies(&w) {
            let [c0, r0, c1, r1] = self.copy_span(&w);
            crate::par::chunks_mut(rows, dw, |r, line| {
                if !(r0..r1).contains(&r) {
                    return;
                }
                let row = (w.req_off.1 + r) * self.width + w.req_off.0;
                for c in c0..c1 {
                    if is_valid(row + c) {
                        line[x0 + c] = f(self.values.get(row + c));
                    }
                }
            });
            return Ok(());
        }
        // The destination pixels written, relative to the window, and the source pixels read.
        let [c0, r0, c1, r1] = self.written(&w);
        let [sx0, sy0, sx1, sy1] = self.cover();
        if sx1 <= sx0 || sy1 <= sy0 {
            return Ok(());
        }
        let (fx, fy, fw, fh) = (w.dreq_off.0, w.dreq_off.1, w.dreq_size.0, w.dreq_size.1);
        if self.resampling == Resampling::Nearest {
            const EPS: f64 = 1e-10;
            let incx = fw / ow as f64;
            let incy = fh / oh as f64;
            crate::par::chunks_mut(rows, dw, |r, line| {
                if !(r0..r1).contains(&r) {
                    return;
                }
                let sy = ((r as f64 + 0.5) * incy + fy + EPS).max(sy0 as f64).min((sy1 - 1) as f64) as usize;
                let mut sxf = 0.5 * incx + fx + EPS;
                for c in 0..ow {
                    let sx = sxf.max(sx0 as f64).min((sx1 - 1) as f64) as usize;
                    sxf += incx;
                    if !(c0..c1).contains(&c) {
                        continue;
                    }
                    let i = sy * self.width + sx;
                    if is_valid(i) {
                        line[x0 + c] = f(self.values.get(i));
                    }
                }
            });
            return Ok(());
        }
        let retained = retained
            .checked_add(output_bytes)
            .ok_or_else(|| Error::ResourceLimit("a resampling working set is too large to allocate".into()))?;
        let res = if self.resampling == Resampling::Average {
            gdal_average(self, w.req_off, (fx, fy, fw, fh), (ow, oh), retained)
        } else {
            resample_window(self, (fx, fy, fw, fh), (ow, oh), retained)
        }?;
        // A `sum` adds shares of source pixels, which neither the source type's precision nor its
        // range holds: it is passed on as it is, for `f` to store in the output's type.
        let keep = self.working_float || self.resampling == Resampling::Sum;
        crate::par::chunks_mut(rows, dw, |r, line| {
            if !(r0..r1).contains(&r) {
                return;
            }
            for c in c0..c1 {
                match res[r * ow + c] {
                    Some(v) => line[x0 + c] = f(if keep { v } else { convert_value(v, self.src_type) }),
                    None => {
                        if let Some(v) = self.invalid_value {
                            line[x0 + c] = f(v);
                        }
                    }
                }
            }
        });
        Ok(())
    }

    /// How a read of the whole destination of `dw × dh` pixels copies source pixels, when it
    /// copies them one for one, without resampling: `Ok(None)` when it reads nothing, `Err(())`
    /// when it resamples.
    pub fn plain_copy(&self, dw: usize, dh: usize) -> std::result::Result<Option<PlainCopy>, ()> {
        let Some(w) = self.window(dw as f64, dh as f64, dw, dh) else { return Ok(None) };
        let (ow, oh) = w.out_size;
        if ow == 0 || oh == 0 {
            return Ok(None);
        }
        if !self.copies(&w) {
            return Err(());
        }
        // Source pixels past the source's edges (or, within the rectangles, outside them), and
        // rows past the destination's, are not read.
        let [c0, r0, c1, r1] = self.copy_span(&w);
        let cols = c1 - c0;
        let rows = (r1 - r0).min(dh.saturating_sub(w.out_off.1 + r0));
        if cols == 0 || rows == 0 {
            return Ok(None);
        }
        let (src, dst_y) = ((w.req_off.0 + c0, w.req_off.1 + r0), w.out_off.1 + r0);
        Ok(Some(PlainCopy { src, dst_x: w.out_off.0 + c0, cols, dst_rows: dst_y..dst_y + rows }))
    }

    /// Whether a read through `w` copies source pixels one for one: only a window of the same
    /// scale, which requests as many source pixels as it writes, by `nearest` when each
    /// destination pixel's centre is in the source pixel copied, and by the other methods on
    /// whole offsets only. Within its rectangles, a `sum` copies only into destination pixels
    /// wholly inside `dst_rect`. Another window is resampled at its offset: a `sum` shares each
    /// source pixel by the area inside each destination pixel.
    fn copies(&self, w: &Window) -> bool {
        const EPSILON: f64 = 1e-10;
        let whole = |f: f64, i: usize| (f - i as f64).abs() <= EPSILON;
        let same_scale = whole(w.dreq_size.0, w.out_size.0) && whole(w.dreq_size.1, w.out_size.1);
        let aligned = whole(w.dreq_off.0, w.req_off.0) && whole(w.dreq_off.1, w.req_off.1);
        if w.req_size != w.out_size || !same_scale {
            return false;
        }
        // The source pixel holding the first destination pixel's centre.
        let nearest = |f: f64, i: usize| (f + 0.5 + EPSILON).floor() == i as f64;
        match self.resampling {
            Resampling::Nearest => nearest(w.dreq_off.0, w.req_off.0) && nearest(w.dreq_off.1, w.req_off.1),
            Resampling::Sum => aligned && (!self.within_rects || self.inside(w)),
            _ => aligned,
        }
    }

    /// Whether each destination pixel of `w` is wholly inside `dst_rect`.
    fn inside(&self, w: &Window) -> bool {
        const EPSILON: f64 = 1e-10;
        let [dxo, dyo, dxs, dys] = self.dst_rect;
        let (x0, y0) = (w.out_off.0 as f64, w.out_off.1 as f64);
        let (x1, y1) = (x0 + w.out_size.0 as f64, y0 + w.out_size.1 as f64);
        dxo <= x0 + EPSILON && x1 <= dxo + dxs + EPSILON && dyo <= y0 + EPSILON && y1 <= dyo + dys + EPSILON
    }

    /// The source window a read's areas are clipped to, `[x0, y0, x1, y1]`: within its
    /// rectangles, `src_rect`, otherwise none.
    fn src_bounds(&self) -> [f64; 4] {
        if !self.within_rects {
            return [f64::NEG_INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::INFINITY];
        }
        let [x, y, xs, ys] = self.src_rect;
        [x, y, x + xs, y + ys]
    }

    /// The source pixels a read uses, `[x0, y0, x1, y1]`: within its rectangles, those that meet
    /// `src_rect`, otherwise all.
    fn cover(&self) -> [usize; 4] {
        if !self.within_rects {
            return [0, 0, self.width, self.height];
        }
        const EPSILON: f64 = 1e-10;
        let [x0, y0, x1, y1] = self.src_bounds();
        let span = |lo: f64, hi: f64, n: usize| {
            let a = (lo + EPSILON).floor().clamp(0.0, n as f64) as usize;
            (a, ((hi - EPSILON).ceil().clamp(0.0, n as f64) as usize).max(a))
        };
        let ((x0, x1), (y0, y1)) = (span(x0, x1, self.width), span(y0, y1, self.height));
        [x0, y0, x1, y1]
    }

    /// The destination pixels a read through `w` writes, `[c0, r0, c1, r1]` relative to the
    /// window: within its rectangles, a method sampling a point writes only those whose centres
    /// are inside `dst_rect` (a centre on its left or top edge is inside, on its right or bottom
    /// edge outside).
    fn written(&self, w: &Window) -> [usize; 4] {
        let (ow, oh) = w.out_size;
        if !self.within_rects || self.resampling.is_area_based() {
            return [0, 0, ow, oh];
        }
        const EPSILON: f64 = 1e-10;
        let [dxo, dyo, dxs, dys] = self.dst_rect;
        // The first pixel whose centre is at `edge` or past it.
        let first = |edge: f64, off: usize, n: usize| ((edge - 0.5 - EPSILON).ceil() - off as f64).clamp(0.0, n as f64) as usize;
        let span = |lo: f64, hi: f64, off: usize, n: usize| {
            let a = first(lo, off, n);
            (a, first(hi, off, n).max(a))
        };
        let ((c0, c1), (r0, r1)) = (span(dxo, dxo + dxs, w.out_off.0, ow), span(dyo, dyo + dys, w.out_off.1, oh));
        [c0, r0, c1, r1]
    }

    /// The destination pixels a copy through `w` writes, `[c0, r0, c1, r1]` relative to the
    /// window: those [`Self::written`] gives whose source pixels are among those it uses.
    fn copy_span(&self, w: &Window) -> [usize; 4] {
        let [c0, r0, c1, r1] = self.written(w);
        let [sx0, sy0, sx1, sy1] = self.cover();
        let span = |lo: usize, hi: usize, s0: usize, s1: usize, off: usize| {
            let a = lo.max(s0.saturating_sub(off));
            (a, hi.min(s1.saturating_sub(off)).max(a))
        };
        let ((c0, c1), (r0, r1)) = (span(c0, c1, sx0, sx1, w.req_off.0), span(r0, r1, sy0, sy1, w.req_off.1));
        [c0, r0, c1, r1]
    }

    /// `VRTSimpleSource::GetSrcDstWindow()` for a request covering the whole destination.
    fn window(&self, xsize: f64, ysize: f64, buf_w: usize, buf_h: usize) -> Option<Window> {
        let [sxo, syo, sxs, sys] = self.src_rect;
        let [dxo, dyo, dxs, dys] = self.dst_rect;
        if sxs == 0.0 || sys == 0.0 || dxs == 0.0 || dys == 0.0 {
            return None;
        }
        let (xoff, yoff) = (0.0, 0.0);
        if xoff >= dxo + dxs || yoff >= dyo + dys || xoff + xsize <= dxo || yoff + ysize <= dyo {
            return None;
        }
        let (mut rxo, mut ryo, mut rxs, mut rys) = (xoff, yoff, xsize, ysize);
        let (mut mod_x, mut mod_y) = (false, false);
        if rxo < dxo {
            rxs = rxs + rxo - dxo;
            rxo = dxo;
            mod_x = true;
        }
        if ryo < dyo {
            rys = rys + ryo - dyo;
            ryo = dyo;
            mod_y = true;
        }
        if rxo + rxs > dxo + dxs {
            rxs = dxo + dxs - rxo;
            mod_x = true;
        }
        if ryo + rys > dyo + dys {
            rys = dyo + dys - ryo;
            mod_y = true;
        }
        let (scx, scy) = (sxs / dxs, sys / dys);
        let mut fxo = (rxo - dxo) * scx + sxo;
        let mut fyo = (ryo - dyo) * scy + syo;
        let mut fxs = rxs * scx;
        let mut fys = rys * scy;
        if !(fxo.is_finite() && fyo.is_finite() && fxs >= 0.0 && fys >= 0.0) {
            return None;
        }
        if fxo < 0.0 {
            fxs += fxo;
            fxo = 0.0;
            mod_x = true;
        }
        if fyo < 0.0 {
            fys += fyo;
            fyo = 0.0;
            mod_y = true;
        }
        const EPSILON: f64 = 1e-10;
        let fractional = [sxo, syo, dxo, dyo].iter().any(|v| (v - v.round()).abs() > EPSILON);
        let (mut ixo, mut iyo) = if self.resampling == Resampling::Nearest && fractional {
            ((fxo + 0.5 + EPSILON) as i64, (fyo + 0.5 + EPSILON) as i64)
        } else {
            (fxo as i64, fyo as i64)
        };
        if fxo - ixo as f64 > 1.0 - 1e-3 {
            ixo += 1;
            fxo = ixo as f64;
        }
        if fyo - iyo as f64 > 1.0 - 1e-3 {
            iyo += 1;
            fyo = iyo as f64;
        }
        let mut ixs = ((fxs + 0.5).floor() as i64).max(1);
        let mut iys = ((fys + 0.5).floor() as i64).max(1);
        let (w, h) = (self.width as i64, self.height as i64);
        if ixo + ixs > w {
            ixs = w - ixo;
            mod_x = true;
        }
        if fxo + fxs > w as f64 {
            fxs = w as f64 - fxo;
            mod_x = true;
        }
        if iyo + iys > h {
            iys = h - iyo;
            mod_y = true;
        }
        if fyo + fys > h as f64 {
            fys = h as f64 - fyo;
            mod_y = true;
        }
        if ixo >= w || iyo >= h || ixs <= 0 || iys <= 0 {
            return None;
        }
        let (mut oxo, mut oyo, mut oxs, mut oys) = (0i64, 0i64, buf_w as i64, buf_h as i64);
        const EPS: f64 = 1e-3;
        let src_to_dst = |x: f64, y: f64| ((x - sxo) * dxs / sxs + dxo, (y - syo) * dys / sys + dyo);
        let (ulx, uly) = src_to_dst(fxo, fyo);
        let (lrx, lry) = src_to_dst(fxo + fxs, fyo + fys);
        if mod_x {
            let scale = buf_w as f64 / xsize;
            let out_xo = (ulx - xoff) * scale;
            oxo = if out_xo <= 0.0 { 0 } else { (out_xo + EPS) as i64 };
            let delta = (out_xo - oxo as f64) / scale / dxs * sxs;
            fxo -= delta;
            fxs += delta;
            let out_right = (lrx - xoff) * scale;
            if out_right < out_xo {
                return None;
            }
            let right = (out_right - EPS).ceil() as i64;
            oxs = (right - oxo).min(buf_w as i64 - oxo);
            fxs += (right as f64 - out_right) / scale / dxs * sxs;
        }
        if mod_y {
            let scale = buf_h as f64 / ysize;
            let out_yo = (uly - yoff) * scale;
            oyo = if out_yo <= 0.0 { 0 } else { (out_yo + EPS) as i64 };
            let delta = (out_yo - oyo as f64) / scale / dys * sys;
            fyo -= delta;
            fys += delta;
            let out_bottom = (lry - yoff) * scale;
            if out_bottom < out_yo {
                return None;
            }
            let bottom = (out_bottom - EPS).ceil() as i64;
            oys = (bottom - oyo).min(buf_h as i64 - oyo);
            fys += (bottom as f64 - out_bottom) / scale / dys * sys;
        }
        if oxs <= 0 || oys <= 0 {
            return None;
        }
        Some(Window {
            req_off: (ixo as usize, iyo as usize),
            req_size: (ixs as usize, iys as usize),
            dreq_off: (fxo, fyo),
            dreq_size: (fxs, fys),
            out_off: (oxo as usize, oyo as usize),
            out_size: (oxs as usize, oys as usize),
        })
    }
}

/// A read of source pixels one for one ([`SourceRead::plain_copy`]): source pixel
/// `(src.0 + c, src.1 + r)` goes to destination pixel `(dst_x + c, dst_rows.start + r)`, for `c`
/// below `cols`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlainCopy {
    pub src: (usize, usize),
    pub dst_x: usize,
    pub cols: usize,
    pub dst_rows: std::ops::Range<usize>,
}

struct Window {
    req_off: (usize, usize),
    req_size: (usize, usize),
    dreq_off: (f64, f64),
    dreq_size: (f64, f64),
    out_off: (usize, usize),
    out_size: (usize, usize),
}

/// The type GDAL resamples a source type in (`GDALGetOvrWorkDataType()`) for the averaging and
/// convolution methods.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkType {
    U8,
    U16,
    F32,
    F64,
}

impl WorkType {
    fn of(dt: DataType) -> Self {
        match dt {
            DataType::U8 => Self::U8,
            DataType::U16 => Self::U16,
            DataType::I8 | DataType::I16 | DataType::F32 => Self::F32,
            _ => Self::F64,
        }
    }

    /// A value in the type, as `static_cast<T>()` makes it.
    fn cast(self, v: f64) -> f64 {
        match self {
            Self::U8 => v as u8 as f64,
            Self::U16 => v as u16 as f64,
            Self::F32 => v as f32 as f64,
            Self::F64 => v,
        }
    }

    /// `GDALGetNoDataReplacementValue()`: what a computed value equal to the nodata value
    /// becomes.
    fn nodata_replacement(self, nodata: f64) -> f64 {
        let (min, max) = match self {
            Self::U8 => (0.0, 255.0),
            Self::U16 => (0.0, 65535.0),
            Self::F32 => (f32::MIN as f64, f32::MAX as f64),
            Self::F64 => (f64::MIN, f64::MAX),
        };
        if nodata > max || nodata < min {
            return 0.0;
        }
        match self {
            Self::U8 | Self::U16 if nodata == max => max - 1.0,
            Self::U8 | Self::U16 => nodata + 1.0,
            Self::F32 => {
                let f = nodata as f32;
                (if nodata == max { f.next_down() } else { f.next_up() }) as f64
            }
            Self::F64 => {
                if nodata == max {
                    nodata.next_down()
                } else {
                    nodata.next_up()
                }
            }
        }
    }
}

/// The chunks `GDALRasterBand::RasterIOResampled()` processes a request in: destination
/// windows `(x, y, width, height)`, each with the source window `(x0, y0, x1, y1)` it reads
/// (with `radius` pixels of margin). Chunks hold at most 1024 × 1024 source pixels; a raster
/// whose block is as wide as the request would be cut in height first, which is not known here,
/// so the larger dimension is cut, as for tiled rasters.
fn gdal_chunks(
    s: &SourceRead,
    req_off: (usize, usize),
    ratio: (f64, f64),
    out: (usize, usize),
    radius: usize,
) -> Vec<([usize; 4], [usize; 4])> {
    let (w, h) = (s.width as f64, s.height as f64);
    let (mut bw, mut bh) = out;
    loop {
        let cx = (3.0 + bw as f64 * ratio.0).min(w) as usize;
        let cy = (3.0 + bh as f64 * ratio.1).min(h) as usize;
        if (bw == 1 && bh == 1) || cx * cy <= 1024 * 1024 {
            break;
        }
        if bw > 1 && (cx > cy || bh == 1) {
            bw /= 2;
        } else {
            bh /= 2;
        }
    }
    let factor = |r: f64| ((0.5 + r) as usize).max(1);
    let (mx, my) = (radius * factor(ratio.0), radius * factor(ratio.1));
    let mut chunks = Vec::new();
    for dy in (0..out.1).step_by(bh) {
        let ny = bh.min(out.1 - dy);
        let y0 = req_off.1 + (dy as f64 * ratio.1) as usize;
        let y1 = (req_off.1 + 1 + ((dy + ny) as f64 * ratio.1).ceil() as usize).min(s.height);
        for dx in (0..out.0).step_by(bw) {
            let nx = bw.min(out.0 - dx);
            let x0 = req_off.0 + (dx as f64 * ratio.0) as usize;
            let x1 = (req_off.0 + 1 + ((dx + nx) as f64 * ratio.0).ceil() as usize).min(s.width);
            chunks
                .push(([dx, dy, nx, ny], [x0.saturating_sub(mx), y0.saturating_sub(my), (x1 + mx).min(s.width), (y1 + my).min(s.height)]));
        }
    }
    chunks
}

/// Reserve optional Float64 source-resampling samples (reference: GDAL 3.13.3).
/// They coexist with the destination and borrowed source/mask buffers.
fn resampling_values(s: &SourceRead, width: usize, height: usize, retained: usize) -> Result<Vec<Option<f64>>> {
    let what = "a source resampling working set";
    let n = crate::dataset::fallible::len(&[width, height], what)?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let bytes = n
        .checked_mul(std::mem::size_of::<Option<f64>>())
        .filter(|&n| n <= isize::MAX as usize)
        .and_then(|n| n.checked_add(retained))
        .and_then(|n| n.checked_add(s.values.byte_len()))
        .and_then(|n| n.checked_add(s.valid.map_or(0, |v| v.len())));
    crate::dataset::fallible::check_usable(bytes, || format!("{what} is too large to allocate")).map_err(Error::ResourceLimit)?;
    Ok(crate::dataset::fallible::filled(n, None, what)?)
}

/// Averages a source window into `out` pixels with the arithmetic of GDAL's overview code
/// (`GDALResampleChunk_AverageOrRMS_T()`, through `GDALRasterBand::RasterIOResampled()`): per
/// chunk, a weighted sum scaled by the pixel count where the chunk has no nodata pixel (and, at a
/// factor of two, GDAL's sum of quarters in the work type), a sum over the valid pixels otherwise,
/// in the work type of the source's data type; `None` where no pixel is valid.
pub(crate) fn gdal_average(
    s: &SourceRead,
    req_off: (usize, usize),
    win: (f64, f64, f64, f64),
    out: (usize, usize),
    retained: usize,
) -> Result<Vec<Option<f64>>> {
    gdal_average_with_origin(s, req_off, win, out, None, retained)
}

/// Average with an explicit RasterIO reference frame, preserving virtual-output offsets.
pub(crate) fn gdal_average_with_origin(
    s: &SourceRead,
    req_off: (usize, usize),
    win: (f64, f64, f64, f64),
    out: (usize, usize),
    origin: Option<[f64; 8]>,
    retained: usize,
) -> Result<Vec<Option<f64>>> {
    let (fx, fy, fw, fh) = win;
    let (ow, oh) = out;
    let mut res = resampling_values(s, ow, oh, retained)?;
    if ow == 0 || oh == 0 {
        return Ok(res);
    }
    let ratio = (fw / ow as f64, fh / oh as f64);
    // Offsets are relative to the request's integer origin, as in GDAL.
    let o = origin.unwrap_or([
        0.,
        0.,
        -(req_off.0 as f64),
        -(req_off.1 as f64),
        fx - req_off.0 as f64,
        fy - req_off.1 as f64,
        ratio.0,
        ratio.1,
    ]);
    let ratio = (o[6], o[7]);
    let delta = (o[4], o[5]);
    let source_origin = (o[2] as i64, o[3] as i64);
    let wt = WorkType::of(s.src_type);
    let float = matches!(wt, WorkType::F32 | WorkType::F64);
    let nodata = s.valid.and(s.invalid_value);
    let replace = |v: f64| match nodata {
        Some(nd) if v == wt.cast(nd) => wt.nodata_replacement(nd),
        _ => v,
    };
    let val = |x: usize, y: usize| wt.cast(s.values.get(y * s.width + x));
    let valid = |x: usize, y: usize| s.valid.is_none_or(|m| m[y * s.width + x]);
    // Within its rectangles, a read averages the part of each destination pixel inside them:
    // footprints are clipped to `src_rect`, request-relative `[x0, y0, x1, y1]`, and only the
    // source pixels that meet it are read.
    let clip = (origin.is_none() && s.within_rects).then(|| {
        let [x0, y0, x1, y1] = s.src_bounds();
        let (ox, oy) = (req_off.0 as f64, req_off.1 as f64);
        [x0 - ox, y0 - oy, x1 - ox, y1 - oy]
    });
    let cover = s.cover();
    for (dst, src) in gdal_chunks(s, req_off, ratio, out, 0) {
        let [dx0, dy0, nx, ny] = dst;
        let [mut cx0, mut cy0, mut cx1, mut cy1] = src;
        if clip.is_some() {
            (cx0, cy0) = (cx0.max(cover[0]), cy0.max(cover[1]));
            (cx1, cy1) = (cx1.min(cover[2]).max(cx0), cy1.min(cover[3]).max(cy0));
        }
        // GDAL reads the chunk's mask: all valid, it averages without one; none valid, nodata.
        let mut any_valid = false;
        let mut all_valid = true;
        for y in cy0..cy1 {
            for x in cx0..cx1 {
                let v = valid(x, y);
                any_valid |= v;
                all_valid &= v;
            }
        }
        if !any_valid {
            continue;
        }
        let masked = !all_valid;
        // Request-relative chunk bounds.
        let (rx0, rx1) = (cx0 as i64 + source_origin.0, cx1 as i64 + source_origin.0);
        let (ry0, ry1) = (cy0 as i64 + source_origin.1, cy1 as i64 + source_origin.1);
        struct SrcX {
            x0: i64,
            x1: i64,
            left: f64,
            right: f64,
            full: f64,
            empty: bool,
        }
        let mut spacing_two = true;
        let mut last_x1 = -1i64;
        let xs: Vec<SrcX> = (dx0..dx0 + nx)
            .map(|i| {
                let a = delta.0 + (i as f64 + o[0]) * ratio.0;
                let b = delta.0 + (i as f64 + o[0] + 1.) * ratio.0;
                let (a, b, clipped) = match clip {
                    Some(c) if a < c[0] || b > c[2] => (a.max(c[0]), b.min(c[2]), true),
                    _ => (a, b, false),
                };
                // A clipped footprint is not two whole pixels.
                spacing_two &= !clipped;
                let x0 = ((a + 1e-8) as i64).max(rx0);
                let mut x1 = (b - 1e-8).ceil() as i64;
                if x1 == x0 {
                    x1 += 1;
                }
                x1 = x1.min(rx1);
                let left = if x1 == x0 + 1 { 1.0 } else { 1.0 - (a - x0 as f64) };
                let right = 1.0 - (x1 as f64 - b);
                let mut full = left;
                if x0 + 1 < x1 {
                    full += (x1 - x0 - 2) as f64;
                    full += right;
                }
                if x1 - x0 != 2 || (last_x1 >= 0 && last_x1 != x0) {
                    spacing_two = false;
                }
                last_x1 = x1;
                SrcX { x0, x1, left, right, full, empty: clipped && b <= a }
            })
            .collect();
        // Absolute source coordinates of request-relative ones.
        let ax = |x: i64| (x - source_origin.0) as usize;
        let ay = |y: i64| (y - source_origin.1) as usize;
        for j in dy0..dy0 + ny {
            let a = delta.1 + (j as f64 + o[1]) * ratio.1;
            let b = delta.1 + (j as f64 + o[1] + 1.) * ratio.1;
            let (a, b, clipped) = match clip {
                Some(c) if a < c[1] || b > c[3] => (a.max(c[1]), b.min(c[3]), true),
                _ => (a, b, false),
            };
            if clipped && b <= a {
                continue;
            }
            let y0 = ((a + 1e-8) as i64).max(ry0);
            let mut y1 = (b - 1e-8).ceil() as i64;
            if y1 == y0 {
                y1 += 1;
            }
            y1 = y1.min(ry1);
            let line = &mut res[j * ow..(j + 1) * ow];
            if spacing_two && !clipped && y1 == y0 + 2 && !masked {
                // A factor of two without nodata.
                let (r0, r1) = (ay(y0), ay(y0 + 1));
                let quad = |c: usize| {
                    let x = ax(xs[c].x0);
                    (val(x, r0), val(x + 1, r0), val(x, r1), val(x + 1, r1))
                };
                match wt {
                    WorkType::U8 | WorkType::U16 => {
                        for c in 0..nx {
                            let (p, q, r, t) = quad(c);
                            line[dx0 + c] = Some(((p + q + r + t + 2.0) / 4.0).floor());
                        }
                    }
                    WorkType::F32 => {
                        let q = |v: f64| 0.25f32 * v as f32;
                        let mut c = 0;
                        // SSE2, four pixels at a time: (a + c) + (b + d) of the quarters.
                        while nx >= 4 && c < nx - 3 {
                            for k in c..c + 4 {
                                let (p0, p1, p2, p3) = quad(k);
                                line[dx0 + k] = Some(((q(p0) + q(p2)) + (q(p1) + q(p3))) as f64);
                            }
                            c += 4;
                        }
                        for k in c..nx {
                            let (p0, p1, p2, p3) = quad(k);
                            line[dx0 + k] = Some((q(p0) + q(p1) + q(p2) + q(p3)) as f64);
                        }
                    }
                    WorkType::F64 => {
                        let mut c = 0;
                        while nx >= 2 && c < nx - 1 {
                            for k in c..c + 2 {
                                let (p0, p1, p2, p3) = quad(k);
                                line[dx0 + k] = Some((0.25 * p0 + 0.25 * p2) + (0.25 * p1 + 0.25 * p3));
                            }
                            c += 2;
                        }
                        for k in c..nx {
                            let (p0, p1, p2, p3) = quad(k);
                            line[dx0 + k] = Some(0.25 * p0 + 0.25 * p1 + 0.25 * p2 + 0.25 * p3);
                        }
                    }
                }
                continue;
            }
            let bottom = if y0 + 1 == y1 { 1.0 } else { 1.0 - (a - y0 as f64) };
            let top = 1.0 - (y1 as f64 - b);
            let mut full_col = bottom;
            if y0 + 1 < y1 {
                full_col += (y1 - y0 - 2) as f64;
                full_col += top;
            }
            for (c, sx) in xs.iter().enumerate().filter(|(_, sx)| !sx.empty) {
                let (mut total, mut total_weight, mut mul) = (0.0, 0.0, 1.0);
                if !masked {
                    let mut inv = 1.0;
                    if float {
                        mul = ((y1 - y0) * (sx.x1 - sx.x0)) as f64;
                        inv = 1.0 / mul;
                    }
                    let mut counter = y1 - y0 - 1;
                    let mut wy = bottom;
                    let mut y = y0;
                    loop {
                        let row = ay(y);
                        let mut t = val(ax(sx.x0), row) * inv * sx.left;
                        if sx.x0 + 1 < sx.x1 {
                            for x in sx.x0 + 1..sx.x1 - 1 {
                                t += val(ax(x), row) * inv;
                            }
                            t += val(ax(sx.x1 - 1), row) * inv * sx.right;
                        }
                        total += t * wy;
                        counter -= 1;
                        if counter < 0 {
                            break;
                        }
                        y += 1;
                        wy = if counter == 0 { top } else { 1.0 };
                    }
                    total_weight = sx.full * full_col;
                } else {
                    let mut count = 0usize;
                    for y in y0..y1 {
                        let row = ay(y);
                        let (mut t, mut tw) = (0.0, 0.0);
                        if valid(ax(sx.x0), row) {
                            count += 1;
                            tw = sx.left;
                            t = val(ax(sx.x0), row) * sx.left;
                        }
                        if sx.x0 < sx.x1 - 1 {
                            for x in sx.x0 + 1..sx.x1 - 1 {
                                if valid(ax(x), row) {
                                    count += 1;
                                    tw += 1.0;
                                    t += val(ax(x), row);
                                }
                            }
                            if valid(ax(sx.x1 - 1), row) {
                                count += 1;
                                tw += sx.right;
                                t += val(ax(sx.x1 - 1), row) * sx.right;
                            }
                        }
                        let wy = if y == y0 {
                            bottom
                        } else if y + 1 == y1 {
                            top
                        } else {
                            1.0
                        };
                        total += t * wy;
                        total_weight += tw * wy;
                    }
                    if count == 0 {
                        continue;
                    }
                }
                let v = match wt {
                    WorkType::U8 | WorkType::U16 => wt.cast(total / total_weight + 0.5),
                    _ => wt.cast(mul * (total / total_weight)),
                };
                line[dx0 + c] = Some(replace(v));
            }
        }
    }
    Ok(res)
}

/// Peak area/convolution scratch, including concurrent rows and vector growth.
/// Reference: GDAL 3.13.3 area kernels; storage follows this implementation's value/weight pairs.
fn resampling_scratch_bytes(s: &SourceRead, ratio: (f64, f64), out: (usize, usize)) -> Option<usize> {
    if out.0 == 0 || out.1 == 0 {
        return Some(0);
    }
    if !s.resampling.is_area_based() {
        let radius = s.resampling.radius().max(1) as f64;
        let taps = |ratio: f64, limit: usize| ((2.0 * radius * ratio.max(1.0)).ceil() as usize).saturating_add(2).min(limit);
        let columns = taps(ratio.0, s.width);
        let rows = taps(ratio.1, s.height);
        // Horizontal taps are kept for every output column; each concurrent row holds
        // vertical weights and a row of Option<f64> intermediate values.
        let horizontal =
            columns.checked_mul(std::mem::size_of::<f64>())?.checked_add(std::mem::size_of::<(usize, Vec<f64>)>())?.checked_mul(out.0)?;
        let per_row = rows.checked_mul(std::mem::size_of::<f64>() + std::mem::size_of::<Option<f64>>())?;
        let workers = out.1.min(crate::par::threads());
        return per_row.checked_mul(workers)?.checked_add(horizontal).filter(|&n| n <= isize::MAX as usize);
    }
    let columns = (ratio.0.ceil() as usize).saturating_add(1).min(s.width);
    let rows = (ratio.1.ceil() as usize).saturating_add(1).min(s.height);
    let samples = columns.checked_mul(rows)?;
    if samples == 0 {
        return Some(0);
    }
    let capacity = samples.max(4).checked_next_power_of_two()?;
    let values = capacity.checked_mul(std::mem::size_of::<(f64, f64)>())?;
    // A growing vector can retain its old half-capacity while requesting the new buffer.
    let growing_values = values.checked_add(values / 2)?;
    let extra = match s.resampling {
        Resampling::Mode => {
            let distinct = match s.values {
                super::BandView::U8(_) | super::BandView::I8(_) => samples.min(256),
                super::BandView::U16(_) | super::BandView::I16(_) => samples.min(65536),
                _ => samples,
            };
            let counts = distinct.max(4).checked_next_power_of_two()?.checked_mul(std::mem::size_of::<(f64, f32)>())?;
            counts.checked_add(counts / 2)?
        }
        // Stable sorting may allocate auxiliary storage for the contributing pairs.
        Resampling::Med | Resampling::Q1 | Resampling::Q3 => samples.checked_mul(std::mem::size_of::<(f64, f64)>())?,
        _ => 0,
    };
    let per_row = growing_values.max(values.checked_add(extra)?);
    let workers = if out.1 == 1 { 1 } else { out.1.min(crate::par::threads()) };
    per_row.checked_mul(workers).filter(|&bytes| bytes <= isize::MAX as usize)
}

/// Resamples a floating-point source window to `out` pixels as GDAL's overview code does
/// (`GDALRasterIOResampled()`): separable convolution kernels stretched when downsampling,
/// coverage-weighted averages, or the most frequent value. A `sum` adds each valid source pixel
/// times the share of its area inside the destination pixel.
fn resample_window(s: &SourceRead, win: (f64, f64, f64, f64), out: (usize, usize), retained: usize) -> Result<Vec<Option<f64>>> {
    let (fx, fy, fw, fh) = win;
    let (ow, oh) = out;
    let (rx, ry) = (fw / ow as f64, fh / oh as f64);
    let is_valid = |i: usize| s.valid.is_none_or(|m| m[i]);
    let scratch = resampling_scratch_bytes(s, (rx, ry), out)
        .and_then(|scratch| retained.checked_add(scratch))
        .ok_or_else(|| Error::ResourceLimit("resampling scratch is too large to allocate".into()))?;
    let mut res = resampling_values(s, ow, oh, scratch)?;
    if ow == 0 {
        return Ok(res);
    }
    match s.resampling {
        Resampling::Average
        | Resampling::Rms
        | Resampling::Mode
        | Resampling::Min
        | Resampling::Max
        | Resampling::Med
        | Resampling::Q1
        | Resampling::Q3
        | Resampling::Sum => {
            // Within its rectangles, a destination pixel takes the part of its area inside them.
            let [bx0, by0, bx1, by1] = s.src_bounds();
            crate::par::chunks_mut(&mut res, ow, |r, line| {
                let mut vals: Vec<(f64, f64)> = Vec::new();
                let y0 = fy + r as f64 * ry;
                let y1 = y0 + ry;
                let (y0, y1) = (y0.max(by0), y1.min(by1));
                for (c, cell) in line.iter_mut().enumerate() {
                    let x0 = fx + c as f64 * rx;
                    let x1 = x0 + rx;
                    let (x0, x1) = (x0.max(bx0), x1.min(bx1));
                    if s.within_rects && (x1 <= x0 || y1 <= y0) {
                        continue;
                    }
                    vals.clear();
                    let iy0 = (y0 + 1e-8).floor().max(0.0) as usize;
                    let iy1 = ((y1 - 1e-8).ceil() as usize).min(s.height).max(iy0 + 1).min(s.height);
                    let ix0 = (x0 + 1e-8).floor().max(0.0) as usize;
                    let ix1 = ((x1 - 1e-8).ceil() as usize).min(s.width).max(ix0 + 1).min(s.width);
                    for y in iy0..iy1 {
                        let wy = (y1.min(y as f64 + 1.0) - y0.max(y as f64)).max(0.0);
                        for x in ix0..ix1 {
                            let i = y * s.width + x;
                            if !is_valid(i) {
                                continue;
                            }
                            let wx = (x1.min(x as f64 + 1.0) - x0.max(x as f64)).max(0.0);
                            let weight =
                                if matches!(s.resampling, Resampling::Average | Resampling::Rms | Resampling::Sum) { wx * wy } else { 1.0 };
                            if weight > 0.0 {
                                vals.push((s.values.get(i), weight));
                            }
                        }
                    }
                    *cell = super::warp::area_value(s.resampling, &mut vals, true);
                }
            });
        }
        method => {
            let radius = method.radius().max(1) as f64;
            // A kernel's taps on the source pixels the read uses, `[lo, hi)`.
            let taps = |dst: usize, ratio: f64, off: f64, lo: usize, hi: usize| -> (usize, Vec<f64>) {
                let center = (dst as f64 + 0.5) * ratio + off;
                let scale = if 1.0 / ratio >= 1.0 { 1.0 } else { 1.0 / ratio };
                let r = radius / scale;
                let start = ((center - r + 0.5).floor() as i64).max(lo as i64) as usize;
                let stop = ((center + r + 0.5) as i64).min(hi as i64).max(start as i64) as usize;
                let w = (start..stop).map(|p| method.weight(scale * (p as f64 - center + 0.5))).collect();
                (start, w)
            };
            let [sx0, sy0, sx1, sy1] = s.cover();
            let xt: Vec<(usize, Vec<f64>)> = (0..ow).map(|c| taps(c, rx, fx, sx0, sx1)).collect();
            let negative_weights = matches!(method, Resampling::Cubic | Resampling::Lanczos);
            // GDAL's work type is `float` for these types (`GDALGetOvrWorkDataType`), `double` else:
            // the result is rounded to float for them. The sums themselves stay in f64.
            let single = matches!(s.src_type, DataType::U8 | DataType::U16 | DataType::I8 | DataType::I16 | DataType::F32);
            let work = |v: f64| if single { v as f32 as f64 } else { v };
            crate::par::chunks_mut(&mut res, ow, |r, line| {
                let (y0, wys) = taps(r, ry, fy, sy0, sy1);
                // GDAL's `GDALResampleChunk_ConvolutionT` filters rows first: each row is the
                // average of its valid pixels by their horizontal weights, or invalid when it has
                // none (or too few consecutive ones for a kernel with negative weights). The
                // column of rows is then the average of its valid rows by their vertical weights,
                // invalid by the same rules. With nodata, a pass keeps a total weight above 0.0,
                // with no epsilon, as GDAL 3.13.3 does (`dfWeightSum > 0.0` in both passes of
                // `GDALResampleChunk_ConvolutionT`, gcore/overview.cpp).
                let mut rows: Vec<Option<f64>> = Vec::with_capacity(wys.len());
                for c in 0..ow {
                    let (x0, wxs) = &xt[c];
                    rows.clear();
                    for j in 0..wys.len() {
                        let row = (y0 + j) * s.width;
                        let (mut acc, mut wsum) = (0.0, 0.0);
                        // An infinite nodata value (weight 0) makes the row NaN, as GDAL adds
                        // `value * weight * mask` for every pixel.
                        let mut infinite = false;
                        let (mut run, mut longest) = (0, 0);
                        for (i, wx) in wxs.iter().enumerate() {
                            let idx = row + x0 + i;
                            if is_valid(idx) {
                                acc += s.values.get(idx) * wx;
                                wsum += wx;
                                run += 1;
                                longest = longest.max(run);
                            } else {
                                infinite |= s.values.get(idx).is_infinite();
                                run = 0;
                            }
                        }
                        let skipped = negative_weights && longest < wxs.len() / 2;
                        rows.push(if skipped || (s.valid.is_some() && wsum <= 0.0) {
                            None
                        } else if infinite {
                            Some(f64::NAN)
                        } else if wsum != 0.0 {
                            Some(acc / wsum)
                        } else {
                            Some(acc)
                        });
                    }
                    let (mut acc, mut wsum) = (0.0, 0.0);
                    let (mut run, mut longest) = (0, 0);
                    for (h, wy) in rows.iter().zip(&wys) {
                        if let Some(h) = h {
                            acc += h * wy;
                            wsum += wy;
                            run += 1;
                            longest = longest.max(run);
                        } else {
                            run = 0;
                        }
                    }
                    if s.valid.is_some() {
                        if !(negative_weights && longest < wys.len() / 2) && wsum > 0.0 {
                            line[c] = Some(work(acc / wsum));
                        }
                    } else if wsum != 0.0 {
                        line[c] = Some(work(acc / wsum));
                    }
                }
            });
        }
    }
    Ok(res)
}

// ─── A small XML reader ─────────────────────────────────────────────────────────────────────────

pub(crate) mod xml {
    use crate::{Error, Result};

    /// An element: name, attributes, text and children.
    #[derive(Debug, Default, Clone)]
    pub struct Element {
        pub name: String,
        pub attrs: Vec<(String, String)>,
        pub text: String,
        pub children: Vec<Element>,
    }

    impl Element {
        pub fn to_xml(&self) -> String {
            let attrs = self.attrs.iter().map(|(k, v)| format!(" {k}=\"{}\"", escape(v))).collect::<String>();
            let children = self.children.iter().map(Self::to_xml).collect::<String>();
            let text = if matches!(self.name.as_str(), "SourceFilename" | "SourceDataset") {
                escape_filename(&self.text)
            } else {
                escape(&self.text)
            };
            format!("<{}{attrs}>{text}{children}</{}>", self.name, self.name)
        }

        pub fn attr(&self, name: &str) -> Option<&str> {
            self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        }

        pub fn child(&self, name: &str) -> Option<&Element> {
            self.children.iter().find(|c| c.name == name)
        }
    }

    pub fn escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    /// Preserve leading filename whitespace through CPL MiniXML's literal-space skipping.
    pub fn escape_filename(s: &str) -> String {
        let prefix = s.len() - s.trim_start_matches([' ', '\t', '\r', '\n']).len();
        let mut text = String::new();
        for byte in s[..prefix].bytes() {
            text.push_str(&format!("&#{byte};"));
        }
        text.push_str(&escape(&s[prefix..]));
        text
    }

    fn unescape(s: &str) -> String {
        if !s.contains('&') {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(i) = rest.find('&') {
            out.push_str(&rest[..i]);
            let after = &rest[i..];
            let end = after.find(';').unwrap_or(0);
            let ent = &after[1..end.max(1)];
            let ch = match ent {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
                e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            match ch {
                Some(c) if end > 0 => {
                    out.push(c);
                    rest = &after[end + 1..];
                }
                _ => {
                    out.push('&');
                    rest = &after[1..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Parses a document and returns its root element.
    pub fn parse(text: &str) -> Result<Element> {
        let err = |m: &str| Error::InvalidData(format!("VRT XML: {m}"));
        let b = text.as_bytes();
        let mut i = 0;
        let mut stack: Vec<Element> = vec![Element::default()];
        let mut cdata_end = usize::MAX;
        while i < b.len() {
            if b[i] != b'<' {
                let end = text[i..].find('<').map_or(b.len(), |k| i + k);
                if let Some(top) = stack.last_mut() {
                    let mut value = &text[i..end];
                    if matches!(top.name.as_str(), "SourceFilename" | "SourceDataset") {
                        if i == cdata_end {
                            if !value.trim().is_empty() {
                                return Err(err("mixed XML source filename content"));
                            }
                            value = "";
                        } else if top.text.is_empty() {
                            value = value.trim_start_matches([' ', '\t', '\r', '\n']);
                        }
                    }
                    top.text.push_str(&unescape(value));
                }
                i = end;
                continue;
            }
            if text[i..].starts_with("<![CDATA[") {
                let end = text[i + 9..].find("]]>").map(|k| i + 9 + k).ok_or_else(|| err("unterminated CDATA"))?;
                if let Some(top) = stack.last_mut() {
                    if matches!(top.name.as_str(), "SourceFilename" | "SourceDataset") && !top.text.is_empty() {
                        return Err(err("mixed XML source filename content"));
                    }
                    top.text.push_str(&text[i + 9..end]);
                }
                i = end + 3;
                cdata_end = i;
            } else if text[i..].starts_with("<!--") {
                if stack.last().is_some_and(|top| matches!(top.name.as_str(), "SourceFilename" | "SourceDataset")) {
                    return Err(err("invalid XML source filename content"));
                }
                i = text[i..].find("-->").map(|k| i + k + 3).ok_or_else(|| err("unterminated comment"))?;
            } else if text[i..].starts_with("<?") || text[i..].starts_with("<!") {
                i = text[i..].find('>').map(|k| i + k + 1).ok_or_else(|| err("unterminated declaration"))?;
            } else if text[i..].starts_with("</") {
                let end = text[i..].find('>').map(|k| i + k).ok_or_else(|| err("unterminated end tag"))?;
                let name = text[i + 2..end].trim();
                let el = stack.pop().ok_or_else(|| err("unbalanced tags"))?;
                if el.name != name {
                    return Err(err(&format!("</{name}> closes <{}>", el.name)));
                }
                stack.last_mut().ok_or_else(|| err("unbalanced tags"))?.children.push(el);
                i = end + 1;
            } else {
                // Find the end of the tag, skipping quoted attribute values.
                let mut j = i + 1;
                let mut quote = None;
                while j < b.len() {
                    match (quote, b[j]) {
                        (None, b'"' | b'\'') => quote = Some(b[j]),
                        (Some(q), c) if c == q => quote = None,
                        (None, b'>') => break,
                        _ => {}
                    }
                    j += 1;
                }
                if j >= b.len() {
                    return Err(err("unterminated tag"));
                }
                let self_closing = b[j - 1] == b'/';
                let inner = &text[i + 1..if self_closing { j - 1 } else { j }];
                let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
                let mut el = Element { name: inner[..name_end].to_string(), ..Default::default() };
                let mut rest = inner[name_end..].trim_start();
                while !rest.is_empty() {
                    let eq = rest.find('=').ok_or_else(|| err("attribute without value"))?;
                    let key = rest[..eq].trim().to_string();
                    let after = rest[eq + 1..].trim_start();
                    let q = after.chars().next().ok_or_else(|| err("attribute without value"))?;
                    if q != '"' && q != '\'' {
                        return Err(err("unquoted attribute"));
                    }
                    let close = after[1..].find(q).ok_or_else(|| err("unterminated attribute"))?;
                    el.attrs.push((key, unescape(&after[1..1 + close])));
                    rest = after[close + 2..].trim_start();
                }
                if self_closing {
                    stack.last_mut().ok_or_else(|| err("unbalanced tags"))?.children.push(el);
                } else {
                    stack.push(el);
                }
                i = j + 1;
            }
        }
        if stack.len() != 1 {
            return Err(err("unclosed elements"));
        }
        stack.pop().and_then(|mut d| d.children.pop()).ok_or_else(|| err("empty document"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_writes_xml() {
        let text = r#"<?xml version="1.0"?>
<!-- a comment -->
<VRTDataset rasterXSize="20" rasterYSize="10">
  <SRS dataAxisToSRSAxisMapping="1,2">EPSG:32610</SRS>
  <GeoTransform>  5.0e5,  3.0e1,  0.0,  4.0e6,  0.0, -3.0e1</GeoTransform>
  <VRTRasterBand dataType="Int16" band="1">
    <NoDataValue>-32768</NoDataValue>
    <ColorInterp>Gray</ColorInterp>
    <ComplexSource>
      <SourceFilename relativeToVRT="1">a &amp; b.tif</SourceFilename>
      <SourceBand>1</SourceBand>
      <SrcRect xOff="0" yOff="0" xSize="10" ySize="10" />
      <DstRect xOff="10" yOff="0" xSize="10" ySize="10" />
      <NODATA>0</NODATA>
    </ComplexSource>
  </VRTRasterBand>
</VRTDataset>"#;
        let v = VrtDataset::from_xml(text).unwrap();
        assert_eq!((v.width, v.height), (20, 10));
        assert_eq!(v.geo_transform.unwrap()[5], -30.0);
        assert_eq!(v.srs.as_ref().unwrap().epsg, Some(32610));
        let s = &v.bands[0].sources[0];
        assert_eq!(s.filename, "a & b.tif");
        assert!(s.relative_to_vrt && s.complex);
        assert_eq!(s.dst_rect, [10.0, 0.0, 10.0, 10.0]);
        assert_eq!(s.nodata, Some(0.0));
        assert_eq!(v.bands[0].data_type, DataType::I16);
        let again = VrtDataset::from_xml(&v.to_xml()).unwrap();
        assert_eq!(again.bands, v.bands);
        assert_eq!(again.geo_transform, v.geo_transform);
    }

    #[test]
    fn reads_windows() {
        // A 4×4 source placed at (2, 1) in a 8×6 destination.
        let values: Vec<f64> = (0..16).map(f64::from).collect();
        let r = SourceRead {
            values: crate::ops::BandView::F64(&values),
            valid: None,
            width: 4,
            height: 4,
            src_type: DataType::F32,
            src_rect: [0.0, 0.0, 4.0, 4.0],
            dst_rect: [2.0, 1.0, 4.0, 4.0],
            resampling: Resampling::Nearest,
            working_float: false,
            invalid_value: None,
            within_rects: true,
        };
        let mut out = vec![-1.0; 48];
        r.read(8, 6, &mut out, |v| v).unwrap();
        assert_eq!(out[8 + 2], 0.0);
        assert_eq!(out[4 * 8 + 5], 15.0);
        assert_eq!(out[0], -1.0);
        // Downsampling by 2 with average.
        let r = SourceRead { dst_rect: [0.0, 0.0, 2.0, 2.0], resampling: Resampling::Average, ..r };
        let mut out = vec![-1.0; 4];
        r.read(2, 2, &mut out, |v| v).unwrap();
        assert_eq!(out, vec![2.5, 4.5, 10.5, 12.5]);
    }

    /// Interpolating kernels weigh nodata pixels as GDAL does: each source row is averaged over
    /// its valid pixels by their horizontal weights, then the rows over the valid ones by their
    /// vertical weights, rather than all valid weights at once. The expected values are those of
    /// `gdalbuildvrt -r <method> -tr 5 5` (GDAL 3.13.3) read with `gdal_translate`, for a 20 x 20
    /// Float32 tile of 10 m pixels whose pixel (row 7, column 9) is nodata.
    #[test]
    fn kernels_average_each_row_of_a_source_with_nodata_over_its_valid_pixels() {
        let mut values: Vec<f64> = (0..400).map(|k| ((k % 37) * 3 + k / 20) as f64).collect();
        values[7 * 20 + 9] = -9999.0;
        let valid: Vec<bool> = values.iter().map(|v| *v != -9999.0).collect();
        let read = |resampling| {
            let r = SourceRead {
                values: crate::ops::BandView::F64(&values),
                valid: Some(&valid),
                width: 20,
                height: 20,
                src_type: DataType::F32,
                src_rect: [0.0, 0.0, 20.0, 20.0],
                dst_rect: [0.0, 0.0, 40.0, 40.0],
                resampling,
                working_float: false,
                invalid_value: None,
                within_rects: true,
            };
            let mut out = vec![f64::NAN; 1600];
            r.read(40, 40, &mut out, |v| v).unwrap();
            out
        };
        // The 16 pixels whose bilinear kernel reaches the nodata pixel, rows 13 to 16 and
        // columns 17 to 20. Weighing all valid weights at once gave 47.6 for the first and up to
        // 20.3 more than GDAL at (row 15, column 18).
        #[rustfmt::skip]
        let bilinear = [
            45.0625, 46.1875, 48.8125, 49.9375,
            19.6875, 20.0625, 24.9375, 25.3125,
            22.4375, 22.8125, 27.6875, 28.0625,
            53.3125, 54.4375, 57.0625, 58.1875,
        ];
        let out = read(Resampling::Bilinear);
        for (i, expected) in bilinear.iter().enumerate() {
            let (row, col) = (13 + i / 4, 17 + i % 4);
            assert_eq!(out[row * 40 + col], *expected, "bilinear ({row}, {col})");
        }
        let out = read(Resampling::Cubic);
        for ((row, col), expected) in [((12, 16), 69.85305786132812), ((14, 18), -7.288028717041016), ((16, 20), 63.35245132446289)] {
            let got = out[row * 40 + col];
            assert!((got - expected).abs() <= 1e-5, "cubic ({row}, {col}): {got} vs {expected}");
        }
    }

    /// A source resampled with `sum` gives each destination pixel the part of each source pixel
    /// that lies inside it.
    #[test]
    fn sum_resampling_shares_each_source_pixel_by_the_area_inside_each_destination_pixel() {
        let values: Vec<f64> = (1..=9).map(f64::from).collect();
        // 2 × 2 pixels into 4 × 4: a quarter of a source pixel in each.
        let r = SourceRead {
            values: crate::ops::BandView::F64(&values[..4]),
            valid: None,
            width: 2,
            height: 2,
            src_type: DataType::F32,
            src_rect: [0.0, 0.0, 2.0, 2.0],
            dst_rect: [0.0, 0.0, 4.0, 4.0],
            resampling: Resampling::Sum,
            working_float: false,
            invalid_value: None,
            within_rects: true,
        };
        let mut out = vec![-1.0; 16];
        r.read(4, 4, &mut out, |v| v).unwrap();
        let quarters: Vec<f64> =
            [1.0, 1.0, 2.0, 2.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0, 3.0, 3.0, 4.0, 4.0].iter().map(|v| v / 4.0).collect();
        assert_eq!(out, quarters);
        // 3 × 3 pixels into 2 × 2: a source pixel whole, halves of two and a quarter of one in each.
        let r = SourceRead {
            values: crate::ops::BandView::F64(&values),
            width: 3,
            height: 3,
            src_rect: [0.0, 0.0, 3.0, 3.0],
            dst_rect: [0.0, 0.0, 2.0, 2.0],
            ..r
        };
        let mut out = vec![-1.0; 4];
        r.read(2, 2, &mut out, |v| v).unwrap();
        assert_eq!(out, vec![5.25, 8.25, 14.25, 17.25]);
    }

    /// A `sum` read of a source window that starts a fraction of a pixel in, or spans a fraction
    /// of a pixel more than the destination, gives each destination pixel the share of each source
    /// pixel inside it. Only a window of whole pixels, one per destination pixel, is copied.
    #[test]
    fn sum_resampling_shares_source_pixels_of_a_window_with_fractional_edges() {
        let values: Vec<f64> = (1..=9).map(|v| f64::from(v) * 10.0).collect();
        // What a destination pixel covering `[x0, x1] × [y0, y1]` of the 3 × 3 source holds.
        let share = |x0: f64, x1: f64, y0: f64, y1: f64| -> f64 {
            let mut total = 0.0;
            for (i, v) in values.iter().enumerate() {
                let (x, y) = ((i % 3) as f64, (i / 3) as f64);
                total += v * (x1.min(x + 1.0) - x0.max(x)).max(0.0) * (y1.min(y + 1.0) - y0.max(y)).max(0.0);
            }
            total
        };
        let base = SourceRead {
            values: crate::ops::BandView::F64(&values),
            valid: None,
            width: 3,
            height: 3,
            src_type: DataType::F64,
            src_rect: [0.0, 0.0, 2.0, 2.0],
            dst_rect: [0.0, 0.0, 2.0, 2.0],
            resampling: Resampling::Sum,
            working_float: false,
            invalid_value: None,
            within_rects: true,
        };
        for src_rect in [[0.25, 0.0, 2.0, 2.0], [0.0, 0.25, 2.0, 2.0], [0.0, 0.0, 2.25, 2.0], [0.0, 0.0, 2.0, 2.25]] {
            let r = SourceRead { src_rect, ..base };
            let mut out = vec![-1.0; 4];
            r.read(2, 2, &mut out, |v| v).unwrap();
            let [x, y, w, h] = src_rect;
            let (sx, sy) = (w / 2.0, h / 2.0);
            let expected: Vec<f64> = (0..4)
                .map(|i| {
                    let (c, r) = ((i % 2) as f64, (i / 2) as f64);
                    share(x + c * sx, x + (c + 1.0) * sx, y + r * sy, y + (r + 1.0) * sy)
                })
                .collect();
            assert_eq!(out, expected, "{src_rect:?}");
            assert_eq!(r.plain_copy(2, 2), Err(()), "{src_rect:?}");
        }
        // `[10, 20, 30]` read from x = 0.25.
        let r = SourceRead { src_rect: [0.25, 0.0, 2.0, 2.0], ..base };
        let mut out = vec![-1.0; 4];
        r.read(2, 2, &mut out, |v| v).unwrap();
        assert_eq!(out[..2], [12.5, 22.5]);
        let r = SourceRead { src_rect: [1.0, 1.0, 2.0, 2.0], ..base };
        assert_eq!(r.plain_copy(2, 2), Ok(Some(PlainCopy { src: (1, 1), dst_x: 0, cols: 2, dst_rows: 0..2 })));
    }

    /// A `sum` read is not rounded to the source's type: its fractions, and sums beyond that
    /// type's range, reach the caller, which stores them in the output's type. A Byte pixel of 1
    /// upsampled to 2 × 2 gives four 0.25 in a floating-point output, and four Byte pixels of 200
    /// summed into one give 800 in a UInt16 output.
    #[test]
    fn sum_resampling_is_not_rounded_to_the_source_type() {
        let one = [1.0];
        let r = SourceRead {
            values: crate::ops::BandView::F64(&one),
            valid: None,
            width: 1,
            height: 1,
            src_type: DataType::U8,
            src_rect: [0.0, 0.0, 1.0, 1.0],
            dst_rect: [0.0, 0.0, 2.0, 2.0],
            resampling: Resampling::Sum,
            working_float: false,
            invalid_value: None,
            within_rects: true,
        };
        let mut quarters = vec![-1.0; 4];
        r.read(2, 2, &mut quarters, |v| v).unwrap();
        let two_hundreds = [200.0; 4];
        let r = SourceRead {
            values: crate::ops::BandView::F64(&two_hundreds),
            width: 2,
            height: 2,
            src_rect: [0.0, 0.0, 2.0, 2.0],
            dst_rect: [0.0, 0.0, 1.0, 1.0],
            ..r
        };
        let mut total = vec![-1.0; 1];
        r.read(1, 1, &mut total, |v| convert_value(v, DataType::U16)).unwrap();
        assert_eq!((quarters, total), (vec![0.25; 4], vec![800.0]));
    }

    /// Within its rectangles, a read copies source pixels one for one only where a copy is what
    /// its method gives, along rows as along columns, over `[10, 20, 30, 40]`. A window a quarter
    /// pixel in is resampled, but by `nearest`. A window of whole pixels, 2.5 pixels long, is
    /// copied into the pixels whose centres it covers (`nearest`, `bilinear`) or into all those it
    /// meets (`average`), and shared by area by a `sum`. A window from 0.6 placed at 0.25 has as
    /// many source pixels as it writes, but the pixels nearest the centres start a pixel before.
    #[test]
    fn a_read_within_its_rectangles_copies_only_what_its_method_gives() {
        let values = [10.0, 20.0, 30.0, 40.0];
        for along_row in [true, false] {
            // The source's size, and the destination's.
            let ((width, height), (dw, dh)) = if along_row { ((4, 1), (3, 1)) } else { ((1, 4), (1, 3)) };
            let rect = |off: f64, len: f64| if along_row { [off, 0.0, len, 1.0] } else { [0.0, off, 1.0, len] };
            let copy = |n: usize| {
                let (cols, rows) = if along_row { (n, 1) } else { (1, n) };
                PlainCopy { src: (0, 0), dst_x: 0, cols, dst_rows: 0..rows }
            };
            let (quarter_in, whole) = ((rect(0.25, 2.0), rect(0.0, 2.0)), (rect(0.0, 2.5), rect(0.0, 2.5)));
            for ((src_rect, dst_rect), resampling, expected, copied) in [
                (quarter_in, Resampling::Nearest, [10.0, 20.0, -1.0], Some(2)),
                (quarter_in, Resampling::Bilinear, [12.5, 22.5, -1.0], None),
                (quarter_in, Resampling::Average, [12.5, 22.5, -1.0], None),
                (whole, Resampling::Nearest, [10.0, 20.0, -1.0], Some(2)),
                (whole, Resampling::Bilinear, [10.0, 20.0, -1.0], Some(2)),
                (whole, Resampling::Average, [10.0, 20.0, 30.0], Some(3)),
                (whole, Resampling::Sum, [10.0, 20.0, 15.0], None),
                ((rect(0.6, 2.5), rect(0.25, 2.5)), Resampling::Nearest, [10.0, 20.0, 30.0], None),
            ] {
                let r = SourceRead {
                    values: crate::ops::BandView::F64(&values),
                    valid: None,
                    width,
                    height,
                    src_type: DataType::F64,
                    src_rect,
                    dst_rect,
                    resampling,
                    working_float: false,
                    invalid_value: None,
                    within_rects: true,
                };
                let mut out = vec![-1.0; 3];
                r.read(dw, dh, &mut out, |v| v).unwrap();
                let case = format!("{resampling:?} {src_rect:?} at {dst_rect:?}");
                assert_eq!(out, expected, "{case}");
                assert_eq!(r.plain_copy(dw, dh), copied.map(|n| Some(copy(n))).ok_or(()), "{case}");
            }
        }
    }

    /// Within its rectangles, `nearest` reads no source pixel past the `SrcRect`, even for a
    /// destination pixel whose centre is inside the `DstRect` by less than the tolerance its
    /// sample takes: the first pixel of `[10, 20, 30]` upsampled 2.5 times into a `DstRect` 1.5e-10
    /// pixel longer, whose last centre samples the `SrcRect`'s edge.
    #[test]
    fn nearest_reads_no_pixel_past_the_src_rect_at_its_edge() {
        let values = [10.0, 20.0, 30.0];
        let r = SourceRead {
            values: crate::ops::BandView::F64(&values),
            valid: None,
            width: 3,
            height: 1,
            src_type: DataType::F64,
            src_rect: [0.0, 0.0, 1.0, 1.0],
            dst_rect: [0.0, 0.0, 2.5 + 1.5e-10, 1.0],
            resampling: Resampling::Nearest,
            working_float: false,
            invalid_value: None,
            within_rects: true,
        };
        let mut out = vec![-1.0; 3];
        r.read(3, 1, &mut out, |v| v).unwrap();
        assert_eq!(out, [10.0; 3]);
    }

    /// A VRT source reads only the pixels that meet its `SrcRect` and places them only inside its
    /// `DstRect`, whatever their offsets. A window of the same size with a fractional offset is
    /// resampled by the source's method, `nearest` picking the nearest pixel. A method sampling a
    /// point writes only the pixels whose centres are inside the `DstRect`; a method over an area
    /// gives a pixel partly inside it the part of the source inside it. Over the one-row sources
    /// `[10, 20, 30]` and `[10, 20, 30, 40]`, through each kind of source, opened, read lazily,
    /// read by windows of one pixel and materialized; -1, the band's nodata, is where the source
    /// writes nothing.
    #[test]
    fn a_source_reads_only_its_src_rect_into_its_dst_rect() {
        let dir = tempfile::tempdir().unwrap();
        let write_source = |values: &[f64]| {
            let path = dir.path().join(format!("source{}.tif", values.len())).to_string_lossy().into_owned();
            let grid = Grid { width: values.len(), height: 1, geo_transform: [0.0, 1.0, 0.0, 1.0, 0.0, -1.0] };
            let ds =
                crate::ops::build_raster(grid, vec![values.to_vec()], DataType::F32, None, &SpatialRef::default(), convert_value).unwrap();
            crate::write_raster_dataset(&ds, &path, &Default::default()).unwrap();
            path
        };
        let (three, four) = (write_source(&[10.0, 20.0, 30.0]), write_source(&[10.0, 20.0, 30.0, 40.0]));
        // (source, band width, SrcRect x offset, DstRect x offset, each method's row): both
        // rectangles are 2 x 1 pixels.
        let cases = [
            (
                &three,
                2,
                0.25,
                0.0,
                [
                    ("nearest", vec![10.0, 20.0]),
                    ("bilinear", vec![12.5, 22.5]),
                    ("average", vec![12.5, 22.5]),
                    ("sum", vec![12.5, 22.5]),
                    ("max", vec![20.0, 30.0]),
                    ("min", vec![10.0, 20.0]),
                ],
            ),
            (
                &three,
                3,
                0.0,
                0.25,
                [
                    ("nearest", vec![10.0, 20.0, -1.0]),
                    ("bilinear", vec![10.0, 17.5, -1.0]),
                    ("average", vec![10.0, 17.5, 20.0]),
                    ("sum", vec![7.5, 17.5, 5.0]),
                    ("max", vec![10.0, 20.0, 20.0]),
                    ("min", vec![10.0, 10.0, 20.0]),
                ],
            ),
            (
                &four,
                3,
                1.0,
                0.5,
                [
                    ("nearest", vec![20.0, 30.0, -1.0]),
                    ("bilinear", vec![20.0, 25.0, -1.0]),
                    ("average", vec![20.0, 25.0, 30.0]),
                    ("sum", vec![10.0, 25.0, 15.0]),
                    ("max", vec![20.0, 30.0, 30.0]),
                    ("min", vec![20.0, 20.0, 30.0]),
                ],
            ),
        ];
        let mut wrong = Vec::new();
        for (path, width, sx, dx, methods) in &cases {
            let average = methods.iter().find(|(m, _)| *m == "average").map(|(_, row)| row).unwrap();
            let sources = methods
                .iter()
                .flat_map(|(method, row)| ["SimpleSource", "ComplexSource", "KernelFilteredSource"].map(|tag| (tag, Some(*method), row)))
                .chain([("AveragedSource", None, average)]);
            for (tag, method, expected) in sources {
                let resampling = method.map(|m| format!(" resampling=\"{m}\"")).unwrap_or_default();
                // A kernel of one pixel of weight 1 leaves the source as it is.
                let kernel = if tag == "KernelFilteredSource" { "<Kernel><Size>1</Size><Coefs>1</Coefs></Kernel>" } else { "" };
                let text = format!(
                    "<VRTDataset rasterXSize=\"{width}\" rasterYSize=\"1\"><VRTRasterBand dataType=\"Float32\" band=\"1\">\
                     <NoDataValue>-1</NoDataValue><{tag}{resampling}><SourceFilename relativeToVRT=\"0\">{path}</SourceFilename>\
                     <SourceBand>1</SourceBand><SrcRect xOff=\"{sx}\" yOff=\"0\" xSize=\"2\" ySize=\"1\"/>\
                     <DstRect xOff=\"{dx}\" yOff=\"0\" xSize=\"2\" ySize=\"1\"/>{kernel}</{tag}></VRTRasterBand></VRTDataset>"
                );
                let file = dir.path().join("read.vrt");
                std::fs::write(&file, &text).unwrap();
                let file = file.to_string_lossy().into_owned();
                let mut lazy = crate::open_raster_lazy(&file).unwrap();
                let pixel = |c: usize| crate::Window::new(c, 0, 1, 1);
                let mut reads = vec![
                    ("opened", crate::open_raster(&file).unwrap().raster.data.to_f64_vec()),
                    ("read lazily", lazy.read().unwrap().raster.data.to_f64_vec()),
                    (
                        "read by windows of one pixel",
                        (0..*width).map(|c| lazy.read_window(pixel(c), &Default::default()).unwrap().raster.data.to_f64_vec()[0]).collect(),
                    ),
                ];
                // `VrtDataset` reads every kind of source but a kernel-filtered one.
                if tag != "KernelFilteredSource" {
                    let materialized = VrtDataset::from_xml(&text).unwrap().materialize(dir.path()).unwrap();
                    reads.push(("materialized", materialized.raster.data.to_f64_vec()));
                }
                let source = method.map_or(tag.to_string(), |m| format!("{tag} {m}"));
                for (how, got) in reads {
                    if &got != expected {
                        wrong.push(format!("{source}, SrcRect x {sx} at x {dx}, {how}: {got:?}, not {expected:?}"));
                    }
                }
            }
        }
        assert!(wrong.is_empty(), "{} reads differ:\n{}", wrong.len(), wrong.join("\n"));
    }
}
