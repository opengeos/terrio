//! Datasets in memory: opening from bytes and writing to bytes, for callers that hold the data
//! already (uploads, HTTP bodies, language bindings) or have no file system (WebAssembly).
//!
//! A file name goes with the bytes: its extension tells the format, as a path's does, and its stem
//! names vector layers.
//!
//! In native builds, the bytes go through a temporary directory and the file drivers, so every
//! format reads and writes exactly as from files. WebAssembly builds have no file system and use
//! in-memory codecs instead, for the formats that have one:
//!
//! - reading: GeoTIFF, Esri ASCII grid, JPEG 2000 (JP2 and J2K), GeoPackage (vector layers and rasters), PMTiles and
//!   MBTiles (raster and vector tiles), MVT tiles, GeoJSON, TopoJSON,
//!   FlatGeobuf, CSV, GeoParquet, KML, KMZ, GML, GPX, DWG, DXF, OSM PBF and XML, and zip archives of these,
//!   and Shapefile and MapInfo (MIF/MID and TAB) in zip archives (or their `.shp` or `.mif` alone);
//! - writing: GeoTIFF and COG (with lossless compressions), Esri ASCII grid, JPEG 2000, GeoPackage (vector
//!   layers and rasters), PMTiles and MBTiles, GeoJSON, TopoJSON, FlatGeobuf, CSV, GeoParquet, KML, KMZ, GML (without
//!   its schema), GPX, DWG, DXF and Shapefile ZIP archives (`.shp.zip`).
//!
//! Formats made of several files (a shapefile's `.shp`, `.dbf`, `.prj`...) are written as a zip
//! archive of them.
//!
//! With the `gdal` feature, what terrio cannot read is opened by GDAL from memory, through its
//! `/vsimem/` file system, and extensions terrio has no driver for are written by the GDAL
//! driver that claims them (see `gdal_backend::open_bytes` and `gdal_backend::write_vector_bytes`).
//!
//! ```no_run
//! let bytes = std::fs::read("countries.geojson")?;
//! let ds = terrio::memory::open_bytes(bytes, "countries.geojson")?;
//! # let layers = match ds { terrio::Dataset::Vector(v) => v.layers, _ => unreachable!() };
//! let fgb = terrio::memory::write_vector_bytes(&layers, "countries.fgb", &Default::default())?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use crate::formats::{RasterDriver, VectorDriver, extension};
use crate::{Dataset, Error, LazyRaster, RasterDataset, RasterWriteOptions, Result, VectorDataset, VectorOpenOptions, VectorWriteOptions};
use wbvector::Layer;

pub use crate::drivers::vector::geojson::parse as parse_geojson_layer;
pub use crate::drivers::vector::shapefile::to_zip_bytes as write_shapefile_zip_bytes;

/// Opens a dataset, raster or vector, from its bytes. `name` is a file name whose extension tells
/// the format (`dem.tif`, `roads.fgb`, `parcels.zip`).
pub fn open_bytes(bytes: Vec<u8>, name: &str) -> Result<Dataset> {
    #[cfg(not(target_family = "wasm"))]
    {
        let (_dir, path) = native::spill(&bytes, name)?;
        let result = crate::dataset::open_native(&path);
        // What terrio cannot read, GDAL reads from memory (`/vsimem/`).
        #[cfg(feature = "gdal")]
        let result = crate::dataset::gdal_fallback(&path, result, |_| crate::gdal_backend::open_bytes(bytes, name));
        result
    }
    #[cfg(target_family = "wasm")]
    codec::open(bytes, name, &VectorOpenOptions::default())
}

/// Opens a raster from its bytes.
pub fn open_raster_bytes(bytes: Vec<u8>, name: &str) -> Result<RasterDataset> {
    // HEC-RAS files open as vector layers; a raster open reads their HDF5 arrays, as before.
    #[cfg(feature = "hecras")]
    if crate::drivers::hdf5::parse_subdataset(name).is_none() && crate::drivers::hecras::driver::is_hecras_bytes(&bytes) {
        return crate::drivers::hdf5::read_bytes(bytes, name, None);
    }
    match open_bytes(bytes, name)? {
        Dataset::Raster(r) => Ok(r),
        Dataset::Vector(_) => Err(Error::Unsupported(format!("'{name}' is a vector dataset, not a raster"))),
    }
}

/// Opens a raster from its bytes for reading by window. GeoTIFFs are decoded one window at a time;
/// other formats are decoded whole.
pub fn open_raster_lazy_bytes(bytes: Vec<u8>, name: &str) -> Result<LazyRaster> {
    if crate::tiles::archive_kind(name).is_some() {
        #[cfg(feature = "tiles")]
        return Ok(LazyRaster::from_tiles(crate::tiles::raster::open_lazy_bytes(bytes)?));
        #[cfg(not(feature = "tiles"))]
        return Err(Error::disabled("tiles"));
    }
    #[cfg(feature = "jp2")]
    if crate::drivers::jp2::is_jpeg2000(&bytes) {
        return crate::drivers::jp2::open_lazy_bytes(bytes);
    }
    #[cfg(feature = "grib")]
    if crate::drivers::grib::is_grib(&bytes) {
        return crate::drivers::grib::open_lazy_bytes(bytes);
    }
    if codec::is_tiff(&bytes) {
        if let Some(r) = LazyRaster::from_tiff_bytes(bytes)? {
            return Ok(r);
        }
        return Err(Error::InvalidData(format!("'{name}' is not a valid TIFF")));
    }
    open_raster_bytes(bytes, name).map(LazyRaster::from)
}

/// Opens a Zarr store (or GeoZarr) from its files: `(path, bytes)` pairs, paths relative to the
/// store's directory or its parent (`.zgroup`, `dem/.zarray`, `dem/0.0`...), as a key-value store
/// holds them.
pub fn open_zarr_store(files: Vec<(String, Vec<u8>)>) -> Result<RasterDataset> {
    let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
    let root = codec::zarr_root(&names).ok_or_else(|| Error::InvalidData("not a Zarr store: no .zgroup, .zarray or zarr.json".into()))?;
    codec::open_zarr_store(files, &root)
}

