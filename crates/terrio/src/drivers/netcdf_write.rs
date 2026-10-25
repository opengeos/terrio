//! NetCDF writing: the classic formats (CDF-1, CDF-2 with 64-bit offsets, and CDF-5 for 64-bit
//! integers) and netCDF-4 (an HDF5 file, see [`super::hdf5_write`]), laid out as GDAL's netCDF
//! driver writes rasters, following the CF conventions.
//!
//! - one data variable, `Band1` (or `band_data`, with a leading `band` dimension, for several
//!   bands), its rows stored south to north as GDAL stores them;
//! - `lon`/`lat` coordinate variables for geographic CRSs, `x`/`y` for the others, holding the
//!   pixel centres;
//! - a grid mapping variable with the CF projection parameters of the common projections, and the
//!   CRS as WKT in `crs_wkt` and `spatial_ref` and the geotransform in `GeoTransform`, as GDAL
//!   writes them;
//! - nodata as `_FillValue`; unsigned types in CDF-1/2 and the netCDF-4 classic model as signed
//!   ones with `_Unsigned = "true"`;
//! - in netCDF-4, the coordinate variables as HDF5 dimension scales, and with `COMPRESS=DEFLATE`
//!   every variable chunked (data variables by row, as GDAL chunks them) and compressed with the
//!   shuffle and deflate filters.
//!
//! The format is described at
//! <https://docs.unidata.ucar.edu/netcdf-c/current/file_format_specifications.html>.

use super::common::{self, Interleave, Layout};
use super::hdf5_write::{self, H5Type, Values};
use crate::{Error, Result};
use std::path::Path;
use wbraster::{DataType, Raster};

const NC_DIMENSION: u32 = 0x0A;
const NC_VARIABLE: u32 = 0x0B;
const NC_ATTRIBUTE: u32 = 0x0C;

/// NetCDF external types.
#[derive(Debug, Clone, Copy, PartialEq)]
enum NcType {
    Byte = 1,
    Char = 2,
    Short = 3,
    Int = 4,
    Float = 5,
    Double = 6,
    UByte = 7,
    UShort = 8,
    UInt = 9,
    Int64 = 10,
    UInt64 = 11,
}

/// An attribute value.
enum Attr {
    Text(String),
    Doubles(Vec<f64>),
    Shorts(Vec<i16>),
    /// Raw big-endian values of a type.
    Raw(NcType, usize, Vec<u8>),
}

struct Var {
    name: String,
    dims: Vec<usize>,
    attrs: Vec<(String, Attr)>,
    nc_type: NcType,
    /// The data, big-endian, unpadded.
    data: Vec<u8>,
}

/// Which format: the classic versions, or netCDF-4.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Version {
    /// CDF-1: 32-bit offsets.
    Classic = 1,
    /// CDF-2: 64-bit offsets.
    Offset64 = 2,
    /// CDF-5: 64-bit sizes and the unsigned and 64-bit integer types.
    Cdf5 = 5,
    /// NetCDF-4 (HDF5), with its unsigned and 64-bit integer types.
    Nc4 = 4,
    /// NetCDF-4 restricted to the classic model (the classic types).
    Nc4Classic = 3,
}

impl Version {
    /// Whether the format has the unsigned and 64-bit integer types.
    fn extended_types(self) -> bool {
        matches!(self, Self::Cdf5 | Self::Nc4)
    }
}

/// GDAL's netCDF creation options.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Options<'a> {
    /// `FORMAT`: `NC` (the default), `NC2`, `NC5`, `NC4` or `NC4C`.
    pub format: Option<&'a str>,
    /// `COMPRESS`: `NONE` or `DEFLATE` (netCDF-4 only: classic formats become `NC4C`, as in GDAL).
    pub compress: Option<&'a str>,
    /// `ZLEVEL`: the deflate level, 1 to 9 (default 1).
    pub zlevel: Option<&'a str>,
    /// `CHUNKING`: whether compressed variables are chunked by row (the default) or whole.
    pub chunking: Option<&'a str>,
    /// `GROUP`: the netCDF-4 group (`/science/grid`) the raster's variables go in, created with
    /// its parents (terrio's option; netCDF-4 only).
    pub group: Option<&'a str>,
}

struct Writer {
    version: Version,
    out: Vec<u8>,
}

impl Writer {
    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    /// A count or size: 32 bits, 64 in CDF-5.
    fn non_neg(&mut self, v: u64) {
        if self.version == Version::Cdf5 {
            self.out.extend_from_slice(&v.to_be_bytes());
        } else {
            self.u32(v as u32);
        }
    }

    fn pad(&mut self) {
        while !self.out.len().is_multiple_of(4) {
            self.out.push(0);
        }
    }

    fn name(&mut self, s: &str) {
        self.non_neg(s.len() as u64);
        self.out.extend_from_slice(s.as_bytes());
        self.pad();
    }

