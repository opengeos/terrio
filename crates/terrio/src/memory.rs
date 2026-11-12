//! Datasets in memory: opening from bytes and writing to bytes, for callers that hold the data
//! already (uploads, HTTP bodies, language bindings) or have no file system (WebAssembly).
//!
//! A file name goes with the bytes: its extension tells the format, as a path's does, and its stem
//! names vector layers.
//!
//! Native reads use a temporary directory and the file drivers. Byte writes use shared codecs
//! where needed to keep a standalone document or an explicit archive; other native writes use
//! temporary files. WebAssembly uses in-memory codecs for the formats that have one:
//!
//! - reading: GeoTIFF, Esri ASCII grid, JPEG 2000 (JP2 and J2K), GeoPackage (vector layers and rasters), PMTiles and
//!   MBTiles (raster and vector tiles), MVT tiles, GeoJSON, TopoJSON,
//!   FlatGeobuf, CSV, XLSX, GeoParquet, KML, KMZ, GML, GPX, DWG, DXF, OSM PBF and XML, and zip archives of these,
//!   and Shapefile and MapInfo (MIF/MID and TAB) in zip archives (or their `.shp` or `.mif` alone);
//! - writing: GeoTIFF and COG (with lossless compressions), Esri ASCII grid, JPEG 2000, GeoPackage (vector
//!   layers and rasters), PMTiles and MBTiles, GeoJSON, TopoJSON, FlatGeobuf, CSV, XLSX, GeoParquet, KML, KMZ, GML (without
//!   its schema), GPX, DWG, DXF and Shapefile ZIP archives (`.shp.zip`).
//!
//! Shapefile output requires `.shp.zip`; native MapInfo output requires `.mif.zip` or `.tab.zip`.
//! GML byte output is a standalone document without its schema sidecar on every build.
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

use crate::formats::{Kind, RasterDriver, VectorDriver, extension};
use crate::{Dataset, Error, LazyRaster, RasterDataset, RasterWriteOptions, Result, VectorDataset, VectorOpenOptions, VectorWriteOptions};
use wbvector::Layer;

pub use crate::drivers::vector::geojson::parse as parse_geojson_layer;
pub use crate::drivers::vector::shapefile::to_zip_bytes as write_shapefile_zip_bytes;

#[cfg(test)]
mod range_tests;

/// Opens a dataset, raster or vector, from its bytes. `name` is a file name whose extension tells
/// the format (`dem.tif`, `roads.fgb`, `parcels.zip`).
pub fn open_bytes(bytes: Vec<u8>, name: &str) -> Result<Dataset> {
    open_bytes_of_kind(bytes, name, None)
}

/// A vector filename extension suggested by the shared byte identifiers, without opening a
/// dataset. Unrecognizable content, including unsigned CSV, needs a filename from the caller.
/// Identification is only a hint: the ordinary reader still validates the data and driver policy.
/// The pyogrio compatibility caller targets Pyogrio 0.13.0 / GDAL 3.12.2.
pub fn suggest_vector_extension(bytes: &[u8]) -> Option<&'static str> {
    let driver = crate::inspection::identify_bytes_unjudged(bytes, "")
        .and_then(|identified| crate::formats::VectorDriver::from_name(&identified.driver))
        .or_else(|| {
            // Anonymous buffers contain the complete document. A bounded header can end
            // inside features before a root type placed last; inspect that root before
            // falling back to format-like strings inside properties.
            let prefix = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
            let first = prefix.iter().copied().take(16 * 1024).find(|byte| !byte.is_ascii_whitespace());
            if !matches!(first, Some(b'{' | 0x1e)) {
                return None;
            }
            let text = std::str::from_utf8(bytes).ok()?.trim_start_matches('\u{feff}').trim_start();
            let sequence = text.starts_with('\u{1e}');
            let json = text.trim_start_matches('\u{1e}').trim_start();
            if !json.starts_with('{') {
                return None;
            }
            use crate::formats::VectorDriver;
            match crate::inspection::root_json_type(json)?.as_str() {
                "Topology" => Some(VectorDriver::TopoJSON),
                "CityJSON" => Some(VectorDriver::CityJSON),
                "FeatureCollection" | "Feature" | "Point" | "MultiPoint" | "LineString" | "MultiLineString" | "Polygon"
                | "MultiPolygon" | "GeometryCollection" => Some(if sequence { VectorDriver::GeoJSONSeq } else { VectorDriver::GeoJSON }),
                _ => None,
            }
        })
        .or_else(|| crate::formats::identify_bytes("", bytes).by_content.as_deref().and_then(crate::formats::VectorDriver::from_name))
        .or_else(|| codec::vector_driver(bytes, ""))?;
    // Anonymous buffers contain the whole object. Require the binary envelope rather than
    // a prefix that could also be a CSV column name; the reader still validates the payload.
    match driver {
        crate::formats::VectorDriver::FlatGeobuf
            if bytes.len() < 8
                || !bytes.starts_with(b"fgb")
                || !matches!(bytes[3], 2 | 3)
                || bytes.get(4..7) != Some(b"fgb".as_slice()) =>
        {
            return None;
        }
        crate::formats::VectorDriver::Parquet => {
            if bytes.len() < 12 || !bytes.starts_with(b"PAR1") || !bytes.ends_with(b"PAR1") {
                return None;
            }
            let metadata_length = u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().ok()?);
            if usize::try_from(metadata_length).ok()? > bytes.len() - 12 {
                return None;
            }
        }
        crate::formats::VectorDriver::OSM if crate::drivers::vector::citygml::is_xml_prefix(bytes) => return Some("osm"),
        _ => {}
    }
    driver.info().extensions.first().copied()
}

/// [`open_bytes`] for a dataset of `kind`: its name and content must pass that kind's allowed
/// drivers (see [`crate::formats::set_allowed_drivers`]), as a file's must.
fn open_bytes_of_kind(bytes: Vec<u8>, name: &str, kind: Option<Kind>) -> Result<Dataset> {
    #[cfg(not(target_family = "wasm"))]
    {
        let (_dir, path) = native::spill(&bytes, name)?;
        crate::formats::check_allowed(&path, kind)?;
        check_vector_archive_filename(&bytes, name)?;
        let result = crate::dataset::open_native(&path);
        // What terrio cannot read, GDAL reads from memory (`/vsimem/`).
        #[cfg(feature = "gdal")]
        let result = crate::dataset::gdal_fallback(&path, result, |_| crate::gdal_backend::open_bytes(bytes, name));
        result
    }
    #[cfg(target_family = "wasm")]
    {
        crate::formats::check_allowed_bytes(name, &bytes, kind)?;
        let ds = codec::open(bytes, name, &VectorOpenOptions::default())?;
        if let Dataset::Raster(raster) = &ds {
            crate::declared::check_dataset(raster)?;
        }
        Ok(ds)
    }
}

/// Opens a raster from its bytes.
pub fn open_raster_bytes(bytes: Vec<u8>, name: &str) -> Result<RasterDataset> {
    // HEC-RAS files open as vector layers; a raster open reads their HDF5 arrays, as before.
    #[cfg(feature = "hecras")]
    if crate::drivers::hdf5::parse_subdataset(name).is_none() && crate::drivers::hecras::driver::is_hecras_bytes(&bytes) {
        crate::formats::check_allowed_bytes(name, &bytes, Some(Kind::Raster))?;
        let ds = crate::drivers::hdf5::read_bytes(bytes, name, None)?;
        crate::declared::check_dataset(&ds)?;
        return Ok(ds);
    }
    match open_bytes_of_kind(bytes, name, Some(Kind::Raster))? {
        Dataset::Raster(r) => Ok(r),
        Dataset::Vector(_) => Err(Error::Unsupported(format!("'{name}' is a vector dataset, not a raster"))),
    }
}

/// Opens a raster from its bytes for reading by window. GeoTIFFs are decoded one window at a time;
/// other formats are decoded whole.
pub fn open_raster_lazy_bytes(bytes: Vec<u8>, name: &str) -> Result<LazyRaster> {
    let r = open_raster_lazy_bytes_unchecked(bytes, name)?;
    // What it declares (see `crate::declared`), from its header when it is read by window.
    crate::declared::check_lazy(&r)?;
    Ok(r)
}

