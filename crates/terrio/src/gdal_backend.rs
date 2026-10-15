//! The optional GDAL backend (the `gdal` cargo feature): opens anything GDAL can when terrio has
//! no driver for it, and writes rasters with GDAL's drivers.
//!
//! This is the only part of terrio that needs a C library. It is off by default; with the
//! feature enabled, GDAL (3.4 or later) must be installed where `pkg-config` (or `GDAL_HOME`)
//! finds it.
//!
//! Datasets GDAL opens are converted into terrio's data model, so everything else (operations,
//! writing, `info`) works on them unchanged:
//!
//! - rasters become a [`RasterDataset`] read into memory: every band, with the geotransform, the
//!   CRS, the nodata value of the first band, and the dataset's metadata (subdatasets included);
//! - vector layers become [`wbvector::Layer`]s, with their schema, geometries (curves
//!   linearized) and CRS.
//!
//! [`crate::open`], [`crate::open_raster`], [`crate::open_vector`] and [`crate::list_layers`]
//! fall back to GDAL when terrio's own drivers cannot open a path, and use GDAL directly for paths
//! starting with [`PREFIX`] (`GDAL:`). GDAL's own dataset names work too, such as
//! `HDF5:"file.h5"://group/dataset` or `HDF4_SDS:UNKNOWN:"file.hdf":0`. Remote paths are handed
//! to GDAL's virtual file systems (`/vsicurl/`, `/vsis3/`, `/vsizip/`).
//!
//! ```no_run
//! # #[cfg(feature = "gdal")] {
//! // A GRIB2 file: terrio has no GRIB driver, so GDAL reads it.
//! let grib = terrio::open_raster("forecast.grib2")?;
//! // Force GDAL even for a format terrio reads itself.
//! let tif = terrio::open_raster("GDAL:dem.tif")?;
//! // Write a format terrio cannot write, with GDAL's driver of that name.
//! terrio::gdal_backend::write_raster(&tif, "dem.ntf", "NITF", &[])?;
//! # }
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::dataset::{VectorDataset, VectorOpenOptions, finish_vector};
use crate::drivers::common;
use crate::formats::{RasterDriver, VectorDriver};
use crate::vfs::Location;
use crate::{Dataset, Error, RasterDataset, Result};
use gdal::raster::GdalDataType;
use gdal::vector::{FieldValue as GdalValue, LayerAccess, OGRFieldType, OGRwkbGeometryType};
use gdal::{Dataset as GdalDataset, DatasetOptions, DriverManager, GdalOpenFlags, Metadata};
use wbraster::raster::RasterData;
use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// The prefix that sends a path to GDAL rather than terrio's own drivers: `GDAL:file.hdf`.
pub const PREFIX: &str = "GDAL:";

/// The version of the GDAL library terrio is linked with, such as `3.12.2`.
pub fn gdal_version() -> String {
    gdal::version_info("RELEASE_NAME")
}

/// Whether the linked GDAL has a driver with this short name (`HFA`, `NITF`, `GRIB`).
pub fn has_driver(name: &str) -> bool {
    DriverManager::get_driver_by_name(name).is_ok()
}

/// The path without its [`PREFIX`], when it has one (case-insensitive).
pub fn strip_prefix(path: &str) -> Option<&str> {
    path.get(..PREFIX.len()).filter(|p| p.eq_ignore_ascii_case(PREFIX)).map(|_| &path[PREFIX.len()..])
}

/// Opens a dataset with GDAL: its rasters when it has any (or its first subdataset), otherwise
/// its vector layers.
pub fn open(path: &str) -> Result<Dataset> {
    open_with(path, &VectorOpenOptions::default())
}