    fn attrs(&mut self, attrs: &[(String, Attr)]) {
        if attrs.is_empty() {
            self.u32(0);
            self.non_neg(0);
            return;
        }
        self.u32(NC_ATTRIBUTE);
        self.non_neg(attrs.len() as u64);
        for (name, a) in attrs {
            self.name(name);
            match a {
                Attr::Text(s) => {
                    self.u32(NcType::Char as u32);
                    self.non_neg(s.len() as u64);
                    self.out.extend_from_slice(s.as_bytes());
                }
                Attr::Doubles(v) => {
                    self.u32(NcType::Double as u32);
                    self.non_neg(v.len() as u64);
                    for x in v {
                        self.out.extend_from_slice(&x.to_be_bytes());
                    }
                }
                Attr::Shorts(v) => {
                    self.u32(NcType::Short as u32);
                    self.non_neg(v.len() as u64);
                    for x in v {
                        self.out.extend_from_slice(&x.to_be_bytes());
                    }
                }
                Attr::Raw(t, n, bytes) => {
                    self.u32(*t as u32);
                    self.non_neg(*n as u64);
                    self.out.extend_from_slice(bytes);
                }
            }
            self.pad();
        }
    }
}

fn type_size(t: NcType) -> usize {
    match t {
        NcType::Byte | NcType::Char | NcType::UByte => 1,
        NcType::Short | NcType::UShort => 2,
        NcType::Int | NcType::UInt | NcType::Float => 4,
        NcType::Double | NcType::Int64 | NcType::UInt64 => 8,
    }
}

/// Encodes a file: dimensions `(name, length)`, global attributes and variables.
fn encode(version: Version, dims: &[(String, usize)], globals: &[(String, Attr)], vars: &[Var]) -> Vec<u8> {
    let mut w = Writer { version, out: Vec::new() };
    w.out.extend_from_slice(&[b'C', b'D', b'F', version as u8]);
    w.non_neg(0); // no record variables
    w.u32(NC_DIMENSION);
    w.non_neg(dims.len() as u64);
    for (name, len) in dims {
        w.name(name);
        w.non_neg(*len as u64);
    }
    w.attrs(globals);
    w.u32(NC_VARIABLE);
    w.non_neg(vars.len() as u64);
    // The header, with the data offsets patched once its size is known.
    let mut begins = Vec::new();
    for v in vars {
        w.name(&v.name);
        w.non_neg(v.dims.len() as u64);
        for d in &v.dims {
            w.non_neg(*d as u64);
        }
        w.attrs(&v.attrs);
        w.u32(v.nc_type as u32);
        let vsize = v.data.len().next_multiple_of(4) as u64;
        // A variable too large for a 32-bit size records the largest one (allowed for the last).
        w.non_neg(if version == Version::Cdf5 { vsize } else { vsize.min(u32::MAX as u64) });
        begins.push(w.out.len());
        if version == Version::Classic {
            w.u32(0);
        } else {
            w.out.extend_from_slice(&0u64.to_be_bytes());
        }
    }
    let mut offset = w.out.len() as u64;
    for (v, at) in vars.iter().zip(begins) {
        if version == Version::Classic {
            w.out[at..at + 4].copy_from_slice(&(offset as u32).to_be_bytes());
        } else {
            w.out[at..at + 8].copy_from_slice(&offset.to_be_bytes());
        }
        offset += v.data.len().next_multiple_of(4) as u64;
    }
    for v in vars {
        w.out.extend_from_slice(&v.data);
        w.pad();
    }
    w.out
}

/// The NetCDF type holding a raster type, and whether it needs `_Unsigned = "true"`.
fn nc_type(t: DataType, version: Version) -> (NcType, bool) {
    let cdf5 = version.extended_types();
    match t {
        DataType::U8 if cdf5 => (NcType::UByte, false),
        DataType::U8 => (NcType::Byte, true),
        DataType::I8 => (NcType::Byte, false),
        DataType::I16 => (NcType::Short, false),
        DataType::U16 if cdf5 => (NcType::UShort, false),
        DataType::U16 => (NcType::Short, true),
        DataType::I32 => (NcType::Int, false),
        DataType::U32 if cdf5 => (NcType::UInt, false),
        DataType::U32 => (NcType::Int, true),
        DataType::I64 => (NcType::Int64, false),
        DataType::U64 => (NcType::UInt64, false),
        DataType::F32 => (NcType::Float, false),
        DataType::F64 => (NcType::Double, false),
    }
}

/// A value of a raster type, big-endian. `None` when the type cannot hold it.
fn encode_value(t: DataType, v: f64) -> Option<Vec<u8>> {
    let int = |lo: f64, hi: f64| v.fract() == 0.0 && v >= lo && v <= hi;
    Some(match t {
        DataType::U8 if int(0.0, 255.0) => vec![v as u8],
        DataType::I8 if int(-128.0, 127.0) => (v as i8).to_be_bytes().to_vec(),
        DataType::U16 if int(0.0, 65535.0) => (v as u16).to_be_bytes().to_vec(),
        DataType::I16 if int(-32768.0, 32767.0) => (v as i16).to_be_bytes().to_vec(),
        DataType::U32 if int(0.0, u32::MAX as f64) => (v as u32).to_be_bytes().to_vec(),
        DataType::I32 if int(i32::MIN as f64, i32::MAX as f64) => (v as i32).to_be_bytes().to_vec(),
        DataType::U64 if int(0.0, u64::MAX as f64) => (v as u64).to_be_bytes().to_vec(),
        DataType::I64 if int(i64::MIN as f64, i64::MAX as f64) => (v as i64).to_be_bytes().to_vec(),
        DataType::F32 if v.is_nan() || v.is_infinite() || (v as f32) as f64 == v => (v as f32).to_be_bytes().to_vec(),
        DataType::F64 => v.to_be_bytes().to_vec(),
        _ => return None,
    })
}

