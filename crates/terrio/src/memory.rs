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
//!   FlatGeobuf, CSV, GeoParquet, KML, KMZ, GML, GPX, OSM PBF and XML, and zip archives of these,
//!   and Shapefile and MapInfo (MIF/MID and TAB) in zip archives (or their `.shp` or `.mif` alone);
//! - writing: GeoTIFF and COG (with lossless compressions), Esri ASCII grid, JPEG 2000, GeoPackage (vector
//!   layers and rasters), PMTiles and MBTiles, GeoJSON, TopoJSON, FlatGeobuf, CSV, GeoParquet, KML, KMZ, GML (without
//!   its schema) and GPX.
//!
//! Formats made of several files (a shapefile's `.shp`, `.dbf`, `.prj`...) are written as a zip
//! archive of them.
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

/// Opens a dataset, raster or vector, from its bytes. `name` is a file name whose extension tells
/// the format (`dem.tif`, `roads.fgb`, `parcels.zip`).
pub fn open_bytes(bytes: Vec<u8>, name: &str) -> Result<Dataset> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (_dir, path) = native::spill(&bytes, name)?;
        crate::open(&path)
    }
    #[cfg(target_arch = "wasm32")]
    codec::open(bytes, name, &VectorOpenOptions::default())
}

/// Opens a raster from its bytes.
pub fn open_raster_bytes(bytes: Vec<u8>, name: &str) -> Result<RasterDataset> {
    match open_bytes(bytes, name)? {
        Dataset::Raster(r) => Ok(r),
        Dataset::Vector(_) => Err(Error::Unsupported(format!("'{name}' is a vector dataset, not a raster"))),
    }
}

/// Opens a raster from its bytes for reading by window. GeoTIFFs are decoded one window at a time;
/// other formats are decoded whole.
pub fn open_raster_lazy_bytes(bytes: Vec<u8>, name: &str) -> Result<LazyRaster> {
    if crate::tiles::archive_kind(name).is_some() {
        return Ok(LazyRaster::from_tiles(crate::tiles::raster::open_lazy_bytes(bytes)?));
    }
    if codec::is_tiff(&bytes) {
        if let Some(r) = LazyRaster::from_tiff_bytes(bytes)? {
            return Ok(r);
        }
        return Err(Error::InvalidData(format!("'{name}' is not a valid TIFF")));
    }
    open_raster_bytes(bytes, name).map(LazyRaster::from)
}

/// Opens a vector dataset from its bytes.
pub fn open_vector_bytes(bytes: Vec<u8>, name: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (_dir, path) = native::spill(&bytes, name)?;
        crate::open_vector(&path, options)
    }
    #[cfg(target_arch = "wasm32")]
    match codec::open(bytes, name, options)? {
        Dataset::Vector(v) => Ok(v),
        Dataset::Raster(_) => Err(Error::Unsupported(format!("'{name}' is a raster, not a vector dataset"))),
    }
}

/// Writes a raster to bytes, declaring its nodata value when it has one. `name` is a file name
/// whose extension tells the format, unless the options name the driver.
pub fn write_raster_bytes(ds: &RasterDataset, name: &str, options: &RasterWriteOptions) -> Result<Vec<u8>> {
    let driver = match options.driver {
        Some(d) => d,
        None => RasterDriver::for_extension(name)
            .ok_or_else(|| Error::InvalidArgument(format!("cannot tell the output format of '{name}': give it explicitly")))?,
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let dir = native::temp_dir()?;
        let path = dir.path().join(native::file_name(name, driver.info().extensions.first().copied().unwrap_or("tif")));
        let path = path.to_str().ok_or_else(|| Error::InvalidArgument(format!("invalid file name '{name}'")))?;
        crate::write_raster_dataset(ds, path, &RasterWriteOptions { driver: Some(driver), ..options.clone() })?;
        native::collect(dir.path(), path)
    }
    #[cfg(target_arch = "wasm32")]
    codec::write_raster(ds, driver, options, std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("raster"))
}

/// Writes vector layers to bytes. `name` is a file name whose extension tells the format, unless
/// the options name the driver. GeoPackage, GPX, KML, KMZ and TopoJSON hold several layers.
pub fn write_vector_bytes(layers: &[Layer], name: &str, options: &VectorWriteOptions) -> Result<Vec<u8>> {
    let driver = match options.driver {
        Some(d) => d,
        None => VectorDriver::for_extension(name)
            .ok_or_else(|| Error::InvalidArgument(format!("cannot tell the output format of '{name}': give it explicitly")))?,
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let dir = native::temp_dir()?;
        let path = dir.path().join(native::file_name(name, driver.info().extensions.first().copied().unwrap_or("dat")));
        let path = path.to_str().ok_or_else(|| Error::InvalidArgument(format!("invalid file name '{name}'")))?;
        crate::write_vector(layers, path, &VectorWriteOptions { driver: Some(driver), ..options.clone() })?;
        native::collect(dir.path(), path)
    }
    #[cfg(target_arch = "wasm32")]
    codec::write_vector(layers, driver, options)
}