fn open_raster_lazy_bytes_unchecked(bytes: Vec<u8>, name: &str) -> Result<LazyRaster> {
    // Tile archives, JPEG 2000, GRIB and TIFF are decoded from memory, without a file to check.
    crate::formats::check_allowed_bytes(name, &bytes, Some(Kind::Raster))?;
    if let Some(kind) = crate::tiles::archive_kind(name) {
        // An MBTiles archive under another archive's name is found by its content: read only if
        // the MBTiles driver is allowed.
        if bytes.starts_with(b"SQLite format 3\0") && kind != crate::tiles::ArchiveKind::MBTiles {
            crate::formats::check_driver_allowed("MBTiles", name, Kind::Raster)?;
        }
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
    if crate::drivers::grib::is_grib_bytes(&bytes) {
        // Found by its content whatever its name: read only if the GRIB driver is allowed.
        crate::drivers::check_recognised(RasterDriver::GRIB, name)?;
        return crate::drivers::grib::open_lazy_bytes(bytes.into());
    }
    if codec::is_tiff(&bytes) {
        if let Some(r) = LazyRaster::from_tiff_bytes(bytes)? {
            return Ok(r);
        }
        return Err(Error::InvalidData(format!("'{name}' is not a valid TIFF")));
    }
    open_raster_bytes(bytes, name).map(LazyRaster::from)
}

/// Backwards-compatible name for the input opened by the shared translate policy.
pub use crate::ops::translate::OpenedRaster as OpenedRasterBytes;

/// [`open_raster_lazy_bytes`], falling back to [`open_raster_bytes`] for a TIFF the lazy reader
/// refuses (as `terrio translate` falls back to reading a file whole), without copying the bytes:
/// the lazy reader shares them, and they are taken back from it when it fails. Invalid arguments
/// and resource refusals of the lazy reader are kept, as are the errors of other formats, which
/// the whole decode would read with the same decoder.
///
/// # Errors
///
/// As [`open_raster_lazy_bytes`], or as [`open_raster_bytes`] after a fallback.
pub fn open_raster_lazy_bytes_or_whole(bytes: Vec<u8>, name: &str) -> Result<OpenedRasterBytes> {
    crate::ops::translate::open_bytes(bytes, name, crate::ops::overview::OverviewSelection::Auto)
}

/// Opens a Zarr store (or GeoZarr) from its files: `(path, bytes)` pairs, paths relative to the
/// store's directory or its parent (`.zgroup`, `dem/.zarray`, `dem/0.0`...), as a key-value store
/// holds them.
pub fn open_zarr_store(files: Vec<(String, Vec<u8>)>) -> Result<RasterDataset> {
    let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
    let root = codec::zarr_root(&names).ok_or_else(|| Error::InvalidData("not a Zarr store: no .zgroup, .zarray or zarr.json".into()))?;
    // Opened as Zarr by its caller: only if the process-wide raster allow-list has Zarr, as a
    // de-registered GDAL driver opens no store.
    crate::formats::check_driver_allowed("Zarr", if root.is_empty() { "the Zarr store" } else { &root }, Kind::Raster)?;
    let ds = codec::open_zarr_store(files, &root)?;
    crate::declared::check_dataset(&ds)?;
    Ok(ds)
}

/// Opens a vector dataset from its bytes.
pub fn open_vector_bytes(bytes: Vec<u8>, name: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    #[cfg(not(target_family = "wasm"))]
    {
        let (_dir, path) = native::spill(&bytes, name)?;
        crate::formats::check_allowed(&path, Some(Kind::Vector))?;
        check_vector_archive_filename(&bytes, name)?;
        let result = crate::dataset::open_vector_native(&path, options);
        #[cfg(feature = "gdal")]
        let result = crate::dataset::gdal_fallback(&path, result, |_| crate::gdal_backend::open_vector_bytes(bytes, name, options));
        result
    }
    #[cfg(target_family = "wasm")]
    match {
        crate::formats::check_allowed_bytes(name, &bytes, Some(Kind::Vector))?;
        codec::open(bytes, name, options)?
    } {
        Dataset::Vector(v) => Ok(v),
        Dataset::Raster(_) => Err(Error::Unsupported(format!("'{name}' is a raster, not a vector dataset"))),
    }
}

/// Refuses a multi-file archive mislabeled as a single vector document.
fn check_vector_archive_filename(bytes: &[u8], name: &str) -> Result<()> {
    if codec::is_zip(bytes) && matches!(crate::formats::extension(name).as_deref(), Some("gml" | "mif" | "tab")) {
        return Err(Error::InvalidArgument(format!(
            "'{name}' contains a ZIP archive: use a .zip filename to read its dataset and sidecars"
        )));
    }
    Ok(())
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
    // JP2 byte exports, including WASM, retain the source's absent transform just as
    // file conversion does. An explicit output transform/GEOREFERENCED still wins.
    let mut jp2_options;
    let options = if driver == RasterDriver::JP2 {
        jp2_options = options.clone();
        crate::drivers::common::keep_unreferenced(&mut jp2_options, name, ds.geotransform_present.unwrap_or(true));
        &jp2_options
    } else {
        options
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
/// the options name the driver. GeoPackage, GPX, KML, KMZ, TopoJSON and XLSX hold several layers; CAD combines them into drawing layers.
/// Shapefiles require an explicit ZIP filename (`roads.shp.zip`, or `.zip` with the
/// Shapefile driver). The result is `application/zip` on native and WASM alike.
/// Bare `.shp` names are refused: use [`crate::write_vector`] for filesystem members.
/// See [`write_shapefile_zip_bytes`] for supported data and refusals.
/// GML bytes are one XML document without an XSD sidecar; use a `.gml` filename.
/// Native MapInfo bytes require `.mif.zip` or `.tab.zip`. A `.zip` name with the MapInfo driver
/// defaults to MIF members. MapInfo byte writing remains unavailable in WebAssembly.
pub fn write_vector_bytes(layers: &[Layer], name: &str, options: &VectorWriteOptions) -> Result<Vec<u8>> {
    let archive = name.to_ascii_lowercase().ends_with(".zip");
    let driver = match options.driver.or_else(|| {
        if archive {
            match VectorDriver::for_extension(&name[..name.len() - 4]) {
                Some(driver @ (VectorDriver::Shapefile | VectorDriver::MapInfo | VectorDriver::GML)) => Some(driver),
                _ => VectorDriver::for_extension(name),
            }
        } else {
            VectorDriver::for_extension(name)
        }
    }) {
        Some(d) => d,
        None => {
            #[cfg(feature = "gdal")]
            if let Some(d) = crate::gdal_backend::driver_for_path(name, true) {
                crate::dataset::ensure_vector_options_apply(options, None)?;
                return crate::gdal_backend::write_vector_bytes(layers, name, &d, &[], &[]);
            }
            return Err(Error::InvalidArgument(format!("cannot tell the output format of '{name}': give it explicitly")));
        }
    };
    crate::dataset::ensure_vector_options_apply(options, Some(driver))?;
    if driver == VectorDriver::GML {
        if archive {
            return Err(Error::InvalidArgument(
                "GML byte output is one XML document: use a .gml filename; use write_vector for a schema sidecar".into(),
            ));
        }
        crate::formats::check_driver_allowed(driver.name(), name, Kind::Vector)?;
        return codec::write_vector(layers, driver, options);
    }
    #[cfg(target_family = "wasm")]
    if driver == VectorDriver::MapInfo {
        return Err(Error::Unsupported("MapInfo byte output is unavailable in WebAssembly".into()));
    }
    if driver == VectorDriver::MapInfo && !archive {
        let extension = if crate::formats::extension(name).as_deref() == Some("tab") { "tab" } else { "mif" };
        return Err(Error::InvalidArgument(format!(
            "MapInfo bytes are a ZIP archive: use a .{extension}.zip filename (MIME application/zip)"
        )));
    }
    if driver == VectorDriver::Shapefile {
        if !archive {
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
        let member_name = if driver == VectorDriver::MapInfo {
            let stem = &name[..name.len() - 4];
            let member = native::file_name(stem, "mif");
            if matches!(crate::formats::extension(&member).as_deref(), Some("mif" | "tab")) { member } else { format!("{member}.mif") }
        } else {
            name.to_string()
        };
        let path = dir.path().join(native::file_name(&member_name, driver.info().extensions.first().copied().unwrap_or("dat")));
        let path = path.to_str().ok_or_else(|| Error::InvalidArgument(format!("invalid file name '{name}'")))?;
        crate::write_vector(layers, path, &VectorWriteOptions { driver: Some(driver), ..options.clone() })?;
        native::collect(dir.path(), path)
    }
    #[cfg(target_family = "wasm")]
    {
        let mut options = options.clone();
        options.xlsx = options.xlsx.for_name(name);
        codec::write_vector(layers, driver, &options)
    }
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
    /// when there are several (a MapInfo table and its sidecars).
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
    use std::io::Cursor;

    /// Whether the bytes start with a TIFF or BigTIFF signature.
    pub(crate) fn is_tiff(b: &[u8]) -> bool {
        matches!(b.get(..4), Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+"))
    }

    pub(super) fn is_zip(b: &[u8]) -> bool {
        b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06")
    }

    /// The vector driver of a file, by its extension or, without one, its first bytes.
    pub(super) fn vector_driver(bytes: &[u8], name: &str) -> Option<VectorDriver> {
        let json = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes).trim_ascii_start();
        if json.starts_with(b"{")
            && let Ok(text) = std::str::from_utf8(json)
            && crate::inspection::root_json_type(text.trim_start()).as_deref() == Some("CityJSON")
        {
            return Some(VectorDriver::CityJSON);
        }
        if crate::drivers::vector::citygml::looks_like(json) {
            return Some(VectorDriver::CityGML);
        }
        // The protocol buffer of a query shares `.pbf` with OpenStreetMap; `.osm.pbf` is OpenStreetMap's.
        if matches!(VectorDriver::for_extension(name), None | Some(VectorDriver::OSM))
            && !name.to_ascii_lowercase().ends_with(".osm.pbf")
            && crate::drivers::vector::esripbf::is_esripbf(bytes)
        {
            return Some(VectorDriver::ESRIPBF);
        }
        // ESRI JSON shares `.json` with GeoJSON, and query results often have no extension.
        if json.starts_with(b"{")
            && matches!(VectorDriver::for_extension(name), None | Some(VectorDriver::GeoJSON))
            && extension(name).as_deref() != Some("geojson")
            && std::str::from_utf8(json).is_ok_and(crate::drivers::vector::esrijson::is_esrijson_document)
        {
            return Some(VectorDriver::ESRIJSON);
        }
        if let Some(d) = VectorDriver::for_extension(name) {
            return Some(d);
        }
        #[cfg(feature = "xlsx")]
        if crate::drivers::vector::xlsx::looks_like(bytes, name) {
            return Some(VectorDriver::XLSX);
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
        // Bytes never designate a service, even when their caller-supplied label does.
        options.oapif.validate_source("in-memory dataset")?;
        check_vector_archive_filename(&bytes, name)?;
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
            return finish_vector(layers, name, driver, options);
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
            return finish_vector(layers, name, VectorDriver::MVT, options);
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
        if crate::drivers::grib::is_grib_bytes(&bytes) {
            return crate::drivers::grib::read_bytes(&bytes).map(Dataset::Raster);
        }
        // A workbook is a ZIP archive too, read by its own driver.
        #[cfg(feature = "xlsx")]
        let workbook = crate::drivers::vector::xlsx::looks_like(&bytes, name);
        #[cfg(not(feature = "xlsx"))]
        if is_zip(&bytes) && matches!(ext.as_str(), "xlsx" | "xlsm") {
            return Err(crate::Error::disabled("xlsx"));
        }
        #[cfg(not(feature = "xlsx"))]
        let workbook = false;
        if is_zip(&bytes) && ext != "kmz" && !workbook {
            return open_zip(&bytes, name, options);
        }
        if RasterDriver::for_extension(name) == Some(RasterDriver::AAIGrid) {
            crate::drivers::declared_header::check_ascii_bytes(&bytes)?;
            let raster = wbraster::formats::esri_ascii::parse(Cursor::new(&bytes), name)?;
            return Ok(Dataset::Raster(RasterDataset::with_driver(raster, RasterDriver::AAIGrid, true)));
        }
        let driver = vector_driver(&bytes, name).ok_or_else(|| Error::Unsupported(format!("'{name}': format not recognized")))?;
        // The content may have chosen the driver from past the bytes the allow-list judged.
        crate::formats::check_driver_allowed(driver.name(), name, Kind::Vector)?;
        if matches!(driver, VectorDriver::KML | VectorDriver::KMZ) {
            let (layers, style_table) = crate::drivers::vector::read_kml_bytes(bytes, name, driver)?;
            let layers: Vec<Layer> = layers.into_iter().map(|l| crate::dataset::fix_layer(l, name, driver)).collect();
            return finish_vector(layers, name, driver, options).map(|d| match d {
                Dataset::Vector(v) => Dataset::Vector(VectorDataset { style_table, ..v }),
                raster => raster,
            });
        }
        let Some(layers) = crate::drivers::vector::read_bytes(bytes, name, driver, options)? else {
            return Err(Error::Unsupported(format!(
                "the {} format cannot be read from memory in this build (in memory: GeoTIFF, AAIGrid, GeoPackage, JPEG 2000, GRIB, netCDF, \
                 HDF5, Zarr in zip archives, GeoJSON, TopoJSON, FlatGeobuf, CSV, GeoParquet, KML, KMZ, GML, GPX, OSM PBF, and \
                 Shapefile and MapInfo in zip archives)",
                driver.name()
            )));
        };
        let layers: Vec<Layer> = layers.into_iter().map(|l| crate::dataset::fix_layer(l, name, driver)).collect();
        finish_vector(layers, name, driver, options)
    }

    /// Opens the main dataset of a zip archive held in memory.
    ///
    /// Its members are read within the archive limits (`crate::vfs::ZipLimits`), as `/vsizip/`
    /// reads the same archive on disk.
    fn open_zip(bytes: &[u8], name: &str, options: &VectorOpenOptions) -> Result<Dataset> {
        let mut zip = crate::vfs::LimitedZip::open(Cursor::new(bytes))?;
        let mut names = Vec::new();
        for i in 0..zip.archive().len() {
            let (member, _, is_file) = zip.entry(i)?;
            if is_file {
                names.push(member);
            }
        }
        names.sort();
        // A Zarr store: its files, read under a directory in memory.
        if let Some(root) = zarr_root(&names) {
            // Only the store's files: other members do not count toward the limits.
            let prefix = format!("{root}/");
            let mut files = Vec::with_capacity(names.len());
            for n in names.iter().filter(|n| root.is_empty() || n.trim_start_matches("./").starts_with(&prefix)) {
                files.push((n.clone(), zip.read_name(n, u64::MAX)?));
            }
            return open_zarr_store(files, &root).map(Dataset::Raster);
        }
        let main: String = crate::vfs::MAIN_DATASET_EXTENSIONS
            .iter()
            .find_map(|ext| names.iter().find(|n| extension(n.as_str()).as_deref() == Some(*ext)))
            .cloned()
            .ok_or_else(|| Error::Unsupported(format!("no dataset found in '{name}'")))?;
        let member = zip.read_name(&main, u64::MAX)?;
        let driver = VectorDriver::for_extension(&main);
        if let Some(driver @ (VectorDriver::Shapefile | VectorDriver::MapInfo)) = driver {
            // The sidecars: the members named as the main file, with another extension.
            let base = &main[..main.len() - extension(&main).map_or(0, |e| e.len() + 1)];
            let zip = std::cell::RefCell::new(zip);
            // A sidecar past the archive limits refuses the dataset, rather than opening it without.
            let refused = std::cell::RefCell::new(None);
            let sidecar = |ext: &str| -> Option<Vec<u8>> {
                let member = names.iter().find(|n| {
                    n.len() == base.len() + ext.len() + 1 && n.starts_with(base) && extension(n.as_str()).as_deref() == Some(ext)
                })?;
                match zip.borrow_mut().read_name(member, u64::MAX) {
                    Ok(bytes) => Some(bytes),
                    Err(e) if e.is_resource_refusal() => {
                        refused.borrow_mut().get_or_insert(e);
                        None
                    }
                    Err(error) => {
                        if ext == "prj" {
                            crate::drivers::vector::shapefile::warn_prj_read_error(member, &error);
                        }
                        None
                    }
                }
            };
            let layers = crate::drivers::vector::read_parts(&member, &main, driver, &sidecar, options);
            if let Some(e) = refused.into_inner() {
                return Err(e);
            }
            let layers = layers?;
            let layers: Vec<Layer> = layers.into_iter().map(|l| crate::dataset::fix_layer(l, &main, driver)).collect();
            return finish_vector(layers, &main, driver, options);
        }
        // An archive in the member shares what is left of this one's limits.
        let mut ds = zip.nested(|| open(member, &main, options))?;
        // A raster's `.aux.xml` sidecar (a JPEG 2000's nodata value).
        let aux = format!("{main}.aux.xml");
        if let Dataset::Raster(r) = &mut ds
            && names.contains(&aux)
        {
            let xml = String::from_utf8(zip.read_name(&aux, u64::MAX)?).map_err(|_| Error::InvalidData(format!("'{aux}' is not UTF-8")))?;
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
    pub(super) fn finish_vector(layers: Vec<Layer>, name: &str, driver: VectorDriver, options: &VectorOpenOptions) -> Result<Dataset> {
        let names: Vec<String> = layers.iter().map(|l| l.name.clone()).collect();
        let mut layers = crate::dataset::select_layers(layers, name, options)?;
        // Preserve empty MVT datasets; explicit missing selections are checked above.
        if layers.is_empty() && driver != VectorDriver::MVT {
            return Err(Error::InvalidArgument(format!("no layer named {} (layers: {})", options.layers.join(", "), names.join(", "))));
        }
        layers.iter_mut().for_each(|layer| options.keep_range(layer));
        Ok(Dataset::Vector(VectorDataset::new(layers, driver)))
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
                let inherited = options.inherited_from(ds, Some(driver));
                let jp2 = crate::drivers::jp2::to_bytes(
                    &ds.raster,
                    crate::drivers::common::georeferenced(&inherited.creation_options),
                    inherited.geo_transform,
                    &inherited.band_metadata,
                    &inherited.creation_options,
                )?;
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
    /// Reference: GDAL 3.13.3. The WASM codec and native writer use the requested output grid.
    #[test]
    #[cfg(feature = "jp2")]
    fn jp2_codec_applies_explicit_output_transform() {
        let raster = crate::Raster::from_data_native(
            crate::RasterConfig { cols: 32, rows: 24, bands: 1, data_type: crate::DataType::U8, ..Default::default() },
            crate::raster::RasterData::U8(vec![7; 32 * 24]),
        )
        .unwrap();
        let mut ds = crate::RasterDataset::new(raster, false);
        ds.geotransform_present = Some(false);
        let transform = [500000.0, 10.0, 0.0, 4600000.0, 0.0, -10.0];
        let options = crate::RasterWriteOptions { geo_transform: Some(transform), ..Default::default() };
        let codec_bytes = super::codec::write_raster(&ds, crate::formats::RasterDriver::JP2, &options, "grid").unwrap();
        let file_bytes = super::write_raster_bytes(&ds, "grid.jp2", &options).unwrap();
        for bytes in [codec_bytes, file_bytes] {
            let output = crate::drivers::jp2::read_bytes(&bytes).unwrap();
            assert_eq!(output.geotransform_present, Some(true));
            assert_eq!(output.geo_transform(), transform);
            assert_eq!(output.raster.data.to_f64_vec(), vec![7.0; 32 * 24]);
        }
        for transform in [[500000.0, 10.0, 1.0, 4600000.0, 2.0, -10.0], [500000.0, 10.0, 0.0, 4600000.0, 0.0, 10.0]] {
            let options = crate::RasterWriteOptions { geo_transform: Some(transform), ..Default::default() };
            let bytes = super::codec::write_raster(&ds, crate::formats::RasterDriver::JP2, &options, "grid").unwrap();
            let output = crate::drivers::jp2::read_bytes(&bytes).unwrap();
            assert_eq!(output.geo_transform(), transform);
            assert_eq!(output.raster.data.to_f64_vec(), vec![7.0; 32 * 24]);
        }
    }

    use super::*;
    use wbraster::raster::RasterData;
    use wbraster::{DataType, Raster};

    #[test]
    fn empty_mvt_keeps_an_empty_dataset_without_a_layer_selection() {
        let dataset = codec::finish_vector(Vec::new(), "empty.mvt", VectorDriver::MVT, &Default::default()).unwrap();
        assert!(matches!(dataset, Dataset::Vector(dataset) if dataset.layers.is_empty()));
        let options = VectorOpenOptions { layers: vec!["missing".into()], ..Default::default() };
        let error = codec::finish_vector(Vec::new(), "empty.mvt", VectorDriver::MVT, &options).unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(message) if message.contains("no layer named missing")));
    }

    fn packaging_layer() -> Layer {
        parse_geojson_layer(
            br#"{"type":"FeatureCollection","name":"points","features":[{"type":"Feature","properties":{"name":"first","n":1},"geometry":{"type":"Point","coordinates":[10,46]}},{"type":"Feature","properties":{"name":"second","n":2},"geometry":{"type":"Point","coordinates":[11,46]}}]}"#,
            "points",
        )
        .unwrap()
    }

    #[test]
    fn gml_bytes_are_a_document_under_their_filename() {
        let bytes = write_vector_bytes(&[packaging_layer()], "points.gml", &Default::default()).unwrap();
        assert!(!bytes.starts_with(b"PK"), "a .gml filename must contain a GML document");
        let back = open_vector_bytes(bytes.clone(), "points.gml", &Default::default()).unwrap();
        assert_eq!(back.layers[0].features.len(), 2);
        let Dataset::Vector(back) = codec::open(bytes, "points.gml", &Default::default()).unwrap() else { panic!() };
        assert_eq!(back.layers[0].features.len(), 2);
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn mapinfo_bytes_require_explicit_archive_filenames() {
        for extension in ["mif", "tab"] {
            let error = write_vector_bytes(&[packaging_layer()], &format!("points.{extension}"), &Default::default()).unwrap_err();
            assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
            assert!(error.to_string().contains(&format!(".{extension}.zip")), "{error}");
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn named_mapinfo_archives_round_trip_through_each_reader() {
        for extension in ["mif", "tab"] {
            let name = format!("points.{extension}.zip");
            let bytes = write_vector_bytes(&[packaging_layer()], &name, &Default::default()).unwrap();
            assert!(bytes.starts_with(b"PK"));
            let back = open_vector_bytes(bytes.clone(), &name, &Default::default()).unwrap();
            assert_eq!(back.layers[0].features.len(), 2);
            let Dataset::Vector(back) = codec::open(bytes, &name, &Default::default()).unwrap() else { panic!() };
            assert_eq!(back.layers[0].features.len(), 2);
        }
    }

    #[test]
    fn mislabeled_vector_archives_are_refused_instead_of_decoded_as_documents() {
        use std::io::Write;

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("points.geojson", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(br#"{"type":"FeatureCollection","features":[]}"#).unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        for extension in ["gml", "mif", "tab"] {
            let name = format!("points.{extension}");
            for error in [
                open_bytes(bytes.clone(), &name).unwrap_err(),
                open_vector_bytes(bytes.clone(), &name, &Default::default()).unwrap_err(),
                codec::open(bytes.clone(), &name, &Default::default()).unwrap_err(),
            ] {
                assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
                assert!(error.to_string().contains("ZIP"), "{error}");
                assert!(error.to_string().contains(".zip"), "{error}");
            }
        }
    }

    #[test]
    fn unreadable_projection_member_is_diagnosed_by_the_memory_codec() {
        let bytes = include_bytes!("../tests/data/shapefile/prj_bad_crc.zip");
        let _ = crate::warnings::take();
        let result = codec::open(bytes.to_vec(), "points.zip", &Default::default());
        let warnings = crate::warnings::take();
        let Dataset::Vector(dataset) = result.unwrap() else { panic!() };
        assert_eq!(dataset.layers[0].features.len(), 3);
        assert_eq!(dataset.layers[0].crs_wkt(), None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("pts.prj cannot be read"));
        assert!(warnings[0].to_ascii_lowercase().contains("checksum"));
    }

    #[test]
    fn projection_member_budget_refusals_are_not_downgraded_to_warnings() {
        let bytes = include_bytes!("../tests/data/shapefile/prj_bad_crc.zip");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let main_size = archive.by_name("pts.shp").unwrap().size();
        let before_projection = main_size + archive.by_name("pts.dbf").unwrap().size();
        let _ = crate::warnings::take();
        crate::config::push_scope(std::collections::HashMap::from([("TERRIO_ZIP_MAX_SIZE".into(), Some(before_projection.to_string()))]));
        let result = codec::open(bytes.to_vec(), "points.zip", &Default::default());
        crate::config::pop_scope();
        let warnings = crate::warnings::take();
        assert!(result.is_err_and(|error| error.is_resource_refusal()));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn vector_byte_hints_share_signatures_and_xml_probes() {
        for (bytes, extension) in [
            (b"SQLite format 3\0".as_slice(), Some("gpkg")),
            (b"fgb\x03fgb\0".as_slice(), Some("fgb")),
            (b"fgb\x02fgb\0".as_slice(), Some("fgb")),
            (b"PAR1\0\0\0\0PAR1".as_slice(), Some("parquet")),
            (br#"{"type":"FeatureCollection","features":[]}"#.as_slice(), Some("geojson")),
            (br#"{"type":"FeatureCollection","features":[],"Topology":"survey"}"#.as_slice(), Some("geojson")),
            (b"<kml/>".as_slice(), Some("kml")),
            (b"<gpx/>".as_slice(), Some("gpx")),
            (br#"<osm version="0.6"/>"#.as_slice(), Some("osm")),
            (br#"<osm version="0.6"><tag k="name" v="gml survey"/></osm>"#.as_slice(), Some("osm")),
            (b"\0\0\0\x0b\x0a\x09OSMHeader".as_slice(), Some("pbf")),
            (b"<gml:FeatureCollection/>".as_slice(), Some("gml")),
            (b"WKT,id\nPOINT (1 2),1\n".as_slice(), None),
            (b"fgb_id,lat,lon\n1,2,3\n".as_slice(), None),
            (b"PAR1_id,lat,lon\n1,2,3\n".as_slice(), None),
            (b"PAR1,id\nx,PAR1".as_slice(), None),
            (b"fgb,fgb,name\n1,2,3\n".as_slice(), None),
            (b"PAR1".as_slice(), None),
            (b"II*\0".as_slice(), None),
            (b"".as_slice(), None),
        ] {
            assert_eq!(suggest_vector_extension(bytes), extension, "{bytes:?}");
        }
    }

    #[test]
    fn late_geojson_root_type_is_not_relabelled_by_property_text() {
        let bytes = format!(
            r#"{{"features":[{{"type":"Feature","properties":{{"Topology":"{}"}},"geometry":{{"type":"Point","coordinates":[1,2]}}}}],"type":"FeatureCollection"}}"#,
            "x".repeat(20_000)
        );
        assert_eq!(suggest_vector_extension(bytes.as_bytes()), Some("geojson"));
    }

    #[test]
    fn a_misspelled_layer_name_is_refused_in_memory() {
        let bytes = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let names = |n: &[&str]| VectorOpenOptions { layers: n.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        // The codecs WebAssembly reads with; native builds read a spilled file.
        let Dataset::Vector(kept) = codec::open(bytes.to_vec(), "points.geojson", &names(&["points"])).unwrap() else { panic!() };
        assert_eq!(kept.layers.len(), 1);
        let error = codec::open(bytes.to_vec(), "points.geojson", &names(&["points", "pionts"])).unwrap_err();
        assert_eq!(error.to_string(), "no layer named pionts (layers: points)");
    }

    #[test]
    fn named_layers_answer_to_the_file_stem_through_each_reader() {
        let bytes = br#"{"type":"FeatureCollection","name":"roads","features":[{"type":"Feature","properties":{"v":7},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let options = |names: &[&str]| VectorOpenOptions { layers: names.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        // Exercise the WASM codecs directly, as well as native paths and native spilled bytes.
        for names in [vec![], vec!["map"], vec!["roads"], vec!["roads", "map", "map"]] {
            let o = options(&names);
            let path = dir.path().join("map.geojson");
            std::fs::write(&path, bytes).unwrap();
            let datasets = [
                codec::open(bytes.to_vec(), "map.geojson", &o).unwrap(),
                Dataset::Vector(crate::open_vector(path.to_str().unwrap(), &o).unwrap()),
                Dataset::Vector(open_vector_bytes(bytes.to_vec(), "map.geojson", &o).unwrap()),
            ];
            for ds in datasets {
                let Dataset::Vector(ds) = ds else { panic!() };
                assert_eq!(ds.layers.len(), 1);
                assert_eq!(ds.layers[0].name, "roads");
                assert_eq!(ds.layers[0].features.len(), 1);
            }
        }
        for names in [vec!["missing"], vec!["map", "missing"]] {
            let o = options(&names);
            let error = codec::open(bytes.to_vec(), "map.geojson", &o).unwrap_err();
            assert_eq!(error.to_string(), "no layer named missing (layers: roads)");
        }
    }

    #[test]
    fn named_layer_selection_is_consistent_across_codecs() {
        let source = codec::open(br#"{"type":"FeatureCollection","name":"roads","features":[{"type":"Feature","properties":{"v":7},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#.to_vec(), "source.geojson", &Default::default()).unwrap();
        let Dataset::Vector(source) = source else { panic!() };
        let mut extensions = vec!["geojson", "kml", "kmz", "gml"];
        if cfg!(feature = "flatgeobuf") {
            extensions.push("fgb");
        }
        if cfg!(feature = "parquet") {
            extensions.push("parquet");
        }
        if cfg!(feature = "sqlite") {
            extensions.push("gpkg");
        }
        if cfg!(feature = "xlsx") {
            extensions.push("xlsx");
        }
        let dir = tempfile::tempdir().unwrap();
        for ext in extensions {
            let filename = format!("map.{ext}");
            let bytes = write_vector_bytes(&source.layers, &filename, &Default::default()).unwrap();
            let o = VectorOpenOptions { layers: vec!["map".into()], ..Default::default() };
            let Dataset::Vector(ds) = codec::open(bytes.clone(), &filename, &o).unwrap() else { panic!("{ext}") };
            assert_eq!(ds.layers.len(), 1, "{ext}");
            assert_eq!(ds.layers[0].features.len(), 1, "{ext}");
            let path = dir.path().join(&filename);
            crate::write_vector(&source.layers, path.to_str().unwrap(), &Default::default()).unwrap();
            let native = crate::open_vector(path.to_str().unwrap(), &o).unwrap_or_else(|e| panic!("{ext}: {e}"));
            assert_eq!(native.layers[0].name, ds.layers[0].name, "{ext}");
            let o = VectorOpenOptions { layers: vec!["map".into(), "missing".into()], ..Default::default() };
            let error = crate::open_vector(path.to_str().unwrap(), &o).unwrap_err();
            assert_eq!(error.to_string(), format!("no layer named missing (layers: {})", ds.layers[0].name), "{ext}");
            if matches!(ext, "kml" | "kmz" | "gpkg" | "xlsx") {
                let mut other = source.layers[0].clone();
                other.name = "rivers".into();
                let bytes = write_vector_bytes(&[source.layers[0].clone(), other], &filename, &Default::default()).unwrap();
                let o = VectorOpenOptions { layers: vec!["roads".into(), "map".into()], ..Default::default() };
                let error = codec::open(bytes, &filename, &o).unwrap_err();
                assert_eq!(error.to_string(), "no layer named map (layers: roads, rivers)", "{ext}");
            }
        }
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn zip_layer_selection_uses_member_names_through_each_reader() {
        let bytes = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let source = open_vector_bytes(bytes.to_vec(), "source.geojson", &Default::default()).unwrap();
        for filename in ["member.shp.zip", "member.mif.zip"] {
            let bytes = write_vector_bytes(&source.layers, filename, &Default::default()).unwrap();
            let member = VectorOpenOptions { layers: vec!["member".into()], ..Default::default() };
            let Dataset::Vector(codec) = codec::open(bytes.clone(), "archive.zip", &member).unwrap() else { panic!() };
            let native = open_vector_bytes(bytes.clone(), "archive.zip", &member).unwrap();
            assert_eq!(codec.layers[0].name, "member", "{filename}");
            assert_eq!(codec.layers[0].features[0].geometry, native.layers[0].features[0].geometry, "{filename}");
            let archive = VectorOpenOptions { layers: vec!["archive".into()], ..Default::default() };
            let native_error = open_vector_bytes(bytes.clone(), "archive.zip", &archive).unwrap_err();
            assert_eq!(native_error.to_string(), "no layer named archive (layers: member)", "{filename}");
            let codec_error = codec::open(bytes, "archive.zip", &archive).unwrap_err();
            assert_eq!(codec_error.to_string(), native_error.to_string(), "{filename}");
        }
    }

    #[test]
    fn a_source_without_layers_is_refused_in_memory() {
        let error = codec::finish_vector(Vec::new(), "empty.geojson", VectorDriver::GeoJSON, &VectorOpenOptions::default()).unwrap_err();
        assert!(error.to_string().starts_with("no layer named"), "{error}");
    }

    /// A Zarr store opened from its files is a Zarr open, as GDAL's de-registered driver opens no
    /// store: refused under a raster allow-list without Zarr, opened with it or without a list.
    #[cfg(feature = "zarr")]
    #[test]
    fn a_zarr_store_in_memory_opens_only_with_zarr_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s.zarr");
        crate::write_raster_dataset(&raster(1, DataType::U8), store.to_str().unwrap(), &RasterWriteOptions::default()).unwrap();
        let (mut files, mut dirs) = (Vec::new(), vec![store]);
        while let Some(d) = dirs.pop() {
            for path in std::fs::read_dir(&d).unwrap().flatten().map(|e| e.path()) {
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    let name = path.strip_prefix(dir.path()).unwrap().to_string_lossy().replace('\\', "/");
                    files.push((name, std::fs::read(&path).unwrap()));
                }
            }
        }
        let open = |drivers: Option<&str>| {
            if let Some(list) = drivers {
                let option = ("TERRIO_ALLOWED_DRIVERS".to_string(), Some(list.to_string()));
                crate::config::push_scope(std::collections::HashMap::from([option]));
            }
            let out = open_zarr_store(files.clone()).map(|d| d.driver.name()).map_err(|e| e.to_string());
            if drivers.is_some() {
                crate::config::pop_scope();
            }
            out
        };
        assert_eq!(open(None), Ok("Zarr"));
        assert_eq!(open(Some("GTiff")), Err("'s.zarr' holds Zarr data, and the Zarr driver is not allowed (allowed: GTIFF)".to_string()));
        assert_eq!(open(Some("GTiff,Zarr")), Ok("Zarr"));
    }

    #[test]
    fn memory_codec_preserves_color_overrides() {
        let mut ds = raster(1, DataType::U8);
        ds.band_metadata = vec![crate::BandMetadata { color_interp: Some("Undefined".into()), ..Default::default() }];
        let bytes = codec::write_raster(&ds, RasterDriver::GTiff, &RasterWriteOptions::default(), "raster").unwrap();
        let back = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().header();
        assert_eq!(back.band_color_interp(1), "Undefined");
    }

    #[test]
    fn a_kml_style_table_travels_from_opening_to_writing() {
        let kml = r##"<kml><Document>
            <Style id="base"><LineStyle><color>ff0000ff</color><width>2</width></LineStyle></Style>
            <Placemark><styleUrl>#base</styleUrl><Point><coordinates>30,-2</coordinates></Point></Placemark>
            </Document></kml>"##;
        let style = "PEN(c:#FF0000FF,w:2.000000px)";
        let field = |v: &VectorDataset| {
            let l = &v.layers[0];
            l.features[0].attributes[l.schema.field_index("OGR_STYLE").unwrap()].clone()
        };
        let ds = open_vector_bytes(kml.as_bytes().to_vec(), "a.kml", &Default::default()).unwrap();
        assert_eq!(ds.style_table, [("base".to_string(), style.to_string())]);
        assert_eq!(field(&ds), wbvector::FieldValue::Text("@base".into()));
        let options = VectorWriteOptions { style_table: ds.style_table.clone(), ..Default::default() };
        for name in ["o.kml", "o.kmz"] {
            let back = open_vector_bytes(write_vector_bytes(&ds.layers, name, &options).unwrap(), name, &Default::default()).unwrap();
            assert_eq!(back.style_table, ds.style_table, "{name}");
            assert_eq!(field(&back), field(&ds), "{name}");
        }
        // No other format holds a style table: refused, not dropped, and the references resolve.
        assert!(matches!(write_vector_bytes(&ds.layers, "o.geojson", &options), Err(Error::InvalidArgument(_))));
        let mut resolved = ds.clone();
        resolved.resolve_styles();
        assert_eq!(field(&resolved), wbvector::FieldValue::Text(style.into()));
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
    fn geopackage_writer_can_preserve_unknown_declarations_in_both_codecs() {
        let mut layer = Layer::new("unknown").with_epsg(4326);
        layer.features.push(crate::Feature { fid: 1, geometry: Some(crate::Geometry::point(1., 2.)), attributes: vec![] });
        let layers = [layer];
        let declaration = |bytes: Vec<u8>| {
            let db = crate::drivers::sqlite::Db::new(bytes).unwrap();
            let table = db.table("gpkg_geometry_columns").unwrap();
            table.get(&table.rows[0], "geometry_type_name").unwrap().as_str().unwrap().to_string()
        };
        assert_eq!(declaration(write_vector_bytes(&layers, "x.gpkg", &Default::default()).unwrap()), "POINT");
        let options = VectorWriteOptions { preserve_declared_geometry_type: true, ..Default::default() };
        for bytes in
            [write_vector_bytes(&layers, "x.gpkg", &options).unwrap(), codec::write_vector(&layers, VectorDriver::GPKG, &options).unwrap()]
        {
            assert_eq!(declaration(bytes.clone()), "GEOMETRY");
            let back = open_vector_bytes(bytes, "x.gpkg", &Default::default()).unwrap();
            assert_eq!(back.layers[0].features[0].geometry, layers[0].features[0].geometry);
        }
        assert!(write_vector_bytes(&layers, "x.geojson", &options).unwrap_err().to_string().contains("GPKG"));
    }

    #[test]
    fn geopackage_mixed_measures_are_optional_and_nulls_do_not_change_layout() {
        use crate::{Feature, Geometry};
        use wbvector::Coord;
        for (second, expected) in [(Some(Geometry::point(4., 5.)), 2), (None, 1)] {
            let mut layer = Layer::new("mixed").with_epsg(4326);
            layer.features = vec![
                Feature { fid: 1, attributes: vec![], geometry: Some(Geometry::Point(Coord { m: Some(3.), ..Coord::xy(1., 2.) })) },
                Feature { fid: 2, attributes: vec![], geometry: second },
            ];
            for preserve in [false, true] {
                let options = VectorWriteOptions { preserve_declared_geometry_type: preserve, ..Default::default() };
                for bytes in [
                    write_vector_bytes(std::slice::from_ref(&layer), "mixed.gpkg", &options).unwrap(),
                    codec::write_vector(std::slice::from_ref(&layer), VectorDriver::GPKG, &options).unwrap(),
                ] {
                    let db = crate::drivers::sqlite::Db::new(bytes.clone()).unwrap();
                    let table = db.table("gpkg_geometry_columns").unwrap();
                    assert_eq!(table.get(&table.rows[0], "m").unwrap().as_i64(), Some(expected));
                    let back = open_vector_bytes(bytes, "mixed.gpkg", &Default::default()).unwrap();
                    for (a, b) in layer.features.iter().zip(&back.layers[0].features) {
                        assert_eq!(a.geometry, b.geometry);
                    }
                }
            }
        }
    }

    #[test]
    fn geopackage_mixed_heights_are_optional_and_declarations_are_checked() {
        use crate::{Feature, Geometry, GeometryType};
        let mut layer = Layer::new("mixed").with_epsg(4326);
        layer.geom_type = Some(GeometryType::Point);
        layer.features = vec![
            Feature { fid: 1, attributes: vec![], geometry: Some(Geometry::Point(wbvector::Coord::xyz(1., 2., 3.))) },
            Feature { fid: 2, attributes: vec![], geometry: Some(Geometry::point(4., 5.)) },
        ];
        for preserve in [false, true] {
            let options = VectorWriteOptions { preserve_declared_geometry_type: preserve, ..Default::default() };
            let bytes = write_vector_bytes(std::slice::from_ref(&layer), "mixed.gpkg", &options).unwrap();
            let db = crate::drivers::sqlite::Db::new(bytes.clone()).unwrap();
            let table = db.table("gpkg_geometry_columns").unwrap();
            assert_eq!(table.get(&table.rows[0], "z").unwrap().as_i64(), Some(2));
            let report = crate::gpkg_validate::validate_gpkg_bytes(&bytes, &Default::default()).unwrap();
            assert!(report.is_valid, "{:?}", report.errors);
        }
        layer.geom_type = Some(GeometryType::Polygon);
        let options = VectorWriteOptions { preserve_declared_geometry_type: true, ..Default::default() };
        assert!(
            write_vector_bytes(std::slice::from_ref(&layer), "bad.gpkg", &options)
                .unwrap_err()
                .to_string()
                .contains("does not match declared")
        );
        assert!(codec::write_vector(std::slice::from_ref(&layer), VectorDriver::GPKG, &options).is_err());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.gpkg");
        assert!(crate::write_vector(&[layer], path.to_str().unwrap(), &options).is_err());
        assert!(!path.exists());
        let mut collection = Layer::new("collection").with_epsg(4326);
        collection.geom_type = Some(GeometryType::GeometryCollection);
        collection.features.push(Feature {
            fid: 1,
            attributes: vec![],
            geometry: Some(Geometry::MultiPoint(vec![wbvector::Coord::xy(1., 2.)])),
        });
        let bytes = write_vector_bytes(&[collection], "valid.gpkg", &options).unwrap();
        let report = crate::gpkg_validate::validate_gpkg_bytes(&bytes, &Default::default()).unwrap();
        assert!(report.is_valid, "{:?}", report.errors);
    }

    #[test]
    fn geopackage_typed_empty_polygons_have_valid_wkb_in_mixed_z_layers() {
        use crate::{Feature, Geometry};
        use wbvector::{Coord, Ring};
        let empty = || Geometry::polygon(vec![], vec![]);
        for geometry in [empty(), Geometry::MultiPolygon(vec![(Ring(vec![]), vec![])]), Geometry::GeometryCollection(vec![empty()])] {
            let mut layer = Layer::new("polygons").with_epsg(4326);
            layer.features = vec![
                Feature {
                    fid: 1,
                    attributes: vec![],
                    geometry: Some(Geometry::polygon(vec![Coord::xyz(0., 0., 1.), Coord::xyz(2., 0., 2.), Coord::xyz(0., 2., 3.)], vec![])),
                },
                Feature { fid: 2, attributes: vec![], geometry: Some(geometry) },
            ];
            for preserve in [false, true] {
                let options = VectorWriteOptions { preserve_declared_geometry_type: preserve, ..Default::default() };
                let bytes = write_vector_bytes(std::slice::from_ref(&layer), "empty.gpkg", &options).unwrap();
                let report = crate::gpkg_validate::validate_gpkg_bytes(&bytes, &Default::default()).unwrap();
                assert!(report.is_valid, "{:?}", report.errors);
                let back = open_vector_bytes(bytes, "empty.gpkg", &Default::default()).unwrap();
                let g = back.layers[0].features[1].geometry.as_ref().unwrap();
                assert!(g.all_coords().is_empty());
                assert_eq!(g.geom_type(), layer.features[1].geometry.as_ref().unwrap().geom_type());
            }
        }
    }

    #[test]
    fn geopackage_preserves_zm_values_and_registers_dimensions() {
        for wkt in [
            "POINT M (1 2 30)",
            "LINESTRING ZM (0 0 10 100,1 2 20 200)",
            "POLYGON ZM ((0 0 1 10,2 0 2 20,0 2 3 30,0 0 1 10))",
            "MULTIPOINT M ((1 2 30),(3 4 50))",
            "MULTILINESTRING M ((0 0 10,1 2 20))",
            "MULTIPOLYGON M (((0 0 10,2 0 20,0 2 30,0 0 10)))",
            "GEOMETRYCOLLECTION ZM (POINT ZM (1 2 3 4),LINESTRING ZM (0 0 1 10,1 2 2 20))",
        ] {
            let csv = format!("WKT\n\"{wkt}\"\n");
            let source = open_vector_bytes(csv.into_bytes(), "source.csv", &Default::default()).unwrap();
            let geometry = source.layers[0].features[0].geometry.as_ref().unwrap();
            assert!(crate::vector::geometry_dims(geometry).1, "{wkt}");
            for preserve in [false, true] {
                let options = VectorWriteOptions { preserve_declared_geometry_type: preserve, ..Default::default() };
                for bytes in [
                    write_vector_bytes(&source.layers, "x.gpkg", &options).unwrap(),
                    codec::write_vector(&source.layers, VectorDriver::GPKG, &options).unwrap(),
                ] {
                    let db = crate::drivers::sqlite::Db::new(bytes.clone()).unwrap();
                    let table = db.table("gpkg_geometry_columns").unwrap();
                    assert_eq!(table.get(&table.rows[0], "m").unwrap().as_i64(), Some(1));
                    let back = open_vector_bytes(bytes, "x.gpkg", &Default::default()).unwrap();
                    assert_eq!(back.layers[0].features[0].geometry, source.layers[0].features[0].geometry, "{wkt}");
                }
            }
        }
    }

    #[test]
    fn preserved_geometry_declarations_support_transactional_replacement_and_append() {
        use crate::ops::vector_filter::{WriteMode, write_vector_with_mode};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("layers.gpkg");
        let path = path.to_str().unwrap();
        let layer = |name: &str| {
            let mut l = Layer::new(name).with_epsg(4326);
            l.features.push(crate::Feature { fid: 1, geometry: Some(crate::Geometry::point(1., 2.)), attributes: vec![] });
            l
        };
        let unknown = layer("unknown");
        let mut kept = layer("kept");
        kept.add_field(crate::FieldDef::new("payload", crate::FieldType::Blob));
        kept.features[0].attributes.push(crate::FieldValue::Blob(vec![42; 1024 * 1024]));
        crate::write_vector(&[unknown.clone(), kept.clone()], path, &Default::default()).unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(path).unwrap().ino()
        };
        let options = VectorWriteOptions { preserve_declared_geometry_type: true, ..Default::default() };
        write_vector_with_mode(std::slice::from_ref(&unknown), path, &options, WriteMode::Overwrite).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(path).unwrap().ino(), inode, "ordinary GeoPackages keep the in-place update path");
        }
        write_vector_with_mode(&[layer("new")], path, &options, WriteMode::Append).unwrap();
        write_vector_with_mode(&[unknown], path, &options, WriteMode::Append).unwrap();
        let db = crate::drivers::sqlite::Db::new(std::fs::read(path).unwrap()).unwrap();
        let table = db.table("gpkg_geometry_columns").unwrap();
        for row in &table.rows {
            let name = table.get(row, "table_name").unwrap().as_str().unwrap();
            let kind = table.get(row, "geometry_type_name").unwrap().as_str().unwrap();
            assert_eq!(kind, if name == "kept" { "POINT" } else { "GEOMETRY" });
        }
        let back = crate::open_vector(path, &Default::default()).unwrap();
        let kept_back = back.layers.iter().find(|l| l.name == "kept").unwrap();
        assert_eq!(kept_back.features[0].attributes, kept.features[0].attributes);
        assert_eq!(back.layers.iter().find(|l| l.name == "unknown").unwrap().len(), 2);
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

    /// The in-memory JPEG 2000 codec (WebAssembly's) writes no GeoJP2 box for a raster without a
    /// geotransform (`GEOREFERENCED=NO`) and without a CRS, as the file writer does; with a CRS,
    /// the box is written.
    #[test]
    fn jpeg2000_codec_writes_no_geojp2_box_without_georeferencing() {
        const GEOJP2: [u8; 16] = [0xb1, 0x4b, 0xf8, 0xbd, 0x08, 0x3d, 0x4b, 0x43, 0xa5, 0xae, 0x8c, 0xd7, 0xd5, 0xa6, 0xce, 0x03];
        let has_box = |bytes: &[u8]| bytes.windows(20).any(|w| w[..4] == *b"uuid" && w[4..] == GEOJP2);
        let unreferenced = RasterWriteOptions { creation_options: vec![("GEOREFERENCED".into(), "NO".into())], ..Default::default() };
        let mut ds = raster(3, DataType::U8);
        ds.has_nodata = false;
        assert!(has_box(&codec::write_raster(&ds, RasterDriver::JP2, &unreferenced, "x").unwrap()));
        ds.raster.crs = Default::default();
        let jp2 = codec::write_raster(&ds, RasterDriver::JP2, &unreferenced, "x").unwrap();
        assert!(crate::drivers::jp2::is_jpeg2000(&jp2) && !has_box(&jp2));
        assert!(has_box(&codec::write_raster(&ds, RasterDriver::JP2, &Default::default(), "x").unwrap()));
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
    fn tiff_bytes_open_lazily_or_whole() {
        let ds = raster(2, DataType::I32);
        let bytes = write_raster_bytes(&ds, "a.tif", &Default::default()).unwrap();
        let Ok(OpenedRasterBytes::Lazy(mut r)) = open_raster_lazy_bytes_or_whole(bytes, "a.tif") else { panic!() };
        assert_eq!(r.read().unwrap().raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
        // Bytes neither reader reads are refused; other formats open as the lazy opener opens them.
        let mut broken = b"II*\0".to_vec();
        broken.extend([0xff; 60]);
        assert!(open_raster_lazy_bytes_or_whole(broken, "b.tif").is_err());
        let asc = b"ncols 2\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2\n".to_vec();
        assert!(matches!(open_raster_lazy_bytes_or_whole(asc, "c.asc"), Ok(OpenedRasterBytes::Lazy(_))));
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
        for name in ["p.shp.zip", "p.mif.zip"] {
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

    /// A zip archive opened from memory reads its members within the archive limits, as
    /// `/vsizip/` reads the same archive on disk.
    #[test]
    fn zip_bytes_are_read_within_the_zip_limits() {
        let gj = format!(
            r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","properties":{{}},"geometry":{{"type":"Point","coordinates":[1,2]}}}}]}}{}"#,
            " ".repeat(1 << 20)
        );
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buf);
        writer.start_file("p.geojson", zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut writer, gj.as_bytes()).unwrap();
        writer.finish().unwrap();
        let zip = buf.into_inner();
        let scoped = |key: &str, value: &str| {
            crate::config::with_options(&[(key, value)], || codec::open(zip.clone(), "p.zip", &Default::default()))
        };
        let Dataset::Vector(v) = codec::open(zip.clone(), "p.zip", &Default::default()).unwrap() else { panic!() };
        assert_eq!(v.layers[0].features.len(), 1);
        for (key, value) in [("TERRIO_ZIP_MAX_SIZE", "1000"), ("TERRIO_ZIP_MAX_RATIO", "10"), ("TERRIO_ZIP_MAX_ENTRIES", "0")] {
            let refused = scoped(key, value);
            assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains(key)), "{key}: {:?}", refused.err());
        }
    }

    /// A shapefile sidecar past the archive limits refuses the dataset, rather than it opening
    /// without the sidecar's attributes.
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn zip_bytes_refuse_a_sidecar_past_the_zip_limits() {
        let gj = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"id":7},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let v = open_vector_bytes(gj.as_bytes().to_vec(), "p.geojson", &Default::default()).unwrap();
        let zip = write_vector_bytes(&v.layers, "p.shp.zip", &Default::default()).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip.clone())).unwrap();
        let shp = (0..archive.len())
            .map(|i| archive.by_index_raw(i).map(|f| (f.name().unwrap().to_string(), f.size())).unwrap())
            .find(|(name, _)| name.ends_with(".shp"))
            .unwrap()
            .1;
        let Dataset::Vector(back) = codec::open(zip.clone(), "p.zip", &Default::default()).unwrap() else { panic!() };
        assert_eq!(back.layers[0].features.len(), 1);
        // Room for the .shp, not for its sidecars too.
        let refused =
            crate::config::with_options(&[("TERRIO_ZIP_MAX_SIZE", &shp.to_string())], || codec::open(zip, "p.zip", &Default::default()));
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("TERRIO_ZIP_MAX_SIZE")), "{:?}", refused.err());
    }

    /// A zip archive inside a zip archive shares the outer one's `TERRIO_ZIP_MAX_SIZE`, so nesting
    /// cannot multiply it.
    #[test]
    fn nested_zip_bytes_share_the_zip_size_limit() {
        let zip_of = |name: &str, bytes: &[u8]| {
            let mut buf = std::io::Cursor::new(Vec::new());
            let mut writer = zip::ZipWriter::new(&mut buf);
            writer.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut writer, bytes).unwrap();
            writer.finish().unwrap();
            buf.into_inner()
        };
        let gj = format!(
            r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","properties":{{}},"geometry":{{"type":"Point","coordinates":[1,2]}}}}]}}{}"#,
            " ".repeat(4000)
        );
        let inner = zip_of("p.geojson", gj.as_bytes());
        let outer = zip_of("inner.geojson", &inner);
        let Dataset::Vector(v) = codec::open(outer.clone(), "p.zip", &Default::default()).unwrap() else { panic!() };
        assert_eq!(v.layers[0].features.len(), 1);
        // Room for either member alone, not for both.
        let limit = (gj.len().max(inner.len()) + 10).to_string();
        let refused = crate::config::with_options(&[("TERRIO_ZIP_MAX_SIZE", &limit)], || codec::open(outer, "p.zip", &Default::default()));
        // The message names the configured limit, not what was left of it.
        let expected = format!("more than the {limit} bytes allowed (TERRIO_ZIP_MAX_SIZE), shared with the archives it is in");
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.ends_with(&expected)), "{:?}", refused.err());
        // Archives nest at most 8 deep.
        let mut nested = zip_of("p.geojson", gj.as_bytes());
        for _ in 0..8 {
            nested = zip_of("inner.geojson", &nested);
        }
        let refused = codec::open(nested, "p.zip", &Default::default());
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("nest more than 8 deep")), "{:?}", refused.err());
    }

    /// What an archive nested in a member reads counts toward the outer archive's later reads, and
    /// the outer budget is restored when the nested open panics.
    #[test]
    fn nested_zip_reads_are_charged_to_the_outer_archive() {
        use crate::vfs::LimitedZip;
        let zip_of = |members: &[(&str, &[u8])]| {
            let mut buf = std::io::Cursor::new(Vec::new());
            let mut writer = zip::ZipWriter::new(&mut buf);
            for (name, bytes) in members {
                writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                std::io::Write::write_all(&mut writer, bytes).unwrap();
            }
            writer.finish().unwrap();
            buf.into_inner()
        };
        let inner = zip_of(&[("a", &[b'x'; 600])]);
        let outer = zip_of(&[("inner.zip", &inner), ("b", &[b'y'; 300])]);
        let (refused, panicked) = crate::config::with_options(&[("TERRIO_ZIP_MAX_SIZE", "1000")], || {
            let mut zip = LimitedZip::open(std::io::Cursor::new(outer)).unwrap();
            let member = zip.read_name("inner.zip", u64::MAX).unwrap();
            let read =
                zip.nested(|| LimitedZip::open(std::io::Cursor::new(member.clone())).unwrap().read_name("a", u64::MAX).map(|b| b.len()));
            assert_eq!(read.unwrap(), 600);
            // inner.zip, then its 600 bytes, then 300 more: past the 1,000 allowed.
            let refused = zip.read_name("b", u64::MAX);
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| zip.nested(|| panic!("nested open"))));
            (refused, panicked)
        });
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("TERRIO_ZIP_MAX_SIZE")), "{refused:?}");
        assert!(panicked.is_err());
        // No budget is left behind for later, unrelated archives.
        let fresh = zip_of(&[("c", &[b'z'; 2000])]);
        assert_eq!(LimitedZip::open(std::io::Cursor::new(fresh)).unwrap().read_name("c", u64::MAX).unwrap().len(), 2000);
    }
}
