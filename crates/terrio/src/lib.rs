//! Pure-Rust geospatial raster and vector data I/O.
//!
//! terrio opens rasters and vector datasets in many formats through one API, converts between
//! them, and reprojects. Format codecs come from the `wbraster`, `wbvector` and `wbprojection`
//! crates; terrio adds format detection, remote and archived inputs, fixes for what those drivers
//! miss, and GDAL-style operations.
//!
//! ```no_run
//! let ds = terrio::open("countries.gpkg")?;
//! let summary = terrio::info::info(&ds, &Default::default());
//! println!("{}", summary.to_text());
//! # Ok::<(), terrio::Error>(())
//! ```

mod cog;
pub mod crs;
mod dataset;
mod drivers;
mod error;
pub mod formats;
mod geotiff;
#[cfg(not(target_arch = "wasm32"))]
pub mod http;
pub mod info;
pub mod memory;
pub mod ops;
#[cfg(not(target_arch = "wasm32"))]
pub mod s3;
pub mod vector;

/// Raster types and resampling methods.
pub mod raster {
    pub use wbraster::{ReprojectOptions, ResampleMethod};
}
pub mod vfs;

pub use cog::{LazyRaster, Window, WindowOptions, open_raster_lazy};
pub use dataset::{
    Dataset, RasterDataset, RasterWriteOptions, VectorDataset, VectorOpenOptions, VectorWriteOptions, geo_transform, list_layers, open,
    open_raster, open_vector, write_raster, write_raster_dataset, write_vector,
};
pub use drivers::vector::csv::{CsvGeometry, CsvOptions};
pub use drivers::vector::geojson::GeoJsonOptions;
pub use error::{Error, Result};

/// The raster type: pixels, georeferencing and CRS, in memory.
pub use wbraster::{DataType, Raster, RasterConfig};
/// The vector types: layers of features with geometries and attributes.
pub use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// The version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
