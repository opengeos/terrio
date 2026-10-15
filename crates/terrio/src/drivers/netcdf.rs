//! NetCDF (classic CDF-1/2/5 and netCDF-4/HDF5), read with the pure-Rust `netcdf-reader` crate
//! and interpreted as GDAL's netCDF driver does: one variable at a time (a file with several
//! opens its first; `NETCDF:"file.nc":var` selects one), its last two dimensions as rows and
//! columns and any others as bands, the geotransform from the coordinate variables (south-up
//! grids turned north-up), the CRS from the CF grid mapping, `_FillValue` as nodata, and
//! `scale_factor`/`add_offset` reported as metadata (the values stay packed).

use super::common;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use netcdf_reader::{NcAttrValue, NcAttribute, NcFile, NcFormat, NcGroup, NcType, NcVariable};
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

/// Whether a file is a netCDF file: a classic one, or an HDF5 one with one of netCDF's extensions
/// (GDAL's HDF5 driver reads the others).
pub(crate) fn is_netcdf(path: &Path) -> bool {
    let mut b = [0u8; 8];
    if std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_err() {
        return false;
    }
    let netcdf_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["nc", "cdf", "nc2", "nc3", "nc4", "gmac"].iter().any(|n| e.eq_ignore_ascii_case(n)));
    b.starts_with(b"CDF") || (netcdf_ext && super::hdf5::has_signature(path))
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

/// A variable and the groups it is in (the root first).
struct VarRef<'a> {
    groups: Vec<&'a NcGroup>,
    v: &'a NcVariable,
    /// Its full name as GDAL names it: `var` in the root group, `/group/var` below it.
    full: String,
}

impl VarRef<'_> {
    /// The path `netcdf-reader` reads it by: `group/subgroup/var`.
    fn read_path(&self) -> String {
        self.full.trim_start_matches('/').to_string()
    }

    /// A variable named `name` visible from this one's group (its group, then the enclosing
    /// ones), with its read path.
    fn lookup(&self, name: &str) -> Option<(String, &NcVariable)> {
        for depth in (0..self.groups.len()).rev() {
            if let Some(v) = self.groups[depth].variables.iter().find(|v| v.name == name) {
                let prefix: Vec<&str> = self.groups[1..=depth].iter().map(|g| g.name.as_str()).collect();
                let path = if prefix.is_empty() { name.to_string() } else { format!("{}/{name}", prefix.join("/")) };
                return Some((path, v));
            }
        }
        None
    }
}

/// Every variable, in GDAL's order: the root group's in definition order, then each subgroup's
/// (in definition order, recursively).
fn all_variables<'a>(f: &'a NcFile, path: &Path, hdf5: &dyn Fn() -> Option<hdf5_reader::Hdf5File>) -> Result<Vec<VarRef<'a>>> {
    let root = f.root_group().map_err(|e| err(path, e))?;
    let classic = matches!(f.format(), NcFormat::Classic | NcFormat::Offset64 | NcFormat::Cdf5);
    // NetCDF-4 groups and variables come back in name order; their object headers were allocated
    // in definition order, which is GDAL's order.
    let addresses = if classic { None } else { hdf5() };
    let group_address = |group_path: &str| -> u64 {
        addresses
            .as_ref()
            .and_then(|h| {
                let mut g = h.root_group().ok()?;
                for part in group_path.split('/').filter(|p| !p.is_empty()) {
                    g = g.group(part).ok()?;
                }
                Some(g.address())
            })
            .unwrap_or(u64::MAX)
    };
    fn visit<'a>(chain: Vec<&'a NcGroup>, prefix: &str, classic: bool, group_address: &dyn Fn(&str) -> u64, out: &mut Vec<VarRef<'a>>) {
        let g = *chain.last().unwrap_or(&chain[0]);
        let mut vars: Vec<&NcVariable> = g.variables.iter().collect();
        if !classic {
            vars.sort_by_key(|v| v.data_offset);
        }
        for v in vars {
            let full = if prefix.is_empty() { v.name.clone() } else { format!("{prefix}/{}", v.name) };
            out.push(VarRef { groups: chain.clone(), v, full });
        }
        let mut children: Vec<(&NcGroup, String)> = g.groups.iter().map(|c| (c, format!("{prefix}/{}", c.name))).collect();
        if !classic {
            children.sort_by_key(|(_, p)| group_address(p));
        }
        for (c, p) in children {
            let mut next = chain.clone();
            next.push(c);
            visit(next, &p, classic, group_address, out);
        }
    }
    let mut out = Vec::new();
    visit(vec![root], "", classic, &group_address, &mut out);
    Ok(out)
}

