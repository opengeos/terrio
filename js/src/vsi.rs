//! File management in WebAssembly: the in-memory file system (`/vsimem/`), zip archives in it,
//! and listings of remote directories through `fetch` (GDAL's `gdal vsi`).
//!
//! WebAssembly has no local file system and no signed requests: local paths fail, and writes to
//! object stores are not available (`listUrl` lists public buckets and SAS-token URLs).

use crate::{js_err, parse_options, to_js};
use serde::Deserialize;
use terrio::vsi::{self, CopyOptions, ListOptions, RemoteLister, SyncOptions};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct VsiListOptions {
    recursive: bool,
    depth: Option<usize>,
    absolute: bool,
    long: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct VsiCopyOptions {
    recursive: bool,
    skip_errors: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct VsiSyncOptions {
    recursive: bool,
    strategy: Option<String>,
}

/// An entry as JavaScript sees it.
fn entries_js(entries: &[vsi::Entry]) -> Result<JsValue, JsError> {
    let list: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "type": if e.is_dir() { "directory" } else { "file" },
                "size": e.size,
                "modified": e.modified,
                "lastModificationDate": e.modified_string(),
                "etag": e.etag,
                "permissions": e.permissions,
            })
        })
        .collect();
    to_js(&list)
}

/// Lists a `/vsimem/` directory, or a zip archive in memory (`/vsizip//vsimem/a.zip`), as
/// `gdal vsi list` does. Options: `recursive`, `depth`, `absolute`, `long` (fetch missing entry metadata).
#[wasm_bindgen(js_name = vsiList)]
pub fn vsi_list(path: &str, options: JsValue) -> Result<JsValue, JsError> {
    let o: VsiListOptions = parse_options(&options)?;
    let entries =
        vsi::list(path, &ListOptions { recursive: o.recursive, depth: o.depth, absolute: o.absolute, details: o.long }).map_err(js_err)?;
    entries_js(&entries)
}

/// Describes a location, or returns `null` when nothing is there.
#[wasm_bindgen(js_name = vsiStat)]
pub fn vsi_stat(path: &str) -> Result<JsValue, JsError> {
    match vsi::stat(path).map_err(js_err)? {
        Some(e) => Ok(js_sys::Array::from(&entries_js(&[e])?).get(0)),
        None => Ok(JsValue::NULL),
    }
}

/// Writes bytes to a `/vsimem/` file, replacing it.
#[wasm_bindgen(js_name = vsiWrite)]
pub fn vsi_write(path: &str, bytes: &[u8]) -> Result<(), JsError> {
    vsi::write(path, bytes).map_err(js_err)
}

/// Reads a whole `/vsimem/` file or a member of an archive in memory.
#[wasm_bindgen(js_name = vsiRead)]
pub fn vsi_read(path: &str) -> Result<Vec<u8>, JsError> {
    vsi::read(path).map_err(js_err)
}

/// Copies a file, or a directory with `recursive`, within `/vsimem/` (archive members out of
/// archives). Options: `recursive`, `skipErrors`. Returns the report.
#[wasm_bindgen(js_name = vsiCopy)]
pub fn vsi_copy(src: &str, dst: &str, options: JsValue) -> Result<JsValue, JsError> {
    let o: VsiCopyOptions = parse_options(&options)?;
    to_js(&vsi::copy(src, dst, &CopyOptions { recursive: o.recursive, skip_errors: o.skip_errors }).map_err(js_err)?)
}

/// Deletes a `/vsimem/` file, or a directory with `recursive` (the root of `/vsimem/` is
/// refused). Options: `{ recursive }`; the original boolean argument is also accepted.
/// Returns the report.
#[wasm_bindgen(js_name = vsiDelete)]
pub fn vsi_delete(path: &str, options: JsValue) -> Result<JsValue, JsError> {
    #[derive(Deserialize, Default)]
    #[serde(default, deny_unknown_fields)]
    struct DeleteOptions {
        recursive: bool,
    }
    // Keep the original boolean argument compatible with existing callers.
    let recursive = match options.as_bool() {
        Some(recursive) => recursive,
        None => parse_options::<DeleteOptions>(&options)?.recursive,
    };
    to_js(&vsi::delete(path, recursive).map_err(js_err)?)
}

