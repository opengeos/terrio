//! NetCDF (classic CDF-1/2/5 and netCDF-4/HDF5), read with the pure-Rust `netcdf-reader` crate
//! and interpreted as GDAL's netCDF driver does: one variable at a time (a file with several
//! opens its first; `NETCDF:"file.nc":var` selects one), its last two dimensions as rows and
//! columns and any others as bands, the geotransform from the coordinate variables (south-up
//! grids turned north-up), the CRS from the CF grid mapping, `_FillValue` as nodata, and
//! `scale_factor`/`add_offset` reported as metadata (the values stay packed).

use super::common;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use netcdf_reader::{NcAttrValue, NcAttribute, NcFile, NcType, NcVariable};
use std::path::Path;
use wbraster::CrsInfo;
use wbraster::raster::RasterData;

fn err(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("{}: {e}", path.display()))
}

/// Parses GDAL's subdataset syntax, `NETCDF:"file.nc":var` (or unquoted): `(file, variable)`.
pub(crate) fn parse_subdataset(s: &str) -> Option<(String, String)> {
    let rest = s.get(..7).filter(|p| p.eq_ignore_ascii_case("NETCDF:")).map(|_| &s[7..])?;
    if let Some(quoted) = rest.strip_prefix('"') {
        let (file, var) = quoted.split_once("\":")?;
        return Some((file.to_string(), var.to_string()));
    }
    let (file, var) = rest.rsplit_once(':')?;
    Some((file.to_string(), var.to_string()))
}

/// Whether a file starts with a NetCDF (or HDF5) signature.
pub(crate) fn is_netcdf(path: &Path) -> bool {
    let mut b = [0u8; 8];
    if std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_err() {
        return false;
    }
    b.starts_with(b"CDF") || b == *b"\x89HDF\r\n\x1a\n"
}

fn attr<'a>(v: &'a NcVariable, name: &str) -> Option<&'a NcAttrValue> {
    v.attributes().iter().find(|a| a.name == name).map(|a| &a.value)
}

fn attr_str(v: &NcVariable, name: &str) -> Option<String> {
    attr(v, name).and_then(NcAttrValue::as_string)
}

fn attr_f64(v: &NcVariable, name: &str) -> Option<f64> {
    attr(v, name).and_then(NcAttrValue::as_f64)
}

fn attr_text(a: &NcAttribute) -> String {
    match &a.value {
        NcAttrValue::Chars(s) => s.clone(),
        NcAttrValue::Strings(v) => v.join(","),
        other => {
            let v = other.as_f64_vec().unwrap_or_default();
            let parts: Vec<String> = v.iter().map(|x| if x.is_nan() { "nan".into() } else { format!("{x}") }).collect();
            if parts.len() == 1 { parts[0].clone() } else { format!("{{{}}}", parts.join(",")) }
        }
    }
}

/// The data variables: two or more dimensions, and not a coordinate or bounds variable.
fn data_variables(f: &NcFile) -> Result<Vec<&NcVariable>> {
    let vars = f.variables().map_err(|e| Error::InvalidData(e.to_string()))?;
    let bounds: Vec<String> = vars.iter().filter_map(|v| attr_str(v, "bounds")).collect();
    let mut out: Vec<&NcVariable> = vars
        .iter()
        .filter(|v| v.ndim() >= 2 && !v.is_coordinate_variable() && !bounds.contains(&v.name) && v.dtype().is_primitive())
        .collect();
    // NetCDF-4 variables come back in name order; their object headers were allocated in
    // definition order, which is GDAL's subdataset order.
    if !matches!(f.format(), netcdf_reader::NcFormat::Classic | netcdf_reader::NcFormat::Offset64 | netcdf_reader::NcFormat::Cdf5) {
        out.sort_by_key(|v| v.data_offset);
    }
    Ok(out)
}

