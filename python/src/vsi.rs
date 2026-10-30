//! File management on local, in-memory, remote and archive locations (`terrio vsi`, GDAL's
//! `gdal vsi`).

use crate::{Detach, PyResultExt};
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
    m.add_function(wrap_pyfunction!(sozip_create, m)?)?;
    m.add_function(wrap_pyfunction!(sozip_optimize, m)?)?;
    m.add_function(wrap_pyfunction!(sozip_list, m)?)?;
    m.add_function(wrap_pyfunction!(sozip_validate, m)?)?;
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
    let entries = py.detach_result(|| vsi::list(path, &options)).py()?;
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
    let e = py.detach_result(|| vsi::stat(path)).py()?;
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
    let r = py.detach_result(|| vsi::copy(src, dst, &CopyOptions { recursive, skip_errors })).py()?;
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
    let r = py.detach_result(|| vsi::delete(path, recursive)).py()?;
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
    let r = py.detach_result(|| vsi::rename(src, dst)).py()?;
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
    let r = py.detach_result(|| vsi::sync(src, dst, &options)).py()?;
    report(py, &r)
}

/// Reads a whole file of any location (an archive member, an object, ``/vsimem/``...).
///
/// Archive members are read within the limits a raster open keeps (``TERRIO_ZIP_MAX_ENTRIES``,
/// ``TERRIO_ZIP_MAX_SIZE``, ``TERRIO_ZIP_MAX_RATIO``), and a file more than the process can hold
/// is refused before it is read.
///
/// Args:
///     path: The file.
///
/// Returns:
///     bytes: Its contents.
#[pyfunction]
fn vsi_read<'py>(py: Python<'py>, path: &str) -> PyResult<Bound<'py, PyBytes>> {
    let data = py.detach_result(|| vsi::read(path)).py()?;
    // `MemoryError` when Python cannot allocate the copy.
    PyBytes::new_with(py, data.len(), |bytes| {
        bytes.copy_from_slice(&data);
        Ok(())
    })
}

/// Writes a file (local, ``/vsimem/``, S3 or Azure), replacing it. Local files are written to a
/// temporary file renamed into place.
///
/// Args:
///     path: The file.
///     data: Its contents.
#[pyfunction]
fn vsi_write(py: Python<'_>, path: &str, data: &[u8]) -> PyResult<()> {
    py.detach_result(|| vsi::write(path, data)).py()
}

/// A serializable value as Python objects (through JSON).
fn to_py_value<'py, T: serde::Serialize>(py: Python<'py>, value: &T) -> PyResult<Bound<'py, PyAny>> {
    let text = serde_json::to_string(value).map_err(|e| crate::invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}

#[allow(clippy::too_many_arguments)]
fn sozip_options(
    enable: &str,
    chunk_size: u32,
    min_file_size: u64,
    overwrite: bool,
    recursive: bool,
    junk_paths: bool,
    content_type: Option<String>,
) -> PyResult<vsi::sozip::SozipOptions> {
    if chunk_size == 0 {
        return Err(crate::invalid("chunk_size must not be 0"));
    }
    Ok(vsi::sozip::SozipOptions { enable: enable.parse().py()?, chunk_size, min_file_size, overwrite, recursive, junk_paths, content_type })
}

/// Creates a Seek-Optimized ZIP (SOZip) archive, or adds files to an existing one, as
/// `gdal vsi sozip create` does.
///
/// Args:
///     inputs: The files (or directories, with ``recursive``) to add.
///     output: The archive (a local path or ``/vsimem/``).
///     enable: ``"auto"`` (files of at least ``min_file_size``), ``"yes"`` or ``"no"``.
///     chunk_size: The uncompressed bytes of a chunk.
///     min_file_size: The smallest file seek-optimized in auto mode.
///     overwrite: Replace an existing archive rather than add to it.
///     recursive: Add the files of directories.
///     junk_paths: Store names without directories.
///     content_type: A ``Content-Type`` stored with each file.
///
/// Returns:
///     dict: ``added``, ``seek_optimized`` and ``bytes``.
#[pyfunction]
#[pyo3(signature = (inputs, output, *, enable="auto", chunk_size=32768, min_file_size=1048576, overwrite=false, recursive=false,
    junk_paths=false, content_type=None))]
#[allow(clippy::too_many_arguments)]
fn sozip_create<'py>(
    py: Python<'py>,
    inputs: Vec<String>,
    output: &str,
    enable: &str,
    chunk_size: u32,
    min_file_size: u64,
    overwrite: bool,
    recursive: bool,
    junk_paths: bool,
    content_type: Option<String>,
) -> PyResult<Bound<'py, PyAny>> {
    let options = sozip_options(enable, chunk_size, min_file_size, overwrite, recursive, junk_paths, content_type)?;
    let inputs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let r = py.detach_result(|| vsi::sozip::create(&inputs, output, &options)).py()?;
    to_py_value(py, &r)
}

/// Rewrites a zip archive as a SOZip archive, as `gdal vsi sozip optimize` does.
///
/// Args:
///     input: The zip archive.
///     output: The SOZip archive to write.
///     enable: ``"auto"``, ``"yes"`` or ``"no"``.
///     chunk_size: The uncompressed bytes of a chunk.
///     min_file_size: The smallest file seek-optimized in auto mode.
///     overwrite: Replace an existing output.
///
/// Returns:
///     dict: ``added``, ``seek_optimized`` and ``bytes``.
#[pyfunction]
#[pyo3(signature = (input, output, *, enable="auto", chunk_size=32768, min_file_size=1048576, overwrite=false))]
fn sozip_optimize<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    enable: &str,
    chunk_size: u32,
    min_file_size: u64,
    overwrite: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let options = sozip_options(enable, chunk_size, min_file_size, overwrite, false, false, None)?;
    let r = py.detach_result(|| vsi::sozip::optimize(input, output, &options)).py()?;
    to_py_value(py, &r)
}

/// Lists a zip archive's members with their SOZip status, as `gdal vsi sozip list` does.
///
/// Args:
///     input: The archive (local, ``/vsimem/`` or remote).
///
/// Returns:
///     list[dict]: ``name``, ``size``, ``compressed_size``, ``modified``, ``method``,
///     ``sozip_chunk_size`` (``None`` without a valid index) and ``properties``.
#[pyfunction]
fn sozip_list<'py>(py: Python<'py>, input: &str) -> PyResult<Bound<'py, PyAny>> {
    let members = py.detach_result(|| vsi::sozip::list(input)).py()?;
    to_py_value(py, &members)
}

/// Validates a zip archive and its SOZip indexes, as `gdal vsi sozip validate` does.
///
/// Args:
///     input: The archive.
///
/// Returns:
///     dict: ``valid``, ``sozip_files``, ``messages`` and ``errors``.
#[pyfunction]
fn sozip_validate<'py>(py: Python<'py>, input: &str) -> PyResult<Bound<'py, PyAny>> {
    let v = py.detach_result(|| vsi::sozip::validate(input)).py()?;
    to_py_value(py, &v)
}