/// Moves (renames) a `/vsimem/` file or directory. Returns the report.
#[wasm_bindgen(js_name = vsiMove)]
pub fn vsi_move(src: &str, dst: &str) -> Result<JsValue, JsError> {
    to_js(&vsi::rename(src, dst).map_err(js_err)?)
}

/// Copies the files missing or out of date in the target, within `/vsimem/`. Options:
/// `recursive`, `strategy` (`timestamp`, `ETag` or `overwrite`). Returns the report.
#[wasm_bindgen(js_name = vsiSync)]
pub fn vsi_sync(src: &str, dst: &str, options: JsValue) -> Result<JsValue, JsError> {
    let o: VsiSyncOptions = parse_options(&options)?;
    let strategy = o.strategy.as_deref().unwrap_or("timestamp").parse().map_err(js_err)?;
    to_js(&vsi::sync(src, dst, &SyncOptions { recursive: o.recursive, strategy }).map_err(js_err)?)
}

fn get(v: &JsValue, key: &str) -> JsValue {
    js_sys::Reflect::get(v, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn js_error(e: JsValue) -> JsError {
    let msg = e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())).or_else(|| e.as_string()).unwrap_or_else(|| format!("{e:?}"));
    JsError::new(&msg)
}

fn cancel_body(value: &JsValue) {
    if let Ok(cancel) = get(value, "cancel").dyn_into::<js_sys::Function>() {
        let _ = cancel.call0(value);
    }
}

/// Streams listing pages only as far as their size limits, matching the native reader.
async fn listing_body(response: &JsValue, lister: &RemoteLister) -> Result<Vec<u8>, JsError> {
    let limit = if lister.is_html() { RemoteLister::HTML_BODY_LIMIT } else { RemoteLister::OBJECT_BODY_LIMIT };
    let size_error = if lister.is_html() { "HTTP directory index exceeds 4 MiB" } else { "object-store listing page exceeds 16 MiB" };
    let headers = get(response, "headers");
    let header = |name: &str| -> Option<String> {
        get(&headers, "get").dyn_into::<js_sys::Function>().ok()?.call1(&headers, &name.into()).ok()?.as_string()
    };
    let body = get(response, "body");
    if header("content-length").and_then(|s| s.parse::<usize>().ok()).is_some_and(|n| n > limit) {
        cancel_body(&body);
        return Err(JsError::new(size_error));
    }
    let content_type = header("content-type").unwrap_or_default().to_ascii_lowercase();
    if lister.is_html()
        && !content_type.is_empty()
        && !(content_type.starts_with("text/html") || content_type.starts_with("application/xhtml+xml"))
    {
        cancel_body(&body);
        return Err(JsError::new("not a listable HTML directory index"));
    }
    let get_reader =
        get(&body, "getReader").dyn_into::<js_sys::Function>().map_err(|_| JsError::new("HTML response needs a readable byte stream"))?;
    let reader = get_reader.call0(&body).map_err(js_error)?;
    let read = get(&reader, "read").dyn_into::<js_sys::Function>().map_err(|_| JsError::new("HTML response has no stream reader"))?;
    let mut data = Vec::new();
    let mut sniffed = !lister.is_html();
    loop {
        let promise = read.call0(&reader).map_err(js_error)?.dyn_into::<js_sys::Promise>().map_err(js_error)?;
        let chunk = JsFuture::from(promise).await.map_err(js_error)?;
        if get(&chunk, "done").as_bool().unwrap_or(false) {
            if !sniffed {
                lister.check_html_prefix(&data).map_err(js_err)?;
            }
            return Ok(data);
        }
        let value =
            get(&chunk, "value").dyn_into::<js_sys::Uint8Array>().map_err(|_| JsError::new("HTML stream returned a non-byte chunk"))?;
        let length = value.length() as usize;
        if length > limit - data.len() {
            cancel_body(&reader);
            return Err(JsError::new(size_error));
        }
        let start = data.len();
        data.resize(start + length, 0);
        value.copy_to(&mut data[start..]);
        if !sniffed && data.len() >= RemoteLister::HTML_SNIFF_BYTES {
            if let Err(error) = lister.check_html_prefix(&data) {
                cancel_body(&reader);
                return Err(js_err(error));
            }
            sniffed = true;
        }
    }
}