/// [`open`], with options for vector datasets.
pub(crate) fn open_with(path: &str, options: &VectorOpenOptions) -> Result<Dataset> {
    let _quiet = Quiet::new();
    let ds = gdal_open(path, GdalOpenFlags::GDAL_OF_RASTER | GdalOpenFlags::GDAL_OF_VECTOR)?;
    if ds.raster_count() > 0 || !subdatasets_of(&ds).is_empty() {
        return raster_from(&ds, path).map(Dataset::Raster);
    }
    vector_from(ds, path, options).map(Dataset::Vector)
}

/// Opens a raster with GDAL. A container of subdatasets without bands of its own (HDF4, HDF5,
/// netCDF) opens its first subdataset; the others are listed in the raster's metadata as
/// `SUBDATASET_N_NAME`, as GDAL lists them, and open by those names.
pub fn open_raster(path: &str) -> Result<RasterDataset> {
    let _quiet = Quiet::new();
    let ds = gdal_open(path, GdalOpenFlags::GDAL_OF_RASTER)?;
    raster_from(&ds, path)
}

/// Opens the vector layers of a dataset with GDAL. `options.layers` selects layers by name, and
/// `options.spatial_filter` is applied as GDAL's spatial filter.
pub fn open_vector(path: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    let _quiet = Quiet::new();
    let ds = gdal_open(path, GdalOpenFlags::GDAL_OF_VECTOR)?;
    vector_from(ds, path, options)
}

/// The names of a vector dataset's layers, as GDAL lists them.
pub fn list_layers(path: &str) -> Result<Vec<String>> {
    let _quiet = Quiet::new();
    let ds = gdal_open(path, GdalOpenFlags::GDAL_OF_VECTOR)?;
    Ok(ds.layers().map(|l| l.name()).collect())
}

/// The subdatasets of a dataset, as `(name, description)` pairs: the names open with
/// [`open_raster`] (or [`crate::open_raster`]).
pub fn subdatasets(path: &str) -> Result<Vec<(String, String)>> {
    let _quiet = Quiet::new();
    let ds = gdal_open(path, GdalOpenFlags::GDAL_OF_RASTER)?;
    Ok(subdatasets_of(&ds))
}

/// Writes a raster with the GDAL driver of this short name (`HFA`, `NITF`, `GRIB`, `KEA`...),
/// with GDAL creation options (`KEY`, `VALUE`). The nodata value is declared when the dataset has
/// one. Drivers that can only copy datasets (`CreateCopy`) work too: the raster goes through an
/// in-memory GDAL dataset.
pub fn write_raster(ds: &RasterDataset, path: &str, driver: &str, creation_options: &[(String, String)]) -> Result<()> {
    write_raster_with(&ds.raster, ds.nodata(), path, driver, creation_options)
}

