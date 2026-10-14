//! The subcommands.

pub mod convert;
pub mod crs;
pub mod formats;
pub mod info;
pub mod polygonize;
pub mod rasterize;
pub mod vector_opts;

/// Runs `f` with standard output discarded, so that drivers' diagnostic prints do not mix with the
/// command's output.
pub fn quiet<T>(f: impl FnOnce() -> T) -> T {
    let _gag = gag::Gag::stdout().ok();
    f()
}