/// The CF grid mapping of a CRS: the variable name (GDAL names it after the mapping, `crs` for
/// geographic CRSs and those without a CF mapping) and its attributes.
fn grid_mapping(s: &crate::crs::SpatialRef) -> (String, Vec<(String, Attr)>) {
    use wbprojection::ProjectionKind as K;
    let mut attrs: Vec<(String, Attr)> = Vec::new();
    let d = |k: &str, v: f64| (k.to_string(), Attr::Doubles(vec![v]));
    let mut name = "crs".to_string();
    let ellipsoid = |attrs: &mut Vec<(String, Attr)>, a: f64, f: f64| {
        attrs.push(("long_name".into(), Attr::Text("CRS definition".into())));
        attrs.push(d("longitude_of_prime_meridian", 0.0));
        attrs.push(d("semi_major_axis", a));
        if f > 0.0 {
            attrs.push(d("inverse_flattening", 1.0 / f));
        } else {
            attrs.push(d("semi_minor_axis", a * (1.0 - f)));
        }
    };
    if let Some(([lat, lon, o_lon], a, es)) = s.rotated_pole_parameters() {
        name = "rotated_pole".into();
        attrs.push(("grid_mapping_name".into(), Attr::Text("rotated_latitude_longitude".into())));
        attrs.extend([d("grid_north_pole_latitude", lat), d("grid_north_pole_longitude", lon), d("north_pole_grid_longitude", o_lon)]);
        ellipsoid(&mut attrs, a, 1.0 - (1.0 - es).sqrt());
    } else if let Ok(crs) = s.to_wbp() {
        let p = crs.projection.params();
        let (fe, fnn, lat0, lon0, k) = (p.false_easting, p.false_northing, p.lat0, p.lon0, p.scale);
        let mapping: Option<(&str, Vec<(String, Attr)>)> = match &p.kind {
            K::Geographic => Some(("latitude_longitude", vec![])),
            K::TransverseMercator => Some((
                "transverse_mercator",
                vec![
                    d("longitude_of_central_meridian", lon0),
                    d("false_easting", fe),
                    d("false_northing", fnn),
                    d("latitude_of_projection_origin", lat0),
                    d("scale_factor_at_central_meridian", k),
                ],
            )),
            K::Utm { zone, south } => Some((
                "transverse_mercator",
                vec![
                    d("longitude_of_central_meridian", -183.0 + 6.0 * *zone as f64),
                    d("false_easting", 500000.0),
                    d("false_northing", if *south { 10_000_000.0 } else { 0.0 }),
                    d("latitude_of_projection_origin", 0.0),
                    d("scale_factor_at_central_meridian", 0.9996),
                ],
            )),
            K::Mercator | K::WebMercator => Some((
                "mercator",
                vec![
                    d("longitude_of_projection_origin", lon0),
                    d("false_easting", fe),
                    d("false_northing", fnn),
                    d("scale_factor_at_projection_origin", k),
                ],
            )),
            K::LambertConformalConic { lat1, lat2 } => Some((
                "lambert_conformal_conic",
                vec![
                    ("standard_parallel".into(), Attr::Doubles(lat2.map_or(vec![*lat1], |l2| vec![*lat1, l2]))),
                    d("longitude_of_central_meridian", lon0),
                    d("latitude_of_projection_origin", lat0),
                    d("false_easting", fe),
                    d("false_northing", fnn),
                ],
            )),
            K::AlbersEqualAreaConic { lat1, lat2 } => Some((
                "albers_conical_equal_area",
                vec![
                    ("standard_parallel".into(), Attr::Doubles(vec![*lat1, *lat2])),
                    d("longitude_of_central_meridian", lon0),
                    d("latitude_of_projection_origin", lat0),
                    d("false_easting", fe),
                    d("false_northing", fnn),
                ],
            )),
            K::LambertAzimuthalEqualArea => Some((
                "lambert_azimuthal_equal_area",
                vec![
                    d("longitude_of_projection_origin", lon0),
                    d("latitude_of_projection_origin", lat0),
                    d("false_easting", fe),
                    d("false_northing", fnn),
                ],
            )),
            K::PolarStereographic { north, lat_ts } => {
                let mut v = vec![
                    d("straight_vertical_longitude_from_pole", lon0),
                    d("latitude_of_projection_origin", if *north { 90.0 } else { -90.0 }),
                ];
                match lat_ts {
                    Some(ts) => v.push(d("standard_parallel", *ts)),
                    None => v.push(d("scale_factor_at_projection_origin", k)),
                }
                v.extend([d("false_easting", fe), d("false_northing", fnn)]);
                Some(("polar_stereographic", v))
            }
            K::Sinusoidal => {
                Some(("sinusoidal", vec![d("longitude_of_central_meridian", lon0), d("false_easting", fe), d("false_northing", fnn)]))
            }
            _ => None,
        };
        if let Some((mapping, params)) = mapping {
            attrs.push(("grid_mapping_name".into(), Attr::Text(mapping.into())));
            attrs.extend(params);
            if mapping != "latitude_longitude" {
                name = mapping.to_string();
            }
        }
        let e = &crs.datum.ellipsoid;
        ellipsoid(&mut attrs, e.a, e.f);
    } else {
        attrs.push(("long_name".into(), Attr::Text("CRS definition".into())));
    }
    if let Some(wkt) = super::wkt::gdal_wkt(s).or_else(|| s.to_wkt()) {
        attrs.push(("crs_wkt".into(), Attr::Text(wkt.clone())));
        attrs.push(("spatial_ref".into(), Attr::Text(wkt)));
    }
    (name, attrs)
}

