//! GDAL virtual rasters (`.vrt`): reading, writing, and the source windowing they share with
//! [`translate`](super::translate).
//!
//! A VRT is an XML description of a raster whose bands are composed of windows of other rasters
//! ("sources"). terrio writes VRTs GDAL opens, and opens the VRTs GDAL writes for mosaics and
//! subsets: `SimpleSource`, `ComplexSource` (with `NODATA`, `ScaleOffset` and `ScaleRatio`) and
//! `AveragedSource`, with `SrcRect`/`DstRect` windows. Derived bands, kernel filters, pixel
//! functions and warped VRTs are not supported.

use super::{Grid, Resampling, band_f64, convert_value, data_type_name, is_nodata, parse_data_type};
use crate::crs::SpatialRef;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wbraster::DataType;

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
    /// Resampling when the source and destination windows differ in size.
    pub resampling: Resampling,
    /// The source's size and data type, written to help GDAL open the VRT lazily.
    pub properties: Option<(usize, usize, DataType)>,
}

impl VrtDataset {
    /// Reads a VRT file's description.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: &str) -> Result<Self> {
        Self::from_xml(&std::fs::read_to_string(path)?)
    }

    /// Parses a VRT description.
    pub fn from_xml(text: &str) -> Result<Self> {
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
        let srs = root.child("SRS").map(|s| SpatialRef::parse(s.text.trim())).transpose()?;
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
                if s.child("LUT").is_some() || s.child("Exponent").is_some() {
                    return Err(Error::Unsupported("VRT lookup tables and exponential scaling are not supported".into()));
                }
                let resampling = match s.attr("resampling") {
                    Some(r) => Resampling::parse(r)?,
                    None if s.name == "AveragedSource" => Resampling::Average,
                    None => Resampling::Nearest,
                };
                sources.push(VrtSource {
                    filename: fname.text.trim().to_string(),
                    relative_to_vrt: fname.attr("relativeToVRT").is_some_and(|v| v.trim() == "1"),
                    band,
                    src_rect,
                    dst_rect,
                    complex,
                    nodata,
                    scale,
                    resampling,
                    properties,
                });
            }
            bands.push(VrtBand { data_type, nodata, sources });
        }
        if bands.is_empty() {
            return Err(Error::InvalidData("VRT: no bands".into()));
        }
        Ok(Self { width, height, geo_transform, srs, bands })
    }

    /// The VRT's XML description, as GDAL writes it.
    pub fn to_xml(&self) -> String {
        let mut s = format!("<VRTDataset rasterXSize=\"{}\" rasterYSize=\"{}\">\n", self.width, self.height);
        // GDAL reads any CRS definition here; an EPSG code is the most portable.
        let srs = self.srs.as_ref().and_then(|s| s.epsg.map(|c| format!("EPSG:{c}")).or_else(|| s.to_wkt()));
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
                    xml::escape(&src.filename)
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
                }
                s += &format!("    </{tag}>\n");
            }
            s += "  </VRTRasterBand>\n";
        }
        s += "</VRTDataset>\n";
        s
    }

    /// Writes the VRT file.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn write(&self, path: &str) -> Result<()> {
        Ok(std::fs::write(path, self.to_xml())?)
    }

    /// Reads the pixels of the virtual raster. `base` is the directory relative source paths are
    /// relative to.
    pub fn materialize(&self, base: &Path) -> Result<RasterDataset> {
        let mut cache: HashMap<PathBuf, RasterDataset> = HashMap::new();
        let n = self.width * self.height;
        let data_type = self.bands[0].data_type;
        let nodata = self.bands[0].nodata;
        let mut out = Vec::with_capacity(self.bands.len());
        let mut srs = self.srs.clone();
        for band in &self.bands {
            let mut buf = vec![band.nodata.unwrap_or(0.0); n];
            for src in &band.sources {
                let path = if src.relative_to_vrt || Path::new(&src.filename).is_relative() {
                    base.join(&src.filename)
                } else {
                    PathBuf::from(&src.filename)
                };
                if !cache.contains_key(&path) {
                    let ds = crate::open_raster(&path.to_string_lossy())?;
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
                let values = band_f64(r, src.band - 1);
                let skip = if src.complex { src.nodata } else { None };
                let valid: Option<Vec<bool>> = skip.map(|nd| super::par_map(&values, |v| !is_nodata(*v, nd)));
                let src_rect = if src.src_rect[0].is_nan() { [0.0, 0.0, r.cols as f64, r.rows as f64] } else { src.src_rect };
                let req = SourceRead {
                    values: &values,
                    valid: valid.as_deref(),
                    width: r.cols,
                    height: r.rows,
                    src_type: r.data_type,
                    src_rect,
                    dst_rect: src.dst_rect,
                    resampling: src.resampling,
                    working_float: src.complex,
                    invalid_value: None,
                };
                req.read(self.width, self.height, &mut buf, |v| match src.scale {
                    Some((off, ratio)) => {
                        let v = v * ratio + off;
                        if is_float_working(r.data_type) { v } else { v as f32 as f64 }
                    }
                    None => v,
                });
            }
            out.push(buf);
        }
        let grid =
            Grid { width: self.width, height: self.height, geo_transform: self.geo_transform.unwrap_or([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]) };
        let mut ds = super::build_raster(grid, out, data_type, nodata, &srs.unwrap_or_default(), convert_value)?;
        ds.driver = RasterDriver::VRT;
        Ok(ds)
    }
}

