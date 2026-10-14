//! NetCDF writing: the classic formats (CDF-1, CDF-2 with 64-bit offsets, and CDF-5 for 64-bit
//! integers), laid out as GDAL's netCDF driver writes rasters, following the CF conventions.
//!
//! - one data variable, `Band1` (or `band_data`, with a leading `band` dimension, for several
//!   bands), its rows stored south to north as GDAL stores them;
//! - `lon`/`lat` coordinate variables for geographic CRSs, `x`/`y` for the others, holding the
//!   pixel centres;
//! - a grid mapping variable with the CF projection parameters of the common projections, and the
//!   CRS as WKT in `crs_wkt` and `spatial_ref` and the geotransform in `GeoTransform`, as GDAL
//!   writes them;
//! - nodata as `_FillValue`; unsigned types in CDF-1/2 as signed ones with `_Unsigned = "true"`.
//!
//! The format is described at
//! <https://docs.unidata.ucar.edu/netcdf-c/current/file_format_specifications.html>.

use super::common::{self, Interleave, Layout};
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

/// Which classic format version.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Version {
    /// CDF-1: 32-bit offsets.
    Classic = 1,
    /// CDF-2: 64-bit offsets.
    Offset64 = 2,
    /// CDF-5: 64-bit sizes and the unsigned and 64-bit integer types.
    Cdf5 = 5,
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
fn encode(version: Version, dims: &[(&str, usize)], globals: &[(String, Attr)], vars: &[Var]) -> Vec<u8> {
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
    let cdf5 = version == Version::Cdf5;
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
    if let Ok(crs) = s.to_wbp() {
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
        attrs.push(("long_name".into(), Attr::Text("CRS definition".into())));
        attrs.push(d("longitude_of_prime_meridian", 0.0));
        attrs.push(d("semi_major_axis", e.a));
        if e.f > 0.0 {
            attrs.push(d("inverse_flattening", 1.0 / e.f));
        } else {
            attrs.push(d("semi_minor_axis", e.b));
        }
    } else {
        attrs.push(("long_name".into(), Attr::Text("CRS definition".into())));
    }
    if let Some(wkt) = super::wkt::gdal_wkt(s).or_else(|| s.to_wkt()) {
        attrs.push(("crs_wkt".into(), Attr::Text(wkt.clone())));
        attrs.push(("spatial_ref".into(), Attr::Text(wkt)));
    }
    (name, attrs)
}

/// Writes a raster as a classic NetCDF file. `format` is GDAL's `FORMAT` creation option: `NC`
/// (CDF-1, the default), `NC2` (CDF-2) or `NC5` (CDF-5, with unsigned and 64-bit integer types,
/// which GDAL builds often cannot read). CDF-2 is used when the data is too large for CDF-1. Outside
/// CDF-5, UInt32 and 64-bit integers are written as doubles, as GDAL writes them.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path, format: Option<&str>) -> Result<()> {
    let version = match format.map(str::to_ascii_uppercase).as_deref() {
        None | Some("NC") => Version::Classic,
        Some("NC2") => Version::Offset64,
        Some("NC5") | Some("CDF5") => Version::Cdf5,
        Some(other) => return Err(Error::InvalidArgument(format!("netCDF format '{other}' is not supported (NC, NC2, NC5)"))),
    };
    // The classic formats have no unsigned 32-bit or 64-bit integers: GDAL reads them as signed,
    // and does not read CDF-5. They are stored as doubles, as GDAL stores them.
    let converted;
    let r = if version != Version::Cdf5 && matches!(r.data_type, DataType::U32 | DataType::I64 | DataType::U64) {
        converted = common::convert(r, DataType::F64);
        &converted
    } else {
        r
    };
    let size = r.data_type.size_bytes() * r.cols * r.rows * r.bands;
    let version = if version == Version::Classic && size > (1 << 31) - (1 << 20) { Version::Offset64 } else { version };
    let crs = common::spatial_ref(&r.crs);
    let geographic = !crs.is_unknown() && crs.is_geographic();
    let (xn, yn) = if geographic { ("lon", "lat") } else { ("x", "y") };

    let mut dims = vec![(xn, r.cols), (yn, r.rows)];
    let multi = r.bands > 1;
    if multi {
        dims.push(("band", r.bands));
    }
    let mut vars = Vec::new();
    let gt = [r.x_min, r.cell_size_x, 0.0, r.y_max(), 0.0, -r.cell_size_y];
    let grid_mapping_name = if crs.is_unknown() {
        None
    } else {
        let (name, mut attrs) = grid_mapping(&crs);
        attrs.push(("GeoTransform".into(), Attr::Text(gt.iter().map(|v| format!("{v} ")).collect())));
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
    // As GDAL marks bytes: signed or not, with their range.
    if r.data_type == DataType::U8 && unsigned {
        attrs.push(("valid_range".into(), Attr::Shorts(vec![0, 255])));
    } else if r.data_type == DataType::I8 {
        attrs.push(("_Unsigned".into(), Attr::Text("false".into())));
        attrs.push(("valid_range".into(), Attr::Shorts(vec![-128, 127])));
    }
    if let Some(bytes) = nodata.and_then(|nd| encode_value(r.data_type, nd)) {
        attrs.push(("_FillValue".into(), Attr::Raw(t, 1, bytes)));
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

    let mut globals: Vec<(String, Attr)> = vec![("Conventions".into(), Attr::Text("CF-1.5".into()))];
    for (k, v) in &r.metadata {
        if let Some(name) = k.strip_prefix("NC_GLOBAL#")
            && name != "Conventions"
        {
            globals.push((name.to_string(), Attr::Text(v.clone())));
        }
    }
    globals.push(("history".into(), Attr::Text("Created by terrio".into())));
    std::fs::write(path, encode(version, &dims, &globals, &vars))?;
    Ok(())
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
                write(&r, Some(3.0), &p, None).unwrap();
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
    fn writes_cdf5() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.nc");
        let r = raster(DataType::U32, 2, 4326);
        write(&r, Some(3.0), &p, Some("NC5")).unwrap();
        assert_eq!(&std::fs::read(&p).unwrap()[..4], b"CDF\x05");
        let ds = super::super::netcdf::read(&p, None).unwrap();
        assert_eq!(ds.raster.data_type, DataType::U32);
        assert_eq!(ds.raster.data.to_f64_vec(), r.data.to_f64_vec());
    }

    #[test]
    fn writes_64_bit_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.nc");
        write(&raster(DataType::F32, 1, 4326), None, &p, Some("NC2")).unwrap();
        assert_eq!(&std::fs::read(&p).unwrap()[..4], b"CDF\x02");
        let ds = super::super::netcdf::read(&p, None).unwrap();
        assert_eq!(ds.nodata(), None);
    }
}