/// Writes a raster with a GDAL driver, declaring `nodata`.
pub(crate) fn write_raster_with(
    r: &wbraster::Raster,
    nodata: Option<f64>,
    path: &str,
    driver: &str,
    creation_options: &[(String, String)],
) -> Result<()> {
    let _quiet = Quiet::new();
    let target =
        DriverManager::get_driver_by_name(driver).map_err(|e| Error::Unsupported(format!("GDAL has no driver '{driver}': {e}")))?;
    let mem_driver = DriverManager::get_driver_by_name("MEM").map_err(gdal_err)?;
    macro_rules! create {
        ($t:ty, $v:ident) => {{
            let mem = mem_driver.create_with_band_type::<$t, _>("", r.cols, r.rows, r.bands).map_err(gdal_err)?;
            if let RasterData::$v(values) = &r.data {
                let n = r.cols * r.rows;
                for b in 0..r.bands {
                    let mut band = mem.rasterband(b + 1).map_err(gdal_err)?;
                    let mut buffer = gdal::raster::Buffer::new((r.cols, r.rows), values[b * n..(b + 1) * n].to_vec());
                    band.write((0, 0), (r.cols, r.rows), &mut buffer).map_err(gdal_err)?;
                }
            }
            mem
        }};
    }
    let mut mem = match r.data_type {
        wbraster::DataType::U8 => create!(u8, U8),
        wbraster::DataType::I8 => create!(i8, I8),
        wbraster::DataType::U16 => create!(u16, U16),
        wbraster::DataType::I16 => create!(i16, I16),
        wbraster::DataType::U32 => create!(u32, U32),
        wbraster::DataType::I32 => create!(i32, I32),
        wbraster::DataType::U64 => create!(u64, U64),
        wbraster::DataType::I64 => create!(i64, I64),
        wbraster::DataType::F32 => create!(f32, F32),
        wbraster::DataType::F64 => create!(f64, F64),
    };
    let gt = crate::geo_transform(r);
    mem.set_geo_transform(&gt).map_err(gdal_err)?;
    let crs = crate::crs::SpatialRef::from_raster_crs(&r.crs);
    let srs = match crs.epsg {
        Some(code) => gdal::spatial_ref::SpatialRef::from_epsg(code).ok(),
        None => crs.to_wkt().and_then(|wkt| gdal::spatial_ref::SpatialRef::from_wkt(&wkt).ok()),
    };
    if let Some(srs) = srs {
        mem.set_spatial_ref(&srs).map_err(gdal_err)?;
    }
    if let Some(nodata) = nodata {
        for b in 1..=r.bands {
            let mut band = mem.rasterband(b).map_err(gdal_err)?;
            match r.data_type {
                wbraster::DataType::U64 => band.set_no_data_value_u64(Some(nodata as u64)),
                wbraster::DataType::I64 => band.set_no_data_value_i64(Some(nodata as i64)),
                _ => band.set_no_data_value(Some(nodata)),
            }
            .map_err(gdal_err)?;
        }
    }
    let mut options = gdal::cpl::CslStringList::new();
    for (k, v) in creation_options {
        options.set_name_value(k, v).map_err(gdal_err)?;
    }
    mem.create_copy(&target, path, &options).map_err(gdal_err)?;
    Ok(())
}

/// Silences GDAL's error and warning messages (they are returned as errors instead) while alive.
struct Quiet;

impl Quiet {
    fn new() -> Self {
        // SAFETY: pushes a handler GDAL provides on the calling thread's handler stack; `drop`
        // pops it on the same thread.
        unsafe { gdal_sys::CPLPushErrorHandler(Some(gdal_sys::CPLQuietErrorHandler)) };
        Quiet
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        // SAFETY: pops the handler `new` pushed.
        unsafe { gdal_sys::CPLPopErrorHandler() };
    }
}

fn gdal_err(e: gdal::errors::GdalError) -> Error {
    Error::InvalidData(format!("GDAL: {e}"))
}

/// The name GDAL opens a terrio location by: remote and archived files through GDAL's virtual
/// file systems, and anything else (GDAL dataset names included) as it is.
fn gdal_name(path: &str) -> String {
    fn name(loc: &Location) -> Option<String> {
        Some(match loc {
            Location::Local(p) => p.to_str()?.to_string(),
            Location::Url(u) => format!("/vsicurl/{u}"),
            Location::S3 { bucket, key } => format!("/vsis3/{bucket}/{key}"),
            Location::Zip { archive, inner } => {
                format!("/vsizip/{}{}", name(archive)?, inner.as_deref().map(|i| format!("/{i}")).unwrap_or_default())
            }
        })
    }
    // GDAL's own virtual file names and dataset names (`HDF5:"f.h5"://x`) go through untouched.
    if path.starts_with("/vsi") || path.contains(":\"") {
        return path.to_string();
    }
    name(&Location::parse(path)).unwrap_or_else(|| path.to_string())
}

fn gdal_open(path: &str, flags: GdalOpenFlags) -> Result<GdalDataset> {
    let name = gdal_name(path);
    let options = DatasetOptions { open_flags: flags, ..Default::default() };
    GdalDataset::open_ex(&name, options).map_err(|e| match e {
        gdal::errors::GdalError::NullPointer { .. } => Error::Unsupported(format!(
            "'{path}': GDAL cannot open it ({})",
            last_error().unwrap_or_else(|| "format not recognized".into())
        )),
        e => gdal_err(e),
    })
}

