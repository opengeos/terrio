//! The subcommands.

pub mod calc;
pub mod catalog;
pub mod convert;
pub mod crs;
pub mod dem;
pub mod fill_nodata;
pub mod filter;
pub mod formats;
pub mod grid;
pub mod info;
pub mod mosaic;
pub mod multidim;

pub mod pansharpen;
pub mod points;
pub mod polygonize;
pub mod processing;
pub mod proximity;
pub mod rasterize;
pub mod rasterout;
pub mod sieve;
pub mod translate;
pub mod vector_geom;
pub mod vector_opts;
pub mod viewshed;
pub mod vsi;
pub mod warp;

/// Runs `f` with standard output discarded, so that drivers' diagnostic prints do not mix with the
/// command's output.
pub fn quiet<T>(f: impl FnOnce() -> T) -> T {
    let _gag = gag::Gag::stdout().ok();
    f()
}

/// The input path to open with `driver`: `gdal` (any case) forces the GDAL backend, which needs
/// a build with the `gdal` feature; other names leave the path to terrio's format detection.
pub fn with_driver(input: &str, driver: Option<&str>) -> terrio::Result<String> {
    match driver {
        Some(d) if d.eq_ignore_ascii_case("gdal") => {
            if cfg!(feature = "gdal") {
                Ok(format!("GDAL:{input}"))
            } else {
                Err(terrio::Error::Unsupported("this terrio was built without the GDAL backend (the `gdal` feature)".into()))
            }
        }
        _ => Ok(input.to_string()),
    }
}