/// The raster variables: two or more dimensions of a numeric type, and not a coordinate or bounds
/// variable.
fn data_variables<'a>(vars: &'a [VarRef<'a>]) -> Vec<&'a VarRef<'a>> {
    let bounds: Vec<String> = vars.iter().filter_map(|r| attr_str(r.v, "bounds")).collect();
    vars.iter()
        .filter(|r| r.v.ndim() >= 2 && !r.v.is_coordinate_variable() && !bounds.contains(&r.v.name) && r.v.dtype().is_primitive())
        .collect()
}

/// GDAL's name of a netCDF type in subdataset descriptions.
fn type_description(t: &NcType) -> &'static str {
    match t {
        NcType::Byte => "8-bit integer",
        NcType::Char => "8-bit character",
        NcType::Short => "16-bit integer",
        NcType::Int => "32-bit integer",
        NcType::Float => "32-bit floating-point",
        NcType::Double => "64-bit floating-point",
        NcType::UByte => "8-bit unsigned integer",
        NcType::UShort => "16-bit unsigned integer",
        NcType::UInt => "32-bit unsigned integer",
        NcType::Int64 => "64-bit integer",
        NcType::UInt64 => "64-bit unsigned integer",
        _ => "",
    }
}

/// The subdatasets of a file, `(name, description)`, as GDAL lists them
/// (`netCDFDataset::CreateSubDatasetList()`): every variable of two or more dimensions.
fn subdatasets(vars: &[VarRef], file: &str) -> Vec<(String, String)> {
    vars.iter()
        .filter(|r| r.v.ndim() >= 2)
        .map(|r| {
            let name = if r.full.contains([' ', ':']) {
                format!("NETCDF:\"{file}\":\"{}\"", r.full)
            } else {
                format!("NETCDF:\"{file}\":{}", r.full)
            };
            let dims: Vec<String> = r.v.dimensions().iter().map(|d| d.size.to_string()).collect();
            let label = attr_str(r.v, "standard_name").unwrap_or_else(|| r.full.clone());
            (name, format!("[{}] {label} ({})", dims.join("x"), type_description(r.v.dtype())))
        })
        .collect()
}

/// Reads a NetCDF variable (by default the first data variable) as a raster. Variables below the
/// root group are named by their path, `/group/var`.
pub(crate) fn read(path: &Path, variable: Option<&str>) -> Result<RasterDataset> {
    let f = NcFile::open(path).map_err(|e| err(path, e))?;
    read_file(&f, path, &|| super::hdf5::open_file(path).ok(), variable)
}

/// Reads a NetCDF variable of a file held in memory: see [`read`]. `name` is the file name.
pub(crate) fn read_bytes(bytes: &[u8], name: &str, variable: Option<&str>) -> Result<RasterDataset> {
    let path = Path::new(name);
    let f = NcFile::from_bytes(bytes).map_err(|e| err(path, e))?;
    read_file(&f, path, &|| super::hdf5::open_bytes(bytes.to_vec(), name).ok(), variable)
}