/// GDAL's last error message, if any.
fn last_error() -> Option<String> {
    // SAFETY: CPLGetLastErrorMsg returns a pointer to a thread-local, NUL-terminated string.
    let msg = unsafe { std::ffi::CStr::from_ptr(gdal_sys::CPLGetLastErrorMsg()) }.to_string_lossy().into_owned();
    (!msg.is_empty()).then_some(msg)
}

fn subdatasets_of(ds: &GdalDataset) -> Vec<(String, String)> {
    let items = ds.metadata_domain("SUBDATASETS").unwrap_or_default();
    let get = |key: String| {
        items.iter().find_map(|kv| kv.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case(&key)).map(|(_, v)| v.to_string()))
    };
    (1..).map_while(|i| get(format!("SUBDATASET_{i}_NAME")).map(|n| (n, get(format!("SUBDATASET_{i}_DESC")).unwrap_or_default()))).collect()
}

fn raster_from(ds: &GdalDataset, path: &str) -> Result<RasterDataset> {
    if ds.raster_count() == 0 {
        let subdatasets = subdatasets_of(ds);
        let Some((first, _)) = subdatasets.first() else {
            return Err(Error::Unsupported(format!("'{path}' has no raster bands")));
        };
        let sub = gdal_open(first, GdalOpenFlags::GDAL_OF_RASTER)?;
        let mut out = raster_from(&sub, first)?;
        out.raster.metadata.retain(|(k, _)| !k.starts_with("SUBDATASET_"));
        for (i, (name, desc)) in subdatasets.iter().enumerate() {
            out.raster.metadata.push((format!("SUBDATASET_{}_NAME", i + 1), name.clone()));
            out.raster.metadata.push((format!("SUBDATASET_{}_DESC", i + 1), desc.clone()));
        }
        return Ok(out);
    }
    let (cols, rows) = ds.raster_size();
    let bands = ds.raster_count();
    let first = ds.rasterband(1).map_err(gdal_err)?;
    let band_type = first.band_type();
    macro_rules! read {
        ($t:ty, $v:ident) => {{
            let mut all: Vec<$t> = Vec::with_capacity(cols * rows * bands);
            for b in 1..=bands {
                let band = ds.rasterband(b).map_err(gdal_err)?;
                let buffer = band.read_band_as::<$t>().map_err(gdal_err)?;
                all.extend_from_slice(buffer.data());
            }
            RasterData::$v(all)
        }};
    }
    let data = match band_type {
        GdalDataType::UInt8 => read!(u8, U8),
        GdalDataType::Int8 => read!(i8, I8),
        GdalDataType::UInt16 => read!(u16, U16),
        GdalDataType::Int16 => read!(i16, I16),
        GdalDataType::UInt32 => read!(u32, U32),
        GdalDataType::Int32 => read!(i32, I32),
        GdalDataType::UInt64 => read!(u64, U64),
        GdalDataType::Int64 => read!(i64, I64),
        GdalDataType::Float32 => read!(f32, F32),
        GdalDataType::Float64 => read!(f64, F64),
        // Complex and unknown types (CInt16, CFloat32...) have no equivalent in terrio's model.
        _ => return Err(Error::Unsupported(format!("'{path}': GDAL band type {} is not supported", band_type.name()))),
    };
    let nodata = match band_type {
        GdalDataType::UInt64 => first.no_data_value_u64().map(|v| v as f64),
        GdalDataType::Int64 => first.no_data_value_i64().map(|v| v as f64),
        _ => first.no_data_value(),
    };
    let gt = ds.geo_transform().unwrap_or(common::NO_GEOTRANSFORM);
    let crs = ds.spatial_ref().ok().and_then(|srs| srs_to_crs(&srs)).unwrap_or_default();
    let mut raster = common::make_raster(cols, rows, bands, data, gt, crs, nodata)?;
    if let Some(scale) = first.scale().filter(|s| *s != 1.0) {
        raster.metadata.push(("scale_factor".into(), format!("{scale}")));
    }
    if let Some(offset) = first.offset().filter(|o| *o != 0.0) {
        raster.metadata.push(("add_offset".into(), format!("{offset}")));
    }
    for item in ds.metadata_domain("").unwrap_or_default() {
        if let Some((k, v)) = item.split_once('=') {
            raster.metadata.push((k.to_string(), v.to_string()));
        }
    }
    for (i, (name, desc)) in subdatasets_of(ds).into_iter().enumerate() {
        raster.metadata.push((format!("SUBDATASET_{}_NAME", i + 1), name));
        raster.metadata.push((format!("SUBDATASET_{}_DESC", i + 1), desc));
    }
    Ok(RasterDataset { raster, driver: RasterDriver::Gdal, has_nodata: nodata.is_some(), affine: None })
}

