//! Pure-Rust geospatial raster, vector and point cloud data I/O.
//!
//! terrio opens rasters, vector datasets and point clouds in many formats through one API,
//! converts between them, reprojects, and runs GDAL-style operations, without C dependencies: it
//! builds anywhere Rust does, WebAssembly included. Format codecs come from the `wbraster`,
//! `wbvector`, `wbprojection` and `laz` crates; terrio adds format detection, remote and archived
//! inputs, its own drivers where those libraries are wrong or missing, and the operations. Results
//! are compared with GDAL's on real-world files.
//!
//! # A tour
//!
//! ## Opening, summarizing and writing
//!
//! [`open`] detects the format and returns a [`Dataset`], a raster or a vector dataset read into
//! memory; [`open_raster`] and [`open_vector`] return one kind. Paths can be local files, URLs,
//! `s3://`, `gs://` and `az://` objects or zip archive members ([`vfs`]). [`info`] summarizes a
//! dataset as `gdalinfo` and `ogrinfo` do.
//!
//! ```no_run
//! let ds = terrio::open("countries.gpkg")?;
//! println!("{}", terrio::info::info(&ds, &Default::default()).to_text());
//!
//! let dem = terrio::open_raster("dem.tif")?;
//! let options = terrio::RasterWriteOptions {
//!     driver: Some(terrio::formats::RasterDriver::Cog),
//!     compression: Some("DEFLATE".into()),
//!     ..Default::default()
//! };
//! terrio::write_raster_dataset(&dem, "dem_cog.tif", &options)?;
//!
//! let countries = terrio::open_vector("countries.gpkg", &Default::default())?;
//! terrio::write_vector(&countries.layers, "countries.fgb", &Default::default())?;
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! ## Reading windows lazily
//!
//! [`open_raster_lazy`] opens a GeoTIFF or COG, local or remote, by its headers; a
//! [`LazyRaster::read_window`] decodes only the tiles it needs, fetched with HTTP range requests.
//! [`LazyRaster::read_window_f32`] reads a window as `f32` values instead, NaN where GDAL's mask
//! band marks pixels invalid (their nodata value, or the raster's mask), scales and offsets
//! applied on request ([`FloatReadOptions`]). [`LazyRaster::read_resampled`] reads a window into
//! an output of another size as GDAL's `RasterIO` does, from the overview it would choose
//! ([`ResampledReadOptions`]).
//!
//! ```no_run
//! let mut cog = terrio::open_raster_lazy("https://example.com/cog.tif")?;
//! let options = terrio::WindowOptions { bands: vec![1], overview: Some(0) };
//! let tile = cog.read_window(terrio::Window::new(0, 0, 512, 512), &options)?;
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! ## Operations
//!
//! [`ops`] holds the operations behind the command-line tool, each with an options struct named
//! after the GDAL utility's options: [`ops::warp`], [`ops::translate`], [`ops::mosaic`],
//! [`ops::rasterize`], [`ops::polygonize`], [`ops::dem`], [`ops::calc`], [`ops::proximity`],
//! [`ops::sieve`], [`ops::geometry`] (centroids, singleparts and vertices), [`ops::footprint`],
//! [`ops::sample`], [`ops::contour`], [`ops::zonal`] and the vector filters of `ogr2ogr`
//! ([`ops::vector_filter`], [`ops::sql::query`]).
//!
//! ```no_run
//! use terrio::crs::SpatialRef;
//! use terrio::ops::{Resampling, dem, warp};
//!
//! let dem = terrio::open_raster("dem.tif")?;
//! let options = warp::WarpOptions {
//!     dst_srs: Some(SpatialRef::parse("EPSG:32613")?),
//!     resolution: Some((30.0, 30.0)),
//!     resampling: Resampling::Bilinear,
//!     ..Default::default()
//! };
//! let utm = warp::warp(&[&dem], &options)?;
//! let shade = dem::hillshade(&utm, &Default::default())?;
//! terrio::write_raster_dataset(&shade, "hillshade.tif", &Default::default())?;
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! ## Coordinate reference systems
//!
//! [`crs::SpatialRef`] parses EPSG codes, WKT and PROJ strings; [`crs::Transformer`] transforms
//! coordinates as GDAL does, picking the datum transformation GDAL picks.
//!
//! ```no_run
//! use terrio::crs::{SpatialRef, Transformer};
//!
//! let wgs84 = SpatialRef::parse("EPSG:4326")?;
//! let utm = SpatialRef::parse("EPSG:32613")?;
//! let (x, y) = Transformer::new(&wgs84, &utm)?.transform(-105.5, 39.5)?;
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! ## Bytes in memory
//!
//! [`memory`] opens datasets from bytes and writes them to bytes, for data already in memory and
//! for WebAssembly, which has no file system.
//!
//! ```no_run
//! let bytes = std::fs::read("dem.tif")?;
//! let dem = terrio::memory::open_raster_bytes(bytes, "dem.tif")?;
//! let asc = terrio::memory::write_raster_bytes(&dem, "dem.asc", &Default::default())?;
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! ## Point clouds
//!
//! [`pointcloud`] reads LAS, LAZ and COPC into columns (COPC in place, by bounds and octree
//! level), writes them, and grids them into rasters.
//!
//! ```no_run
//! use terrio::pointcloud::{self, PointFilter, PointReadOptions, grid};
//!
//! let filter = PointFilter { classes: vec![2], ..Default::default() };
//! let ground = pointcloud::read_points("tile.laz", &PointReadOptions { filter, ..Default::default() })?;
//! let dtm = grid::grid(&ground, &grid::GridOptions { resolution: 1.0, ..Default::default() })?;
//! terrio::write_raster_dataset(&dtm, "dtm.tif", &Default::default())?;
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! # The data model
//!
//! Rasters are [`wbraster::Raster`]s (re-exported as [`Raster`]): band-major typed pixels with
//! georeferencing and a CRS, wrapped in a [`RasterDataset`] that records the driver and whether
//! the nodata value is declared. Vector layers are [`wbvector::Layer`]s (re-exported as
//! [`Layer`]), with [`Feature`]s, a field schema and [`Geometry`]s. These types belong to
//! terrio's public API: use them through terrio's re-exports (here, [`raster`] and [`vector`])
//! rather than depending on `wbraster` or `wbvector` directly, so that the versions always match.
//! A breaking release of either crate that terrio adopts is a breaking release of terrio.
//!
//! ## Raster buffer layout
//!
//! [`Raster::from_data_native`] takes complete band planes in order, each row-major from the
//! top: `data[band * (rows * cols) + row * cols + col]` for zero-based indices. Convert
//! pixel-interleaved RGB/RGBA buffers to planes before passing them to the constructor.
//! [`LazyRaster::read_window`] and floating-point window reads return the same layout, using
//! the returned window's dimensions and requested band order, including at overview levels.
//! [`ops::warp::warp`] borrows band-major source rasters and returns a band-major raster; no
//! reordering is needed between a window read and a warp. [`LazyRaster::read_window`] does not
//! retain the dataset mask; read it separately with [`LazyRaster::read_mask`] when needed.
//! Floating-point window reads apply the mask as NaN. A single band uses
//! `data[row * cols + col]`, without transposing.
//!
//! ```rust
//! use terrio::{DataType, Raster, RasterConfig, RasterDataset, raster::RasterData};
//!
//! let (cols, rows, bands) = (2, 2, 4);
//! let rgba: Vec<u8> = vec![
//!     1, 11, 21, 31, 2, 12, 22, 32, // top row: two RGBA pixels
//!     3, 13, 23, 33, 4, 14, 24, 34, // bottom row
//! ];
//! let pixels = rows * cols;
//! let mut planes = vec![0; pixels * bands];
//! for pixel in 0..pixels {
//!     for band in 0..bands {
//!         planes[band * pixels + pixel] = rgba[pixel * bands + band];
//!     }
//! }
//! assert_eq!(planes, vec![1, 2, 3, 4, 11, 12, 13, 14, 21, 22, 23, 24, 31, 32, 33, 34]);
//! let config = RasterConfig { cols, rows, bands, data_type: DataType::U8, ..Default::default() };
//! let raster = Raster::from_data_native(config, RasterData::U8(planes))?;
//! let RasterData::U8(values) = &raster.data else { panic!("expected U8 pixels") };
//! assert_eq!(values.as_slice(), &[1, 2, 3, 4, 11, 12, 13, 14, 21, 22, 23, 24, 31, 32, 33, 34]);
//! let ds = RasterDataset::new(raster, false);
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! TIFF storage is independent of this buffer layout. The native GTiff writer defaults to
//! pixel-interleaved storage (`INTERLEAVE=PIXEL`, `PlanarConfiguration=1`); request separate
//! planes for multiband rasters with `INTERLEAVE=BAND` (`PlanarConfiguration=2`) in
//! [`RasterWriteOptions::creation_options`]. Single-band output uses `PlanarConfiguration=1`
//! with either option. Both take band-major buffers.
//!
//! # Features
//!
//! Every driver is on by default. With `default-features = false`, terrio keeps its core: GeoTIFF
//! and COG, the raw and ASCII rasters, PNG, JPEG, VRT, STAC, GeoJSON, TopoJSON, Shapefile,
//! KML/KMZ, GML, GPX, CSV and MapInfo, the CRS engine, the operations and the streams. The other
//! drivers are features:
//!
//! - `geotiff`: GeoTIFF and COG, always built (the feature names what a build needs:
//!   `features = ["geotiff"]` is a COG reader and writer);
//! - `parquet`: GeoParquet, the [`arrow`] module and Arrow streams;
//! - `remote`: URLs, `s3://`, `gs://` and `az://` objects, read whole or by range (the [`http`]
//!   and [`s3`] modules), and datum grids from the PROJ CDN; native builds only;
//! - `netcdf-hdf5`: NetCDF, HDF5 and HDF-EOS;
//! - `sqlite`: GeoPackage and SQLite/SpatiaLite;
//! - `tiles`: MBTiles, PMTiles and MVT ([`tiles`]); `avif`: their AVIF tiles;
//! - `zarr`, `jp2` (JPEG 2000), `grib` (GRIB1 and GRIB2), `flatgeobuf`, `osm` (OpenStreetMap);
//! - `xlsx`: XLSX and XLSM workbooks, in pure Rust;
//! - `cad`: DWG and DXF drawings, in pure Rust;
//! - `hecras`: HEC-RAS geometry and plan HDF5 files as vector layers (needs `netcdf-hdf5`);
//! - `pointcloud`: LAS, LAZ, COPC, EPT, PLY and E57 ([`pointcloud`]);
//! - `lerc` and `lzma`: GeoTIFFs with those compressions.
//!
//! The drivers a build leaves out stay in [`formats::RasterDriver`] and
//! [`formats::VectorDriver`], listed as neither read nor written: opening or writing their files
//! fails with [`Error::Unsupported`] ("built without the `zarr` feature").
//!
//! - `libzstd` (off by default): Zstandard through the zstd C library natively, for faster decoding
//!   and higher compression levels than the default pure-Rust `ruzstd`.
//! - `gdal` (off by default): the optional GDAL backend (`gdal_backend`), which opens the formats
//!   terrio has no driver for through GDAL, and writes rasters with GDAL's drivers. It is the only
//!   C dependency, opt-in: GDAL must be installed.
//! - `ndarray` (off by default): raster pixels as `ndarray` arrays, without copying them. Views of
//!   a raster's pixels, of shape `(bands, rows, cols)` or of one band, borrowed or mutable
//!   (`RasterArrayExt`); moves of a dataset's pixels into an owned array and back
//!   (`RasterDataset::into_array3` and `RasterDataset::from_array3`); the masks of lazy reads as
//!   arrays (`LazyRaster::read_mask_array2`); floating-point windows, NaN where pixels are
//!   invalid, as owned arrays, bands first or last (`LazyRaster::read_window_array3`). The crate is
//!   re-exported as `terrio::ndarray`.
//!
//! # Minimum supported Rust version
//!
//! Rust 1.90 (the `rust-version` in `Cargo.toml`, checked in CI). The minimum is raised only when
//! a dependency or a language feature needs it, never to a release less than six months old, and
//! only in a minor release (before 1.0, in any release, noted in the changelog).

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Builds without some drivers leave helpers that only those drivers use.
#![cfg_attr(
    not(all(
        feature = "parquet",
        feature = "remote",
        feature = "avif",
        feature = "netcdf-hdf5",
        feature = "osm",
        feature = "lzma",
        feature = "flatgeobuf",
        feature = "jp2",
        feature = "lerc",
        feature = "pointcloud",
        feature = "tiles",
        feature = "zarr",
        feature = "sqlite"
    )),
    allow(dead_code)
)]

