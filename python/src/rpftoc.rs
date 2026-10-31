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
/// Directory trees must remain stable; atomic overwrite preserves Unix mode bits,
/// not ownership, extended ACLs or extended attributes.
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
        let result = pyo3::types::PyDict::new(py);
        result.set_item("frames", report.frames)?;
        result.set_item("skipped_files", report.skipped_files)?;
        result.set_item("groups", report.groups)?;
        result.set_item("metadata_bytes", report.metadata_bytes)?;
        result.set_item("output_bytes", report.output_bytes)?;
        Ok(result.into_any())
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
    frames: &Bound<'py, PyAny>,
    scale: Option<u32>,
    producer_id: &str,
    producer_name: &str,
    country_code: &str,
    classification: char,
) -> PyResult<Bound<'py, PyBytes>> {
    let options = options(scale, producer_id, producer_name, country_code, classification);
    if frames.len()? > 100_000 {
        return Err(crate::invalid("at most 100000 frames are accepted"));
    }
    let frames: Vec<(String, Py<PyBytes>)> = frames.extract()?;
    // Immutable PyBytes owners remain alive while the GIL is released.
    let inputs: Vec<_> = frames.iter().map(|(name, bytes)| (name.as_str(), bytes.as_bytes(py))).collect();
    let bytes = py
        .detach_result(|| {
            let metadata = inputs
                .iter()
                .map(|(name, bytes)| rpftoc::read_frame(name, &mut std::io::Cursor::new(bytes)))
                .collect::<terrio::Result<Vec<_>>>()?;
            rpftoc::encode(&metadata, &options)
        })
        .py()?;
    Ok(PyBytes::new(py, &bytes))
}