/// Lists a remote directory with `fetch`, as `gdal vsi list` lists `/vsis3/`, `/vsigs/`,
/// `/vsiaz/` and `/vsicurl/`: public S3 buckets (`s3://bucket/dir`), Google Cloud Storage
/// (`gs://`), Azure Blob Storage (`https://account.blob.core.windows.net/container/dir?<SAS>`)
/// and HTML index pages of plain HTTP servers. Options: `recursive`, `depth`, `fetch` (instead of
/// `globalThis.fetch`), `headers`. Returns `{entries, requests, bytes}`.
///
/// In browsers, the store must allow the page's origin (CORS). HTML indexes are streamed,
/// checked with a 4 KiB sniff prefix and limited to 4 MiB; custom fetch must return a Response.
#[wasm_bindgen(js_name = listUrl)]
pub async fn list_url(url: String, options: JsValue) -> Result<JsValue, JsError> {
    let opt = |k: &str| if options.is_object() { get(&options, k) } else { JsValue::UNDEFINED };
    let recursive = opt("recursive").as_bool().unwrap_or(false);
    let depth = opt("depth").as_f64().map(|d| d as usize);
    let headers = opt("headers");
    let fetch: js_sys::Function = match opt("fetch") {
        f if f.is_function() => f.unchecked_into(),
        _ => get(&js_sys::global(), "fetch").dyn_into().map_err(|_| JsError::new("no fetch function: pass one as options.fetch"))?,
    };
    let mut lister = RemoteLister::new(&url, recursive, depth).map_err(js_err)?;
    let (mut requests, mut bytes) = (0u64, 0u64);
    while let Some(next) = lister.next_url() {
        let init = js_sys::Object::new();
        if headers.is_object() {
            js_sys::Reflect::set(&init, &"headers".into(), &headers).map_err(js_error)?;
        }
        let promise: js_sys::Promise = fetch
            .call2(&JsValue::UNDEFINED, &JsValue::from_str(&next), &init)
            .map_err(js_error)?
            .dyn_into()
            .map_err(|_| JsError::new("fetch did not return a promise"))?;
        let resp = JsFuture::from(promise).await.map_err(js_error)?;
        let status = get(&resp, "status").as_f64().unwrap_or(0.0) as u16;
        if !(200..300).contains(&status) {
            cancel_body(&get(&resp, "body"));
            lister.feed(status, &[]).map_err(|e| JsError::new(&format!("{}: {e}", next.split('?').next().unwrap_or_default())))?;
        }
        let data = listing_body(&resp, &lister).await?;
        requests += 1;
        bytes += data.len() as u64;
        lister.feed(status, &data).map_err(|e| JsError::new(&format!("{}: {e}", next.split('?').next().unwrap_or_default())))?;
    }
    let out = js_sys::Object::new();
    js_sys::Reflect::set(&out, &"entries".into(), &entries_js(&lister.finish())?).map_err(js_error)?;
    js_sys::Reflect::set(&out, &"requests".into(), &JsValue::from_f64(requests as f64)).map_err(js_error)?;
    js_sys::Reflect::set(&out, &"bytes".into(), &JsValue::from_f64(bytes as f64)).map_err(js_error)?;
    Ok(out.into())
}