#[cfg(feature = "parquet")]
#[cfg_attr(docsrs, doc(cfg(feature = "parquet")))]
pub mod arrow;
#[cfg(feature = "avif")]
mod avif;
pub mod block_cache;
mod civil_time;
mod cog;
mod cog_validate;
pub mod config;
pub mod crs;
mod dataset;
pub mod dataset_files;
mod declared;
mod deflate_compress;
mod drivers;
#[cfg(test)]
mod entry_allow_list_tests;
mod error;
mod float_read;
pub mod formats;
#[cfg(feature = "hecras")]
#[cfg_attr(docsrs, doc(cfg(feature = "hecras")))]
pub use drivers::hecras;
#[cfg(feature = "gdal")]
#[cfg_attr(docsrs, doc(cfg(feature = "gdal")))]
pub mod gdal_backend;
mod geotiff;
pub mod gpkg_maintenance;
mod gpkg_validate;
#[cfg(feature = "sqlite")]
mod gpkg_validation;
pub use gpkg_validate::{GpkgRequirement, GpkgValidation, GpkgValidationOptions, GpkgViolation, validate_gpkg, validate_gpkg_bytes};
mod histogram;
#[cfg(remote)]
#[cfg_attr(docsrs, doc(cfg(feature = "remote")))]
pub mod http;
pub mod info;
pub mod inspection;
mod jpeg;
mod memfs;
pub mod memory;
pub mod multidim;
#[cfg(feature = "ndarray")]
mod ndarray_ext;
pub mod ops;
mod par;
pub mod parquet_metadata;
mod pixel;
#[cfg(feature = "pointcloud")]
#[cfg_attr(docsrs, doc(cfg(feature = "pointcloud")))]
pub mod pointcloud;
mod resampled_read;
#[cfg(remote)]
#[cfg_attr(docsrs, doc(cfg(feature = "remote")))]
pub mod s3;
pub mod sparse;
mod tiff_codecs;
pub mod tiles;
pub mod vector;
mod zstandard;

