//! Metadata-only RPF TOC bindings for native directories or named frame bytes.
use crate::{Detach, PyResultExt};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use terrio::ops::rpftoc::{self, RpfTocOptions};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(rpftoc_create, m)?)?;
    m.add_function(wrap_pyfunction!(rpftoc_bytes, m)?)
}
fn options(scale: Option<u32>, producer_id: &str, producer_name: &str, country_code: &str, classification: char) -> RpfTocOptions {
    RpfTocOptions {
        scale,
        producer_id: producer_id.into(),
        producer_name: producer_name.into(),
        country_code: country_code.into(),
        classification,
    }
}
/// Recursively index existing CADRG frames, returning counts and metadata bytes read.
/// The output defaults to input/A.TOC; its parent must be input or an ancestor.
#[pyfunction]
#[pyo3(signature=(input, output=None, *, scale=None, producer_id="TERRI", producer_name="Terrio", country_code="", classification='U', overwrite=false))]
#[allow(clippy::too_many_arguments)]
fn rpftoc_create<'py>(
    py: Python<'py>,
    input: std::path::PathBuf,
    output: Option<std::path::PathBuf>,
    scale: Option<u32>,
    producer_id: &str,
    producer_name: &str,
    country_code: &str,
    classification: char,
    overwrite: bool,
) -> PyResult<Bound<'py, PyAny>> {
    #[cfg(not(target_family = "wasm"))]
    {
        let options = options(scale, producer_id, producer_name, country_code, classification);
        let report = py.detach_result(|| rpftoc::create(&input, output.as_deref(), &options, overwrite)).py()?;
        py.import("json")?.call_method1("loads", (serde_json::to_string(&report).map_err(|e| crate::invalid(e.to_string()))?,))
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = (py, input, output, scale, producer_id, producer_name, country_code, classification, overwrite);
        Err(pyo3::exceptions::PyNotImplementedError::new_err("use rpftoc_bytes with uploaded frames in WebAssembly"))
    }
}
/// Encode A.TOC from (relative filename, frame bytes) pairs, without decoding pixels.
/// Keep the frame files at the same relative paths beside the returned TOC.
#[pyfunction]
#[pyo3(signature=(frames, *, scale=None, producer_id="TERRI", producer_name="Terrio", country_code="", classification='U'))]
fn rpftoc_bytes<'py>(
    py: Python<'py>,
    frames: Vec<(String, Py<PyBytes>)>,
    scale: Option<u32>,
    producer_id: &str,
    producer_name: &str,
    country_code: &str,
    classification: char,
) -> PyResult<Bound<'py, PyBytes>> {
    let options = options(scale, producer_id, producer_name, country_code, classification);
    if frames.len() > 100_000 {
        return Err(crate::invalid("at most 100000 frames are accepted"));
    }
    // Borrow only the required metadata ranges from uploaded Python bytes.
    let frames = frames
        .iter()
        .map(|(name, bytes)| rpftoc::read_frame(name, &mut std::io::Cursor::new(bytes.bind(py).as_bytes())))
        .collect::<terrio::Result<Vec<_>>>()
        .py()?;
    let bytes = py.detach_result(|| rpftoc::encode(&frames, &options)).py()?;
    Ok(PyBytes::new(py, &bytes))
}