/// terrio's CRS for a GDAL spatial reference: the EPSG code GDAL identifies (as `gdalsrsinfo -e`
/// does), with GDAL's WKT; terrio's own identification when GDAL finds no code.
fn srs_to_crs(srs: &gdal::spatial_ref::SpatialRef) -> Option<wbraster::CrsInfo> {
    let wkt = srs.to_wkt().ok().filter(|w| !w.is_empty())?;
    let code = match (srs.auth_name().as_deref(), srs.auth_code()) {
        (Some("EPSG"), Ok(code)) => u32::try_from(code).ok(),
        _ => epsg_match(srs).or_else(|| {
            // GDAL's identification of UTM zones and common geographic CRSs (NITF's UTM zones).
            let mut srs = srs.clone();
            srs.auto_identify_epsg().ok()?;
            (srs.auth_name().as_deref() == Some("EPSG")).then(|| srs.auth_code().ok().and_then(|c| u32::try_from(c).ok())).flatten()
        }),
    };
    Some(match code {
        Some(code) => {
            let mut info = wbraster::CrsInfo::from_epsg(code);
            if info.wkt.is_none() {
                info.wkt = Some(wkt);
            }
            info
        }
        None => common::crs_from_text(&wkt).unwrap_or_else(|| wbraster::CrsInfo { wkt: Some(wkt), ..Default::default() }),
    })
}

/// The EPSG CRS GDAL matches a spatial reference with, with at least 90 % confidence.
fn epsg_match(srs: &gdal::spatial_ref::SpatialRef) -> Option<u32> {
    // SAFETY: OSRFindMatches returns an array of `n` owned spatial references and their
    // confidences; both are read, then freed with the functions GDAL documents for them.
    unsafe {
        let mut n = 0;
        let mut confidences: *mut std::ffi::c_int = std::ptr::null_mut();
        let matches = gdal_sys::OSRFindMatches(srs.to_c_hsrs(), std::ptr::null_mut(), &mut n, &mut confidences);
        if matches.is_null() {
            return None;
        }
        let mut found = None;
        for i in 0..usize::try_from(n).unwrap_or(0) {
            if *confidences.add(i) < 90 {
                continue;
            }
            let m = *matches.add(i);
            let name = gdal_sys::OSRGetAuthorityName(m, std::ptr::null());
            let code = gdal_sys::OSRGetAuthorityCode(m, std::ptr::null());
            if !name.is_null() && !code.is_null() && std::ffi::CStr::from_ptr(name).to_bytes() == b"EPSG" {
                found = std::ffi::CStr::from_ptr(code).to_str().ok().and_then(|c| c.parse().ok());
                break;
            }
        }
        gdal_sys::OSRFreeSRSArray(matches);
        gdal_sys::VSIFree(confidences.cast());
        found
    }
}