/// Raster types from `wbraster`, terrio's raster data model: pixel storage, CRS records and
/// reprojection options.
pub mod raster {
    pub use wbraster::raster::{BandView, RasterData};
    pub use wbraster::{CrsInfo, RasterError, ReprojectOptions, ResampleMethod};
}
pub mod catalog;
pub mod check;
pub mod stac;
mod stacta;
pub mod stream;
mod tiff_mask;
pub mod vfs;
pub mod vsi;
pub mod warnings;
/// Spreadsheet workbooks laid out for people: sheets, cell formats, merged ranges and print setup.
#[cfg(feature = "xlsx")]
pub mod xlsx {
    pub use crate::drivers::vector::xlsx_workbook::{
        Align, BorderStyle, CellValue, Format, Orientation, VerticalAlign, Workbook, Worksheet,
    };
}

pub use cog::{LazyRaster, TiffReadLimits, Window, WindowOptions, open_raster_lazy};
pub use cog_validate::{
    CogValidation, CogValidationOptions, CogValidationProfile, gdal_infers_cog_layout, validate_cog, validate_cog_bytes,
    validate_cog_with_options,
};
/// The memory the process can use, which reads refuse windows past.
pub use dataset::fallible::usable_memory;
pub use dataset::{
    BandMetadata, Dataset, Gcp, Geolocation, RasterDataset, RasterWriteOptions, VectorDataset, VectorOpenOptions, VectorWriteOptions,
    geo_transform, is_north_up, list_layers, open, open_raster, open_raster_with_tiff_limits, open_vector, read_gcps, write_netcdf_groups,
    write_raster, write_raster_dataset, write_vector,
};
pub use drivers::gtiff_write::GeoTiffWriter;
pub use drivers::vector::citygml_write::{CityGmlLod0Surface, CityGmlVersion};
pub use drivers::vector::cityjson_write::CityJsonVersion;
pub use drivers::vector::csv::{CsvGeometry, CsvOptions};
pub use drivers::vector::geojson::{GeoJsonIdType, GeoJsonOptions};
pub use drivers::vector::hecras_options::HecRasOpenOptions;
pub use drivers::vector::kml::KmzOptions;
pub use drivers::vector::shapefile::{OrganizePolygons, ShapefileOptions};
pub use drivers::vector::wfs::WfsOpenOptions;
pub use drivers::vector::xlsx_options::{XlsxColumnWidth, XlsxFieldTypes, XlsxHeaders, XlsxOptions};
pub use error::{Error, Result};
pub use float_read::FloatReadOptions;
/// The `ndarray` crate, of the version terrio's array views of raster pixels use.
#[cfg(feature = "ndarray")]
#[cfg_attr(docsrs, doc(cfg(feature = "ndarray")))]
pub use ndarray;
pub use resampled_read::ResampledReadOptions;
// Raster pixels as `ndarray` arrays, without copying them.
#[cfg(feature = "ndarray")]
#[cfg_attr(docsrs, doc(cfg(feature = "ndarray")))]
pub use ndarray_ext::{ArrayError, BandLayout, FloatElement, RasterArrayExt, RasterElement};

