//! The `terrio` command line, for the Python package's `terrio` command (`terrio.__main__`).

use pyo3::prelude::*;
use std::ffi::OsString;

/// Runs the `terrio` command line `args` (the program name first) and returns its exit code.
/// `terrio` is the command line that runs terrio again (`python -m terrio`), which `terrio mcp`
/// runs its tools with.
#[pyfunction]
#[pyo3(signature = (args, terrio = None))]
fn cli_main(py: Python<'_>, args: Vec<OsString>, terrio: Option<Vec<OsString>>) -> i32 {
    py.detach(|| terrio_cli::run_as(args, terrio))
}

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(cli_main, m)?)
}