fn vector_from(ds: GdalDataset, path: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    let names: Vec<String> = ds.layers().map(|l| l.name()).collect();
    if names.is_empty() {
        return Err(Error::Unsupported(format!("'{path}' has no vector layers")));
    }
    let mut layers = Vec::new();
    for mut layer in ds.layers() {
        let name = layer.name();
        if !options.layers.is_empty() && !options.layers.contains(&name) {
            continue;
        }
        if let Some((x0, y0, x1, y1)) = options.spatial_filter {
            layer.set_spatial_filter_rect(x0, y0, x1, y1);
        }
        layers.push(layer_from(&mut layer, &name)?);
    }
    if layers.is_empty() {
        return Err(Error::InvalidArgument(format!("no layer named {} (layers: {})", options.layers.join(", "), names.join(", "))));
    }
    // `finish_vector` names unnamed layers after the file: use the path without GDAL's syntax.
    Ok(finish_vector(layers, strip_prefix(path).unwrap_or(path), VectorDriver::Gdal))
}

fn layer_from(layer: &mut gdal::vector::Layer, name: &str) -> Result<Layer> {
    let mut out = Layer::new(name);
    let defn = layer.defn();
    out.geom_type = geometry_type(defn.geometry_type());
    let kinds: Vec<OGRFieldType::Type> = defn.fields().map(|f| f.field_type()).collect();
    for field in defn.fields() {
        let t = match field.field_type() {
            OGRFieldType::OFTInteger | OGRFieldType::OFTInteger64 => FieldType::Integer,
            OGRFieldType::OFTReal => FieldType::Float,
            OGRFieldType::OFTDate => FieldType::Date,
            OGRFieldType::OFTDateTime => FieldType::DateTime,
            OGRFieldType::OFTBinary => FieldType::Blob,
            OGRFieldType::OFTIntegerList | OGRFieldType::OFTInteger64List | OGRFieldType::OFTRealList | OGRFieldType::OFTStringList => {
                FieldType::Json
            }
            _ => FieldType::Text,
        };
        let mut def = FieldDef::new(field.name(), t);
        if field.width() > 0 {
            def = def.width(field.width() as usize);
        }
        if field.precision() > 0 {
            def = def.precision(field.precision() as usize);
        }
        out.add_field(def);
    }
    if let Some(srs) = layer.spatial_ref()
        && let Some(crs) = srs_to_crs(&srs)
    {
        match crs.epsg {
            Some(code) => out.set_crs_epsg(Some(code)),
            None => out.set_crs_wkt(crs.wkt),
        }
    }
    for (i, feature) in layer.features().enumerate() {
        let geometry = feature.geometry().map(geometry_from).transpose()?.flatten();
        let attributes = (0..kinds.len())
            .map(|k| match feature.field(k) {
                Ok(Some(v)) => value_from(v),
                Ok(None) if kinds[k] == OGRFieldType::OFTBinary => binary_field(&feature, k),
                _ => FieldValue::Null,
            })
            .collect();
        out.push(Feature { fid: feature.fid().unwrap_or(i as u64), geometry, attributes });
    }
    Ok(out)
}

fn geometry_type(t: OGRwkbGeometryType::Type) -> Option<GeometryType> {
    Some(match gdal::vector::geometry_type_flatten(t) {
        OGRwkbGeometryType::wkbPoint => GeometryType::Point,
        OGRwkbGeometryType::wkbLineString | OGRwkbGeometryType::wkbCircularString | OGRwkbGeometryType::wkbCompoundCurve => {
            GeometryType::LineString
        }
        OGRwkbGeometryType::wkbPolygon | OGRwkbGeometryType::wkbCurvePolygon => GeometryType::Polygon,
        OGRwkbGeometryType::wkbMultiPoint => GeometryType::MultiPoint,
        OGRwkbGeometryType::wkbMultiLineString | OGRwkbGeometryType::wkbMultiCurve => GeometryType::MultiLineString,
        OGRwkbGeometryType::wkbMultiPolygon | OGRwkbGeometryType::wkbMultiSurface => GeometryType::MultiPolygon,
        OGRwkbGeometryType::wkbGeometryCollection => GeometryType::GeometryCollection,
        _ => return None,
    })
}