/// Reads a NetCDF variable (by default the first data variable) as a raster.
pub(crate) fn read(path: &Path, variable: Option<&str>) -> Result<RasterDataset> {
    let f = NcFile::open(path).map_err(|e| err(path, e))?;
    let candidates = data_variables(&f)?;
    let var = match variable {
        Some(name) => {
            candidates.iter().find(|v| v.name == name).copied().ok_or_else(|| err(path, format!("no raster variable '{name}'")))?
        }
        None => *candidates.first().ok_or_else(|| err(path, "no variable with two or more dimensions"))?,
    };
    let dims = var.dimensions();
    let nd = dims.len();
    let (rows, cols) = (dims[nd - 2].size as usize, dims[nd - 1].size as usize);
    let bands: usize = dims[..nd - 2].iter().map(|d| d.size as usize).product::<usize>().max(1);
    let unsigned = attr_str(var, "_Unsigned").is_some_and(|s| s.eq_ignore_ascii_case("true"));
    let name = var.name.clone();
    macro_rules! typed {
        ($t:ty, $variant:ident) => {
            RasterData::$variant(f.read_variable::<$t>(&name).map_err(|e| err(path, e))?.iter().copied().collect())
        };
    }
    let data = match (var.dtype(), unsigned) {
        (NcType::Byte, false) => typed!(i8, I8),
        (NcType::Byte, true) | (NcType::UByte, _) | (NcType::Char, _) => typed!(u8, U8),
        (NcType::Short, false) => typed!(i16, I16),
        (NcType::Short, true) | (NcType::UShort, _) => typed!(u16, U16),
        (NcType::Int, false) => typed!(i32, I32),
        (NcType::Int, true) | (NcType::UInt, _) => typed!(u32, U32),
        (NcType::Int64, false) => typed!(i64, I64),
        (NcType::Int64, true) | (NcType::UInt64, _) => typed!(u64, U64),
        (NcType::Float, _) => typed!(f32, F32),
        (NcType::Double, _) => typed!(f64, F64),
        (t, _) => return Err(Error::Unsupported(format!("{}: NetCDF type {t:?} is not supported", path.display()))),
    };

    // Coordinates: the variables named after the last two dimensions.
    let coord = |d: &str| -> Option<Vec<f64>> {
        let v = f.variable(d).ok()?;
        (v.ndim() == 1).then(|| f.read_variable_as_f64(d).ok().map(|a| a.iter().copied().collect())).flatten()
    };
    let (xs, ys) = (coord(&dims[nd - 1].name), coord(&dims[nd - 2].name));
    let grid_mapping = attr_str(var, "grid_mapping").and_then(|g| f.variable(g.split_whitespace().next().unwrap_or_default()).ok());
    let mut gt = None;
    let mut south_up = false;
    if let (Some(x), Some(y)) = (&xs, &ys)
        && cols > 1
        && rows > 1
        && regular(x)
        && regular(y)
    {
        let dx = (x[cols - 1] - x[0]) / (cols - 1) as f64;
        let dy = (y[rows - 1] - y[0]) / (rows - 1) as f64;
        south_up = dy > 0.0;
        let top = if south_up { y[rows - 1] + dy / 2.0 } else { y[0] - dy / 2.0 };
        gt = Some([x[0] - dx / 2.0, dx, 0.0, top, 0.0, -dy.abs()]);
    }
    if gt.is_none() {
        gt = grid_mapping.and_then(|g| attr_str(g, "GeoTransform")).and_then(|s| {
            let v: Vec<f64> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            <[f64; 6]>::try_from(v).ok()
        });
    }
    let degrees = |d: &str, unit: &str| {
        f.variable(d).ok().and_then(|v| attr_str(v, "units")).is_some_and(|u| u.to_ascii_lowercase().starts_with(unit))
    };
    let crs = match grid_mapping {
        Some(g) => grid_mapping_crs(g),
        // Longitude and latitude axes without a grid mapping: WGS 84, as GDAL assumes.
        None if degrees(&dims[nd - 1].name, "degrees_e") && degrees(&dims[nd - 2].name, "degrees_n") => Some(CrsInfo::from_epsg(4326)),
        None => None,
    };
    let nodata = attr_f64(var, "_FillValue").or_else(|| attr_f64(var, "missing_value"));
    let mut raster = common::make_raster(cols, rows, bands, data, gt.unwrap_or(common::NO_GEOTRANSFORM), crs.unwrap_or_default(), nodata)?;
    if south_up {
        flip(&mut raster);
    }
    for key in ["scale_factor", "add_offset"] {
        if let Some(v) = attr_f64(var, key) {
            raster.metadata.push((key.to_string(), format!("{v}")));
        }
    }
    for a in f.global_attributes().map_err(|e| err(path, e))? {
        raster.metadata.push((format!("NC_GLOBAL#{}", a.name), attr_text(a)));
    }
    if candidates.len() > 1 {
        let file = path.display();
        for (i, v) in candidates.iter().enumerate() {
            raster.metadata.push((format!("SUBDATASET_{}_NAME", i + 1), format!("NETCDF:\"{file}\":{}", v.name)));
        }
    }
    Ok(RasterDataset { raster, driver: RasterDriver::NetCDF, has_nodata: nodata.is_some() })
}

/// Whether coordinates are evenly spaced (within 0.1% of a step).
fn regular(v: &[f64]) -> bool {
    if v.len() < 2 {
        return false;
    }
    let step = (v[v.len() - 1] - v[0]) / (v.len() - 1) as f64;
    step != 0.0 && v.windows(2).all(|w| ((w[1] - w[0]) - step).abs() <= step.abs() * 1e-3)
}

