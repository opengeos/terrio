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
//! The other way, [`write_raster`] and [`write_vector`] write terrio's rasters and vector layers
//! with any GDAL driver that creates datasets (ERDAS Imagine, NITF, KEA; DXF, MapInfo, ESRI File
//! Geodatabases with `OpenFileGDB`, GML, SQLite...), and [`open_bytes`], [`write_raster_bytes`] and
//! [`write_vector_bytes`] do the same in memory, through GDAL's `/vsimem/` file system, for
//! [`crate::memory`].
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
//! // Vector layers too: an ESRI File Geodatabase, with GDAL's OpenFileGDB driver.
//! let roads = terrio::open_vector("roads.gpkg", &Default::default())?;
//! terrio::gdal_backend::write_vector(&roads.layers, "roads.gdb", "OpenFileGDB", &[], &[])?;
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
    write_raster_gt(&ds.raster, ds.nodata(), ds.complex, ds.affine, path, driver, creation_options)
}

/// GDAL's complex type of a part type: `CInt16` for `Int16` parts...
fn complex_type(t: wbraster::DataType) -> Option<gdal_sys::GDALDataType::Type> {
    use gdal_sys::GDALDataType::*;
    Some(match t {
        wbraster::DataType::I16 => GDT_CInt16,
        wbraster::DataType::I32 => GDT_CInt32,
        wbraster::DataType::F32 => GDT_CFloat32,
        wbraster::DataType::F64 => GDT_CFloat64,
        _ => return None,
    })
}

/// A MEM dataset of complex bands (`CInt16`... `CFloat64`) holding a raster's pairs of real and
/// imaginary bands.
fn complex_mem_dataset(r: &wbraster::Raster) -> Result<GdalDataset> {
    let t = complex_type(r.data_type).ok_or_else(|| Error::Unsupported(format!("complex bands of {:?} parts", r.data_type)))?;
    let bands = r.bands / 2;
    let mem = DriverManager::get_driver_by_name("MEM").map_err(gdal_err)?;
    let too_large = || Error::InvalidArgument("raster too large for GDAL".into());
    let (cols, rows) = (i32::try_from(r.cols).map_err(|_| too_large())?, i32::try_from(r.rows).map_err(|_| too_large())?);
    let n = r.cols * r.rows;
    let size = r.data_type.size_bytes();
    // Each band's values, real and imaginary parts interleaved, in native byte order.
    macro_rules! interleave {
        ($v:ident) => {
            (0..bands)
                .map(|b| {
                    let (re, im) = (&$v[2 * b * n..(2 * b + 1) * n], &$v[(2 * b + 1) * n..(2 * b + 2) * n]);
                    let mut out = Vec::with_capacity(n * 2 * size);
                    for i in 0..n {
                        out.extend_from_slice(&re[i].to_ne_bytes());
                        out.extend_from_slice(&im[i].to_ne_bytes());
                    }
                    out
                })
                .collect::<Vec<Vec<u8>>>()
        };
    }
    let buffers = match &r.data {
        RasterData::I16(v) => interleave!(v),
        RasterData::I32(v) => interleave!(v),
        RasterData::F32(v) => interleave!(v),
        RasterData::F64(v) => interleave!(v),
        _ => return Err(Error::Unsupported(format!("complex bands of {:?} parts", r.data_type))),
    };
    let name = std::ffi::CString::default();
    // SAFETY: the MEM dataset is created with `bands` bands of the complex type and owned by the
    // returned `Dataset`; each RasterIO call writes `cols` x `rows` complex values from a buffer
    // of that many.
    unsafe {
        let h = gdal_sys::GDALCreate(mem.c_driver(), name.as_ptr(), cols, rows, bands as i32, t, std::ptr::null_mut());
        if h.is_null() {
            return Err(Error::InvalidData(format!("GDAL: cannot create a complex MEM dataset: {}", last_error().unwrap_or_default())));
        }
        let ds = GdalDataset::from_c_dataset(h);
        for (b, mut buf) in buffers.into_iter().enumerate() {
            let band = gdal_sys::GDALGetRasterBand(h, b as i32 + 1);
            let e = gdal_sys::GDALRasterIO(
                band,
                gdal_sys::GDALRWFlag::GF_Write,
                0,
                0,
                cols,
                rows,
                buf.as_mut_ptr().cast(),
                cols,
                rows,
                t,
                0,
                0,
            );
            if e != gdal_sys::CPLErr::CE_None {
                return Err(Error::InvalidData(format!("GDAL: {}", last_error().unwrap_or_default())));
            }
        }
        Ok(ds)
    }
}

