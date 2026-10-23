//! The optional GDAL backend, as Python sees it: its version, and where it finds its data files.

use pyo3::prelude::*;

/// The version of GDAL that terrio is built with, such as `"3.12.4"`, or `None` when it is built
/// without the optional GDAL backend (the `terrio` wheels on PyPI; `terrio-gdal` has it).
#[pyfunction]
fn gdal_version() -> Option<String> {
    #[cfg(feature = "gdal")]
    return Some(terrio::gdal_backend::gdal_version());
    #[cfg(not(feature = "gdal"))]
    None
}

/// Points GDAL at its data files, PROJ at its database and GDAL's HTTPS requests at a
/// certificate bundle; `None` leaves a setting as it is. Called on import by
/// `terrio._gdal_env.configure` with the files the `terrio-gdal` wheels bundle.
#[cfg(feature = "gdal")]
#[pyfunction]
#[pyo3(signature = (gdal_data=None, proj_data=None, ca_bundle=None))]
fn _set_gdal_data_paths(gdal_data: Option<&str>, proj_data: Option<&str>, ca_bundle: Option<&str>) -> PyResult<()> {
    use crate::PyResultExt;
    terrio::gdal_backend::set_data_paths(gdal_data, proj_data, ca_bundle).py()
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(gdal_version, m)?)?;
    #[cfg(feature = "gdal")]
    m.add_function(wrap_pyfunction!(_set_gdal_data_paths, m)?)?;
    Ok(())
}