/// Whether GDAL's complex sources work in double precision for this source type.
fn is_float_working(dt: DataType) -> bool {
    matches!(dt, DataType::F64 | DataType::I32 | DataType::U32 | DataType::I64 | DataType::U64)
}

/// Opens a VRT file and reads its pixels.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_vrt(path: &str) -> Result<RasterDataset> {
    let vrt = VrtDataset::open(path)?;
    let base = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
    vrt.materialize(&base)
}

/// Opens a VRT file and reads its pixels.
#[cfg(target_arch = "wasm32")]
pub fn read_vrt(_path: &str) -> Result<RasterDataset> {
    Err(Error::Unsupported("VRT files are not supported in WebAssembly".into()))
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
/// (`VRTSimpleSource::GetSrcDstWindow()` followed by `GDALRasterBand::RasterIO()`).
pub(crate) struct SourceRead<'a> {
    /// The source band's pixels.
    pub values: &'a [f64],
    /// Which source pixels are valid (`None`: all).
    pub valid: Option<&'a [bool]>,
    /// The source's size.
    pub width: usize,
    /// The source's height.
    pub height: usize,
    /// The source's data type (resampled values are rounded to it, as GDAL does).
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
}

impl SourceRead<'_> {
    /// Reads into a destination `out` of `dw × dh` pixels, setting each valid destination pixel
    /// the source covers to `f(value)`. Rows are read in parallel.
    pub fn read(&self, dw: usize, dh: usize, out: &mut [f64], f: impl Fn(f64) -> f64 + Sync + Send) {
        let Some(w) = self.window(dw as f64, dh as f64, dw, dh) else { return };
        let (ow, oh) = (w.out_size.0, w.out_size.1);
        if ow == 0 || oh == 0 {
            return;
        }
        let is_valid = |i: usize| self.valid.is_none_or(|m| m[i]);
        // The destination rows the source covers.
        let start = (w.out_off.1 * dw).min(out.len());
        let end = ((w.out_off.1 + oh) * dw).min(out.len());
        let rows = &mut out[start..end];
        let x0 = w.out_off.0;
        if w.req_size.0 == ow && w.req_size.1 == oh {
            crate::par::chunks_mut(rows, dw, |r, line| {
                let sy = w.req_off.1 + r;
                if sy >= self.height {
                    return;
                }
                for c in 0..ow {
                    let sx = w.req_off.0 + c;
                    if sx >= self.width {
                        continue;
                    }
                    let i = sy * self.width + sx;
                    if is_valid(i) {
                        line[x0 + c] = f(self.values[i]);
                    }
                }
            });
            return;
        }
        let (fx, fy, fw, fh) = (w.dreq_off.0, w.dreq_off.1, w.dreq_size.0, w.dreq_size.1);
        if self.resampling == Resampling::Nearest {
            const EPS: f64 = 1e-10;
            let incx = fw / ow as f64;
            let incy = fh / oh as f64;
            crate::par::chunks_mut(rows, dw, |r, line| {
                let sy = ((r as f64 + 0.5) * incy + fy + EPS).max(0.0).min((self.height - 1) as f64) as usize;
                let mut sxf = 0.5 * incx + fx + EPS;
                for c in 0..ow {
                    let sx = sxf.max(0.0).min((self.width - 1) as f64) as usize;
                    sxf += incx;
                    let i = sy * self.width + sx;
                    if is_valid(i) {
                        line[x0 + c] = f(self.values[i]);
                    }
                }
            });
            return;
        }
        let res = resample_window(self, (fx, fy, fw, fh), (ow, oh));
        crate::par::chunks_mut(rows, dw, |r, line| {
            for c in 0..ow {
                match res[r * ow + c] {
                    Some(v) => line[x0 + c] = f(if self.working_float { v } else { convert_value(v, self.src_type) }),
                    None => {
                        if let Some(v) = self.invalid_value {
                            line[x0 + c] = f(v);
                        }
                    }
                }
            }
        });
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

struct Window {
    req_off: (usize, usize),
    req_size: (usize, usize),
    dreq_off: (f64, f64),
    dreq_size: (f64, f64),
    out_off: (usize, usize),
    out_size: (usize, usize),
}

/// Resamples a floating-point source window to `out` pixels as GDAL's overview code does
/// (`GDALRasterIOResampled()`): separable convolution kernels stretched when downsampling,
/// coverage-weighted averages, or the most frequent value.
fn resample_window(s: &SourceRead, win: (f64, f64, f64, f64), out: (usize, usize)) -> Vec<Option<f64>> {
    let (fx, fy, fw, fh) = win;
    let (ow, oh) = out;
    let (rx, ry) = (fw / ow as f64, fh / oh as f64);
    let is_valid = |i: usize| s.valid.is_none_or(|m| m[i]);
    let mut res = vec![None; ow * oh];
    if ow == 0 {
        return res;
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
            crate::par::chunks_mut(&mut res, ow, |r, line| {
                let mut vals: Vec<(f64, f64)> = Vec::new();
                let y0 = fy + r as f64 * ry;
                let y1 = y0 + ry;
                for (c, cell) in line.iter_mut().enumerate() {
                    let x0 = fx + c as f64 * rx;
                    let x1 = x0 + rx;
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
                            let weight = if s.resampling == Resampling::Average || s.resampling == Resampling::Rms { wx * wy } else { 1.0 };
                            if weight > 0.0 {
                                vals.push((s.values[i], weight));
                            }
                        }
                    }
                    *cell = super::warp::area_value(s.resampling, &mut vals, true);
                }
            });
        }
        method => {
            let radius = method.radius().max(1) as f64;
            let taps = |dst: usize, ratio: f64, off: f64, size: usize| -> (usize, Vec<f64>) {
                let center = (dst as f64 + 0.5) * ratio + off;
                let scale = if 1.0 / ratio >= 1.0 { 1.0 } else { 1.0 / ratio };
                let r = radius / scale;
                let start = ((center - r + 0.5).floor() as i64).max(0) as usize;
                let stop = ((center + r + 0.5) as i64).min(size as i64).max(start as i64) as usize;
                let w = (start..stop).map(|p| method.weight(scale * (p as f64 - center + 0.5))).collect();
                (start, w)
            };
            let xt: Vec<(usize, Vec<f64>)> = (0..ow).map(|c| taps(c, rx, fx, s.width)).collect();
            crate::par::chunks_mut(&mut res, ow, |r, line| {
                let (y0, wys) = taps(r, ry, fy, s.height);
                for c in 0..ow {
                    let (x0, wxs) = &xt[c];
                    let (mut acc, mut wsum) = (0.0, 0.0);
                    for (j, wy) in wys.iter().enumerate() {
                        let row = (y0 + j) * s.width;
                        for (i, wx) in wxs.iter().enumerate() {
                            let idx = row + x0 + i;
                            if is_valid(idx) {
                                acc += s.values[idx] * wx * wy;
                                wsum += wx * wy;
                            }
                        }
                    }
                    if wsum != 0.0 && (s.valid.is_none() || wsum > 1e-10) {
                        line[c] = Some(acc / wsum);
                    }
                }
            });
        }
    }
    res
}

