//! Configuration that GDAL keeps outside datasets: options scoped to a thread, options bound to
//! path prefixes, the drivers allowed to open files, and network statistics.

use crate::{Detach, PyResultExt};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::collections::HashMap;

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(config_push, m)?)?;
    m.add_function(wrap_pyfunction!(config_pop, m)?)?;
    m.add_function(wrap_pyfunction!(config_get, m)?)?;
    m.add_function(wrap_pyfunction!(sidecars_disabled, m)?)?;
    m.add_function(wrap_pyfunction!(set_path_specific_option, m)?)?;
    m.add_function(wrap_pyfunction!(clear_path_specific_options, m)?)?;
    m.add_function(wrap_pyfunction!(get_path_specific_option, m)?)?;
    m.add_function(wrap_pyfunction!(set_allowed_drivers, m)?)?;
    m.add_function(wrap_pyfunction!(allowed_drivers, m)?)?;
    m.add_function(wrap_pyfunction!(check_drivers, m)?)?;
    m.add_function(wrap_pyfunction!(canonical_driver_name, m)?)?;
    m.add_function(wrap_pyfunction!(identify_drivers, m)?)?;
    m.add_function(wrap_pyfunction!(network_stats, m)?)?;
    m.add_function(wrap_pyfunction!(reset_network_stats, m)?)?;
    m.add_function(wrap_pyfunction!(clear_http_cache, m)?)?;
    m.add_function(wrap_pyfunction!(block_cache_max, m)?)?;
    m.add_function(wrap_pyfunction!(set_block_cache_max, m)?)?;
    m.add_function(wrap_pyfunction!(clear_block_cache, m)?)?;
    m.add_function(wrap_pyfunction!(config_options, m)?)?;
    Ok(())
}

/// Pushes options for the calling thread (`None` unsets one): what `terrio.rio.Env` does.
#[pyfunction]
fn config_push(options: HashMap<String, Option<String>>) {
    terrio::config::push_scope(options);
}

/// Pops the calling thread's innermost options. Returns whether there were any.
#[pyfunction]
fn config_pop() -> bool {
    terrio::config::pop_scope()
}

/// The value of an option for the calling thread: its options, else the environment.
#[pyfunction]
fn config_get(key: &str) -> Option<String> {
    terrio::config::get(key)
}

/// Whether `TERRIO_DISABLE_READDIR_ON_OPEN` (or its other name, `GDAL_DISABLE_READDIR_ON_OPEN`)
/// is `EMPTY_DIR` for the calling thread, which asks that nothing next to a local dataset be read.
#[pyfunction]
fn sidecars_disabled() -> bool {
    terrio::config::sidecars_disabled()
}

/// Binds an option to the files whose names start with `prefix` (GDAL's
/// `VSISetPathSpecificOption`), or unbinds it when `value` is None. `GDAL_HTTP_HEADERS` set so is
/// sent only to that prefix, and never to another host a redirect leads to.
#[pyfunction]
#[pyo3(signature = (prefix, key, value=None))]
fn set_path_specific_option(prefix: &str, key: &str, value: Option<&str>) {
    #[cfg(not(target_arch = "wasm32"))]
    terrio::http::set_path_specific_option(prefix, key, value);
    #[cfg(target_arch = "wasm32")]
    let _ = (prefix, key, value);
}

/// Removes the options bound to `prefix`, or to every prefix (GDAL's
/// `VSIClearPathSpecificOptions`).
#[pyfunction]
#[pyo3(signature = (prefix=None))]
fn clear_path_specific_options(prefix: Option<&str>) {
    #[cfg(not(target_arch = "wasm32"))]
    terrio::http::clear_path_specific_options(prefix);
    #[cfg(target_arch = "wasm32")]
    let _ = prefix;
}