/// Opens a vector dataset from its bytes.
pub fn open_vector_bytes(bytes: Vec<u8>, name: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    #[cfg(not(target_family = "wasm"))]
    {
        let (_dir, path) = native::spill(&bytes, name)?;
        let result = crate::dataset::open_vector_native(&path, options);
        #[cfg(feature = "gdal")]
        let result = crate::dataset::gdal_fallback(&path, result, |_| crate::gdal_backend::open_vector_bytes(bytes, name, options));
        result
    }
    #[cfg(target_family = "wasm")]
    match codec::open(bytes, name, options)? {
        Dataset::Vector(v) => Ok(v),
        Dataset::Raster(_) => Err(Error::Unsupported(format!("'{name}' is a raster, not a vector dataset"))),
    }
}

/// Writes rasters into one netCDF-4 file in memory, each in its group, as
/// [`crate::write_netcdf_groups`] writes files.
pub fn write_netcdf_groups_bytes(rasters: &[(&str, &RasterDataset)], options: &RasterWriteOptions) -> Result<Vec<u8>> {
    if rasters.is_empty() {
        return Err(Error::InvalidArgument("no rasters to write".into()));
    }
    #[cfg(feature = "netcdf-hdf5")]
    return crate::drivers::netcdf_groups_to_bytes(rasters, options);
    #[cfg(not(feature = "netcdf-hdf5"))]
    {
        let _ = options;
        Err(Error::disabled("netcdf-hdf5"))
    }
}

/// Writes a raster to bytes, declaring its nodata value when it has one. `name` is a file name
/// whose extension tells the format, unless the options name the driver.
pub fn write_raster_bytes(ds: &RasterDataset, name: &str, options: &RasterWriteOptions) -> Result<Vec<u8>> {
    let driver = match options.driver.or_else(|| RasterDriver::for_extension(name)) {
        Some(d) => d,
        None => {
            // With the GDAL backend, the GDAL driver of the extension, writing to memory.
            #[cfg(feature = "gdal")]
            if let Some(d) = crate::gdal_backend::driver_for_path(name, false) {
                return crate::gdal_backend::write_raster_bytes(ds, name, &d, &options.creation_options);
            }
            return Err(Error::InvalidArgument(format!("cannot tell the output format of '{name}': give it explicitly")));
        }
    };
    // A Zarr store is a directory: written in memory and zipped, on every platform.
    if driver == RasterDriver::Zarr {
        let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("raster");
        return codec::write_raster(ds, driver, options, stem.trim_end_matches(".zarr"));
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let dir = native::temp_dir()?;
        let path = dir.path().join(native::file_name(name, driver.info().extensions.first().copied().unwrap_or("tif")));
        let path = path.to_str().ok_or_else(|| Error::InvalidArgument(format!("invalid file name '{name}'")))?;
        crate::write_raster_dataset(ds, path, &RasterWriteOptions { driver: Some(driver), ..options.clone() })?;
        native::collect(dir.path(), path)
    }
    #[cfg(target_family = "wasm")]
    codec::write_raster(ds, driver, options, std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("raster"))
}

/// Writes vector layers to bytes. `name` is a file name whose extension tells the format, unless
/// the options name the driver. GeoPackage, GPX, KML, KMZ and TopoJSON hold several layers; CAD combines them into drawing layers.
/// Shapefiles require an explicit ZIP filename (`roads.shp.zip`, or `.zip` with the
/// Shapefile driver). The result is `application/zip` on native and WASM alike.
/// Bare `.shp` names are refused: use [`crate::write_vector`] for filesystem members.
/// See [`write_shapefile_zip_bytes`] for supported data and refusals.
pub fn write_vector_bytes(layers: &[Layer], name: &str, options: &VectorWriteOptions) -> Result<Vec<u8>> {
    let driver = match options.driver.or_else(|| {
        if name.to_ascii_lowercase().ends_with(".shp.zip") { Some(VectorDriver::Shapefile) } else { VectorDriver::for_extension(name) }
    }) {
        Some(d) => d,
        None => {
            #[cfg(feature = "gdal")]
            if let Some(d) = crate::gdal_backend::driver_for_path(name, true) {
                crate::dataset::ensure_kmz_resources_apply(options, None)?;
                return crate::gdal_backend::write_vector_bytes(layers, name, &d, &[], &[]);
            }
            return Err(Error::InvalidArgument(format!("cannot tell the output format of '{name}': give it explicitly")));
        }
    };
    crate::dataset::ensure_kmz_resources_apply(options, Some(driver))?;
    if driver == VectorDriver::Shapefile {
        if !name.to_ascii_lowercase().ends_with(".zip") {
            return Err(Error::InvalidArgument("Shapefile bytes are a ZIP archive: use a .shp.zip filename (MIME application/zip)".into()));
        }
        let [layer] = layers else {
            return Err(Error::InvalidArgument("a Shapefile ZIP holds exactly one layer".into()));
        };
        let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let stem = &file[..file.len() - 4];
        let stem = if stem.to_ascii_lowercase().ends_with(".shp") { &stem[..stem.len() - 4] } else { stem };
        return crate::drivers::vector::shapefile::to_zip_bytes(layer, stem);
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let dir = native::temp_dir()?;
        let path = dir.path().join(native::file_name(name, driver.info().extensions.first().copied().unwrap_or("dat")));
        let path = path.to_str().ok_or_else(|| Error::InvalidArgument(format!("invalid file name '{name}'")))?;
        crate::write_vector(layers, path, &VectorWriteOptions { driver: Some(driver), ..options.clone() })?;
        native::collect(dir.path(), path)
    }
    #[cfg(target_family = "wasm")]
    codec::write_vector(layers, driver, options)
}

/// Native builds: bytes go through a temporary directory and the file drivers.
#[cfg(not(target_family = "wasm"))]
mod native {
    use crate::{Error, Result};
    use std::io::Write;
    use std::path::Path;