// ─── A small XML reader ─────────────────────────────────────────────────────────────────────────

mod xml {
    use crate::{Error, Result};

    /// An element: name, attributes, text and children.
    #[derive(Debug, Default)]
    pub struct Element {
        pub name: String,
        pub attrs: Vec<(String, String)>,
        pub text: String,
        pub children: Vec<Element>,
    }

    impl Element {
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
        while i < b.len() {
            if b[i] != b'<' {
                let end = text[i..].find('<').map_or(b.len(), |k| i + k);
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&unescape(&text[i..end]));
                }
                i = end;
                continue;
            }
            if text[i..].starts_with("<!--") {
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
            values: &values,
            valid: None,
            width: 4,
            height: 4,
            src_type: DataType::F32,
            src_rect: [0.0, 0.0, 4.0, 4.0],
            dst_rect: [2.0, 1.0, 4.0, 4.0],
            resampling: Resampling::Nearest,
            working_float: false,
            invalid_value: None,
        };
        let mut out = vec![-1.0; 48];
        r.read(8, 6, &mut out, |v| v);
        assert_eq!(out[8 + 2], 0.0);
        assert_eq!(out[4 * 8 + 5], 15.0);
        assert_eq!(out[0], -1.0);
        // Downsampling by 2 with average.
        let r = SourceRead { dst_rect: [0.0, 0.0, 2.0, 2.0], resampling: Resampling::Average, ..r };
        let mut out = vec![-1.0; 4];
        r.read(2, 2, &mut out, |v| v);
        assert_eq!(out, vec![2.5, 4.5, 10.5, 12.5]);
    }
}