/// Reads the complex bands of a GDAL dataset as pairs of real and imaginary bands of the part
/// type (`Int16` for `CInt16`...).
fn read_complex(ds: &GdalDataset, t: gdal_sys::GDALDataType::Type, cols: usize, rows: usize, bands: usize) -> Result<RasterData> {
    use gdal_sys::GDALDataType::*;
    let (ct, part) = if t == GDT_CInt16 {
        (GDT_CInt16, wbraster::DataType::I16)
    } else if t == GDT_CInt32 {
        (GDT_CInt32, wbraster::DataType::I32)
    } else if t == GDT_CFloat64 {
        (GDT_CFloat64, wbraster::DataType::F64)
    } else {
        // CFloat16 too, read as CFloat32.
        (GDT_CFloat32, wbraster::DataType::F32)
    };
    let size = part.size_bytes();
    let n = cols * rows;
    let mut raw = vec![0u8; n * 2 * size];
    let mut parts: Vec<u8> = Vec::with_capacity(bands * 2 * n * size);
    for b in 1..=bands {
        let band = ds.rasterband(b).map_err(gdal_err)?;
        // SAFETY: the band handle is valid while `band` lives; the buffer holds `cols` x `rows`
        // complex values of the requested type.
        let e = unsafe {
            gdal_sys::GDALRasterIO(
                band.c_rasterband(),
                gdal_sys::GDALRWFlag::GF_Read,
                0,
                0,
                cols as i32,
                rows as i32,
                raw.as_mut_ptr().cast(),
                cols as i32,
                rows as i32,
                ct,
                0,
                0,
            )
        };
        if e != gdal_sys::CPLErr::CE_None {
            return Err(Error::InvalidData(format!("GDAL: {}", last_error().unwrap_or_default())));
        }
        for offset in [0, size] {
            for i in 0..n {
                let at = i * 2 * size + offset;
                parts.extend_from_slice(&raw[at..at + size]);
            }
        }
    }
    let layout = common::Layout {
        cols,
        rows,
        bands: bands * 2,
        data_type: part,
        little_endian: cfg!(target_endian = "little"),
        interleave: common::Interleave::Bsq,
        offset: 0,
        bottom_up: false,
    };
    common::decode(&parts, &layout)
}

/// Writes a raster with a GDAL driver, declaring `nodata`; pairs of bands as complex bands when
/// `complex`.
pub(crate) fn write_raster_with(
    r: &wbraster::Raster,
    nodata: Option<f64>,
    complex: bool,
    path: &str,
    driver: &str,
    creation_options: &[(String, String)],
) -> Result<()> {
    write_raster_gt(r, nodata, complex, None, path, driver, creation_options)
}

/// [`write_raster_with`], with a rotated or south-up geotransform in place of the raster's when
/// `affine` is given.
pub(crate) fn write_raster_gt(
    r: &wbraster::Raster,
    nodata: Option<f64>,
    complex: bool,
    affine: Option<[f64; 6]>,
    path: &str,
    driver: &str,
    creation_options: &[(String, String)],
) -> Result<()> {
    let _quiet = Quiet::new();
    let target =
        DriverManager::get_driver_by_name(driver).map_err(|e| Error::Unsupported(format!("GDAL has no driver '{driver}': {e}")))?;
    if complex {
        let mut mem = complex_mem_dataset(r)?;
        return finish_copy(&mut mem, r, nodata, affine, path, &target, creation_options);
    }
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
    finish_copy(&mut mem, r, nodata, affine, path, &target, creation_options)
}

