// SPDX-License-Identifier: MIT
//! Remote reads through `fetch` for terrio's WebAssembly modules (`terrio-js`, `terrio-js/zarr`):
//! HTTP range requests into a [`SparseFile`], and objects of a store into [`SparseObjects`], a
//! window of [`MAX_PARALLEL`] requests in flight; [`Remote::run`] runs a synchronous terrio read
//! again until it misses nothing. No WebAssembly exports: the modules export their own.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use terrio::raster::RasterData;
use terrio::sparse::{self, ObjectRange, SparseFile, SparseObjects};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Converts a JavaScript exception into a `JsError` with its message.
pub fn js_error(e: JsValue) -> JsError {
    let msg = e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())).or_else(|| e.as_string()).unwrap_or_else(|| format!("{e:?}"));
    JsError::new(&msg)
}

/// The most a request made while a file opens grows to (`sparse::grow_ranges`).
pub const OPEN_GROWTH_MAX: u64 = 4 << 20;

/// The most bytes of a first block still arriving that an open is tried on
/// ([`Fetcher::open_file_arriving`]): a COG's header is a few kilobytes, and each try stores the
/// bytes arrived so far again.
const ARRIVING_OPEN_MAX: usize = 1 << 20;

/// The most rounds of fetches a read may take.
pub const MAX_ROUNDS: usize = 64;

/// The range requests sent at once: browsers queue more, and servers may rate-limit them.
pub const MAX_PARALLEL: usize = terrio::sparse::PARALLEL_REQUESTS;

/// Sends range requests with `fetch`, counting them.
pub struct Fetcher {
    /// The file or store URL, including any query parameters.
    pub url: String,
    /// The caller's fetch implementation, or the runtime's global fetch.
    pub fetch: js_sys::Function,
    /// Additional HTTP headers supplied by the caller.
    pub headers: JsValue,
    /// Whether absent metadata probes may treat HTTP 403 as missing.
    pub allow_missing_metadata403: bool,
    /// Requests completed or rejected with an HTTP response.
    pub requests: Rc<Cell<u64>>,
    /// Response-body bytes consumed by this fetcher, including those of a body still arriving
    /// once it has ([`Fetcher::open_file_arriving`]).
    pub bytes: Rc<Cell<u64>>,
}

/// What a range request returned.
pub struct Fetched {
    /// The HTTP status code.
    pub status: u16,
    /// The file's length, from `Content-Range`.
    pub total: Option<u64>,
    /// The exposed Content-Range header, absent when hidden by CORS.
    pub content_range: Option<String>,
    /// The response body bytes.
    pub data: Vec<u8>,
}

impl Fetched {
    /// Checks an exposed Content-Range against the request, body and optional known file size.
    /// Missing headers are allowed because browsers can hide them under CORS.
    pub fn validate_content_range(&self, range: ObjectRange, total: Option<u64>) -> Result<(), JsError> {
        if self.status == 206
            && let Some(header) = &self.content_range
            && !valid_content_range(header, range, self.data.len() as u64, total)
        {
            return Err(JsError::new("range response has an unexpected Content-Range"));
        }
        Ok(())
    }
}

/// Reads the exposed range header without requiring it under CORS.
fn range_header(response: &JsValue) -> Option<String> {
    call(&get(response, "headers"), "get", &["content-range".into()]).ok()?.as_string()
}

/// Whether a range header describes exactly the requested bytes without arithmetic overflow.
fn valid_content_range(header: &str, range: ObjectRange, body_len: u64, expected_total: Option<u64>) -> bool {
    let parsed = (|| {
        let (unit, value) = header.trim().split_once(' ')?;
        if !unit.eq_ignore_ascii_case("bytes") {
            return None;
        }
        let (interval, total) = value.trim().split_once('/')?;
        let (first, last) = interval.split_once('-')?;
        let total = if total == "*" { None } else { Some(total.parse::<u64>().ok()?) };
        Some((first.parse::<u64>().ok()?, last.parse::<u64>().ok()?, total))
    })();
    parsed.is_some_and(|(first, last, total)| {
        total.is_none_or(|n| last < n && expected_total.is_none_or(|expected| n == expected))
            && last.checked_sub(first).and_then(|n| n.checked_add(1)) == Some(body_len)
            && match range {
                ObjectRange::At(offset, len) => {
                    first == offset && last.checked_add(1).zip(offset.checked_add(len)).is_some_and(|(end, expected)| end == expected)
                }
                ObjectRange::Suffix(len) => body_len <= len && total.is_none_or(|n| first == n.saturating_sub(len) && last == n - 1),
                ObjectRange::Whole => first == 0 && total.is_none_or(|n| last == n - 1),
            }
    })
}

