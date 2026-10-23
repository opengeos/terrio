//! NetCDF (classic CDF-1/2/5 and netCDF-4/HDF5), read with the pure-Rust `netcdf-reader` crate
//! and interpreted as GDAL's netCDF driver does: one variable at a time (a file with several
//! opens its first; `NETCDF:"file.nc":var` selects one), its last two dimensions as rows and
//! columns and any others as bands (a 3-D variable whose last dimension is `bands` or `band`, as
//! EMIT's radiance and reflectance, has it as bands and its first two as rows and columns), the
//! geotransform from the coordinate variables (south-up grids turned north-up), the CRS from the
//! CF grid mapping, `_FillValue` as nodata, and `scale_factor`/`add_offset` reported as metadata
//! (the values stay packed).

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

    /// The full name GDAL gives a variable visible from this one's group: `lon` in the root
    /// group, `/group/lon` below it.
    fn full_name(&self, name: &str) -> Option<String> {
        let (path, _) = self.lookup(name)?;
        Some(if path.contains('/') { format!("/{path}") } else { path })
    }

    /// A variable named `name` visible from this one's group (its group, then the enclosing
    /// ones), or by its absolute path (`/group/var`), with its read path.
    fn lookup(&self, name: &str) -> Option<(String, &NcVariable)> {
        if let Some(abs) = name.strip_prefix('/') {
            let mut parts: Vec<&str> = abs.split('/').filter(|p| !p.is_empty()).collect();
            let leaf = parts.pop()?;
            let mut g = *self.groups.first()?;
            for p in &parts {
                g = g.groups.iter().find(|c| c.name == *p)?;
            }
            let v = g.variables.iter().find(|v| v.name == leaf)?;
            return Some((abs.trim_matches('/').to_string(), v));
        }
        self.lookup_relative(name)
    }

    fn lookup_relative(&self, name: &str) -> Option<(String, &NcVariable)> {
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

/// The paths of a file's raster variables (`Band1`, `/group/Band1`), as [`read`] names them.
pub(crate) fn data_variable_paths(path: &Path) -> Result<Vec<String>> {
    let f = NcFile::open(path).map_err(|e| err(path, e))?;
    let vars = all_variables(&f, path, &|| super::hdf5::open_file(path).ok())?;
    Ok(data_variables(&vars).iter().map(|r| r.full.clone()).collect())
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
        // The first data variable that no other names in its CF `coordinates` attribute (a
        // geolocation array), as GDAL's netCDF driver ignores those to pick the raster; they stay
        // subdatasets.
        None => {
            let geolocation: Vec<String> = vars
                .iter()
                .filter_map(|r| attr_str(r.v, "coordinates"))
                .flat_map(|c| c.split_whitespace().map(str::to_string).collect::<Vec<_>>())
                .collect();
            *candidates
                .iter()
                .find(|r| !geolocation.contains(&r.v.name))
                .or(candidates.first())
                .ok_or_else(|| err(path, "no variable with two or more dimensions"))?
        }
    };
    let read_path = var.read_path();
    let (var_ref, var) = (var, var.v);
    let dims = var.dimensions();
    let nd = dims.len();
    // The dimensions of the rows and columns: the last two, or the first two before a trailing
    // `bands` dimension (whose values are then pixel-interleaved).
    let interleaved = trailing_bands(var);
    let (y_dim, x_dim) = if interleaved { (0, 1) } else { (nd.saturating_sub(2), nd - 1) };
    let cols = dims[x_dim].size as usize;
    let rows = if nd >= 2 { dims[y_dim].size as usize } else { 1 };
    let bands: usize = if interleaved {
        dims[2].size as usize
    } else if nd > 2 {
        dims[..nd - 2].iter().map(|d| d.size as usize).product::<usize>().max(1)
    } else {
        1
    };
    let unsigned = attr_str(var, "_Unsigned").is_some_and(|s| s.eq_ignore_ascii_case("true"));
    let name = read_path;
    // The values band by band: pixel-interleaved ones are read through their bands axis first.
    macro_rules! values {
        ($t:ty) => {{
            let a = f.read_variable::<$t>(&name).map_err(|e| err(path, e))?;
            if interleaved { a.permuted_axes(vec![2, 0, 1]) } else { a }
        }};
    }
    macro_rules! typed {
        ($t:ty, $variant:ident) => {
            RasterData::$variant(values!($t).iter().copied().collect())
        };
    }
    // Signed types flagged `_Unsigned` keep their bits.
    macro_rules! reinterpret {
        ($t:ty, $u:ty, $variant:ident) => {
            RasterData::$variant(values!($t).iter().map(|v| *v as $u).collect())
        };
    }
    // A compound of two members of the same numeric type: GDAL's complex bands (`CInt16`,
    // `CInt32`, `CFloat32`, `CFloat64`), read as pairs of real and imaginary bands.
    let complex = complex_part(var.dtype());
    let data = match (var.dtype(), unsigned) {
        _ if complex.is_some() => {
            let part = complex.unwrap_or(wbraster::DataType::F32);
            let raw = f.read_variable_native_bytes(&name).map_err(|e| err(path, e))?;
            complex_bands(&raw, part, cols * rows, bands, interleaved)?
        }
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

    // Coordinates: the variables named after the dimensions of the rows and columns.
    // GDAL ignores coordinates not named or marked as spatial ones, unless the file comes from GMT
    // or the variable has `res` and `transform` attributes.
    let ignore_axis_checks = (attr(var, "res").is_some() && attr(var, "transform").is_some())
        || f.global_attributes().is_ok_and(|a| a.iter().any(|a| a.name == "GMT_version"));
    // The axis values, and whether they are stored as floats (spacing is checked more loosely).
    let coord = |d: &str| -> Option<(Vec<f64>, bool, bool)> {
        let (p, v) = var_ref.lookup(d)?;
        let spatial = ignore_axis_checks || is_spatial_axis(v);
        let values = (v.ndim() == 1 && spatial).then(|| f.read_variable_as_f64(&p).ok().map(|a| a.iter().copied().collect())).flatten()?;
        Some((values, matches!(v.dtype(), NcType::Float), is_longitude(v)))
    };
    let (xs, ys) = if nd >= 2 { (coord(&dims[x_dim].name), coord(&dims[y_dim].name)) } else { (None, None) };
    let grid_mapping =
        attr_str(var, "grid_mapping").and_then(|g| var_ref.lookup(g.split_whitespace().next().unwrap_or_default())).map(|(_, v)| v);
    let projected = grid_mapping.and_then(|g| attr_str(g, "grid_mapping_name")).is_some_and(|n| n != "latitude_longitude");
    let xs = xs.map(|(mut x, float, longitude)| {
        if longitude {
            unwrap_longitudes(&mut x);
        }
        (x, float)
    });
    let ys = ys.map(|(y, float, _)| (y, float));
    let mut gt = None;
    let mut south_up = false;
    if let (Some((x, x_float)), Some((y, y_float))) = (&xs, &ys)
        && cols > 1
        && rows > 1
        && evenly_spaced(x, *x_float, false)
        && evenly_spaced(y, *y_float, !projected)
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
    // GDAL takes the CRS from the grid mapping only: longitude and latitude axes alone leave the
    // raster without one.
    let crs = grid_mapping.and_then(grid_mapping_crs);
    // Geolocation arrays: the CF `coordinates` (or 2D `lon` and `lat` on the variable's
    // dimensions), NASA's ocean color and EMIT layouts, or irregular longitude and latitude axes.
    let file = path.display().to_string();
    let mut geolocation = None;
    if nd >= 2 {
        match cf_geolocation(f, var_ref, var) {
            Some((x, y)) => {
                // The geotransform of coordinate axes that are not these arrays goes, unless the
                // CRS is known.
                let axes_are_arrays =
                    Some(&x) == var_ref.full_name(&dims[x_dim].name).as_ref() && Some(&y) == var_ref.full_name(&dims[y_dim].name).as_ref();
                if !axes_are_arrays && crs.is_none() && from_coordinates {
                    gt = None;
                }
                geolocation = Some((x, y));
            }
            None if gt.is_none() => {
                let axis = |d: &str, test: fn(&NcVariable) -> bool| var_ref.lookup(d).is_some_and(|(_, v)| v.ndim() == 1 && test(v));
                // By the axes' names from the root, which may be in the variable's group.
                if axis(&dims[x_dim].name, is_longitude)
                    && axis(&dims[y_dim].name, is_latitude)
                    && let (Some(x), Some(y)) = (var_ref.full_name(&dims[x_dim].name), var_ref.full_name(&dims[y_dim].name))
                {
                    geolocation = Some((x, y));
                }
            }
            None => {}
        }
    }
    let mut nodata = attr_f64(var, "_FillValue").or_else(|| attr_f64(var, "missing_value"));
    // Without one, netCDF's default fill value of the type (not for bytes), as GDAL reports it.
    // netCDF-4 variables written without fill values (`_NoFill`): HDF5 datasets whose fill value
    // is undefined, as the netCDF library reads them.
    // Looked for only without a fill value attribute: netCDF-4 files are opened again for it.
    let no_fill = || {
        attr_str(var, "_NoFill").is_some_and(|v| v.eq_ignore_ascii_case("true"))
            || (!matches!(f.format(), NcFormat::Classic | NcFormat::Offset64 | NcFormat::Cdf5)
                && super::hdf5::open_file(path)
                    .ok()
                    .and_then(|h| h.dataset(&format!("/{}", var_ref.read_path())).ok())
                    .is_some_and(|d| d.fill_value().is_none_or(|fv| !fv.defined)))
    };
    if nodata.is_none() && !unsigned && complex.is_none() && !no_fill() {
        nodata = match var.dtype() {
            NcType::Short => Some(-32767.0),
            NcType::Int => Some(-2147483647.0),
            NcType::Float => Some(9.969_209_968_386_869e36),
            NcType::Double => Some(9.969_209_968_386_869e36),
            NcType::UShort => Some(65535.0),
            NcType::UInt => Some(4294967295.0),
            NcType::Int64 => Some(-9223372036854775806.0),
            NcType::UInt64 => Some(18446744073709551614.0),
            _ => None,
        };
    }
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
    if complex.is_some() {
        nodata = None;
    }
    let raster_bands = if complex.is_some() { bands * 2 } else { bands };
    let mut raster =
        common::make_raster(cols, rows, raster_bands, data, gt.unwrap_or(common::NO_GEOTRANSFORM), crs.unwrap_or_default(), nodata)?;
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
    let mut out = RasterDataset::with_driver(raster, RasterDriver::NetCDF, nodata.is_some());
    out.complex = complex.is_some();
    // The arrays' CRS: the geographic CRS of the raster's (WGS 84 without one), as GDAL.
    let geographic = out.crs().geographic_wkt();
    out.geolocation = geolocation.map(|(x, y)| crate::Geolocation {
        srs: geographic.unwrap_or_else(|| common::WGS84_LAT_LONG_WKT.to_string()),
        x_dataset: format!("NETCDF:\"{file}\":{x}"),
        x_band: 1,
        y_dataset: format!("NETCDF:\"{file}\":{y}"),
        y_band: 1,
        pixel_offset: 0.0,
        pixel_step: 1.0,
        line_offset: 0.0,
        line_step: 1.0,
        pixel_center: true,
    });
    Ok(out)
}

/// Whether a variable is laid out as EMIT's radiance and reflectance, `(downtrack, crosstrack,
/// bands)`: three dimensions, the last named `bands` or `band` (exactly), which GDAL's netCDF driver
/// reads as the bands, the first two as the rows and columns.
fn trailing_bands(v: &NcVariable) -> bool {
    let dims = v.dimensions();
    dims.len() == 3 && matches!(dims[2].name.as_str(), "bands" | "band")
}

/// Whether a variable holds longitudes, as GDAL's `NCDFIsVarLongitude()` decides for an existing
/// variable: by its units, standard name, axis or long name, not with units `m` or `1`.
fn is_longitude(v: &NcVariable) -> bool {
    is_axis(v, &["degrees_east", "degree_east", "degrees_E"], "longitude", "X")
}

/// Whether a variable holds latitudes (`NCDFIsVarLatitude()`).
fn is_latitude(v: &NcVariable) -> bool {
    is_axis(v, &["degrees_north", "degree_north", "degrees_N"], "latitude", "Y")
}

fn is_axis(v: &NcVariable, units: &[&str], name: &str, axis: &str) -> bool {
    let get = |k: &str| attr_str(v, k).map(|s| s.trim_end_matches('\0').to_string());
    let is = |k: &str, values: &[&str]| get(k).is_some_and(|s| values.iter().any(|x| s.eq_ignore_ascii_case(x)));
    let found = is("units", units) || is("standard_name", &[name]) || is("axis", &[axis]) || is("long_name", &[name]);
    found && !is("units", &["m", "1"])
}

/// The geolocation arrays of a variable, as GDAL's netCDF driver finds them
/// (`ProcessCFGeolocation()`): the longitude and latitude variables its CF `coordinates`
/// attribute names (or 2D `lon` and `lat` variables on its last two dimensions), else NASA's
/// ocean color layout (`navigation_data`) and EMIT's (`location`, for 2-D variables and those of a
/// trailing `bands` dimension). Their full names.
fn cf_geolocation(f: &NcFile, var_ref: &VarRef, var: &NcVariable) -> Option<(String, String)> {
    let dims = var.dimensions();
    let nd = dims.len();
    let same_dims = |a: &[netcdf_reader::NcDimension], b: &[netcdf_reader::NcDimension]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.name == y.name && x.size == y.size)
    };
    let coordinates = attr_str(var, "coordinates").or_else(|| {
        // `lon` and `lat` in the variable's own group, on its last two dimensions (so never for a
        // variable of a trailing `bands` dimension, as in GDAL).
        let group = var_ref.groups.last()?;
        let lon = group.variables.iter().find(|v| v.name == "lon")?;
        let lat = group.variables.iter().find(|v| v.name == "lat")?;
        (lon.ndim() == 2
            && lat.ndim() == 2
            && same_dims(lon.dimensions(), lat.dimensions())
            && same_dims(lon.dimensions(), &dims[nd - 2..]))
        .then(|| "lon lat".to_string())
    });
    if let Some(coordinates) = coordinates {
        let tokens: Vec<&str> = coordinates.split_whitespace().collect();
        if tokens.len() < 2 {
            return None;
        }
        let (mut x, mut y) = (None, None);
        for t in tokens {
            let Some((_, v)) = var_ref.lookup(t) else { continue };
            if is_longitude(v) {
                x = Some(t);
            } else if is_latitude(v) {
                y = Some(t);
            }
        }
        return Some((var_ref.full_name(x?)?, var_ref.full_name(y?)?));
    }
    let root = f.root_group().ok()?;
    let group_named = |name: &str| root.groups.iter().find(|g| g.name == name);
    let pair = |g: &NcGroup, lon: &str, lat: &str| -> Option<(Vec<netcdf_reader::NcDimension>, Vec<netcdf_reader::NcDimension>)> {
        let (lon, lat) = (g.variables.iter().find(|v| v.name == lon)?, g.variables.iter().find(|v| v.name == lat)?);
        (lon.ndim() == 2 && lat.ndim() == 2).then(|| (lon.dimensions().to_vec(), lat.dimensions().to_vec()))
    };
    // NASA ocean color level 2: geophysical_data variables, navigation_data/longitude and
    // latitude of the same sizes.
    let in_group = var_ref.groups.last().map(|g| g.name.as_str());
    if in_group == Some("geophysical_data")
        && nd == 2
        && let Some(nav) = group_named("navigation_data")
        && let Some((lon, lat)) = pair(nav, "longitude", "latitude")
        && lon.iter().map(|d| &d.name).eq(lat.iter().map(|d| &d.name))
        && lon.iter().map(|d| d.size).eq(dims.iter().map(|d| d.size))
    {
        return Some(("/navigation_data/longitude".into(), "/navigation_data/latitude".into()));
    }
    // EMIT: location/lon and lat on the variable's first two dimensions (its rows and columns),
    // by name.
    if (nd == 2 || trailing_bands(var))
        && let Some(loc) = group_named("location")
        && let Some((lon, lat)) = pair(loc, "lon", "lat")
        && same_dims(&lon, &lat)
        && same_dims(&lon, &dims[..2])
    {
        return Some(("/location/lon".into(), "/location/lat".into()));
    }
    None
}