/// Writes a raster as a NetCDF file, with GDAL's creation options (see [`Options`]): classic
/// (CDF-1, the default; CDF-2 with `NC2`; CDF-5 with `NC5`, which GDAL builds often cannot read) or
/// netCDF-4 (`NC4`, or `NC4C` for the classic model), chunked by row and compressed with the shuffle
/// and deflate filters with `COMPRESS=DEFLATE`. CDF-2 is used when the data is too large for CDF-1.
/// Outside CDF-5 and netCDF-4, UInt32 and 64-bit integers are written as doubles, as GDAL writes
/// them. With `GROUP`, the variables go in that netCDF-4 group.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path, options: &Options) -> Result<()> {
    std::fs::write(path, to_bytes(r, nodata, options)?)?;
    Ok(())
}

/// Encodes a raster as a NetCDF file: see [`write`].
pub(crate) fn to_bytes(r: &Raster, nodata: Option<f64>, options: &Options) -> Result<Vec<u8>> {
    groups_to_bytes(&[(options.group.unwrap_or(""), r, nodata)], options)
}

/// The format a file is written in, and the deflate level when compressed.
fn version_of(options: &Options, groups: bool) -> Result<(Version, Option<u32>)> {
    let mut version = match options.format.map(str::to_ascii_uppercase).as_deref() {
        None if groups => Version::Nc4,
        None | Some("NC") => Version::Classic,
        Some("NC2") => Version::Offset64,
        Some("NC5") | Some("CDF5") => Version::Cdf5,
        Some("NC4") => Version::Nc4,
        Some("NC4C") => Version::Nc4Classic,
        Some(other) => {
            return Err(Error::InvalidArgument(format!("netCDF format '{other}' is not supported (NC, NC2, NC5, NC4, NC4C)")));
        }
    };
    if groups && version != Version::Nc4 {
        return Err(Error::InvalidArgument("netCDF groups need the netCDF-4 format (FORMAT=NC4)".into()));
    }
    let deflate = match options.compress.map(str::to_ascii_uppercase).as_deref() {
        None | Some("NONE") => None,
        Some("DEFLATE") => {
            let level = options.zlevel.and_then(|z| z.trim().parse::<u32>().ok()).filter(|z| (1..=9).contains(z)).unwrap_or(1);
            // Compression needs netCDF-4: GDAL switches to its classic model.
            if !matches!(version, Version::Nc4 | Version::Nc4Classic) {
                version = Version::Nc4Classic;
            }
            Some(level)
        }
        Some(other) => return Err(Error::InvalidArgument(format!("netCDF compression '{other}' is not supported (NONE, DEFLATE)"))),
    };
    Ok((version, deflate))
}