/// Reads a JavaScript property, returning undefined when property access fails.
pub fn get(v: &JsValue, key: &str) -> JsValue {
    js_sys::Reflect::get(v, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

/// Calls a method of a JavaScript object.
pub fn call(obj: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, JsError> {
    let f: js_sys::Function = get(obj, method).dyn_into().map_err(|_| JsError::new(&format!("no method {method}")))?;
    let a = js_sys::Array::new();
    for x in args {
        a.push(x);
    }
    f.apply(obj, &a).map_err(js_error)
}

/// Lets a promise settle in the background with its rejection handled: an unhandled rejection
/// ends a Node process, and waiting for it could hang on a stream whose cancellation never settles.
fn settle(value: JsValue) {
    if let Ok(promise) = value.dyn_into::<js_sys::Promise>() {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = JsFuture::from(promise).await;
        });
    }
}

/// Cancels active readers without waiting for their cancellation promises.
fn cancel_all(readers: &RefCell<Vec<JsValue>>) {
    for reader in readers.borrow_mut().drain(..) {
        if let Ok(value) = call(&reader, "cancel", &[]) {
            settle(value);
        }
    }
}

/// Release an unused response without replacing the original request error, and without waiting
/// for the cancellation ([`settle`]).
fn cancel_body(response: &JsValue) {
    let body = get(response, "body");
    if !body.is_null()
        && !body.is_undefined()
        && let Ok(value) = call(&body, "cancel", &[])
    {
        settle(value);
    }
}

impl Fetcher {
    /// Starts a request for bytes `first..=last` (all of them with `None`), or a `HEAD` request.
    pub fn request(&self, range: Option<(u64, u64)>, head: bool) -> Result<js_sys::Promise, JsError> {
        self.request_to(&self.url, range.map(|(a, b)| format!("bytes={a}-{b}")), head)
    }

    /// Starts a request to `url` with a `Range` header (`bytes=...`), or a `HEAD` request.
    pub fn request_to(&self, url: &str, range: Option<String>, head: bool) -> Result<js_sys::Promise, JsError> {
        let headers = js_sys::Object::new();
        if self.headers.is_object() {
            // `Headers` instances and arrays of pairs are iterable: their entries, not their own
            // properties.
            let iterable = js_sys::Reflect::has(&self.headers, &js_sys::Symbol::iterator()).unwrap_or(false);
            let given = if iterable {
                js_sys::Object::from_entries(&self.headers).map_err(js_error)?
            } else {
                js_sys::Object::from(self.headers.clone())
            };
            js_sys::Object::assign(&headers, &given);
        }
        if let Some(range) = range {
            js_sys::Reflect::set(&headers, &"Range".into(), &range.into()).map_err(js_error)?;
        }
        let init = js_sys::Object::new();
        js_sys::Reflect::set(&init, &"headers".into(), &headers).map_err(js_error)?;
        if head {
            js_sys::Reflect::set(&init, &"method".into(), &"HEAD".into()).map_err(js_error)?;
        }
        let p = self.fetch.call2(&JsValue::UNDEFINED, &JsValue::from_str(url), &init).map_err(js_error)?;
        p.dyn_into::<js_sys::Promise>().map_err(|_| JsError::new("fetch did not return a promise"))
    }

    /// Waits for a response and reads it: an error unless its status is 2xx.
    pub async fn response(&self, promise: js_sys::Promise) -> Result<Fetched, JsError> {
        let f = self.any_response(promise).await?;
        if !(200..300).contains(&f.status) {
            return Err(JsError::new(&format!("{}: HTTP status {}", self.url, f.status)));
        }
        Ok(f)
    }

    /// Waits for a response and reads it, whatever its status.
    pub async fn any_response(&self, promise: js_sys::Promise) -> Result<Fetched, JsError> {
        let resp = JsFuture::from(promise).await.map_err(js_error)?;
        let status = get(&resp, "status").as_f64().unwrap_or(0.0) as u16;
        let header = |name: &str| -> Option<String> { call(&get(&resp, "headers"), "get", &[name.into()]).ok()?.as_string() };
        let content_range = header("content-range");
        let total = content_range.as_deref().and_then(|r| r.rsplit('/').next()?.trim().parse().ok());
        let buf = JsFuture::from(call(&resp, "arrayBuffer", &[])?.dyn_into::<js_sys::Promise>().map_err(|_| JsError::new("no body"))?)
            .await
            .map_err(js_error)?;
        let data = js_sys::Uint8Array::new(&buf).to_vec();
        self.requests.set(self.requests.get() + 1);
        self.bytes.set(self.bytes.get() + data.len() as u64);
        Ok(Fetched { status, total, content_range, data })
    }

    /// The length of the file, from a `HEAD` request's `Content-Length`.
    pub async fn head_len(&self) -> Result<Option<u64>, JsError> {
        let resp = JsFuture::from(self.request(None, true)?).await.map_err(js_error)?;
        let status = get(&resp, "status").as_f64().unwrap_or(0.0) as u16;
        if !(200..300).contains(&status) {
            return Err(JsError::new(&format!("{}: HEAD: HTTP status {status}", self.url)));
        }
        let len = call(&get(&resp, "headers"), "get", &["content-length".into()])?.as_string();
        Ok(len.and_then(|l| l.trim().parse().ok()))
    }

    /// Fetches byte ranges `(offset, length)` into the file, [`MAX_PARALLEL`] requests at a time
    /// ([`in_window`]). Once a server that ignores `Range` answers with the whole file, or a
    /// request fails, the other bodies are cancelled, not read: those being read (their readers
    /// held here), and those whose responses come after; a body cut short is not kept.
    pub async fn fetch_into(&self, file: &SparseFile, ranges: &[(u64, u64)]) -> Result<(), JsError> {
        // Set when a server that ignores `Range` sends the whole file: the other ranges are in it.
        let whole = Cell::new(false);
        let failed = Cell::new(false);
        let readers = RefCell::new(Vec::new());
        let (whole, failed, readers) = (&whole, &failed, &readers);
        let cancel_readers = || cancel_all(readers);
        in_window(ranges, |&(offset, len)| async move {
            if whole.get() || failed.get() {
                return Ok(());
            }
            let result: Result<(), JsError> = async {
                let last = offset
                    .checked_add(len)
                    .filter(|&end| len > 0 && end <= file.len())
                    .map(|end| end - 1)
                    .ok_or_else(|| JsError::new("file range exceeds file length"))?;
                let response = JsFuture::from(self.request(Some((offset, last)), false)?).await.map_err(js_error)?;
                if whole.get() || failed.get() {
                    cancel_body(&response);
                    return Ok(());
                }
                let status = get(&response, "status").as_f64().unwrap_or(0.0) as u16;
                if !(200..300).contains(&status) {
                    cancel_body(&response);
                    self.requests.set(self.requests.get() + 1);
                    return Err(JsError::new(&format!("{}: HTTP status {status}", self.url)));
                }
                if status == 200 {
                    // The whole file: the bodies of the other ranges are not needed.
                    whole.set(true);
                    cancel_readers();
                }
                let data = self.read_body(&response, readers).await?;
                if failed.get() || (status == 206 && whole.get()) {
                    // Cut short by a failure, or by the whole file elsewhere: not kept.
                    return Ok(());
                }
                match status {
                    206 if data.len() as u64 == len => {
                        let response = Fetched { status, total: None, content_range: range_header(&response), data };
                        response.validate_content_range(ObjectRange::At(offset, len), Some(file.len()))?;
                        file.insert(offset, &response.data);
                    }
                    200 if data.len() as u64 == file.len() => file.insert(0, &data),
                    _ => {
                        return Err(JsError::new(&format!(
                            "{}: bytes {offset}-{last}: unexpected response (status {status}, {} bytes): the server must \
                             answer Range requests, without a Content-Encoding",
                            self.url,
                            data.len()
                        )));
                    }
                }
                Ok(())
            }
            .await;
            if result.is_err() && !failed.replace(true) {
                cancel_readers();
            }
            result
        })
        .await
    }

    /// Fetches parts of objects of a store at the URL into `objects`, [`MAX_PARALLEL`] requests at
    /// a time ([`in_window`]): `key` at `url/key`. Missing objects (404) are handed over as missing.
    /// With explicit opt-in, metadata probes also accept 403, as anonymous S3 stores can
    /// answer for absent keys. Denied chunks/shards remain errors rather than fill-valued data.
    pub async fn fetch_objects(&self, objects: &SparseObjects, parts: &[(String, ObjectRange)]) -> Result<(), JsError> {
        let url = self.url.split('#').next().unwrap_or(&self.url);
        let (base, query) = url.split_once('?').map_or((url, String::new()), |(b, q)| (b, format!("?{q}")));
        let base = base.trim_end_matches('/');
        let query = query.as_str();
        // Once a request fails, the bodies of the others are cancelled, not read: those being
        // read (their readers held here), and those whose responses come after.
        let failed = Cell::new(false);
        let readers = RefCell::new(Vec::new());
        let (failed, readers) = (&failed, &readers);
        in_window(parts, |(key, range)| async move {
            let result: Result<(), JsError> = async {
                let header = match *range {
                    ObjectRange::Whole => None,
                    ObjectRange::At(o, n) => Some(format!(
                        "bytes={o}-{}",
                        o.checked_add(n.saturating_sub(1)).ok_or_else(|| JsError::new("object range overflow"))?
                    )),
                    ObjectRange::Suffix(n) => Some(format!("bytes=-{}", n.max(1))),
                };
                let encoded = key
                    .split('/')
                    .map(|part| js_sys::encode_uri_component(part).as_string().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("/");
                let request = self.request_to(&format!("{base}/{encoded}{query}"), header, false)?;
                let response = JsFuture::from(request).await.map_err(js_error)?;
                if failed.get() {
                    cancel_body(&response);
                    return Ok(());
                }
                let status = get(&response, "status").as_f64().unwrap_or(0.0) as u16;
                if status != 200 && status != 206 {
                    cancel_body(&response);
                    self.requests.set(self.requests.get() + 1);
                    if status == 404
                        || (status == 403
                            && self.allow_missing_metadata403
                            && matches!(key.rsplit('/').next(), Some(".zmetadata" | ".zarray" | ".zgroup" | "zarr.json")))
                    {
                        objects.insert(key, *range, None);
                        return Ok(());
                    }
                    return Err(JsError::new(&format!("{base}/{key}: HTTP status {status}")));
                }
                let data = self.read_body(&response, readers).await?;
                if failed.get() {
                    // Cancelled by a failure elsewhere: the bytes read are cut short, not held.
                    return Ok(());
                }
                let f = Fetched { status, total: None, content_range: range_header(&response), data };
                if f.status == 206 && !matches!(*range, ObjectRange::At(_, 0) | ObjectRange::Suffix(0)) {
                    // Browsers may hide Content-Range under CORS. When exposed, check
                    // position as well as length; a same-sized wrong range corrupts pixels.
                    f.validate_content_range(*range, None)?;
                    if let ObjectRange::Suffix(n) = *range
                        && f.data.len() as u64 > n
                    {
                        return Err(JsError::new("object suffix response exceeds requested length"));
                    }
                    if let ObjectRange::At(_, n) = *range
                        && f.data.len() as u64 != n
                    {
                        return Err(JsError::new("object range response has an unexpected length"));
                    }
                }
                let data = match (f.status, *range) {
                    (200 | 206, ObjectRange::At(_, 0) | ObjectRange::Suffix(0)) => Some(Vec::new()),
                    (200 | 206, ObjectRange::Whole) | (206, _) => Some(f.data),
                    // A server that ignores `Range` sends the whole object: the part asked.
                    (200, ObjectRange::At(o, n)) => Some(
                        f.data
                            .get(
                                usize::try_from(o).map_err(|_| JsError::new("object offset exceeds address space"))?
                                    ..usize::try_from(o.checked_add(n).ok_or_else(|| JsError::new("object range overflow"))?)
                                        .map_err(|_| JsError::new("object range exceeds address space"))?,
                            )
                            .map(<[u8]>::to_vec)
                            .ok_or_else(|| JsError::new(&format!("{base}/{key}: bytes {o}+{n} past the end of the object")))?,
                    ),
                    (200, ObjectRange::Suffix(n)) => {
                        Some(f.data[f.data.len().saturating_sub(usize::try_from(n).unwrap_or(usize::MAX))..].to_vec())
                    }
                    (status, _) => return Err(JsError::new(&format!("{base}/{key}: HTTP status {status}"))),
                };
                objects.insert(key, *range, data);
                Ok(())
            }
            .await;
            if result.is_err() && !failed.replace(true) {
                cancel_all(readers);
            }
            result
        })
        .await
    }

    /// Reads a response's body with a reader held in `readers` while it reads, so that a failed
    /// request elsewhere can cancel it (see [`Fetcher::fetch_objects`]): a cancelled read ends
    /// early, with the bytes read so far. A response without a stream body is read whole.
    async fn read_body(&self, response: &JsValue, readers: &RefCell<Vec<JsValue>>) -> Result<Vec<u8>, JsError> {
        let mut data = Vec::new();
        let body = get(response, "body");
        // A response from a custom `fetch` may have no stream body: read whole, as before.
        let reader = if body.is_null() || body.is_undefined() { None } else { call(&body, "getReader", &[]).ok() };
        if let Some(reader) = reader {
            readers.borrow_mut().push(reader.clone());
            let read: Result<(), JsError> = async {
                loop {
                    let promise = call(&reader, "read", &[])?.dyn_into::<js_sys::Promise>().map_err(|_| JsError::new("no body"))?;
                    let chunk = JsFuture::from(promise).await.map_err(js_error)?;
                    if get(&chunk, "done").is_truthy() {
                        return Ok(());
                    }
                    let bytes = js_sys::Uint8Array::new(&get(&chunk, "value"));
                    let start = data.len();
                    data.resize(start + bytes.length() as usize, 0);
                    bytes.copy_to(&mut data[start..]);
                }
            }
            .await;
            readers.borrow_mut().retain(|r| r != &reader);
            read?;
        } else {
            let promise = call(response, "arrayBuffer", &[])?.dyn_into::<js_sys::Promise>().map_err(|_| JsError::new("no body"))?;
            data = js_sys::Uint8Array::new(&JsFuture::from(promise).await.map_err(js_error)?).to_vec();
        }
        self.requests.set(self.requests.get() + 1);
        self.bytes.set(self.bytes.get() + data.len() as u64);
        Ok(data)
    }
}

/// Runs `f` on each of `items`, [`MAX_PARALLEL`] at a time, the next starting as soon as one
/// ends rather than when the slowest of a batch does: a read of many chunks keeps its requests in
/// flight without waiting on the slowest of each batch. Each call reads or releases its own
/// response. Once one fails, none starts; those started finish, so that no rejected promise is
/// left unhandled (which ends a Node process) and no body unread, and the first failure is
/// returned.
pub async fn in_window<'a, T, F, Fut>(items: &'a [T], f: F) -> Result<(), JsError>
where
    F: Fn(&'a T) -> Fut,
    Fut: std::future::Future<Output = Result<(), JsError>>,
{
    use futures_util::stream::{self, StreamExt};
    let failed = Cell::new(false);
    let (failed, f) = (&failed, &f);
    let results: Vec<Result<(), JsError>> = stream::iter(items)
        .map(|item| async move {
            if failed.get() {
                return Ok(());
            }
            let r = f(item).await;
            if r.is_err() {
                failed.set(true);
            }
            r
        })
        .buffer_unordered(MAX_PARALLEL)
        .collect()
        .await;
    results.into_iter().collect()
}

/// Whether this runs in a server runtime (Node.js, Bun, Deno), whose `fetch` opens as many
/// connections to a host as requests in flight, rather than in a browser, which keeps 6 to an
/// HTTP/1.1 server and queues the rest. An Electron renderer has Node's `process` but a browser's
/// `document` and connections: not a server.
pub fn server_runtime() -> bool {
    let global = js_sys::global();
    (get(&get(&get(&global, "process"), "versions"), "node").is_string() || !get(&global, "Deno").is_undefined())
        && get(&global, "document").is_undefined()
}

/// Pixels of `bands` bands, band after band, as one typed array of their type per band.
pub fn typed_bands(data: RasterData, count: usize) -> js_sys::Array {
    macro_rules! split {
        ($v:expr, $t:ty) => {{
            let n = $v.len() / count.max(1);
            $v.chunks(n.max(1)).take(count).map(|b| JsValue::from(<$t>::from(b))).collect()
        }};
    }
    match data {
        RasterData::U8(v) => split!(v, js_sys::Uint8Array),
        RasterData::I8(v) => split!(v, js_sys::Int8Array),
        RasterData::U16(v) => split!(v, js_sys::Uint16Array),
        RasterData::I16(v) => split!(v, js_sys::Int16Array),
        RasterData::U32(v) => split!(v, js_sys::Uint32Array),
        RasterData::I32(v) => split!(v, js_sys::Int32Array),
        RasterData::U64(v) => split!(v, js_sys::BigUint64Array),
        RasterData::I64(v) => split!(v, js_sys::BigInt64Array),
        RasterData::F32(v) => split!(v, js_sys::Float32Array),
        RasterData::F64(v) => split!(v, js_sys::Float64Array),
    }
}

impl Fetcher {
    /// A fetcher of `url` with the `fetch` and `headers` of `options` (the global `fetch` without
    /// one), and the rest of the options, without those two.
    pub fn from_options(url: &str, options: &JsValue) -> Result<(Self, JsValue), JsError> {
        let (fetch, headers) =
            if options.is_object() { (get(options, "fetch"), get(options, "headers")) } else { (JsValue::UNDEFINED, JsValue::UNDEFINED) };
        let rest = if options.is_object() {
            let copy = js_sys::Object::assign(&js_sys::Object::new(), &js_sys::Object::from(options.clone()));
            let _ = js_sys::Reflect::delete_property(&copy, &"fetch".into());
            let _ = js_sys::Reflect::delete_property(&copy, &"headers".into());
            copy.into()
        } else {
            options.clone()
        };
        let fetch: js_sys::Function = if fetch.is_function() {
            fetch.unchecked_into()
        } else {
            get(&js_sys::global(), "fetch").dyn_into().map_err(|_| JsError::new("no fetch function: pass one as options.fetch"))?
        };
        let fetcher =
            Self { url: url.to_string(), fetch, headers, allow_missing_metadata403: false, requests: Rc::default(), bytes: Rc::default() };
        Ok((fetcher, rest))
    }

    /// Fetches a file's first block of `block` bytes (clamped to 512 bytes–16 MiB), which also
    /// tells the file's length, and returns the file in blocks of that size, holding them, and
    /// those first bytes. A server that ignores `Range` sends the whole file: every block is then
    /// held, and nothing more is requested.
    pub async fn open_file(&self, block: u64) -> Result<(SparseFile, Vec<u8>), JsError> {
        let block = block.clamp(512, 16 << 20);
        let response = JsFuture::from(self.request(Some((0, block - 1)), false)?).await.map_err(js_error)?;
        self.open_from(response, block).await
    }

    /// As [`open_file`](Self::open_file), but opens the file with `open` while its first block
    /// arrives: after each piece of the body, `open` runs on the bytes arrived so far, and once it
    /// succeeds without missing any, the file is returned with its result, the rest of the block
    /// still arriving into the file (see [`Remote::wait_for_arriving`]). A COG's header is a few
    /// kilobytes at the start of its first block: the open then takes the time to the first bytes
    /// rather than to the whole block. Without a stream body, or when `open` needs more than the
    /// block, the block is read whole and the result is `None`: open the file with [`Remote::run`].
    pub async fn open_file_arriving<T>(
        &self,
        block: u64,
        mut open: impl FnMut(&SparseFile) -> terrio::Result<T>,
    ) -> Result<(SparseFile, Option<(T, Arriving)>), JsError> {
        let block = block.clamp(512, 16 << 20);
        let response = JsFuture::from(self.request(Some((0, block - 1)), false)?).await.map_err(js_error)?;
        let status = get(&response, "status").as_f64().unwrap_or(0.0) as u16;
        let content_range = range_header(&response);
        let total: Option<u64> = content_range.as_deref().and_then(|r| r.rsplit('/').next()?.trim().parse().ok());
        let body = get(&response, "body");
        // The reader locks the body: taken only to stream it.
        let streamed = status == 206 && total.is_some() && !body.is_null() && !body.is_undefined();
        let reader = if streamed { call(&body, "getReader", &[]).ok() } else { None };
        let (Some(len), Some(reader)) = (total, reader) else {
            // A whole file (200), an error, no exposed length, or no stream: as `open_file`,
            // from this response.
            let (file, _) = self.open_from(response, block).await?;
            return Ok((file, None));
        };
        let expected = block.min(len);
        let file = SparseFile::with_block_size(len, block);
        let mut data = Vec::new();
        let opened = loop {
            if let Some(chunk) = next_chunk(&reader).await? {
                append(&mut data, &chunk);
            } else {
                break None;
            }
            if data.len() as u64 >= expected {
                break None;
            }
            if data.len() > ARRIVING_OPEN_MAX {
                continue;
            }
            file.insert_arriving(0, &data);
            let result = open(&file);
            if file.missing().is_empty()
                && let Ok(value) = result
            {
                break Some(value);
            }
        };
        self.requests.set(self.requests.get() + 1);
        let Some(value) = opened else {
            self.bytes.set(self.bytes.get() + data.len() as u64);
            if let Err(e) = finish_first(&file, &data, expected, content_range.as_deref(), len) {
                // A body longer than asked: released, not read on.
                let _ = call(&reader, "cancel", &[]).map(settle);
                return Err(e);
            }
            return Ok((file, None));
        };
        // The rest of the block arrives in the background; a failure leaves it missing, fetched
        // again by the read that needs it.
        let (rest, bytes) = (file.clone(), self.bytes.clone());
        let done = wasm_bindgen_futures::future_to_promise(async move {
            let result: Result<(), JsError> = async {
                while let Some(chunk) = next_chunk(&reader).await? {
                    append(&mut data, &chunk);
                }
                bytes.set(bytes.get() + data.len() as u64);
                finish_first(&rest, &data, expected, content_range.as_deref(), len)
            }
            .await;
            if result.is_err() {
                let _ = call(&reader, "cancel", &[]).map(settle);
            }
            Ok(JsValue::UNDEFINED)
        });
        Ok((file, Some((value, Arriving { done, end: expected }))))
    }

    /// The file of a first-block response, read whole: see [`open_file`](Self::open_file).
    async fn open_from(&self, response: JsValue, block: u64) -> Result<(SparseFile, Vec<u8>), JsError> {
        let first = self.response(js_sys::Promise::resolve(&response)).await?;
        let len = match (first.status, first.total) {
            (206, Some(total)) => total,
            (206, None) => match self.head_len().await? {
                Some(len) => len,
                None => {
                    return Err(JsError::new(&format!(
                        "{}: the server's response has no Content-Range (cross-origin servers must expose it) and no Content-Length",
                        // Without the query, which may hold a signed URL's credentials.
                        self.url.split(['?', '#']).next().unwrap_or(&self.url)
                    )));
                }
            },
            _ => first.data.len() as u64,
        };
        let file = SparseFile::with_block_size(len, block);
        if first.status == 206 {
            let expected = block.min(len);
            first.validate_content_range(ObjectRange::At(0, expected), Some(len))?;
            if first.data.len() as u64 != expected {
                return Err(JsError::new("initial range response has an unexpected length"));
            }
        }
        file.insert(0, &first.data);
        Ok((file, first.data))
    }
}

/// The rest of a first block still arriving ([`Fetcher::open_file_arriving`]): a promise that
/// settles once it has arrived or failed, and the end of the bytes it brings.
pub struct Arriving {
    done: js_sys::Promise,
    end: u64,
}

/// The next piece of a body from its stream reader, or `None` at its end.
async fn next_chunk(reader: &JsValue) -> Result<Option<js_sys::Uint8Array>, JsError> {
    let promise = call(reader, "read", &[])?.dyn_into::<js_sys::Promise>().map_err(|_| JsError::new("no body"))?;
    let chunk = JsFuture::from(promise).await.map_err(js_error)?;
    Ok((!get(&chunk, "done").is_truthy()).then(|| js_sys::Uint8Array::new(&get(&chunk, "value"))))
}

fn append(data: &mut Vec<u8>, chunk: &js_sys::Uint8Array) {
    let start = data.len();
    data.resize(start + chunk.length() as usize, 0);
    chunk.copy_to(&mut data[start..]);
}

/// Checks a first block arrived whole (`expected` bytes of a file of `len`) and stores it.
fn finish_first(file: &SparseFile, data: &[u8], expected: u64, content_range: Option<&str>, len: u64) -> Result<(), JsError> {
    if data.len() as u64 != expected {
        return Err(JsError::new("initial range response has an unexpected length"));
    }
    if let Some(header) = content_range
        && !valid_content_range(header, ObjectRange::At(0, expected), expected, Some(len))
    {
        return Err(JsError::new("range response has an unexpected Content-Range"));
    }
    file.insert(0, data);
    Ok(())
}

/// A remote file or store read by terrio's synchronous readers: the blocks or objects fetched,
/// kept up to a cache size between reads, and the reads queued, which run one at a time as they
/// share them.
pub struct Remote {
    /// Sparse blocks of a remote file, empty for object stores.
    pub file: SparseFile,
    /// A store's objects (the file is then empty).
    pub objects: Option<Arc<SparseObjects>>,
    /// Transport configuration and request accounting shared by these reads.
    pub fetcher: Fetcher,
    /// The bytes kept between reads.
    pub cache_size: u64,
    /// The end of the last read queued.
    queue: RefCell<js_sys::Promise>,
    /// The rest of the first block, while it arrives.
    arriving: RefCell<Option<Arriving>>,
}

/// A read's turn, which lets the next queued read run when dropped.
struct Turn(js_sys::Function);

impl Drop for Turn {
    fn drop(&mut self) {
        let _ = self.0.call0(&JsValue::NULL);
    }
}

impl Remote {
    /// Creates a queued reader over a file or object store with the given byte-cache budget.
    pub fn new(file: SparseFile, objects: Option<Arc<SparseObjects>>, fetcher: Fetcher, cache_size: u64) -> Self {
        Self {
            file,
            objects,
            fetcher,
            cache_size,
            queue: RefCell::new(js_sys::Promise::resolve(&JsValue::UNDEFINED)),
            arriving: RefCell::new(None),
        }
    }

    /// Lets reads wait for the rest of a first block still arriving, rather than request it again.
    pub fn wait_for_arriving(&self, arriving: Arriving) {
        *self.arriving.borrow_mut() = Some(arriving);
    }

    /// Waits for the reads queued before this one to finish.
    async fn turn(&self) -> Turn {
        let mut resolve = None;
        let mine = js_sys::Promise::new(&mut |res, _| resolve = Some(res));
        let previous = self.queue.replace(mine);
        let _ = JsFuture::from(previous).await;
        Turn(resolve.expect("a promise's executor runs at once"))
    }

    /// Runs a read until it no longer misses blocks or objects, fetching them in between, and
    /// returns its result; an error when a request fails, or after [`MAX_ROUNDS`] rounds. While
    /// `opening()` (no reader yet), requests grow with the header read so far, so that a large
    /// header (a COG's IFDs and tile tables) takes a few requests rather than one per block. Reads
    /// of one remote run one after another, even when their promises are awaited together.
    pub async fn run<T>(
        &self,
        opening: impl Fn() -> bool,
        mut read: impl FnMut() -> terrio::Result<T>,
    ) -> Result<terrio::Result<T>, JsError> {
        let _turn = self.turn().await;
        for _ in 0..MAX_ROUNDS {
            let result = read();
            let mut missing = self.file.missing();
            if opening() {
                let block = self.file.block_size();
                missing = sparse::grow_ranges(&missing, self.file.fetched().0.min(OPEN_GROWTH_MAX), block, self.file.len());
            } else if !missing.is_empty()
                && server_runtime()
                && let Some(pieces) = sparse::parallel_ranges(&missing, self.file.block_size())
            {
                missing = pieces;
            }
            // Bytes still arriving are waited for, not requested again.
            let arriving = if missing.iter().any(|&(o, _)| self.arriving.borrow().as_ref().is_some_and(|a| o < a.end)) {
                self.arriving.borrow_mut().take()
            } else {
                None
            };
            if let Some(a) = &arriving {
                missing = missing
                    .into_iter()
                    .filter_map(|(o, n)| if o >= a.end { Some((o, n)) } else { (o + n > a.end).then(|| (a.end, o + n - a.end)) })
                    .collect();
            }
            let parts = self.objects.as_ref().map(|o| o.missing()).unwrap_or_default();
            if missing.is_empty() && parts.is_empty() && arriving.is_none() {
                self.file.trim(self.cache_size);
                if let Some(o) = &self.objects {
                    o.trim(self.cache_size);
                }
                return Ok(result);
            }
            if let Some(o) = &self.objects {
                self.fetcher.fetch_objects(o, &parts).await?;
            }
            self.fetcher.fetch_into(&self.file, &missing).await?;
            if let Some(a) = arriving {
                // Settles once the rest has arrived or failed (then requested again, next round).
                let _ = JsFuture::from(a.done).await;
            }
        }
        let name = self.fetcher.url.split(['?', '#']).next().unwrap_or("remote");
        Err(JsError::new(&format!("{name}: the read still needed more bytes after {MAX_ROUNDS} rounds of requests")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Response positions and lengths must match the requested bytes, including suffixes.
    #[test]
    fn content_ranges_match_requested_positions() {
        assert!(valid_content_range("bytes 4-7/8", ObjectRange::At(4, 4), 4, Some(8)));
        assert!(valid_content_range("Bytes 4-7/8", ObjectRange::At(4, 4), 4, Some(8)));
        assert!(valid_content_range("bytes 4-7/*", ObjectRange::At(4, 4), 4, Some(8)));
        assert!(valid_content_range("bytes 4-7/8", ObjectRange::Suffix(4), 4, None));
        assert!(valid_content_range("bytes 4-7/*", ObjectRange::Suffix(4), 4, None));
        assert!(valid_content_range("bytes 0-7/8", ObjectRange::Whole, 8, None));
        assert!(!valid_content_range("bytes 5-8/9", ObjectRange::At(4, 4), 4, None));
        assert!(!valid_content_range("bytes 0-3/8", ObjectRange::Suffix(4), 4, None));
        assert!(!valid_content_range("bytes 1-7/8", ObjectRange::Whole, 7, None));
        assert!(!valid_content_range("bytes 0-3/4", ObjectRange::At(0, 4), 4, Some(8)));
    }

    /// Malformed intervals cannot wrap their length or masquerade as an empty response.
    #[test]
    fn content_ranges_reject_overflow() {
        let overflow = format!("bytes 0-{}/*", u64::MAX);
        assert!(!valid_content_range(&overflow, ObjectRange::Whole, 0, None));
        assert!(!valid_content_range(&overflow, ObjectRange::Suffix(4), 0, None));
        assert!(!valid_content_range("bytes 7-4/*", ObjectRange::At(7, 2), 2, None));
        assert!(!valid_content_range("bytes 0-0/0", ObjectRange::Whole, 1, None));
        let end = format!("bytes {0}-{0}/*", u64::MAX);
        assert!(!valid_content_range(&end, ObjectRange::At(u64::MAX, 1), 1, None));
    }
}