/// The type of the parts of a complex variable, as GDAL's netCDF driver reads compounds: two
/// members of the same type, `short`, `int`, `float` or `double`, one after the other.
fn complex_part(t: &NcType) -> Option<wbraster::DataType> {
    let NcType::Compound { size, fields } = t else { return None };
    let [first, second] = fields.as_slice() else { return None };
    if second.dtype != first.dtype {
        return None;
    }
    let part = match first.dtype {
        NcType::Short => wbraster::DataType::I16,
        NcType::Int => wbraster::DataType::I32,
        NcType::Float => wbraster::DataType::F32,
        NcType::Double => wbraster::DataType::F64,
        _ => return None,
    };
    (first.offset == 0 && second.offset as usize == part.size_bytes() && *size as usize == 2 * part.size_bytes()).then_some(part)
}

/// Splits complex elements (real then imaginary part, native byte order), band by band or
/// pixel-interleaved (`interleaved`), into a real and an imaginary band per band.
fn complex_bands(raw: &[u8], part: wbraster::DataType, plane: usize, bands: usize, interleaved: bool) -> Result<RasterData> {
    let size = part.size_bytes();
    if raw.len() < plane * bands * 2 * size {
        return Err(Error::InvalidData("complex variable shorter than its dimensions".into()));
    }
    let mut parts = Vec::with_capacity(plane * bands * 2 * size);
    for b in 0..bands {
        for offset in [0, size] {
            for i in 0..plane {
                let element = if interleaved { i * bands + b } else { b * plane + i };
                let at = element * 2 * size + offset;
                parts.extend_from_slice(&raw[at..at + size]);
            }
        }
    }
    let layout = common::Layout {
        cols: plane,
        rows: 1,
        bands: bands * 2,
        data_type: part,
        little_endian: cfg!(target_endian = "little"),
        interleave: common::Interleave::Bsq,
        offset: 0,
        bottom_up: false,
    };
    common::decode(&parts, &layout)
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

/// Whether coordinates are evenly spaced, as GDAL checks them: the first, middle and last steps
/// equal within 0.05 % (0.2 % for floats); for latitudes (`lenient`), within 0.1 degree.
fn evenly_spaced(v: &[f64], float: bool, lenient: bool) -> bool {
    let n = v.len();
    if n < 2 {
        return false;
    }
    if n == 2 {
        return true;
    }
    let (begin, middle, last) = (v[1] - v[0], v[n / 2 + 1] - v[n / 2], v[n - 1] - v[n - 2]);
    let eps = if float { 0.002 } else { 0.0005 } * begin.abs().max(middle.abs()).max(last.abs());
    let below = |a: f64, b: f64, e: f64| (a - b).abs() <= e;
    (below(begin, last, eps) && below(begin, middle, eps) && below(middle, last, eps))
        || (lenient && below(begin, last, 0.1) && below(begin, middle, 0.1) && below(middle, last, 0.1))
}

/// Longitudes as GDAL reads them: ]180, 540] shifted to ]-180, 180], and a single wrap around
/// the antimeridian (west east of east) undone.
fn unwrap_longitudes(x: &mut [f64]) {
    let n = x.len();
    if n < 2 {
        return;
    }
    if x[0].min(x[n - 1]) > 180.0 && x[0].max(x[n - 1]) <= 540.0 {
        x.iter_mut().for_each(|v| *v -= 360.0);
    }
    if n > 2 && x[0] >= x[n - 1] && x.windows(2).filter(|w| w[1] < w[0]).count() == 1 {
        let last = x[n - 1];
        x.iter_mut().filter(|v| **v > last).for_each(|v| *v -= 360.0);
    }
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
    super::cf::cf_crs(|k| attr_str(g, k), |k| attr(g, k).and_then(NcAttrValue::as_f64_vec))
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

    #[test]
    fn complex_compounds_have_two_parts_of_one_type() {
        let field = |name: &str, offset: u64, dtype: NcType| netcdf_reader::NcCompoundField { name: name.into(), offset, dtype };
        let compound = |size: u32, fields| NcType::Compound { size, fields };
        let ok = compound(8, vec![field("r", 0, NcType::Float), field("i", 4, NcType::Float)]);
        assert_eq!(complex_part(&ok), Some(wbraster::DataType::F32));
        // The second part of another type, or not right after the first.
        assert_eq!(complex_part(&compound(8, vec![field("r", 0, NcType::Float), field("i", 4, NcType::Int)])), None);
        assert_eq!(complex_part(&compound(16, vec![field("r", 0, NcType::Float), field("i", 8, NcType::Float)])), None);
    }
}