/// The option `key` bound to the file named `path` (GDAL's `VSIGetPathSpecificOption`), or None.
#[pyfunction]
fn get_path_specific_option(path: &str, key: &str) -> Option<String> {
    #[cfg(not(target_arch = "wasm32"))]
    return terrio::http::path_specific_option(path, key);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (path, key);
        None
    }
}

/// The kind of driver an allow-list applies to: `"raster"`, `"vector"`, `"pointcloud"`, or `None`
/// for every kind.
fn driver_kind(kind: Option<&str>) -> PyResult<Option<terrio::formats::Kind>> {
    match kind.map(str::to_ascii_lowercase).as_deref() {
        None => Ok(None),
        Some("raster") => Ok(Some(terrio::formats::Kind::Raster)),
        Some("vector") => Ok(Some(terrio::formats::Kind::Vector)),
        Some("pointcloud") => Ok(Some(terrio::formats::Kind::PointCloud)),
        Some(other) => Err(crate::invalid(format!("kind must be 'raster', 'vector', 'pointcloud' or None, not {other:?}"))),
    }
}

/// Restricts, process-wide, the drivers datasets of `kind` ("raster", "vector", "pointcloud", or
/// None for every kind) may be opened with (terrio or GDAL names). The first call for a kind sets
/// its list; later calls intersect with it, so they narrow it but never re-enable a driver, as
/// GDAL's de-registration lasts for the process. Lifting a list once set (`drivers=None`) raises
/// InvalidArgumentError.
#[pyfunction]
#[pyo3(signature = (drivers, kind=None))]
fn set_allowed_drivers(drivers: Option<Vec<String>>, kind: Option<&str>) -> PyResult<()> {
    let kind = driver_kind(kind)?;
    let names: Option<Vec<&str>> = drivers.as_ref().map(|d| d.iter().map(String::as_str).collect());
    terrio::formats::set_allowed_drivers(names.as_deref(), kind).py()
}

/// The drivers datasets of `kind` ("raster", "vector" or "pointcloud") may be opened with (upper
/// case), or None when nothing restricts them. For `kind=None` (a dataset whose kind is unknown
/// until it is opened: a raster or a vector, as point clouds are read by readers of their own), a
/// driver must be allowed by the list of each of those kinds it serves.
#[pyfunction]
#[pyo3(signature = (kind=None))]
fn allowed_drivers(kind: Option<&str>) -> PyResult<Option<Vec<String>>> {
    Ok(terrio::formats::allowed_drivers(driver_kind(kind)?))
}

/// Raises UnsupportedError unless the dataset's name and content point only to `drivers`
/// (terrio or GDAL names). Nothing is opened. A remote file's first bytes may be requested,
/// without the GIL.
#[pyfunction]
fn check_drivers(py: Python<'_>, path: &str, drivers: Vec<String>) -> PyResult<()> {
    py.detach_result(|| terrio::formats::check_drivers(path, &drivers)).py()
}

/// A driver name as the allow-list, `check_drivers` and the raster writers read it: in upper case,
/// without the whitespace around it, and with the names of JPEG 2000 drivers (`JP2OpenJPEG`,
/// `JPEG2000`...) as `JP2`.
#[pyfunction]
fn canonical_driver_name(name: &str) -> String {
    terrio::formats::canonical_driver_name(name)
}

/// What a dataset's name and first bytes say its format is, as the allow-list judges it:
/// `(by_name, by_content)`. Not `terrio.identify`, which is format identification
/// (`inspection.rs`). The first bytes are read without the GIL.
#[pyfunction]
fn identify_drivers(py: Python<'_>, path: &str) -> PyResult<(Vec<String>, Option<String>)> {
    let id = py.detach_result(|| Ok(terrio::formats::identify(path))).py()?;
    Ok((id.by_name, id.by_content))
}

