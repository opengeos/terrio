//! The `terrio` command line, for the Python package's `terrio` command (`terrio.__main__`).

use pyo3::prelude::*;
use std::ffi::OsString;

/// Runs the `terrio` command line `args` (the program name first) and returns its exit code.
#[pyfunction]
fn cli_main(py: Python<'_>, args: Vec<OsString>) -> i32 {
    py.detach(|| terrio_cli::run(args))
}

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(cli_main, m)?)
}
