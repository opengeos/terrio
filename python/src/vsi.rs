//! File management on local, in-memory, remote and archive locations (`terrio vsi`, GDAL's
//! `gdal vsi`).

use crate::PyResultExt;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use terrio::vsi::{self, CopyOptions, Entry, ListOptions, Report, SyncOptions};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(vsi_list, m)?)?;
    m.add_function(wrap_pyfunction!(vsi_stat, m)?)?;
    m.add_function(wrap_pyfunction!(vsi_copy, m)?)?;
    m.add_function(wrap_pyfunction!(vsi_delete, m)?)?;
    m.add_function(wrap_pyfunction!(vsi_move, m)?)?;
    m.add_function(wrap_pyfunction!(vsi_sync, m)?)?;
    m.add_function(wrap_pyfunction!(vsi_read, m)?)?;
    m.add_function(wrap_pyfunction!(vsi_write, m)?)?;
    Ok(())
}

fn entry<'py>(py: Python<'py>, e: &Entry) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("name", &e.name)?;
    d.set_item("type", if e.is_dir() { "directory" } else { "file" })?;
    d.set_item("size", e.size)?;
    d.set_item("modified", e.modified)?;
    d.set_item("last_modification_date", e.modified_string())?;
    d.set_item("permissions", &e.permissions)?;
    d.set_item("etag", &e.etag)?;
    Ok(d)
}

fn report<'py>(py: Python<'py>, r: &Report) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("copied", r.copied)?;
    d.set_item("bytes", r.bytes)?;
    d.set_item("unchanged", r.unchanged)?;
    d.set_item("deleted", r.deleted)?;
    d.set_item("errors", r.errors.clone())?;
    Ok(d)
}

/// Lists a directory, or describes a file, as `gdal vsi list` does.
///
/// Locations: local paths, ``/vsimem/``, ``s3://`` (``/vsis3/``), ``gs://`` (``/vsigs/``),
/// ``az://`` (``/vsiaz/``), HTTP(S) directory index pages (``/vsicurl/``), and zip and tar
/// archives (``/vsizip/archive.zip/dir``, ``/vsitar/archive.tar.gz``), local or remote.
///
/// Args:
///     path: The directory or file.
///     recursive: List subdirectories too.
///     depth: With ``recursive``, the deepest level listed (1: the directory's own entries).
///     absolute: Name entries by their absolute paths.
///     long: Fetch sizes and dates a listing lacks (one request per file of an HTTP index).
///
/// Returns:
///     list[dict]: The entries, a directory before its contents: ``name`` (relative to
///     ``path``), ``type`` (``"file"`` or ``"directory"``), ``size``, ``modified`` (seconds
///     since the epoch), ``last_modification_date``, ``permissions`` and ``etag`` (``None``
///     when unknown).
///
/// Raises:
///     NotFoundError: If nothing is at ``path``.
#[pyfunction]
#[pyo3(signature = (path, *, recursive=false, depth=None, absolute=false, long=false))]
fn vsi_list<'py>(
    py: Python<'py>,
    path: &str,
    recursive: bool,
    depth: Option<usize>,
    absolute: bool,
    long: bool,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let options = ListOptions { recursive, depth, absolute, details: long };
    let entries = py.detach(|| vsi::list(path, &options)).py()?;
    entries.iter().map(|e| entry(py, e)).collect()
}

/// Describes one location (as ``vsi_list`` describes entries), or ``None`` when nothing is there.
///
/// Args:
///     path: The location.
///
/// Returns:
///     dict | None: The entry, named ``path``.
#[pyfunction]
fn vsi_stat<'py>(py: Python<'py>, path: &str) -> PyResult<Option<Bound<'py, PyDict>>> {
    let e = py.detach(|| vsi::stat(path)).py()?;
    e.map(|e| entry(py, &e)).transpose()
}