/// Native builds: bytes go through a temporary directory and the file drivers.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
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
        if is_tiff(&bytes) {
            let mut r = LazyRaster::from_tiff_bytes(bytes)?.ok_or_else(|| Error::InvalidData(format!("'{name}' is not a valid TIFF")))?;
            return r.read().map(Dataset::Raster);
        }
        let ext = extension(name).unwrap_or_default();
        if bytes.starts_with(b"PMTiles") || ext == "mbtiles" {
            return crate::tiles::open_dataset_bytes(bytes, name, options);
        }
        if (ext == "mvt" || ext == "gz" || ext == "pbf") && crate::tiles::is_mvt(&bytes) {
            let layers = crate::tiles::vector::read_tile_file(&bytes, name, &options.tiles)?;
            return Ok(Dataset::Vector(crate::dataset::finish_vector(layers, name, VectorDriver::MVT)));
        }
        if bytes.starts_with(b"SQLite format 3\0") {
            return crate::drivers::gpkg::open_bytes(bytes, name, options);
        }
        if crate::drivers::jp2::is_jpeg2000(&bytes) {
            return crate::drivers::jp2::read_bytes(&bytes).map(Dataset::Raster);
        }
        if is_zip(&bytes) && ext != "kmz" {
            return open_zip(&bytes, name, options);
        }
        if RasterDriver::for_extension(name) == Some(RasterDriver::AAIGrid) {
            let raster = wbraster::formats::esri_ascii::parse(Cursor::new(&bytes), name)?;
            return Ok(Dataset::Raster(RasterDataset { raster, driver: RasterDriver::AAIGrid, has_nodata: true }));
        }
        let driver = vector_driver(&bytes, name).ok_or_else(|| Error::Unsupported(format!("'{name}': format not recognized")))?;
        let Some(layers) = crate::drivers::vector::read_bytes(bytes, name, driver, options)? else {
            return Err(Error::Unsupported(format!(
                "the {} format cannot be read from memory in this build (in memory: GeoTIFF, AAIGrid, GeoPackage, JPEG 2000, GeoJSON, \
                 TopoJSON, FlatGeobuf, CSV, GeoParquet, KML, KMZ, GML, GPX, OSM PBF, and Shapefile and MapInfo in zip \
                 archives)",
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
            let layers = crate::drivers::vector::read_parts(&member, &main, driver, &sidecar)?;
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
    pub(crate) fn write_raster(ds: &RasterDataset, driver: RasterDriver, options: &RasterWriteOptions, table: &str) -> Result<Vec<u8>> {
        use crate::drivers::gtiff_write::{Compression, TiffOptions, encode};
        match driver {
            RasterDriver::GTiff | RasterDriver::Cog => {
                let cog = driver == RasterDriver::Cog;
                let name = options.compression.as_deref().unwrap_or(if cog { "LZW" } else { "NONE" });
                let compression = Compression::parse(name).ok_or_else(|| {
                    Error::Unsupported(format!("{name} compression cannot be written in this build (NONE, LZW, DEFLATE, PACKBITS, ZSTD)"))
                })?;
                let opts = TiffOptions {
                    compression,
                    tile_size: options.tile_size,
                    bigtiff: options.bigtiff,
                    cog,
                    nodata: ds.nodata(),
                    ..Default::default()
                };
                encode(&ds.raster, &opts)
            }
            RasterDriver::AAIGrid => {
                let mut out = Vec::new();
                wbraster::formats::esri_ascii::write_to(&mut out, &ds.raster)?;
                Ok(out)
            }
            RasterDriver::GPKG => crate::drivers::gpkg::to_bytes(&ds.raster, ds.nodata(), table, options.compression.as_deref()),
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
            RasterDriver::MBTiles | RasterDriver::PMTiles => {
                let kind =
                    if driver == RasterDriver::MBTiles { crate::tiles::ArchiveKind::MBTiles } else { crate::tiles::ArchiveKind::PMTiles };
                crate::tiles::raster_to_bytes(ds, &crate::tiles::raster_write_options(options)?, table, kind)
            }
            d => Err(Error::Unsupported(format!(
                "the {} format cannot be written to memory in this build (GTiff, COG, AAIGrid, GPKG, JP2, MBTiles, PMTiles)",
                d.name()
            ))),
        }
    }

    /// Writes layers to bytes with the in-memory codecs.
    pub(crate) fn write_vector(layers: &[Layer], driver: VectorDriver, options: &VectorWriteOptions) -> Result<Vec<u8>> {
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
    #[cfg(not(target_arch = "wasm32"))]
    fn native_bytes_round_trip() {
        let ds = raster(2, DataType::I32);
        let bytes = write_raster_bytes(&ds, "a.tif", &Default::default()).unwrap();
        let back = open_raster_bytes(bytes, "a.tif").unwrap();
        assert_eq!(back.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
        // Shapefiles come back as a zip archive of their files.
        let gj = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"id":7},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let v = open_vector_bytes(gj.as_bytes().to_vec(), "p.geojson", &Default::default()).unwrap();
        let shp = write_vector_bytes(&v.layers, "p.shp", &Default::default()).unwrap();
        let zip = zip::ZipArchive::new(std::io::Cursor::new(shp)).unwrap();
        let mut names: Vec<String> = zip.file_names().map(|n| n.unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["p.cpg", "p.dbf", "p.prj", "p.shp", "p.shx"]);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn multi_file_formats_from_zip_bytes() {
        let gj = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"id":7,"name":"Zürich"},"geometry":{"type":"Point","coordinates":[8.5,47.4]}},
            {"type":"Feature","properties":{"id":8,"name":"b"},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
        let v = open_vector_bytes(gj.as_bytes().to_vec(), "p.geojson", &Default::default()).unwrap();
        for name in ["p.shp", "p.mif"] {
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