fn read_file(f: &NcFile, path: &Path, hdf5: &dyn Fn() -> Option<hdf5_reader::Hdf5File>, variable: Option<&str>) -> Result<RasterDataset> {
    let vars = all_variables(f, path, hdf5)?;
    let candidates = data_variables(&vars);
    let var = match variable {
        Some(name) => {
            let name = name.trim_matches('"');
            let wanted = name.trim_start_matches('/');
            vars.iter()
                .find(|r| r.full.trim_start_matches('/') == wanted)
                .filter(|r| r.v.ndim() >= 1)
                .ok_or_else(|| err(path, format!("no raster variable '{name}'")))?
        }
        None => *candidates.first().ok_or_else(|| err(path, "no variable with two or more dimensions"))?,
    };
    let read_path = var.read_path();
    let (var_ref, var) = (var, var.v);
    let dims = var.dimensions();
    let nd = dims.len();
    let cols = dims[nd - 1].size as usize;
    let rows = if nd >= 2 { dims[nd - 2].size as usize } else { 1 };
    let bands: usize = if nd > 2 { dims[..nd - 2].iter().map(|d| d.size as usize).product::<usize>().max(1) } else { 1 };
    let unsigned = attr_str(var, "_Unsigned").is_some_and(|s| s.eq_ignore_ascii_case("true"));
    let name = read_path;
    macro_rules! typed {
        ($t:ty, $variant:ident) => {
            RasterData::$variant(f.read_variable::<$t>(&name).map_err(|e| err(path, e))?.iter().copied().collect())
        };
    }
    // Signed types flagged `_Unsigned` keep their bits.
    macro_rules! reinterpret {
        ($t:ty, $u:ty, $variant:ident) => {
            RasterData::$variant(f.read_variable::<$t>(&name).map_err(|e| err(path, e))?.iter().map(|v| *v as $u).collect())
        };
    }
    let data = match (var.dtype(), unsigned) {
        (NcType::Byte, false) => typed!(i8, I8),
        (NcType::Byte, true) => reinterpret!(i8, u8, U8),
        (NcType::UByte, _) | (NcType::Char, _) => typed!(u8, U8),
        (NcType::Short, false) => typed!(i16, I16),
        (NcType::Short, true) => reinterpret!(i16, u16, U16),
        (NcType::UShort, _) => typed!(u16, U16),
        (NcType::Int, false) => typed!(i32, I32),
        (NcType::Int, true) => reinterpret!(i32, u32, U32),
        (NcType::UInt, _) => typed!(u32, U32),
        (NcType::Int64, false) => typed!(i64, I64),
        (NcType::Int64, true) => reinterpret!(i64, u64, U64),
        (NcType::UInt64, _) => typed!(u64, U64),
        (NcType::Float, _) => typed!(f32, F32),
        (NcType::Double, _) => typed!(f64, F64),
        (t, _) => return Err(Error::Unsupported(format!("{}: NetCDF type {t:?} is not supported", path.display()))),
    };

    // Coordinates: the variables named after the last two dimensions.
    // GDAL ignores coordinates not named or marked as spatial ones, unless the file comes from GMT
    // or the variable has `res` and `transform` attributes.
    let ignore_axis_checks = (attr(var, "res").is_some() && attr(var, "transform").is_some())
        || f.global_attributes().is_ok_and(|a| a.iter().any(|a| a.name == "GMT_version"));
    let coord = |d: &str| -> Option<Vec<f64>> {
        let (p, v) = var_ref.lookup(d)?;
        let spatial = ignore_axis_checks || is_spatial_axis(v);
        (v.ndim() == 1 && spatial).then(|| f.read_variable_as_f64(&p).ok().map(|a| a.iter().copied().collect())).flatten()
    };
    let (xs, ys) = if nd >= 2 { (coord(&dims[nd - 1].name), coord(&dims[nd - 2].name)) } else { (None, None) };
    let grid_mapping =
        attr_str(var, "grid_mapping").and_then(|g| var_ref.lookup(g.split_whitespace().next().unwrap_or_default())).map(|(_, v)| v);
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
    let from_coordinates = gt.is_some();
    if gt.is_none() {
        gt = grid_mapping.and_then(|g| attr_str(g, "GeoTransform")).and_then(|s| {
            let v: Vec<f64> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            <[f64; 6]>::try_from(v).ok()
        });
        // Without georeferencing, GDAL reads rows bottom-up (netCDF's convention).
        if gt.is_none() && !from_coordinates && rows > 1 {
            south_up = true;
        }
    }
    let degrees = |d: &str, unit: &str| {
        var_ref.lookup(d).and_then(|(_, v)| attr_str(v, "units")).is_some_and(|u| u.to_ascii_lowercase().starts_with(unit))
    };
    let crs = match grid_mapping {
        Some(g) => grid_mapping_crs(g),
        // Longitude and latitude axes without a grid mapping: WGS 84, as GDAL assumes.
        None if nd >= 2 && degrees(&dims[nd - 1].name, "degrees_e") && degrees(&dims[nd - 2].name, "degrees_n") => {
            Some(CrsInfo::from_epsg(4326))
        }
        None => None,
    };
    let mut nodata = attr_f64(var, "_FillValue").or_else(|| attr_f64(var, "missing_value"));
    // A signed type read as unsigned keeps its fill value's bits.
    if unsigned && let Some(nd) = nodata.filter(|v| *v < 0.0) {
        let bits = match var.dtype() {
            NcType::Byte => 8,
            NcType::Short => 16,
            NcType::Int => 32,
            _ => 0,
        };
        if bits > 0 {
            nodata = Some(nd + (1u64 << bits) as f64);
        }
    }
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
    if variable.is_none() && candidates.len() > 1 {
        for (i, (n, d)) in subdatasets(&vars, &path.display().to_string()).into_iter().enumerate() {
            raster.metadata.push((format!("SUBDATASET_{}_NAME", i + 1), n));
            raster.metadata.push((format!("SUBDATASET_{}_DESC", i + 1), d));
        }
    }
    Ok(RasterDataset { raster, driver: RasterDriver::NetCDF, has_nodata: nodata.is_some(), affine: None })
}