/// Reverses the row order of every band.
fn flip(r: &mut wbraster::Raster) {
    let (cols, rows) = (r.cols, r.rows);
    macro_rules! go {
        ($v:expr) => {
            for band in $v.chunks_mut(cols * rows) {
                for row in 0..rows / 2 {
                    let (a, b) = band.split_at_mut((rows - 1 - row) * cols);
                    a[row * cols..(row + 1) * cols].swap_with_slice(&mut b[..cols]);
                }
            }
        };
    }
    match &mut r.data {
        RasterData::U8(v) => go!(v),
        RasterData::I8(v) => go!(v),
        RasterData::U16(v) => go!(v),
        RasterData::I16(v) => go!(v),
        RasterData::U32(v) => go!(v),
        RasterData::I32(v) => go!(v),
        RasterData::U64(v) => go!(v),
        RasterData::I64(v) => go!(v),
        RasterData::F32(v) => go!(v),
        RasterData::F64(v) => go!(v),
    }
}

/// The CRS of a CF grid mapping variable: its WKT (as GDAL writes it), or its CF parameters.
fn grid_mapping_crs(g: &NcVariable) -> Option<CrsInfo> {
    if let Some(wkt) = attr_str(g, "crs_wkt").or_else(|| attr_str(g, "spatial_ref")) {
        return common::crs_from_text(&wkt);
    }
    let p = |k: &str| attr_f64(g, k);
    let a = p("semi_major_axis").or(p("earth_radius")).unwrap_or(6378137.0);
    let ellps = match (p("inverse_flattening"), p("semi_minor_axis"), p("earth_radius")) {
        (Some(rf), _, _) if rf > 0.0 => format!("+a={a} +rf={rf}"),
        (_, Some(b), _) => format!("+a={a} +b={b}"),
        (_, _, Some(r)) => format!("+R={r}"),
        _ => "+datum=WGS84".to_string(),
    };
    let fe = p("false_easting").unwrap_or(0.0);
    let fnn = p("false_northing").unwrap_or(0.0);
    let lat0 = p("latitude_of_projection_origin").unwrap_or(0.0);
    let lon0 = p("longitude_of_central_meridian")
        .or(p("longitude_of_projection_origin"))
        .or(p("straight_vertical_longitude_from_pole"))
        .unwrap_or(0.0);
    let parallels = attr(g, "standard_parallel").and_then(NcAttrValue::as_f64_vec).unwrap_or_default();
    let xy = format!("+x_0={fe} +y_0={fnn}");
    let proj = match attr_str(g, "grid_mapping_name")?.as_str() {
        "latitude_longitude" => {
            let wgs84 = (a - 6378137.0).abs() < 1e-6 && p("inverse_flattening").is_none_or(|rf| (rf - 298.257223563).abs() < 1e-6);
            if wgs84 {
                return Some(CrsInfo::from_epsg(4326));
            }
            format!("+proj=longlat {ellps}")
        }
        "transverse_mercator" => {
            let k = p("scale_factor_at_central_meridian").unwrap_or(1.0);
            format!("+proj=tmerc +lat_0={lat0} +lon_0={lon0} +k={k} {xy} {ellps}")
        }
        "lambert_conformal_conic" => {
            let lat1 = parallels.first().copied().unwrap_or(lat0);
            let lat2 = parallels.get(1).copied().unwrap_or(lat1);
            format!("+proj=lcc +lat_1={lat1} +lat_2={lat2} +lat_0={lat0} +lon_0={lon0} {xy} {ellps}")
        }
        "albers_conical_equal_area" => {
            let lat1 = parallels.first().copied().unwrap_or(lat0);
            let lat2 = parallels.get(1).copied().unwrap_or(lat1);
            format!("+proj=aea +lat_1={lat1} +lat_2={lat2} +lat_0={lat0} +lon_0={lon0} {xy} {ellps}")
        }
        "lambert_azimuthal_equal_area" => format!("+proj=laea +lat_0={lat0} +lon_0={lon0} {xy} {ellps}"),
        "polar_stereographic" => match parallels.first() {
            Some(ts) => format!("+proj=stere +lat_0={lat0} +lat_ts={ts} +lon_0={lon0} {xy} {ellps}"),
            None => {
                format!("+proj=stere +lat_0={lat0} +lon_0={lon0} +k={} {xy} {ellps}", p("scale_factor_at_projection_origin").unwrap_or(1.0))
            }
        },
        "mercator" => match parallels.first() {
            Some(ts) => format!("+proj=merc +lat_ts={ts} +lon_0={lon0} {xy} {ellps}"),
            None => format!("+proj=merc +lon_0={lon0} +k={} {xy} {ellps}", p("scale_factor_at_projection_origin").unwrap_or(1.0)),
        },
        "sinusoidal" => format!("+proj=sinu +lon_0={lon0} {xy} {ellps}"),
        _ => return None,
    };
    Some(CrsInfo::from_proj4(proj))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subdataset_names() {
        assert_eq!(parse_subdataset("NETCDF:\"/data/a b.nc\":u_wind"), Some(("/data/a b.nc".into(), "u_wind".into())));
        assert_eq!(parse_subdataset("netcdf:wind.nc:v"), Some(("wind.nc".into(), "v".into())));
        assert_eq!(parse_subdataset("wind.nc"), None);
    }
}
