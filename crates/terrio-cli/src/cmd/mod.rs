//! The subcommands.

pub mod calc;
pub mod catalog;
pub mod compare;
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
pub mod pixel_info;
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
pub mod warp;
pub mod xyz;

thread_local! {
    /// The exit status of a command that succeeded, when it is not 0 (`terrio compare`'s
    /// difference count, `terrio pixel-info`'s locations off the raster).
    static EXIT_CODE: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Sets the exit status of the command running on this thread, should it succeed.
pub fn set_exit_code(code: i32) {
    EXIT_CODE.with(|c| c.set(code));
}

/// Takes the exit status set by [`set_exit_code`], resetting it to 0.
pub fn take_exit_code() -> i32 {
    EXIT_CODE.with(|c| c.replace(0))
}

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