/// Copies a file, or a directory with ``recursive``, as `gdal vsi copy` does: into ``dst`` when
/// it is an existing directory, to ``dst`` otherwise. Copies within S3 are server-side.
///
/// Args:
///     src: The file or directory.
///     dst: The destination.
///     recursive: Copy a directory and its subdirectories.
///     skip_errors: Go on after a file fails (listed in the report's ``errors``).
///
/// Returns:
///     dict: ``copied`` (files), ``bytes``, ``unchanged``, ``deleted`` and ``errors``
///     (``(path, reason)`` pairs).
#[pyfunction]
#[pyo3(signature = (src, dst, *, recursive=false, skip_errors=false))]
fn vsi_copy<'py>(py: Python<'py>, src: &str, dst: &str, recursive: bool, skip_errors: bool) -> PyResult<Bound<'py, PyDict>> {
    let r = py.detach(|| vsi::copy(src, dst, &CopyOptions { recursive, skip_errors })).py()?;
    report(py, &r)
}

/// Deletes a file, or a directory and everything below it with ``recursive``.
///
/// Refuses empty paths, file system roots, the home directory, the current directory and its
/// parents, the root of ``/vsimem/``, whole buckets and containers, archives and HTTP locations.
/// Symbolic links are deleted, never followed; object store deletions are bounded by
/// ``prefix/``.
///
/// Args:
///     path: The file or directory.
///     recursive: Delete a directory's contents and the directory.
///
/// Returns:
///     dict: The report (``deleted``: files and directories deleted).
#[pyfunction]
#[pyo3(signature = (path, *, recursive=false))]
fn vsi_delete<'py>(py: Python<'py>, path: &str, recursive: bool) -> PyResult<Bound<'py, PyDict>> {
    let r = py.detach(|| vsi::delete(path, recursive)).py()?;
    report(py, &r)
}

/// Moves (renames) a file or directory, as `gdal vsi move` does. Across file systems or stores,
/// the source is deleted only once everything was copied.
///
/// Args:
///     src: The file or directory.
///     dst: An existing directory to move into, or the new name.
///
/// Returns:
///     dict: The report.
#[pyfunction]
fn vsi_move<'py>(py: Python<'py>, src: &str, dst: &str) -> PyResult<Bound<'py, PyDict>> {
    let r = py.detach(|| vsi::rename(src, dst)).py()?;
    report(py, &r)
}

/// Copies to ``dst`` the files of ``src`` that are missing or out of date there, as
/// `gdal vsi sync` does; nothing is deleted. A source directory ending with ``/`` has its
/// contents synchronized into ``dst``; without it, the directory itself (``dst/<name>``).
///
/// Args:
///     src: The source file or directory.
///     dst: The target.
///     recursive: Synchronize subdirectories too.
///     strategy: ``"timestamp"`` (size and date), ``"ETag"`` (entity tags, or a local file's MD5
///         against an S3 object's) or ``"overwrite"``.
///
/// Returns:
///     dict: The report (``copied`` and ``unchanged`` files).
#[pyfunction]
#[pyo3(signature = (src, dst, *, recursive=false, strategy="timestamp"))]
fn vsi_sync<'py>(py: Python<'py>, src: &str, dst: &str, recursive: bool, strategy: &str) -> PyResult<Bound<'py, PyDict>> {
    let options = SyncOptions { recursive, strategy: strategy.parse().py()? };
    let r = py.detach(|| vsi::sync(src, dst, &options)).py()?;
    report(py, &r)
}

/// Reads a whole file of any location (an archive member, an object, ``/vsimem/``...).
///
/// Args:
///     path: The file.
///
/// Returns:
///     bytes: Its contents.
#[pyfunction]
fn vsi_read<'py>(py: Python<'py>, path: &str) -> PyResult<Bound<'py, PyBytes>> {
    let data = py.detach(|| vsi::read(path)).py()?;
    Ok(PyBytes::new(py, &data))
}

/// Writes a file (local, ``/vsimem/``, S3 or Azure), replacing it. Local files are written to a
/// temporary file renamed into place.
///
/// Args:
///     path: The file.
///     data: Its contents.
#[pyfunction]
fn vsi_write(py: Python<'_>, path: &str, data: &[u8]) -> PyResult<()> {
    py.detach(|| vsi::write(path, data)).py()
}