/// Sets a MEM dataset's georeferencing (`affine`, else the raster's) and nodata value, then
/// copies it with `target`.
fn finish_copy(
    mem: &mut GdalDataset,
    r: &wbraster::Raster,
    nodata: Option<f64>,
    affine: Option<[f64; 6]>,
    path: &str,
    target: &gdal::Driver,
    creation_options: &[(String, String)],
) -> Result<()> {
    let gt = affine.unwrap_or_else(|| crate::geo_transform(r));
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
        for b in 1..=mem.raster_count() {
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
    mem.create_copy(target, path, &options).map_err(gdal_err)?;
    Ok(())
}

/// The short name of the GDAL driver that writes files with this path's extension (`.dxf`: `DXF`,
/// `.gdb`: `OpenFileGDB`, `.ntf`: `NITF`), among the drivers that create vector datasets
/// (`vector`) or rasters. `None` when no driver claims the extension, or several do.
pub fn driver_for_path(path: &str, vector: bool) -> Option<String> {
    let ext = crate::formats::extension(path)?;
    let capability = if vector { "DCAP_VECTOR" } else { "DCAP_RASTER" };
    let mut found: Vec<String> = Vec::new();
    for d in DriverManager::all() {
        let yes = |key: &str| d.metadata_item(key, "").is_some_and(|v| v.eq_ignore_ascii_case("YES"));
        // Vector datasets are written with `GDALCreate` (see [`write_vector`]), rasters either way.
        let creates = yes("DCAP_CREATE") || (!vector && yes("DCAP_CREATECOPY"));
        if !yes(capability) || !creates {
            continue;
        }
        let extensions = d.metadata_item("DMD_EXTENSIONS", "").or_else(|| d.metadata_item("DMD_EXTENSION", ""));
        if extensions.is_some_and(|e| e.split_whitespace().any(|x| x.eq_ignore_ascii_case(&ext))) {
            found.push(d.short_name());
        }
    }
    // GDAL's own choice between drivers sharing an extension (`GDALGetOutputDriversForDatasetName`):
    // the first, unless it is ambiguous.
    match found.as_slice() {
        [one] => Some(one.clone()),
        [first, ..] if ext.eq_ignore_ascii_case("gdb") => Some(first.clone()),
        _ => None,
    }
}

/// Writes vector layers with the GDAL vector driver of this short name (`DXF`, `MapInfo File`,
/// `OpenFileGDB`, `GML`, `SQLite`, `ODS`...), as `ogr2ogr -f` does: one layer per terrio layer,
/// with its fields, geometry type and CRS. `dataset_options` and `layer_options` are GDAL's
/// dataset and layer creation options (`-dsco`, `-lco`). An existing output is replaced.
pub fn write_vector(
    layers: &[Layer],
    path: &str,
    driver: &str,
    dataset_options: &[(String, String)],
    layer_options: &[(String, String)],
) -> Result<()> {
    if layers.is_empty() {
        return Err(Error::InvalidArgument("no layers to write".into()));
    }
    let _quiet = Quiet::new();
    let target =
        DriverManager::get_driver_by_name(driver).map_err(|e| Error::Unsupported(format!("GDAL has no driver '{driver}': {e}")))?;
    let is_vector = target.metadata_item("DCAP_VECTOR", "").is_some_and(|v| v.eq_ignore_ascii_case("YES"));
    if !is_vector || target.metadata_item("DCAP_CREATE", "").is_none() {
        return Err(Error::Unsupported(format!("GDAL's {driver} driver cannot create vector datasets")));
    }
    let p = std::path::Path::new(path);
    if p.exists() {
        // As `ogr2ogr -overwrite`: the driver deletes what it wrote (all of a shapefile's files),
        // anything else is removed as a file, or as a directory only when empty: a directory
        // that is not a dataset of the driver is left alone.
        if target.delete(path).is_err() && p.exists() {
            if !p.is_dir() {
                std::fs::remove_file(p)?;
            } else if std::fs::read_dir(p)?.next().is_none() {
                std::fs::remove_dir(p)?;
            } else {
                return Err(Error::InvalidArgument(format!("'{path}' is a directory that is not a {driver} dataset: not replaced")));
            }
        }
    }
    let mut options = gdal::cpl::CslStringList::new();
    for (k, v) in dataset_options {
        options.set_name_value(k, v).map_err(gdal_err)?;
    }
    let c_path = std::ffi::CString::new(path).map_err(|_| Error::InvalidArgument(format!("invalid path '{path}'")))?;
    // SAFETY: GDALCreate with no bands and an unknown band type creates a vector dataset; the
    // returned handle is owned by the `Dataset`, which closes it.
    let mut ds = unsafe {
        let h = gdal_sys::GDALCreate(target.c_driver(), c_path.as_ptr(), 0, 0, 0, gdal_sys::GDALDataType::GDT_Unknown, options.as_ptr());
        if h.is_null() {
            return Err(Error::InvalidData(format!("GDAL cannot create '{path}': {}", last_error().unwrap_or_default())));
        }
        GdalDataset::from_c_dataset(h)
    };
    let lco: Vec<String> = layer_options.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let lco: Vec<&str> = lco.iter().map(String::as_str).collect();
    for layer in layers {
        write_layer(&mut ds, layer, &lco)?;
    }
    // Errors when the dataset is flushed and closed (field values a format cannot hold were
    // reported as warnings already).
    // SAFETY: resets the calling thread's last error.
    unsafe { gdal_sys::CPLErrorReset() };
    ds.flush_cache().map_err(gdal_err)?;
    drop(ds);
    if let Some(e) = last_failure() {
        return Err(Error::InvalidData(format!("GDAL: writing '{path}': {e}")));
    }
    Ok(())
}

/// GDAL's last error, if it was a failure (`CE_Failure` or worse) rather than a warning.
fn last_failure() -> Option<String> {
    // SAFETY: plain reads of the calling thread's last error.
    let failed = unsafe { gdal_sys::CPLGetLastErrorType() } >= gdal_sys::CPLErr::CE_Failure;
    failed.then(last_error).flatten()
}

/// GDAL's spatial reference for a layer's CRS, in GIS axis order (x, y as terrio stores them).
fn layer_srs(layer: &Layer) -> Option<gdal::spatial_ref::SpatialRef> {
    let crs = layer.crs.as_ref()?;
    let mut srs = match (crs.epsg, crs.wkt.as_deref()) {
        (Some(code), _) => gdal::spatial_ref::SpatialRef::from_epsg(code).ok(),
        (None, Some(wkt)) => gdal::spatial_ref::SpatialRef::from_definition(wkt).ok(),
        _ => None,
    }?;
    srs.set_axis_mapping_strategy(gdal::spatial_ref::AxisMappingStrategy::TraditionalGisOrder);
    Some(srs)
}

/// The geometry type of a layer's features when they share one, a single type and its multi
/// version counting as the multi type (polygons and multipolygons of a GeoPackage layer of
/// unknown type): some drivers (File Geodatabase) cannot create layers of unknown type.
fn common_geometry_type(layer: &Layer) -> Option<GeometryType> {
    use GeometryType as G;
    let mut found: Option<G> = None;
    for g in layer.features.iter().filter_map(|f| f.geometry.as_ref()) {
        let t = g.geom_type();
        found = Some(match (found, t) {
            (None, t) => t,
            (Some(a), b) if a == b => a,
            (Some(G::Point | G::MultiPoint), G::Point | G::MultiPoint) => G::MultiPoint,
            (Some(G::LineString | G::MultiLineString), G::LineString | G::MultiLineString) => G::MultiLineString,
            (Some(G::Polygon | G::MultiPolygon), G::Polygon | G::MultiPolygon) => G::MultiPolygon,
            _ => return None,
        });
    }
    found
}

/// A single geometry as the multi geometry of a layer of that type (`-nlt PROMOTE_TO_MULTI`), so
/// that formats with strict types (SpatiaLite, File Geodatabase) take the polygons of a
/// multipolygon layer.
fn promote(g: &Geometry, layer_type: Option<GeometryType>) -> Option<Geometry> {
    Some(match (g, layer_type?) {
        (Geometry::Point(c), GeometryType::MultiPoint) => Geometry::MultiPoint(vec![c.clone()]),
        (Geometry::LineString(l), GeometryType::MultiLineString) => Geometry::MultiLineString(vec![l.clone()]),
        (Geometry::Polygon { exterior, interiors }, GeometryType::MultiPolygon) => {
            Geometry::MultiPolygon(vec![(exterior.clone(), interiors.clone())])
        }
        _ => return None,
    })
}

/// The OGR geometry type of a layer: its declared type (else the type its features share), with Z
/// when its geometries have heights.
fn ogr_geometry_type(layer: &Layer) -> OGRwkbGeometryType::Type {
    use OGRwkbGeometryType as W;
    let base = match layer.geom_type.or_else(|| common_geometry_type(layer)) {
        Some(GeometryType::Point) => W::wkbPoint,
        Some(GeometryType::LineString) => W::wkbLineString,
        Some(GeometryType::Polygon) => W::wkbPolygon,
        Some(GeometryType::MultiPoint) => W::wkbMultiPoint,
        Some(GeometryType::MultiLineString) => W::wkbMultiLineString,
        Some(GeometryType::MultiPolygon) => W::wkbMultiPolygon,
        Some(GeometryType::GeometryCollection) => W::wkbGeometryCollection,
        None => return W::wkbUnknown,
    };
    let has_z = layer.features.iter().any(|f| f.geometry.as_ref().is_some_and(Geometry::has_z));
    // SAFETY: a pure function of its arguments.
    if has_z { unsafe { gdal_sys::OGR_GT_SetZ(base) } } else { base }
}

fn write_layer(ds: &mut GdalDataset, layer: &Layer, lco: &[&str]) -> Result<()> {
    let srs = layer_srs(layer);
    let gdal_layer = ds
        .create_layer(gdal::vector::LayerOptions {
            name: &layer.name,
            srs: srs.as_ref(),
            ty: ogr_geometry_type(layer),
            options: (!lco.is_empty()).then_some(lco),
        })
        .map_err(|e| {
            Error::InvalidData(format!("GDAL cannot create layer '{}' ({}): {e}", layer.name, last_error().unwrap_or_default()))
        })?;
    // Fields, and the index GDAL gave each (drivers may rename or drop fields, as DXF's fixed
    // schema does).
    let mut indices = Vec::with_capacity(layer.schema.fields().len());
    for field in layer.schema.fields() {
        let (t, subtype) = match field.field_type {
            FieldType::Integer => (OGRFieldType::OFTInteger64, gdal_sys::OGRFieldSubType::OFSTNone),
            FieldType::Float => (OGRFieldType::OFTReal, gdal_sys::OGRFieldSubType::OFSTNone),
            FieldType::Boolean => (OGRFieldType::OFTInteger, gdal_sys::OGRFieldSubType::OFSTBoolean),
            FieldType::Blob => (OGRFieldType::OFTBinary, gdal_sys::OGRFieldSubType::OFSTNone),
            FieldType::Date => (OGRFieldType::OFTDate, gdal_sys::OGRFieldSubType::OFSTNone),
            FieldType::DateTime => (OGRFieldType::OFTDateTime, gdal_sys::OGRFieldSubType::OFSTNone),
            FieldType::Json => (OGRFieldType::OFTString, gdal_sys::OGRFieldSubType::OFSTJSON),
            FieldType::Text => (OGRFieldType::OFTString, gdal_sys::OGRFieldSubType::OFSTNone),
        };
        let name =
            std::ffi::CString::new(field.name.as_str()).map_err(|_| Error::InvalidArgument(format!("field name '{}'", field.name)))?;
        // SAFETY: the field definition is created, configured, copied into the layer by
        // `OGR_L_CreateField` and destroyed here; the layer handle is valid while `gdal_layer`
        // lives. A driver that cannot create a field (DXF's fixed schema) leaves it out, as
        // `ogr2ogr` warns and goes on.
        unsafe {
            let defn = gdal_sys::OGR_Fld_Create(name.as_ptr(), t);
            if field.width > 0 {
                gdal_sys::OGR_Fld_SetWidth(defn, i32::try_from(field.width).unwrap_or(0));
            }
            if field.precision > 0 {
                gdal_sys::OGR_Fld_SetPrecision(defn, i32::try_from(field.precision).unwrap_or(0));
            }
            gdal_sys::OGR_Fld_SetSubType(defn, subtype);
            gdal_sys::OGR_L_CreateField(gdal_layer.c_layer(), defn, 1);
            gdal_sys::OGR_Fld_Destroy(defn);
        }
        indices.push(gdal_layer.defn().field_index(&field.name).ok());
    }
    let defn = gdal_layer.defn();
    let layer_type = layer.geom_type.or_else(|| common_geometry_type(layer));
    for (i, f) in layer.features.iter().enumerate() {
        let mut feature = gdal::vector::Feature::new(defn).map_err(gdal_err)?;
        if let Some(g) = &f.geometry {
            let wkb = match promote(g, layer_type) {
                Some(multi) => multi.to_wkb(),
                None => g.to_wkb(),
            };
            let geometry = gdal::vector::Geometry::from_wkb(&wkb).map_err(gdal_err)?;
            feature.set_geometry(geometry).map_err(gdal_err)?;
        }
        for (value, index) in f.attributes.iter().zip(&indices) {
            let Some(index) = *index else { continue };
            set_value(&mut feature, index, value)?;
        }
        feature.create(&gdal_layer).map_err(|e| {
            Error::InvalidData(format!("GDAL cannot write feature {i} of '{}' ({}): {e}", layer.name, last_error().unwrap_or_default()))
        })?;
    }
    Ok(())
}

fn set_value(feature: &mut gdal::vector::Feature, index: usize, value: &FieldValue) -> Result<()> {
    match value {
        FieldValue::Null => feature.set_field_null(index),
        FieldValue::Integer(v) => feature.set_field_integer64(index, *v),
        FieldValue::Float(v) => feature.set_field_double(index, *v),
        FieldValue::Boolean(v) => feature.set_field_integer(index, i32::from(*v)),
        // GDAL parses dates and times from their ISO 8601 text.
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => feature.set_field_string(index, s),
        FieldValue::Blob(b) => {
            let (Ok(i), Ok(n)) = (i32::try_from(index), i32::try_from(b.len())) else { return Ok(()) };
            // SAFETY: the feature handle is valid; GDAL copies the `n` bytes.
            unsafe { gdal_sys::OGR_F_SetFieldBinary(feature.c_feature(), i, n, b.as_ptr().cast()) };
            Ok(())
        }
    }
    .map_err(gdal_err)
}

/// A unique `/vsimem/` directory for one call.
fn mem_dir() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("/vsimem/terrio-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Removes a `/vsimem/` directory and everything in it.
fn remove_mem_dir(dir: &str) {
    if let Ok(c) = std::ffi::CString::new(dir) {
        // SAFETY: a NUL-terminated path; GDAL removes the in-memory files under it.
        unsafe { gdal_sys::VSIRmdirRecursive(c.as_ptr()) };
    }
}

/// The last component of a file name, or `data` when it has none.
fn mem_file_name(name: &str) -> String {
    match std::path::Path::new(name).file_name().and_then(|s| s.to_str()) {
        Some(n) if !n.is_empty() && n != "." && n != ".." => n.to_string(),
        _ => "data".to_string(),
    }
}

/// Runs `f` on the path of `bytes` as a GDAL in-memory file named `name` (`/vsimem/...`), then
/// frees it. Datasets are read whole, so nothing refers to the file afterwards.
fn with_mem_file<T>(bytes: Vec<u8>, name: &str, f: impl FnOnce(&str) -> Result<T>) -> Result<T> {
    let dir = mem_dir();
    let path = format!("{dir}/{}", mem_file_name(name));
    gdal::vsi::create_mem_file(&path, bytes).map_err(gdal_err)?;
    let result = f(&path);
    remove_mem_dir(&dir);
    result
}

/// Opens a dataset from its bytes with GDAL, through its in-memory file system (`/vsimem/`), as
/// [`open`] opens a path. `name` is a file name: some drivers need its extension. Zip archives
/// are read through `/vsizip/` (a zipped shapefile).
pub fn open_bytes(bytes: Vec<u8>, name: &str) -> Result<Dataset> {
    open_bytes_with(bytes, name, &VectorOpenOptions::default())
}

/// [`open_bytes`] with vector open options.
pub(crate) fn open_bytes_with(bytes: Vec<u8>, name: &str, options: &VectorOpenOptions) -> Result<Dataset> {
    let zipped = bytes.starts_with(b"PK\x03\x04");
    with_mem_file(bytes, name, |path| {
        let result = open_with(path, options);
        match result {
            Err(_) if zipped => open_with(&format!("/vsizip/{path}"), options),
            r => r,
        }
    })
    .map(|ds| match ds {
        // Name the layers after the file, not the in-memory path.
        Dataset::Vector(mut v) => {
            let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("layer").to_string();
            for l in &mut v.layers {
                if l.name.starts_with("/vsimem/") || l.name.starts_with("terrio-") {
                    l.name = stem.clone();
                }
            }
            Dataset::Vector(v)
        }
        r => r,
    })
}

/// Opens the vector layers of a dataset from its bytes with GDAL (see [`open_bytes`]).
pub fn open_vector_bytes(bytes: Vec<u8>, name: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    match open_bytes_with(bytes, name, options)? {
        Dataset::Vector(v) => Ok(v),
        Dataset::Raster(_) => Err(Error::Unsupported(format!("'{name}' is a raster, not a vector dataset"))),
    }
}

/// The bytes of what GDAL wrote under a `/vsimem/` directory: the file `main` itself, or a zip
/// archive of every file written when there are several (a shapefile and its sidecars, a File
/// Geodatabase directory), with their paths relative to the directory.
fn collect_mem_dir(dir: &str, main: &str) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut entries: Vec<String> = gdal::vsi::read_dir(dir, true)
        .map_err(gdal_err)?
        .into_iter()
        .filter_map(|p| p.to_str().map(|s| s.trim_end_matches('/').to_string()))
        .collect();
    entries.sort();
    let mut files = Vec::new();
    for rel in entries {
        let full = format!("{dir}/{rel}");
        // Directories have no buffer: they are skipped. So are GDAL's `.aux.xml` sidecars (PAM),
        // which hold what the format cannot.
        if rel.ends_with(".aux.xml") {
            continue;
        }
        if let Ok(bytes) = gdal::vsi::get_vsi_mem_file_bytes_owned(&full) {
            files.push((rel, full, bytes));
        }
    }
    if let [(_, full, _)] = files.as_slice()
        && full == main
    {
        return Ok(files.pop().map(|f| f.2).unwrap_or_default());
    }
    if files.is_empty() {
        return Err(Error::InvalidData(format!("GDAL wrote nothing for '{}'", main.rsplit('/').next().unwrap_or(main))));
    }
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (rel, _, bytes) in files {
        zip.start_file(rel, opts)?;
        zip.write_all(&bytes)?;
    }
    Ok(zip.finish()?.into_inner())
}

/// Writes a raster to bytes with the GDAL driver of this short name, through GDAL's in-memory
/// file system (see [`write_raster`]). `name` is the file name the driver writes (its extension
/// matters to some); formats of several files come back as a zip archive of them. GDAL's
/// `.aux.xml` sidecar is left out: what only it holds (such as the nodata value of a format
/// without one) is not in the bytes.
pub fn write_raster_bytes(ds: &RasterDataset, name: &str, driver: &str, creation_options: &[(String, String)]) -> Result<Vec<u8>> {
    let dir = mem_dir();
    let path = format!("{dir}/{}", mem_file_name(name));
    let result = write_raster(ds, &path, driver, creation_options).and_then(|()| collect_mem_dir(&dir, &path));
    remove_mem_dir(&dir);
    result
}

/// Writes vector layers to bytes with the GDAL driver of this short name, through GDAL's
/// in-memory file system (see [`write_vector`]). Formats of several files (a shapefile, a File
/// Geodatabase directory) come back as a zip archive of them.
pub fn write_vector_bytes(
    layers: &[Layer],
    name: &str,
    driver: &str,
    dataset_options: &[(String, String)],
    layer_options: &[(String, String)],
) -> Result<Vec<u8>> {
    let dir = mem_dir();
    let path = format!("{dir}/{}", mem_file_name(name));
    let result = write_vector(layers, &path, driver, dataset_options, layer_options).and_then(|()| collect_mem_dir(&dir, &path));
    remove_mem_dir(&dir);
    result
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
    // Every dataset GDAL opens (a `GDAL:` name, the fallback for what terrio's drivers cannot read,
    // bytes through `/vsimem/`) is opened by the GDAL backend, whose own drivers pick the format:
    // only if the process-wide allow-list of each kind asked for has the GDAL driver.
    use crate::formats::Kind;
    for (flag, kind) in [(GdalOpenFlags::GDAL_OF_RASTER, Kind::Raster), (GdalOpenFlags::GDAL_OF_VECTOR, Kind::Vector)] {
        if flags.contains(flag) {
            crate::formats::check_driver_allowed(RasterDriver::Gdal.name(), path, kind)?;
        }
    }
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
    // SAFETY: plain queries of a valid band handle and a data type.
    let raw_type = unsafe { gdal_sys::GDALGetRasterDataType(first.c_rasterband()) };
    if unsafe { gdal_sys::GDALDataTypeIsComplex(raw_type) } != 0 {
        return complex_raster_from(ds, &first, raw_type, path);
    }
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
    let raster =
        common::make_raster(cols, rows, bands, data, ds.geo_transform().unwrap_or(common::NO_GEOTRANSFORM), dataset_crs(ds), nodata)?;
    finish_raster(ds, &first, raster, nodata.is_some(), false)
}

/// The CRS of a GDAL dataset.
fn dataset_crs(ds: &GdalDataset) -> wbraster::CrsInfo {
    ds.spatial_ref().ok().and_then(|srs| srs_to_crs(&srs)).unwrap_or_default()
}

/// A dataset of complex bands (`CInt16`... `CFloat64`), read as pairs of real and imaginary bands.
fn complex_raster_from(
    ds: &GdalDataset,
    first: &gdal::raster::RasterBand,
    t: gdal_sys::GDALDataType::Type,
    path: &str,
) -> Result<RasterDataset> {
    let (cols, rows) = ds.raster_size();
    let bands = ds.raster_count();
    let data = read_complex(ds, t, cols, rows, bands).map_err(|e| Error::InvalidData(format!("'{path}': {e}")))?;
    let nodata = first.no_data_value();
    let raster =
        common::make_raster(cols, rows, bands * 2, data, ds.geo_transform().unwrap_or(common::NO_GEOTRANSFORM), dataset_crs(ds), nodata)?;
    finish_raster(ds, first, raster, nodata.is_some(), true)
}

/// Adds a GDAL dataset's scale, offset, metadata, subdatasets and geolocation arrays to a raster
/// read from it.
fn finish_raster(
    ds: &GdalDataset,
    first: &gdal::raster::RasterBand,
    mut raster: wbraster::Raster,
    has_nodata: bool,
    complex: bool,
) -> Result<RasterDataset> {
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
    let mut out = RasterDataset::with_driver(raster, RasterDriver::Gdal, has_nodata);
    out.complex = complex;
    let geolocation: Vec<(String, String)> = ds
        .metadata_domain("GEOLOCATION")
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    out.geolocation = crate::Geolocation::from_items(&geolocation);
    Ok(out)
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