/// A GDAL geometry as a terrio one, through WKB; curves are linearized as GDAL does by default.
fn geometry_from(g: &gdal::vector::Geometry) -> Result<Option<Geometry>> {
    if g.is_empty() {
        return Ok(None);
    }
    if let Ok(wkb) = g.wkb()
        && let Ok(geometry) = Geometry::from_wkb(&wkb)
    {
        return Ok(Some(geometry));
    }
    // SAFETY: `g` is a valid geometry; the linear copy GDAL returns is owned here and destroyed
    // after its WKB is exported.
    let wkb = unsafe {
        let linear = gdal_sys::OGR_G_GetLinearGeometry(g.c_geometry(), 0.0, std::ptr::null_mut());
        if linear.is_null() {
            return Err(Error::InvalidData(format!("GDAL: cannot linearize a {}", g.geometry_name())));
        }
        let size = gdal_sys::OGR_G_WkbSize(linear) as usize;
        let mut wkb = vec![0u8; size];
        let rv = gdal_sys::OGR_G_ExportToWkb(linear, gdal_sys::OGRwkbByteOrder::wkbNDR, wkb.as_mut_ptr());
        gdal_sys::OGR_G_DestroyGeometry(linear);
        if rv != gdal_sys::OGRErr::OGRERR_NONE {
            return Err(Error::InvalidData(format!("GDAL: cannot export a {} as WKB", g.geometry_name())));
        }
        wkb
    };
    Ok(Some(Geometry::from_wkb(&wkb)?))
}

fn value_from(v: GdalValue) -> FieldValue {
    let json = |items: Vec<serde_json::Value>| FieldValue::Text(serde_json::Value::Array(items).to_string());
    match v {
        GdalValue::IntegerValue(i) => FieldValue::Integer(i.into()),
        GdalValue::Integer64Value(i) => FieldValue::Integer(i),
        GdalValue::RealValue(x) => FieldValue::Float(x),
        GdalValue::StringValue(s) => FieldValue::Text(s),
        GdalValue::DateValue(d) => FieldValue::Date(d.to_string()),
        GdalValue::DateTimeValue(t) => {
            // GDAL reports times without a time zone as UTC: keep them without one.
            if t.offset().local_minus_utc() == 0 {
                FieldValue::DateTime(t.naive_local().format("%Y-%m-%dT%H:%M:%S%.f").to_string())
            } else {
                FieldValue::DateTime(t.to_rfc3339())
            }
        }
        GdalValue::IntegerListValue(v) => json(v.into_iter().map(Into::into).collect()),
        GdalValue::Integer64ListValue(v) => json(v.into_iter().map(Into::into).collect()),
        GdalValue::RealListValue(v) => json(v.into_iter().map(Into::into).collect()),
        GdalValue::StringListValue(v) => json(v.into_iter().map(Into::into).collect()),
    }
}

fn binary_field(feature: &gdal::vector::Feature, field: usize) -> FieldValue {
    let Ok(index) = i32::try_from(field) else { return FieldValue::Null };
    // SAFETY: the feature handle is valid while `feature` lives; GDAL returns a pointer to its own
    // buffer of `len` bytes, copied before the feature can change.
    unsafe {
        let handle = feature.c_feature();
        if gdal_sys::OGR_F_IsFieldSetAndNotNull(handle, index) == 0 {
            return FieldValue::Null;
        }
        let mut len = 0;
        let ptr = gdal_sys::OGR_F_GetFieldAsBinary(handle, index, &mut len);
        if ptr.is_null() || len <= 0 {
            return FieldValue::Blob(Vec::new());
        }
        FieldValue::Blob(std::slice::from_raw_parts(ptr, len as usize).to_vec())
    }
}