/// Encodes rasters as one netCDF file, each in its group (`""` or `/` for the root group;
/// `/science/grid` creates the groups as needed), with its own dimensions, coordinate variables
/// and grid mapping there, as the netCDF library writes groups. Groups need netCDF-4: `FORMAT`
/// defaults to `NC4` when any raster is below the root.
pub(crate) fn groups_to_bytes(rasters: &[(&str, &Raster, Option<f64>)], options: &Options) -> Result<Vec<u8>> {
    let group_of = |g: &str| g.trim_matches('/').to_string();
    let any_group = rasters.iter().any(|(g, ..)| !group_of(g).is_empty());
    for (i, (g, ..)) in rasters.iter().enumerate() {
        if rasters[..i].iter().any(|(o, ..)| group_of(o) == group_of(g)) {
            return Err(Error::InvalidArgument(format!("two rasters in the netCDF group '/{}'", group_of(g))));
        }
    }
    let (version, deflate) = version_of(options, any_group)?;
    let chunking = !options.chunking.is_some_and(|c| matches!(c.to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"));
    let mut dims: Vec<(String, usize)> = Vec::new();
    let mut vars: Vec<Var> = Vec::new();
    let mut globals: Vec<(String, Attr)> = vec![("Conventions".into(), Attr::Text("CF-1.5".into()))];
    for (g, r, nodata) in rasters {
        let group = group_of(g);
        let (rdims, rvars) = raster_vars(r, *nodata, version)?;
        let prefix = |name: &str| if group.is_empty() { name.to_string() } else { format!("{group}/{name}") };
        let base = dims.len();
        dims.extend(rdims.into_iter().map(|(n, size)| (prefix(&n), size)));
        for mut v in rvars {
            v.name = prefix(&v.name);
            v.dims.iter_mut().for_each(|d| *d += base);
            vars.push(v);
        }
        for (k, v) in &r.metadata {
            if let Some(name) = k.strip_prefix("NC_GLOBAL#")
                && name != "Conventions"
                && !globals.iter().any(|(n, _)| n == name)
            {
                globals.push((name.to_string(), Attr::Text(v.clone())));
            }
        }
    }
    // The history of a file read again goes on, newest first, as GDAL records it.
    match globals.iter_mut().find(|(n, _)| n == "history") {
        Some((_, Attr::Text(h))) => *h = format!("Created by terrio\n{h}"),
        _ => globals.push(("history".into(), Attr::Text("Created by terrio".into()))),
    }
    let size: usize = rasters.iter().map(|(_, r, _)| r.data_type.size_bytes() * r.cols * r.rows * r.bands).sum();
    let version = if version == Version::Classic && size > (1 << 31) - (1 << 20) { Version::Offset64 } else { version };
    let bytes = match version {
        Version::Nc4 | Version::Nc4Classic => {
            let storage = Nc4Storage { deflate, chunking, classic_model: version == Version::Nc4Classic };
            hdf5_write::encode(&to_hdf5(&dims, &globals, &vars, &storage))
        }
        _ => encode(version, &dims, &globals, &vars),
    };
    Ok(bytes)
}

/// The dimensions and variables of one raster, as GDAL writes them.
/// The dimensions `(name, length)` and the variables of a raster.
type RasterVars = (Vec<(String, usize)>, Vec<Var>);

fn raster_vars(r: &Raster, nodata: Option<f64>, version: Version) -> Result<RasterVars> {
    // The classic formats have no unsigned 32-bit or 64-bit integers: GDAL reads them as signed,
    // and does not read CDF-5. They are stored as doubles, as GDAL stores them.
    let converted;
    let r = if !version.extended_types() && matches!(r.data_type, DataType::U32 | DataType::I64 | DataType::U64) {
        converted = common::convert(r, DataType::F64);
        &converted
    } else {
        r
    };
    let crs = common::spatial_ref(&r.crs);
    let geographic = !crs.is_unknown() && crs.is_geographic();
    // A rotated-pole grid's axes are CF's grid longitude and latitude, in degrees.
    let rotated = !geographic && crs.rotated_pole_parameters().is_some();
    let (xn, yn) = if geographic {
        ("lon", "lat")
    } else if rotated {
        ("rlon", "rlat")
    } else {
        ("x", "y")
    };

    let mut dims: Vec<(String, usize)> = vec![(xn.to_string(), r.cols), (yn.to_string(), r.rows)];
    let multi = r.bands > 1;
    if multi {
        dims.push(("band".to_string(), r.bands));
    }
    let mut vars = Vec::new();
    let gt = [r.x_min, r.cell_size_x, 0.0, r.y_max(), 0.0, -r.cell_size_y];
    let grid_mapping_name = if crs.is_unknown() {
        None
    } else {
        let (name, mut attrs) = grid_mapping(&crs);
        // As GDAL formats it: `%.17g` and a space after each number.
        attrs.push(("GeoTransform".into(), Attr::Text(gt.iter().map(|v| format!("{} ", common::fmt_g(*v, 17))).collect())));
        vars.push(Var { name: name.clone(), dims: vec![], attrs, nc_type: NcType::Char, data: vec![0] });
        Some(name)
    };

    // Pixel-centre coordinates, with y increasing (rows are stored south to north).
    let ys: Vec<u8> = (0..r.rows).flat_map(|i| (r.y_min + (i as f64 + 0.5) * r.cell_size_y).to_be_bytes()).collect();
    let xs: Vec<u8> = (0..r.cols).flat_map(|i| (r.x_min + (i as f64 + 0.5) * r.cell_size_x).to_be_bytes()).collect();
    let coord_attrs = |axis: &str| -> Vec<(String, Attr)> {
        let t = |s: &str| Attr::Text(s.to_string());
        match (geographic, axis) {
            (true, "y") => {
                vec![("standard_name".into(), t("latitude")), ("long_name".into(), t("latitude")), ("units".into(), t("degrees_north"))]
            }
            (true, _) => {
                vec![("standard_name".into(), t("longitude")), ("long_name".into(), t("longitude")), ("units".into(), t("degrees_east"))]
            }
            (false, a) if rotated => {
                let (std, long) = if a == "y" { ("grid_latitude", "latitude") } else { ("grid_longitude", "longitude") };
                let long = format!("{long} in rotated pole grid");
                let axis = a.to_ascii_uppercase();
                vec![
                    ("standard_name".into(), t(std)),
                    ("long_name".into(), t(&long)),
                    ("units".into(), t("degrees")),
                    ("axis".into(), t(&axis)),
                ]
            }
            (false, a) => {
                let mut v = vec![
                    ("standard_name".into(), t(&format!("projection_{a}_coordinate"))),
                    ("long_name".into(), t(&format!("{a} coordinate of projection"))),
                ];
                if !crs.is_unknown() {
                    v.push(("units".into(), t("m")));
                }
                v
            }
        }
    };
    vars.push(Var { name: yn.into(), dims: vec![1], attrs: coord_attrs("y"), nc_type: NcType::Double, data: ys });
    vars.push(Var { name: xn.into(), dims: vec![0], attrs: coord_attrs("x"), nc_type: NcType::Double, data: xs });
    if multi {
        let bands: Vec<u8> = (1..=r.bands as i32).flat_map(i32::to_be_bytes).collect();
        let attrs = vec![("long_name".into(), Attr::Text("band number".into()))];
        vars.push(Var { name: "band".into(), dims: vec![2], attrs, nc_type: NcType::Int, data: bands });
    }

    // The data variable.
    let (t, unsigned) = nc_type(r.data_type, version);
    let mut attrs: Vec<(String, Attr)> =
        vec![("long_name".into(), Attr::Text(if multi { "GDAL Band Number".into() } else { "GDAL Band Number 1".into() }))];
    if unsigned {
        attrs.push(("_Unsigned".into(), Attr::Text("true".into())));
    }
    // As GDAL marks bytes: signed or not, with their range (netCDF-4 has unsigned bytes).
    if r.data_type == DataType::U8 && unsigned {
        attrs.push(("valid_range".into(), Attr::Shorts(vec![0, 255])));
    } else if r.data_type == DataType::I8 && version != Version::Nc4 {
        attrs.push(("_Unsigned".into(), Attr::Text("false".into())));
        attrs.push(("valid_range".into(), Attr::Shorts(vec![-128, 127])));
    }
    if let Some(bytes) = nodata.and_then(|nd| encode_value(r.data_type, nd)) {
        attrs.push(("_FillValue".into(), Attr::Raw(t, 1, bytes)));
    } else if nodata.is_none() && !unsigned && !matches!(t, NcType::Byte | NcType::UByte) {
        // Without nodata, GDAL declares the type's default fill value (except for bytes).
        let mut be = default_fill(t);
        be.reverse();
        attrs.push(("_FillValue".into(), Attr::Raw(t, 1, be)));
    }
    if let Some(g) = &grid_mapping_name {
        attrs.push(("grid_mapping".into(), Attr::Text(g.clone())));
    }
    let layout = Layout {
        cols: r.cols,
        rows: r.rows,
        bands: r.bands,
        data_type: r.data_type,
        little_endian: false,
        interleave: Interleave::Bsq,
        offset: 0,
        bottom_up: true,
    };
    debug_assert_eq!(type_size(t), r.data_type.size_bytes());
    let dims_of_data = if multi { vec![2, 1, 0] } else { vec![1, 0] };
    vars.push(Var {
        name: if multi { "band_data".into() } else { "Band1".into() },
        dims: dims_of_data,
        attrs,
        nc_type: t,
        data: common::encode(r, &layout),
    });

    Ok((dims, vars))
}

/// How a netCDF-4 file stores its variables.
struct Nc4Storage {
    /// The deflate level, when compressed.
    deflate: Option<u32>,
    /// Whether compressed data variables are chunked by row (else whole).
    chunking: bool,
    /// Whether the file is restricted to the classic model (`NC4C`).
    classic_model: bool,
}

/// The HDF5 type of a netCDF type.
fn h5_type(t: NcType) -> H5Type {
    match t {
        NcType::Byte => H5Type::Int { size: 1, signed: true },
        NcType::Char => H5Type::Str { len: 1 },
        NcType::Short => H5Type::Int { size: 2, signed: true },
        NcType::Int => H5Type::Int { size: 4, signed: true },
        NcType::Float => H5Type::Float { size: 4 },
        NcType::Double => H5Type::Float { size: 8 },
        NcType::UByte => H5Type::Int { size: 1, signed: false },
        NcType::UShort => H5Type::Int { size: 2, signed: false },
        NcType::UInt => H5Type::Int { size: 4, signed: false },
        NcType::Int64 => H5Type::Int { size: 8, signed: true },
        NcType::UInt64 => H5Type::Int { size: 8, signed: false },
    }
}

/// netCDF's default fill value of a type, little-endian (`NC_FILL_*`).
fn default_fill(t: NcType) -> Vec<u8> {
    match t {
        NcType::Byte => (-127i8).to_le_bytes().to_vec(),
        NcType::Char => vec![0],
        NcType::Short => (-32767i16).to_le_bytes().to_vec(),
        NcType::Int => (-2147483647i32).to_le_bytes().to_vec(),
        NcType::Float => (9.969_209_968_386_869e36_f64 as f32).to_le_bytes().to_vec(),
        NcType::Double => 9.969_209_968_386_869e36_f64.to_le_bytes().to_vec(),
        NcType::UByte => vec![255],
        NcType::UShort => 65535u16.to_le_bytes().to_vec(),
        NcType::UInt => u32::MAX.to_le_bytes().to_vec(),
        NcType::Int64 => (-9223372036854775806i64).to_le_bytes().to_vec(),
        NcType::UInt64 => 18446744073709551614u64.to_le_bytes().to_vec(),
    }
}

/// Big-endian values of `size` bytes, made little-endian.
fn to_le(be: &[u8], size: usize) -> Vec<u8> {
    let mut v = be.to_vec();
    if size > 1 {
        for c in v.chunks_exact_mut(size) {
            c.reverse();
        }
    }
    v
}

fn h5_attr(name: &str, a: &Attr) -> hdf5_write::Attr {
    match a {
        Attr::Text(s) => hdf5_write::Attr::text(name, s),
        Attr::Doubles(v) => {
            hdf5_write::Attr::numbers(name, H5Type::Float { size: 8 }, v.len(), v.iter().flat_map(|x| x.to_le_bytes()).collect())
        }
        Attr::Shorts(v) => hdf5_write::Attr::numbers(
            name,
            H5Type::Int { size: 2, signed: true },
            v.len(),
            v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        ),
        Attr::Raw(t, n, bytes) => hdf5_write::Attr::numbers(name, h5_type(*t), *n, to_le(bytes, type_size(*t))),
    }
}

/// A string attribute of the HDF5 dimension scale conventions (NUL-terminated).
fn h5ds_text(name: &str, s: &str) -> hdf5_write::Attr {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    hdf5_write::Attr { name: name.into(), dtype: H5Type::Str { len: bytes.len() as u32 }, shape: None, values: Values::Bytes(bytes) }
}

fn int_attr(name: &str, values: &[i32], scalar: bool) -> hdf5_write::Attr {
    hdf5_write::Attr {
        name: name.into(),
        dtype: H5Type::Int { size: 4, signed: true },
        shape: (!scalar).then(|| vec![values.len() as u64]),
        values: Values::Bytes(values.iter().flat_map(|v| v.to_le_bytes()).collect()),
    }
}

/// The netCDF-4 layout of a file: every dimension a coordinate variable stored as an HDF5
/// dimension scale (`CLASS`, `NAME`, `_Netcdf4Dimid`, `REFERENCE_LIST`), the other variables
/// attached to them (`DIMENSION_LIST`, `_Netcdf4Coordinates`), as netCDF-C writes them.
fn to_hdf5(dims: &[(String, usize)], globals: &[(String, Attr)], vars: &[Var], storage: &Nc4Storage) -> hdf5_write::File {
    // The variable holding each dimension's coordinates (both named by their path in groups).
    let scale_of = |d: usize| vars.iter().position(|v| v.dims == [d] && v.name == dims[d].0);
    // A dimension's name, without its group.
    let leaf = |d: usize| dims[d].0.rsplit('/').next().unwrap_or(&dims[d].0).to_string();
    let mut datasets: Vec<hdf5_write::Dataset> = Vec::new();
    for v in vars {
        let size = type_size(v.nc_type);
        let shape: Option<Vec<u64>> = (!v.dims.is_empty()).then(|| v.dims.iter().map(|d| dims[*d].1 as u64).collect());
        let fill = v
            .attrs
            .iter()
            .find_map(|(k, a)| match a {
                Attr::Raw(_, 1, bytes) if k == "_FillValue" => Some(to_le(bytes, size)),
                _ => None,
            })
            .unwrap_or_else(|| default_fill(v.nc_type));
        let chunks = match (&shape, storage.deflate) {
            (Some(s), Some(_)) if s.len() == 1 => Some(s.clone()),
            (Some(s), Some(_)) => {
                let n = s.len();
                let mut c = vec![1u64; n];
                c[n - 1] = s[n - 1];
                if !storage.chunking {
                    c[n - 2] = s[n - 2];
                }
                Some(c)
            }
            _ => None,
        };
        let mut attrs: Vec<hdf5_write::Attr> = v.attrs.iter().map(|(k, a)| h5_attr(k, a)).collect();
        if let Some(d) = (0..dims.len()).find(|d| scale_of(*d).is_some_and(|i| std::ptr::eq(&vars[i], v))) {
            attrs.push(h5ds_text("CLASS", "DIMENSION_SCALE"));
            attrs.push(h5ds_text("NAME", &leaf(d)));
            attrs.push(int_attr("_Netcdf4Dimid", &[d as i32], true));
            let refs: Vec<(usize, u32)> = vars
                .iter()
                .enumerate()
                .filter(|(_, o)| !std::ptr::eq(*o, v))
                .flat_map(|(i, o)| o.dims.iter().enumerate().filter(|(_, od)| **od == d).map(move |(k, _)| (i, k as u32)))
                .collect();
            if !refs.is_empty() {
                attrs.push(hdf5_write::Attr {
                    name: "REFERENCE_LIST".into(),
                    dtype: H5Type::RefList,
                    shape: Some(vec![refs.len() as u64]),
                    values: Values::ReferenceList(refs),
                });
            }
        } else if !v.dims.is_empty() {
            let scales: Vec<usize> = v.dims.iter().filter_map(|d| scale_of(*d)).collect();
            if scales.len() == v.dims.len() {
                attrs.push(hdf5_write::Attr {
                    name: "DIMENSION_LIST".into(),
                    dtype: H5Type::RefVlen,
                    shape: Some(vec![scales.len() as u64]),
                    values: Values::DimensionList(scales),
                });
            }
            let ids: Vec<i32> = v.dims.iter().map(|d| *d as i32).collect();
            attrs.push(int_attr("_Netcdf4Coordinates", &ids, false));
        }
        datasets.push(hdf5_write::Dataset {
            name: v.name.clone(),
            dtype: h5_type(v.nc_type),
            shape,
            max_shape: None,
            // A scalar character (a grid mapping) is never written, as netCDF-C leaves it.
            data: if v.dims.is_empty() && v.nc_type == NcType::Char { Vec::new() } else { to_le(&v.data, size) },
            fill,
            storage: hdf5_write::Storage { chunks, deflate: storage.deflate },
            attrs,
        });
    }
    let mut attrs: Vec<hdf5_write::Attr> = globals.iter().map(|(k, a)| h5_attr(k, a)).collect();
    // The provenance netCDF-C records (and reports as `_NCProperties`).
    attrs.push(hdf5_write::Attr::text("_NCProperties", concat!("version=2,terrio=", env!("CARGO_PKG_VERSION"))));
    if storage.classic_model {
        attrs.push(int_attr("_nc3_strict", &[1], true));
    }
    hdf5_write::File { attrs, datasets, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::CrsInfo;
    use wbraster::raster::RasterData;

    fn raster(t: DataType, bands: usize, crs: u32) -> Raster {
        let n = 7 * 5 * bands;
        let data = RasterData::from_f64_vec(t, (0..n).map(|i| (i % 200) as f64).collect());
        common::make_raster(7, 5, bands, data, [10.0, 0.5, 0.0, 50.0, 0.0, -0.25], CrsInfo::from_epsg(crs), Some(3.0)).unwrap()
    }

    #[test]
    fn round_trips_every_type() {
        let dir = tempfile::tempdir().unwrap();
        for t in [
            DataType::U8,
            DataType::I8,
            DataType::I16,
            DataType::U16,
            DataType::I32,
            DataType::U32,
            DataType::I64,
            DataType::F32,
            DataType::F64,
        ] {
            for (bands, crs) in [(1, 4326), (3, 32633)] {
                let r = raster(t, bands, crs);
                let p = dir.path().join(format!("t_{t:?}_{bands}.nc"));
                write(&r, Some(3.0), &p, &Options::default()).unwrap();
                let ds = super::super::netcdf::read(&p, None).unwrap();
                let stored = if matches!(t, DataType::U32 | DataType::I64) { DataType::F64 } else { t };
                assert_eq!(ds.raster.data_type, stored, "{t:?}");
                assert_eq!(ds.raster.bands, bands);
                assert_eq!(ds.raster.data.to_f64_vec(), r.data.to_f64_vec(), "{t:?}");
                assert_eq!(ds.nodata(), Some(3.0), "{t:?}");
                assert_eq!(ds.crs().epsg, Some(crs));
                let gt = ds.geo_transform();
                assert!((gt[0] - 10.0).abs() < 1e-9 && (gt[3] - 50.0).abs() < 1e-9 && (gt[5] + 0.25).abs() < 1e-12, "{gt:?}");
            }
        }
    }

    #[test]
    fn history_goes_on_when_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.nc"), dir.path().join("b.nc"));
        write(&raster(DataType::F32, 1, 4326), None, &a, &Options::default()).unwrap();
        let first = super::super::netcdf::read(&a, None).unwrap();
        write(&first.raster, None, &b, &Options::default()).unwrap();
        let second = super::super::netcdf::read(&b, None).unwrap();
        let history: Vec<&String> = second.raster.metadata.iter().filter(|(k, _)| k == "NC_GLOBAL#history").map(|(_, v)| v).collect();
        assert_eq!(history, ["Created by terrio\nCreated by terrio"]);
    }

    #[test]
    fn writes_cdf5() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.nc");
        let r = raster(DataType::U32, 2, 4326);
        write(&r, Some(3.0), &p, &Options { format: Some("NC5"), ..Default::default() }).unwrap();
        assert_eq!(&std::fs::read(&p).unwrap()[..4], b"CDF\x05");
        let ds = super::super::netcdf::read(&p, None).unwrap();
        assert_eq!(ds.raster.data_type, DataType::U32);
        assert_eq!(ds.raster.data.to_f64_vec(), r.data.to_f64_vec());
    }

    #[test]
    fn writes_64_bit_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.nc");
        write(&raster(DataType::F32, 1, 4326), None, &p, &Options { format: Some("NC2"), ..Default::default() }).unwrap();
        assert_eq!(&std::fs::read(&p).unwrap()[..4], b"CDF\x02");
        let ds = super::super::netcdf::read(&p, None).unwrap();
        // GDAL's default fill value, declared as it declares it.
        assert_eq!(ds.nodata(), Some((9.969_209_968_386_869e36_f64 as f32) as f64));
    }

    #[test]
    fn writes_netcdf4() {
        let dir = tempfile::tempdir().unwrap();
        for (format, compress, chunking) in
            [("NC4", None, None), ("NC4", Some("DEFLATE"), None), ("NC4C", Some("DEFLATE"), Some("NO")), ("NC4C", None, None)]
        {
            for t in [DataType::U8, DataType::I8, DataType::U16, DataType::I16, DataType::U32, DataType::I64, DataType::U64, DataType::F64]
            {
                for (bands, crs) in [(1, 4326), (3, 32633)] {
                    let r = raster(t, bands, crs);
                    let p = dir.path().join(format!("t_{format}_{t:?}_{bands}.nc"));
                    let o = Options { format: Some(format), compress, zlevel: Some("6"), chunking, group: None };
                    write(&r, Some(3.0), &p, &o).unwrap();
                    assert_eq!(&std::fs::read(&p).unwrap()[..8], b"\x89HDF\r\n\x1a\n");
                    let ds = super::super::netcdf::read(&p, None).unwrap();
                    let classic_types = format == "NC4C" && matches!(t, DataType::U32 | DataType::I64 | DataType::U64);
                    assert_eq!(ds.raster.data_type, if classic_types { DataType::F64 } else { t }, "{format} {t:?}");
                    assert_eq!(ds.raster.bands, bands);
                    assert_eq!(ds.raster.data.to_f64_vec(), r.data.to_f64_vec(), "{format} {t:?}");
                    assert_eq!(ds.nodata(), Some(3.0));
                    assert_eq!(ds.crs().epsg, Some(crs));
                    let gt = ds.geo_transform();
                    assert!((gt[0] - 10.0).abs() < 1e-9 && (gt[3] - 50.0).abs() < 1e-9 && (gt[5] + 0.25).abs() < 1e-12, "{gt:?}");
                }
            }
        }
    }

    #[test]
    fn compression_switches_to_netcdf4() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.nc");
        write(&raster(DataType::I16, 1, 4326), None, &p, &Options { compress: Some("DEFLATE"), ..Default::default() }).unwrap();
        assert_eq!(&std::fs::read(&p).unwrap()[..4], b"\x89HDF");
        assert!(write(&raster(DataType::I16, 1, 4326), None, &p, &Options { format: Some("NC9"), ..Default::default() }).is_err());
    }
}