/// Whether a coordinate variable is a spatial axis as GDAL decides it (`NCDFIsVarLongitude()`,
/// `NCDFIsVarLatitude()`, `NCDFIsVarProjectionX()`, `NCDFIsVarProjectionY()`): by its units,
/// standard name, axis or long name (not with units `1`, nor `m` for geographic ones), or by its
/// name (`lon`, `longitude`, `lat`, `latitude`).
fn is_spatial_axis(v: &NcVariable) -> bool {
    let get = |k: &str| attr_str(v, k).map(|s| s.trim_end_matches('\0').to_string());
    let is = |k: &str, values: &[&str]| get(k).is_some_and(|s| values.iter().any(|x| s.eq_ignore_ascii_case(x)));
    let units = get("units");
    let unit_is = |u: &str| units.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(u));
    let geographic = |degrees: &[&str], name: &str, axis: &str| {
        (is("units", degrees) || is("standard_name", &[name]) || is("axis", &[axis]) || is("long_name", &[name]))
            && !unit_is("m")
            && !unit_is("1")
    };
    let projected = |std: &str, axis: &str, grid: &str| is("standard_name", &[std, grid]) || is("axis", &[axis]);
    geographic(&["degrees_east", "degree_east", "degrees_E"], "longitude", "X")
        || geographic(&["degrees_north", "degree_north", "degrees_N"], "latitude", "Y")
        || ((projected("projection_x_coordinate", "X", "grid_longitude") || projected("projection_y_coordinate", "Y", "grid_latitude"))
            && !unit_is("1"))
        || ["lon", "longitude", "lat", "latitude"].iter().any(|n| v.name.eq_ignore_ascii_case(n))
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
    cf_crs(|k| attr_str(g, k), |k| attr(g, k).and_then(NcAttrValue::as_f64_vec))
}

/// The CRS of CF grid mapping attributes, given by their text (`text`) and numeric (`numbers`)
/// values: the WKT of `crs_wkt` or `spatial_ref` (as GDAL writes them), or the CF parameters.
pub(crate) fn cf_crs(text: impl Fn(&str) -> Option<String>, numbers: impl Fn(&str) -> Option<Vec<f64>>) -> Option<CrsInfo> {
    if let Some(wkt) = text("crs_wkt").or_else(|| text("spatial_ref")) {
        return common::crs_from_text(&wkt);
    }
    let p = |k: &str| numbers(k).and_then(|v| v.first().copied());
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
    let parallels = numbers("standard_parallel").unwrap_or_default();
    let xy = format!("+x_0={fe} +y_0={fnn}");
    let proj = match text("grid_mapping_name")?.as_str() {
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