/// The HTTP traffic of this process since the last reset, with GDAL's network-statistics counters
/// (`VSINetworkStatsGetAsSerializedJSON`'s `downloaded_bytes`, `uploaded_bytes` and `count`):
/// `{"request_count": n, "received_bytes": n, "sent_bytes": n, "requests": n, "bytes": n,
/// "hosts": {host: {...}}, "files": {name: {...}}}`, with the same counters per host asked for
/// (`host:port`) and per file, named as GDAL names them (`/vsiaz/container/blob`,
/// `/vsicurl/<url>` without its query string). `requests` and `bytes` are the older names of
/// `request_count` and `received_bytes`. Every response counts, error responses and their bodies
/// included, as GDAL counts them. A redirect is not counted, nor the body of its request.
/// `sent_bytes` counts the bodies of the requests counted: uploads (`PUT`), batch deletes and
/// `POST` requests such as STAC searches.
#[pyfunction]
fn network_stats(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    fn counters(d: &Bound<'_, PyDict>, requests: u64, bytes: u64, sent: u64) -> PyResult<()> {
        d.set_item("request_count", requests)?;
        d.set_item("received_bytes", bytes)?;
        d.set_item("sent_bytes", sent)?;
        d.set_item("requests", requests)?;
        d.set_item("bytes", bytes)
    }
    let out = PyDict::new(py);
    let hosts = PyDict::new(py);
    let files = PyDict::new(py);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let total = terrio::http::fetch_stats();
        counters(&out, total.requests, total.bytes, total.sent_bytes)?;
        for (map, stats) in [(&hosts, terrio::http::fetch_stats_by_host()), (&files, terrio::http::fetch_stats_by_file())] {
            for (key, s) in stats {
                let d = PyDict::new(py);
                counters(&d, s.requests, s.bytes, s.sent_bytes)?;
                map.set_item(key, d)?;
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    counters(&out, 0, 0, 0)?;
    out.set_item("hosts", hosts)?;
    out.set_item("files", files)?;
    Ok(out)
}

/// The options the calling thread's `Env` scopes set (GDAL's `CPLGetThreadLocalConfigOptions`),
/// innermost winning; None marks an option a scope unsets. Process-wide options are environment
/// variables.
#[pyfunction]
fn config_options() -> HashMap<String, Option<String>> {
    terrio::config::thread_options()
}

/// Resets the network statistics (GDAL's `VSINetworkStatsReset`).
#[pyfunction]
fn reset_network_stats() {
    #[cfg(not(target_arch = "wasm32"))]
    terrio::http::reset_fetch_stats();
}

/// Empties the process-wide cache of remote files (GDAL's `VSICurlClearCache`): what remote
/// files' requests returned is kept, as GDAL's `/vsicurl/` cache keeps it, so that a file opened
/// again is read without requests; files opened after this call are requested again.
#[pyfunction]
fn clear_http_cache() {
    #[cfg(not(target_arch = "wasm32"))]
    terrio::http::clear_cache();
}

/// The most bytes of decoded GeoTIFF/COG chunks the process-wide block cache keeps (GDAL's
/// `gdal.GetCacheMax`): `TERRIO_CACHEMAX`, else `GDAL_CACHEMAX`, else 64 MiB; 0 means the cache is
/// off.
#[pyfunction]
fn block_cache_max() -> u64 {
    terrio::block_cache::max_bytes()
}

/// Sets the most bytes the block cache keeps (GDAL's `gdal.SetCacheMax`); 0 turns it off. Chunks
/// beyond the new size are dropped, least recently used first. Raises InvalidArgumentError (a
/// ValueError) when `nbytes` is negative.
#[pyfunction]
fn set_block_cache_max(nbytes: i64) -> PyResult<()> {
    let nbytes = u64::try_from(nbytes).map_err(|_| crate::invalid(format!("nbytes must not be negative, not {nbytes}")))?;
    terrio::block_cache::set_max_bytes(nbytes);
    Ok(())
}

/// Empties the block cache of decoded GeoTIFF/COG chunks, giving its memory back.
#[pyfunction]
fn clear_block_cache() {
    terrio::block_cache::clear();
}