    pub(super) fn temp_dir() -> Result<tempfile::TempDir> {
        Ok(tempfile::Builder::new().prefix("terrio-mem-").tempdir()?)
    }

    /// The last component of `name`, or `data.<ext>` when it has none.
    pub(super) fn file_name(name: &str, ext: &str) -> String {
        match Path::new(name).file_name().and_then(|s| s.to_str()) {
            Some(n) if !n.is_empty() && n != "." && n != ".." => n.to_string(),
            _ => format!("data.{ext}"),
        }
    }

    /// Writes the bytes to a file named `name` in a new temporary directory, returned with the
    /// file's path (the directory is removed when dropped).
    pub(super) fn spill(bytes: &[u8], name: &str) -> Result<(tempfile::TempDir, String)> {
        let dir = temp_dir()?;
        let path = dir.path().join(file_name(name, "dat"));
        std::fs::write(&path, bytes)?;
        let path = path.to_str().ok_or_else(|| Error::InvalidArgument(format!("invalid file name '{name}'")))?.to_string();
        Ok((dir, path))
    }

    /// The bytes of what a driver wrote: the file itself, or a zip archive of every file written
    /// when there are several (a shapefile and its sidecars).
    pub(super) fn collect(dir: &Path, main: &str) -> Result<Vec<u8>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_file() {
                files.push(path);
            } else {
                return Err(Error::Unsupported("the format writes a directory, which cannot be returned as bytes".into()));
            }
        }
        if files.len() == 1 && files[0] == Path::new(main) {
            return Ok(std::fs::read(main)?);
        }
        files.sort();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for f in files {
            let name = f.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_string();
            zip.start_file(name, opts)?;
            zip.write_all(&std::fs::read(&f)?)?;
        }
        Ok(zip.finish()?.into_inner())
    }
}