/// The raster type: pixels, georeferencing and CRS, in memory.
pub use wbraster::{DataType, Raster, RasterConfig};
// Building a `Raster` from its pixels without a nodata fill.
pub use pixel::{complex_components_from_iter, raster_from_data};
/// The vector types: layers of features with geometries and attributes.
pub use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// The system allocator, ignoring the release of null pointers.
///
/// OpenJPEG's Rust port, terrio's JPEG 2000 codec (the `jp2` feature), releases code-block
/// buffers it never allocated: null pointers. The C allocators of native builds ignore those; the
/// allocator of `wasm32-unknown-unknown` does not, and traps. Programs built for WebAssembly that
/// read or write JPEG 2000 make this their global allocator (terrio's npm package does); builds
/// without `jp2` need neither it nor the link flag below:
///
/// ```
/// #[global_allocator]
/// static ALLOCATOR: terrio::NullSafeAllocator = terrio::NullSafeAllocator;
/// ```
///
/// Their link also needs `-C link-arg=--allow-undefined` (see `.cargo/config.toml` in terrio's
/// repository): the port is built as a C library too, which leaves the C allocator undefined.
pub struct NullSafeAllocator;

// SAFETY: every call goes to the system allocator, except releases of null pointers, which
// allocate nothing.
unsafe impl std::alloc::GlobalAlloc for NullSafeAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: as the caller guarantees.
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: as the caller guarantees.
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        if !ptr.is_null() {
            // SAFETY: a pointer this allocator returned, as the caller guarantees.
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as the caller guarantees.
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

/// The version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The thread pool of WebAssembly builds with threads: see [`par::set_thread_pool`].
#[cfg(all(threads, target_family = "wasm"))]
pub use par::set_thread_pool;

/// Portable OGC API Features reader and native open options.
pub use drivers::vector::oapif;
pub use drivers::vector::oapif::OapifOpenOptions;

/// This process's id, for naming temporary files. 0 on WASI, which has no process ids
/// (`std::process::id` panics there): the browser build of the command line runs one program at a
/// time, so no other terrio process writes the same names. Most callers also add a counter or
/// create the file only if new; a name that does neither (netCDF's `.nc.tmp-0`) replaces a file
/// already called that.
#[cfg_attr(not(fs), allow(dead_code))]
pub(crate) fn process_id() -> u32 {
    #[cfg(target_os = "wasi")]
    return 0;
    #[cfg(not(target_os = "wasi"))]
    std::process::id()
}
