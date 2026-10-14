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

pub mod crs;
mod dataset;
mod error;
pub mod formats;
mod geotiff;
pub mod info;
pub mod vector;

/// Raster types and resampling methods.
pub mod raster {
    pub use wbraster::{ReprojectOptions, ResampleMethod};
}
pub mod vfs;

pub use dataset::{
    Dataset, RasterDataset, RasterWriteOptions, VectorDataset, VectorOpenOptions, VectorWriteOptions, geo_transform, list_layers, open,
    open_raster, open_vector, write_raster, write_vector,
};
pub use error::{Error, Result};

/// The raster type: pixels, georeferencing and CRS, in memory.
pub use wbraster::{DataType, Raster, RasterConfig};
/// The vector types: layers of features with geometries and attributes.
pub use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// The version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