/// In-memory codecs, used by WebAssembly builds (and tested in all builds).
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(crate) mod codec {
    use super::*;
    use std::io::{Cursor, Read};

    /// Whether the bytes start with a TIFF or BigTIFF signature.
    pub(crate) fn is_tiff(b: &[u8]) -> bool {
        matches!(b.get(..4), Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+"))
    }

    fn is_zip(b: &[u8]) -> bool {
        b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06")
    }

    /// The vector driver of a file, by its extension or, without one, its first bytes.
    fn vector_driver(bytes: &[u8], name: &str) -> Option<VectorDriver> {
        if let Some(d) = VectorDriver::for_extension(name) {
            return Some(d);
        }
        if bytes.starts_with(b"fgb") {
            return Some(VectorDriver::FlatGeobuf);
        }
        if bytes.starts_with(b"PAR1") {
            return Some(VectorDriver::Parquet);
        }
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
        let head = head.trim_start_matches('\u{feff}').trim_start();
        if head.starts_with('{') {
            return Some(if head.contains("\"topology\"") { VectorDriver::TopoJSON } else { VectorDriver::GeoJSON });
        }
        if head.starts_with('<') {
            for (tag, d) in [
                ("<kml", VectorDriver::KML),
                ("<gpx", VectorDriver::GPX),
                ("<osm", VectorDriver::OSM),
                ("featurecollection", VectorDriver::GML),
            ] {
                if head.contains(tag) {
                    return Some(d);
                }
            }
        }
        None
    }

    /// Opens a dataset from bytes with the in-memory codecs.
    pub(crate) fn open(bytes: Vec<u8>, name: &str, options: &VectorOpenOptions) -> Result<Dataset> {
        // Formats this build leaves out, by their extension.
        if let Some(feature) = RasterDriver::for_extension(name).and_then(RasterDriver::missing_feature) {
            return Err(Error::disabled(feature));
        }
        // Subdatasets named as GDAL names them: `NETCDF:"file.nc":var`, `HDF5:"file.h5"://path`.
        #[cfg(not(feature = "netcdf-hdf5"))]
        if bytes.starts_with(b"CDF")
            || bytes.starts_with(b"\x89HDF\r\n\x1a\n")
            || ["netcdf:", "hdf5:"].iter().any(|p| name.to_ascii_lowercase().starts_with(p))
        {
            return Err(Error::disabled("netcdf-hdf5"));
        }
        #[cfg(feature = "netcdf-hdf5")]
        if let Some((file, dataset)) = crate::drivers::hdf5::parse_subdataset(name) {
            return crate::drivers::hdf5::read_bytes(bytes, &file, Some(&dataset)).map(Dataset::Raster);
        }
        #[cfg(feature = "netcdf-hdf5")]
        if let Some((file, variable)) = crate::drivers::netcdf::parse_subdataset(name) {
            return crate::drivers::netcdf::read_bytes(&bytes, &file, Some(&variable)).map(Dataset::Raster);
        }
        #[cfg(feature = "netcdf-hdf5")]
        if bytes.starts_with(b"CDF") {
            return crate::drivers::netcdf::read_bytes(&bytes, name, None).map(Dataset::Raster);
        }
        // HEC-RAS geometry and plan files are HDF5 files that hold vector layers (the HECRAS
        // driver): recognized by their root `File Type` before the HDF5 raster driver claims them.
        #[cfg(feature = "hecras")]
        if crate::drivers::hecras::driver::is_hecras_bytes(&bytes) {
            let driver = VectorDriver::HECRAS;
            let layers = crate::drivers::vector::read_bytes(bytes, name, driver, options)?.unwrap_or_default();
            let layers: Vec<Layer> = layers.into_iter().map(|l| crate::dataset::fix_layer(l, name, driver)).collect();
            return finish_vector(layers, driver, options);
        }
        #[cfg(feature = "netcdf-hdf5")]
        if crate::drivers::hdf5::signature_offset_in(&bytes).is_some() {
            // NetCDF-4 files are HDF5 files with netCDF's extensions.
            let ext = extension(name).unwrap_or_default();
            if ["nc", "nc4", "cdf", "nc2", "nc3"].contains(&ext.as_str()) {
                return crate::drivers::netcdf::read_bytes(&bytes, name, None).map(Dataset::Raster);
            }
            return crate::drivers::hdf5::read_bytes(bytes, name, None).map(Dataset::Raster);
        }
        if is_tiff(&bytes) {
            let mut r = LazyRaster::from_tiff_bytes(bytes)?.ok_or_else(|| Error::InvalidData(format!("'{name}' is not a valid TIFF")))?;
            return r.read().map(Dataset::Raster);
        }
        let ext = extension(name).unwrap_or_default();
        if bytes.starts_with(b"PMTiles") || ext == "mbtiles" {
            #[cfg(feature = "tiles")]
            return crate::tiles::open_dataset_bytes(bytes, name, options);
            #[cfg(not(feature = "tiles"))]
            return Err(Error::disabled("tiles"));
        }
        #[cfg(feature = "tiles")]
        if (ext == "mvt" || ext == "gz" || ext == "pbf") && crate::tiles::is_mvt(&bytes) {
            let layers = crate::tiles::vector::read_tile_file(&bytes, name, &options.tiles)?;
            return Ok(Dataset::Vector(crate::dataset::finish_vector(layers, name, VectorDriver::MVT)));
        }
        if bytes.starts_with(b"SQLite format 3\0") {
            #[cfg(feature = "sqlite")]
            return crate::drivers::gpkg::open_bytes(bytes, name, options);
            #[cfg(not(feature = "sqlite"))]
            return Err(Error::disabled("sqlite"));
        }
        #[cfg(feature = "jp2")]
        if crate::drivers::jp2::is_jpeg2000(&bytes) {
            return crate::drivers::jp2::read_bytes(&bytes).map(Dataset::Raster);
        }
        #[cfg(feature = "grib")]
        if crate::drivers::grib::is_grib(&bytes) {
            return crate::drivers::grib::read_bytes(bytes).map(Dataset::Raster);
        }
        if is_zip(&bytes) && ext != "kmz" {
            return open_zip(&bytes, name, options);
        }
        if RasterDriver::for_extension(name) == Some(RasterDriver::AAIGrid) {
            let raster = wbraster::formats::esri_ascii::parse(Cursor::new(&bytes), name)?;
            return Ok(Dataset::Raster(RasterDataset::with_driver(raster, RasterDriver::AAIGrid, true)));
        }
        let driver = vector_driver(&bytes, name).ok_or_else(|| Error::Unsupported(format!("'{name}': format not recognized")))?;
        let Some(layers) = crate::drivers::vector::read_bytes(bytes, name, driver, options)? else {
            return Err(Error::Unsupported(format!(
                "the {} format cannot be read from memory in this build (in memory: GeoTIFF, AAIGrid, GeoPackage, JPEG 2000, GRIB, netCDF, \
                 HDF5, Zarr in zip archives, GeoJSON, TopoJSON, FlatGeobuf, CSV, GeoParquet, KML, KMZ, GML, GPX, OSM PBF, and \
                 Shapefile and MapInfo in zip archives)",
                driver.name()
            )));
        };
        let layers: Vec<Layer> = layers.into_iter().map(|l| crate::dataset::fix_layer(l, name, driver)).collect();
        finish_vector(layers, driver, options)
    }

    /// Opens the main dataset of a zip archive held in memory.
    fn open_zip(bytes: &[u8], name: &str, options: &VectorOpenOptions) -> Result<Dataset> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
        let mut names = Vec::new();
        for i in 0..zip.len() {
            let f = zip.by_index(i)?;
            if f.is_file() {
                names.push(f.name()?.to_string());
            }
        }
        names.sort();
        // A Zarr store: its files, read under a directory in memory.
        if let Some(root) = zarr_root(&names) {
            let mut files = Vec::with_capacity(names.len());
            for n in &names {
                let mut b = Vec::new();
                zip.by_name(n)?.read_to_end(&mut b)?;
                files.push((n.clone(), b));
            }
            return open_zarr_store(files, &root).map(Dataset::Raster);
        }
        let main: String = crate::vfs::MAIN_DATASET_EXTENSIONS
            .iter()
            .find_map(|ext| names.iter().find(|n| extension(n.as_str()).as_deref() == Some(*ext)))
            .cloned()
            .ok_or_else(|| Error::Unsupported(format!("no dataset found in '{name}'")))?;
        let mut member = Vec::new();
        zip.by_name(&main)?.read_to_end(&mut member)?;
        let driver = VectorDriver::for_extension(&main);
        if let Some(driver @ (VectorDriver::Shapefile | VectorDriver::MapInfo)) = driver {
            // The sidecars: the members named as the main file, with another extension.
            let base = &main[..main.len() - extension(&main).map_or(0, |e| e.len() + 1)];
            let zip = std::cell::RefCell::new(zip);
            let sidecar = |ext: &str| -> Option<Vec<u8>> {
                let member = names.iter().find(|n| {
                    n.len() == base.len() + ext.len() + 1 && n.starts_with(base) && extension(n.as_str()).as_deref() == Some(ext)
                })?;
                let mut out = Vec::new();
                zip.borrow_mut().by_name(member).ok()?.read_to_end(&mut out).ok()?;
                Some(out)
            };
            let layers = crate::drivers::vector::read_parts(&member, &main, driver, &sidecar, options)?;
            let layers: Vec<Layer> = layers.into_iter().map(|l| crate::dataset::fix_layer(l, &main, driver)).collect();
            return finish_vector(layers, driver, options);
        }
        let mut ds = open(member, &main, options)?;
        // A raster's `.aux.xml` sidecar (a JPEG 2000's nodata value).
        let aux = format!("{main}.aux.xml");
        if let Dataset::Raster(r) = &mut ds
            && names.contains(&aux)
        {
            let mut xml = String::new();
            zip.by_name(&aux)?.read_to_string(&mut xml)?;
            crate::drivers::nodata::apply(crate::drivers::common::parse_pam(&xml), r);
        }
        Ok(ds)
    }

    /// The root of the Zarr store among file names: the shortest directory holding a `.zgroup`,
    /// `.zarray` or `zarr.json` (`""` for the top), or `None` when there is none.
    pub(crate) fn zarr_root(names: &[String]) -> Option<String> {
        names
            .iter()
            .filter_map(|n| {
                let n = n.trim_start_matches("./");
                let (dir, file) = n.rsplit_once('/').unwrap_or(("", n));
                [".zgroup", ".zarray", "zarr.json"].contains(&file).then(|| dir.to_string())
            })
            .min_by_key(|d| (d.matches('/').count(), d.len()))
    }

    /// Reads a Zarr store from its files (paths relative to the store's parent, and the root
    /// directory among them), as GDAL reads a store directory.
    #[cfg(feature = "zarr")]
    pub(crate) fn open_zarr_store(files: Vec<(String, Vec<u8>)>, root: &str) -> Result<RasterDataset> {
        let mount = crate::memfs::Mount::new(files.into_iter().map(|(n, b)| (n.trim_start_matches("./").to_string(), b)));
        let dir = if root.is_empty() { mount.path().to_path_buf() } else { mount.path().join(root) };
        crate::drivers::zarr::read(&dir)
    }

    #[cfg(not(feature = "zarr"))]
    pub(crate) fn open_zarr_store(_files: Vec<(String, Vec<u8>)>, _root: &str) -> Result<RasterDataset> {
        Err(Error::disabled("zarr"))
    }

    /// Writes a raster as a Zarr store, returned as a zip archive of its files (in a directory
    /// named `<name>.zarr`).
    #[cfg(feature = "zarr")]
    fn write_zarr(ds: &RasterDataset, options: &RasterWriteOptions, name: &str) -> Result<Vec<u8>> {
        let mount = crate::memfs::Mount::new(std::iter::empty());
        let dir = mount.path().join(format!("{name}.zarr"));
        crate::drivers::zarr::write_with(
            &ds.raster,
            ds.nodata(),
            &dir,
            options.compression.as_deref(),
            &options.creation_options,
            if options.band_metadata.is_empty() { &ds.band_metadata } else { &options.band_metadata },
        )?;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (path, bytes) in mount.files() {
            zip.start_file(path, opts)?;
            std::io::Write::write_all(&mut zip, &bytes)?;
        }
        Ok(zip.finish()?.into_inner())
    }

    /// Keeps the layers the options ask for.
    fn finish_vector(layers: Vec<Layer>, driver: VectorDriver, options: &VectorOpenOptions) -> Result<Dataset> {
        let names: Vec<String> = layers.iter().map(|l| l.name.clone()).collect();
        let layers: Vec<Layer> =
            if options.layers.is_empty() { layers } else { layers.into_iter().filter(|l| options.layers.contains(&l.name)).collect() };
        if layers.is_empty() {
            return Err(Error::InvalidArgument(format!("no layer named {} (layers: {})", options.layers.join(", "), names.join(", "))));
        }
        Ok(Dataset::Vector(VectorDataset { layers, driver }))
    }

    /// Writes a raster to bytes with the in-memory codecs.
    /// `table` names the raster in formats that hold several (GeoPackage).
    #[cfg_attr(not(any(feature = "sqlite", feature = "jp2", feature = "zarr", feature = "tiles")), allow(unused_variables))]
    pub(crate) fn write_raster(ds: &RasterDataset, driver: RasterDriver, options: &RasterWriteOptions, table: &str) -> Result<Vec<u8>> {
        use crate::drivers::gtiff_write::{TiffOptions, encode};
        driver.ensure_built()?;
        match driver {
            RasterDriver::GTiff | RasterDriver::Cog => {
                let inherited = options.inherited_from(ds, Some(driver));
                let options = &inherited;
                let cog = driver == RasterDriver::Cog;
                let mut opts = TiffOptions::from_write_options(options, cog, ds.nodata())?.ok_or_else(|| {
                    let name = options.compression.as_deref().unwrap_or_default();
                    Error::Unsupported(format!(
                        "{name} compression cannot be written in this build (NONE, LZW, DEFLATE, PACKBITS, ZSTD, LERC, LERC_DEFLATE, \
                         LERC_ZSTD, JXL, JPEG, WEBP)"
                    ))
                })?;
                // Complex bands as complex samples, as the file writer stores them.
                opts.complex = ds.complex;
                encode(&ds.raster, &opts)
            }
            RasterDriver::AAIGrid => {
                let mut out = Vec::new();
                wbraster::formats::esri_ascii::write_to(&mut out, &ds.raster)?;
                Ok(out)
            }
            #[cfg(feature = "sqlite")]
            RasterDriver::GPKG => crate::drivers::gpkg::to_bytes(&ds.raster, ds.nodata(), table, options.compression.as_deref()),
            #[cfg(feature = "jp2")]
            RasterDriver::JP2 => {
                let jp2 = crate::drivers::jp2::to_bytes(&ds.raster)?;
                // The nodata value goes to a `.aux.xml` sidecar, the two files zipped as the file
                // drivers' output is.
                let Some(xml) = crate::drivers::common::pam_xml(None, ds.nodata(), ds.raster.bands) else { return Ok(jp2) };
                let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
                let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
                zip.start_file(format!("{table}.jp2"), opts)?;
                std::io::Write::write_all(&mut zip, &jp2)?;
                zip.start_file(format!("{table}.jp2.aux.xml"), opts)?;
                std::io::Write::write_all(&mut zip, xml.as_bytes())?;
                Ok(zip.finish()?.into_inner())
            }
            #[cfg(feature = "netcdf-hdf5")]
            RasterDriver::NetCDF => {
                let co =
                    |key: &str| options.creation_options.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
                let o = crate::drivers::netcdf_write::Options {
                    format: co("FORMAT"),
                    compress: co("COMPRESS").or(options.compression.as_deref()),
                    zlevel: co("ZLEVEL"),
                    chunking: co("CHUNKING"),
                    group: co("GROUP"),
                };
                crate::drivers::netcdf_write::to_bytes(&ds.raster, ds.nodata(), &o)
            }
            #[cfg(feature = "zarr")]
            RasterDriver::Zarr => write_zarr(ds, options, table),
            #[cfg(feature = "tiles")]
            RasterDriver::MBTiles | RasterDriver::PMTiles => {
                let kind =
                    if driver == RasterDriver::MBTiles { crate::tiles::ArchiveKind::MBTiles } else { crate::tiles::ArchiveKind::PMTiles };
                crate::tiles::raster_to_bytes(ds, &crate::tiles::raster_write_options(options)?, table, kind)
            }
            d => Err(Error::Unsupported(format!(
                "the {} format cannot be written to memory in this build (GTiff, COG, AAIGrid, GPKG, JP2, netCDF, Zarr, MBTiles, PMTiles)",
                d.name()
            ))),
        }
    }

    /// Writes layers to bytes with the in-memory codecs.
    pub(crate) fn write_vector(layers: &[Layer], driver: VectorDriver, options: &VectorWriteOptions) -> Result<Vec<u8>> {
        driver.ensure_built()?;
        #[cfg(feature = "tiles")]
        if matches!(driver, VectorDriver::MBTiles | VectorDriver::PMTiles) {
            let kind =
                if driver == VectorDriver::MBTiles { crate::tiles::ArchiveKind::MBTiles } else { crate::tiles::ArchiveKind::PMTiles };
            let name = layers.first().map(|l| l.name.as_str()).unwrap_or("tiles");
            return crate::tiles::vector_to_bytes(layers, &options.tiles, name, kind);
        }
        crate::drivers::vector::write_bytes(layers, driver, options)?.ok_or_else(|| {
            Error::Unsupported(format!(
                "the {} format cannot be written to memory in this build (GeoJSON, TopoJSON, FlatGeobuf, CSV, GeoParquet, \
                 GeoPackage, KML, KMZ, GML, GPX)",
                driver.name()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::raster::RasterData;
    use wbraster::{DataType, Raster};

    #[test]
    fn memory_codec_preserves_color_overrides() {
        let mut ds = raster(1, DataType::U8);
        ds.band_metadata = vec![crate::BandMetadata { color_interp: Some("Undefined".into()), ..Default::default() }];
        let bytes = codec::write_raster(&ds, RasterDriver::GTiff, &RasterWriteOptions::default(), "raster").unwrap();
        let back = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().header();
        assert_eq!(back.band_color_interp(1), "Undefined");
    }

    #[test]
    fn kmz_resources_are_refused_unless_the_output_is_kmz() {
        let layers = [Layer::new("pins").with_crs_epsg(4326)];
        let options = VectorWriteOptions {
            kmz: crate::drivers::vector::kml::KmzOptions { resources: vec![("icons/pin.png".into(), vec![1, 2, 3])] },
            ..Default::default()
        };
        for name in ["pins.geojson", "pins.gpkg", "pins.shp.zip"] {
            assert!(matches!(write_vector_bytes(&layers, name, &options), Err(Error::InvalidArgument(_))), "{name}");
        }
        assert!(write_vector_bytes(&layers, "pins.kmz", &options).is_ok());
    }

    fn raster(bands: usize, dt: DataType) -> RasterDataset {
        let (cols, rows) = (5, 3);
        let data = RasterData::from_f64_vec(dt, (0..cols * rows * bands).map(|v| v as f64).collect());
        let cfg = wbraster::RasterConfig {
            cols,
            rows,
            bands,
            nodata: -1.0,
            data_type: dt,
            crs: crate::crs::SpatialRef::from_epsg(32610).unwrap().to_crs_info(),
            ..Default::default()
        };
        let mut r = Raster::from_data_native(cfg, data).unwrap();
        r.x_min = 500000.0;
        r.y_min = 4000000.0;
        r.cell_size_x = 30.0;
        r.cell_size_y = 30.0;
        RasterDataset::new(r, true)
    }

    #[test]
    fn geotiff_codec_round_trips() {
        let cases = [(1, DataType::U8, "NONE", None), (3, DataType::I16, "DEFLATE", Some(16)), (2, DataType::F32, "LZW", None)];
        for (bands, dt, compression, tile_size) in cases {
            let ds = raster(bands, dt);
            let r = &ds.raster;
            let options = RasterWriteOptions { compression: Some(compression.into()), tile_size, ..Default::default() };
            let bytes = codec::write_raster(&ds, RasterDriver::GTiff, &options, "x").unwrap();
            let ds = codec::open(bytes.clone(), "x.tif", &Default::default()).unwrap();
            let Dataset::Raster(back) = ds else { panic!() };
            assert_eq!((back.raster.cols, back.raster.rows, back.raster.bands), (5, 3, bands));
            assert_eq!(back.raster.data_type, dt);
            assert_eq!(back.raster.data.to_f64_vec(), r.data.to_f64_vec());
            assert_eq!(back.geo_transform(), [500000.0, 30.0, 0.0, 4000090.0, 0.0, -30.0]);
            assert_eq!(back.crs().epsg, Some(32610));
            assert_eq!(back.nodata(), Some(-1.0));
            // Read by window too.
            let mut lazy = open_raster_lazy_bytes(bytes, "x.tif").unwrap();
            assert!(lazy.is_lazy());
            let w = lazy.read_window(crate::Window::new(1, 1, 2, 2), &crate::WindowOptions { bands: vec![bands], overview: None }).unwrap();
            let off = (bands - 1) * 15;
            assert_eq!(w.raster.data.to_f64_vec(), [6, 7, 11, 12].map(|v| (off + v) as f64).to_vec());
        }
    }

    #[test]
    fn vector_codecs_round_trip() {
        let gj = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"name":"a","n":1},"geometry":{"type":"Point","coordinates":[1.5,2.5]}},
            {"type":"Feature","properties":{"name":"b","n":2},"geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}}]}"#;
        let Dataset::Vector(v) = codec::open(gj.as_bytes().to_vec(), "pts.geojson", &Default::default()).unwrap() else { panic!() };
        assert_eq!(v.layers[0].name, "pts");
        assert_eq!(v.layers[0].features.len(), 2);
        assert_eq!(v.layers[0].crs_epsg(), Some(4326));
        for (name, driver) in [
            ("o.fgb", VectorDriver::FlatGeobuf),
            ("o.geojson", VectorDriver::GeoJSON),
            ("o.kml", VectorDriver::KML),
            ("o.csv", VectorDriver::CSV),
            ("o.parquet", VectorDriver::Parquet),
        ] {
            let bytes = codec::write_vector(&v.layers[..1], driver, &Default::default()).unwrap();
            let Dataset::Vector(back) = codec::open(bytes, name, &Default::default()).unwrap() else { panic!() };
            assert_eq!(back.layers[0].features.len(), 2, "{name}");
            assert_eq!(back.layers[0].features[0].geometry, v.layers[0].features[0].geometry, "{name}");
        }
        // A zip archive holding the file.
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("dir/pts.geojson", zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut zip, gj.as_bytes()).unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let Dataset::Vector(z) = codec::open(bytes, "pts.zip", &Default::default()).unwrap() else { panic!() };
        assert_eq!(z.layers[0].features.len(), 2);
    }

    #[test]
    fn geopackage_codec_round_trips() {
        // A raster: Int16 as a gridded coverage, Byte as PNG tiles.
        for (bands, dt) in [(1, DataType::I16), (3, DataType::U8), (1, DataType::F32)] {
            let ds = raster(bands, dt);
            let bytes = codec::write_raster(&ds, RasterDriver::GPKG, &Default::default(), "dem").unwrap();
            let Dataset::Raster(back) = codec::open(bytes, "x.gpkg", &Default::default()).unwrap() else { panic!() };
            assert_eq!((back.raster.cols, back.raster.rows, back.raster.bands, back.raster.data_type), (5, 3, bands, dt));
            assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
            assert_eq!(back.geo_transform(), [500000.0, 30.0, 0.0, 4000090.0, 0.0, -30.0]);
            assert_eq!(back.crs().epsg, Some(32610));
        }
        // Vector layers.
        let gj = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"id":7},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let Dataset::Vector(v) = codec::open(gj.as_bytes().to_vec(), "p.geojson", &Default::default()).unwrap() else { panic!() };
        let bytes = codec::write_vector(&v.layers, VectorDriver::GPKG, &Default::default()).unwrap();
        let Dataset::Vector(back) = codec::open(bytes, "p.gpkg", &Default::default()).unwrap() else { panic!() };
        assert_eq!(back.layers[0].name, "p");
        assert_eq!(back.layers[0].features[0].geometry, v.layers[0].features[0].geometry);
        assert_eq!(back.layers[0].crs_epsg(), Some(4326));
    }

    #[test]
    fn jpeg2000_codec_round_trips() {
        for (bands, dt) in [(1, DataType::I16), (3, DataType::U8), (2, DataType::U16)] {
            let ds = raster(bands, dt);
            let bytes = codec::write_raster(&ds, RasterDriver::JP2, &Default::default(), "x").unwrap();
            // The nodata value comes in a `.aux.xml` sidecar: the two files are zipped.
            assert!(bytes.starts_with(b"PK"));
            let Dataset::Raster(back) = codec::open(bytes, "x.zip", &Default::default()).unwrap() else { panic!() };
            assert_eq!((back.raster.cols, back.raster.rows, back.raster.bands, back.raster.data_type), (5, 3, bands, dt));
            assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
            assert_eq!(back.geo_transform(), [500000.0, 30.0, 0.0, 4000090.0, 0.0, -30.0]);
            assert_eq!(back.crs().epsg, Some(32610));
            assert_eq!(back.nodata(), Some(-1.0));
            // Without nodata, the JP2 file alone, recognized by its signature too.
            let mut plain = ds.clone();
            plain.has_nodata = false;
            let jp2 = codec::write_raster(&plain, RasterDriver::JP2, &Default::default(), "x").unwrap();
            assert!(crate::drivers::jp2::is_jpeg2000(&jp2));
            let Dataset::Raster(back) = codec::open(jp2, "noext", &Default::default()).unwrap() else { panic!() };
            assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
            assert_eq!(back.nodata(), None);
        }
    }

    #[test]
    fn netcdf_hdf5_and_zarr_codecs_round_trip() {
        for (dt, bands) in [(DataType::I16, 1), (DataType::F32, 2)] {
            let ds = raster(bands, dt);
            for format in ["NC", "NC4"] {
                let options = RasterWriteOptions { creation_options: vec![("FORMAT".into(), format.into())], ..Default::default() };
                let bytes = codec::write_raster(&ds, RasterDriver::NetCDF, &options, "x").unwrap();
                let Dataset::Raster(back) = codec::open(bytes.clone(), "x.nc", &Default::default()).unwrap() else { panic!() };
                assert_eq!((back.raster.cols, back.raster.rows, back.raster.bands), (5, 3, bands), "{format}");
                assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec(), "{format}");
                assert_eq!(back.geo_transform(), [500000.0, 30.0, 0.0, 4000090.0, 0.0, -30.0], "{format}");
                assert_eq!(back.crs().epsg, Some(32610), "{format}");
                if format == "NC4" {
                    // A netCDF-4 file is an HDF5 file too.
                    let Dataset::Raster(h5) = codec::open(bytes, "x.h5", &Default::default()).unwrap() else { panic!() };
                    assert_eq!(h5.raster.data.to_f64_vec().len(), 15 * bands);
                }
            }
            let zipped = write_raster_bytes(&ds, "x.zarr", &Default::default()).unwrap();
            let Dataset::Raster(back) = codec::open(zipped.clone(), "x.zip", &Default::default()).unwrap() else { panic!() };
            assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
            assert_eq!(back.geo_transform(), [500000.0, 30.0, 0.0, 4000090.0, 0.0, -30.0]);
            assert_eq!(back.crs().epsg, Some(32610));
            // The store's files, as a key-value store gives them (without the store's directory).
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zipped)).unwrap();
            let mut files = Vec::new();
            for i in 0..zip.len() {
                let mut f = zip.by_index(i).unwrap();
                let name = f.name().unwrap().trim_start_matches("x.zarr/").to_string();
                let mut b = Vec::new();
                std::io::Read::read_to_end(&mut f, &mut b).unwrap();
                files.push((name, b));
            }
            assert!(files.iter().any(|(n, _)| n == ".zgroup"));
            let back = open_zarr_store(files).unwrap();
            assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
        }
    }

    #[test]
    fn lerc_jxl_and_jpeg_geotiffs_round_trip() {
        let (cols, rows) = (40, 33);
        let make = |bands: usize, dt: DataType| {
            let v: Vec<f64> = (0..cols * rows * bands).map(|i| ((i * 7) % 200) as f64).collect();
            let cfg = wbraster::RasterConfig { cols, rows, bands, nodata: -1.0, data_type: dt, ..Default::default() };
            RasterDataset::new(Raster::from_data_native(cfg, RasterData::from_f64_vec(dt, v)).unwrap(), false)
        };
        let cases = [
            ("LERC", DataType::F32, 1, Some(16)),
            ("LERC_DEFLATE", DataType::I16, 2, None),
            ("LERC_ZSTD", DataType::U8, 3, Some(16)),
            ("JXL", DataType::U8, 3, Some(16)),
            ("JXL", DataType::U16, 1, None),
        ];
        for (c, dt, bands, tile) in cases {
            let ds = make(bands, dt);
            let options = RasterWriteOptions { compression: Some(c.into()), tile_size: tile, ..Default::default() };
            let bytes = codec::write_raster(&ds, RasterDriver::GTiff, &options, "x").unwrap();
            let Dataset::Raster(back) = codec::open(bytes, "x.tif", &Default::default()).unwrap() else { panic!() };
            assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec(), "{c} {dt:?}");
        }
        // Lossy: LERC within its error, JPEG close.
        let ds = make(1, DataType::F32);
        let options = RasterWriteOptions {
            compression: Some("LERC".into()),
            creation_options: vec![("MAX_Z_ERROR".into(), "0.5".into())],
            ..Default::default()
        };
        let bytes = codec::write_raster(&ds, RasterDriver::Cog, &options, "x").unwrap();
        let Dataset::Raster(back) = codec::open(bytes, "x.tif", &Default::default()).unwrap() else { panic!() };
        assert!(back.raster.data.to_f64_vec().iter().zip(ds.raster.data.to_f64_vec()).all(|(a, b)| (a - b).abs() <= 0.5));
        for (bands, tile) in [(3, Some(32)), (1, None)] {
            let v: Vec<f64> = (0..cols * rows * bands).map(|i| ((i % cols) * 3 + (i / cols % rows) * 2) as f64).collect();
            let cfg = wbraster::RasterConfig { cols, rows, bands, data_type: DataType::U8, ..Default::default() };
            let ds = RasterDataset::new(Raster::from_data_native(cfg, RasterData::from_f64_vec(DataType::U8, v)).unwrap(), false);
            let options = RasterWriteOptions { compression: Some("JPEG".into()), tile_size: tile, ..Default::default() };
            let bytes = codec::write_raster(&ds, RasterDriver::GTiff, &options, "x").unwrap();
            let Dataset::Raster(back) = codec::open(bytes, "x.tif", &Default::default()).unwrap() else { panic!() };
            let (a, b) = (back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
            let err = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f64>() / a.len() as f64;
            assert!(err < 3.0, "JPEG {bands} bands: mean error {err}");
        }
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn native_bytes_round_trip() {
        let ds = raster(2, DataType::I32);
        let bytes = write_raster_bytes(&ds, "a.tif", &Default::default()).unwrap();
        let back = open_raster_bytes(bytes, "a.tif").unwrap();
        assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
        // Shapefiles come back as a zip archive of their files.
        let gj = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"id":7},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let v = open_vector_bytes(gj.as_bytes().to_vec(), "p.geojson", &Default::default()).unwrap();
        let shp = write_vector_bytes(&v.layers, "p.shp.zip", &Default::default()).unwrap();
        let zip = zip::ZipArchive::new(std::io::Cursor::new(shp)).unwrap();
        let mut names: Vec<String> = zip.file_names().map(|n| n.unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["p.cpg", "p.dbf", "p.prj", "p.shp", "p.shx"]);
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn multi_file_formats_from_zip_bytes() {
        let gj = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"id":7,"name":"Zürich"},"geometry":{"type":"Point","coordinates":[8.5,47.4]}},
            {"type":"Feature","properties":{"id":8,"name":"b"},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let v = open_vector_bytes(gj.as_bytes().to_vec(), "p.geojson", &Default::default()).unwrap();
        for name in ["p.shp.zip", "p.mif"] {
            let zip = write_vector_bytes(&v.layers, name, &Default::default()).unwrap();
            // The in-memory codecs (what WebAssembly builds use), and the native path.
            let Dataset::Vector(back) = codec::open(zip.clone(), "p.zip", &Default::default()).unwrap() else { panic!() };
            let native = open_vector_bytes(zip, "p.zip", &Default::default()).unwrap();
            for l in [&back.layers[0], &native.layers[0]] {
                assert_eq!(l.name, "p", "{name}");
                assert_eq!(l.features.len(), 2, "{name}");
                assert_eq!(l.crs_epsg(), Some(4326), "{name}");
                assert_eq!(l.features[0].geometry, v.layers[0].features[0].geometry, "{name}");
                let i = l.schema.fields().iter().position(|f| f.name == "name").unwrap();
                assert_eq!(l.features[0].attributes[i], wbvector::FieldValue::Text("Zürich".into()), "{name}");
            }
        }
    }
}
