//! Object store operations beyond reading: listings with sizes and dates, the metadata of one
//! object, uploads, deletions and server-side copies, for [`crate::vsi`].
//!
//! Amazon S3 (and S3-compatible services) is listed, read and written with signed requests when
//! credentials are set. Google Cloud Storage is listed and read through its XML API (public
//! buckets). Azure Blob Storage is listed, read and written with the SAS token of the URL (or
//! anonymously for public containers). Plain HTTP servers are listed from their HTML directory
//! index, as GDAL's `/vsicurl/` lists them.

use super::{Client, Directory, HttpOptions, MAX_REDIRECTS, Remote, agent, redact, resolve_redirect, transient};
use crate::s3::{self, Credentials, S3Object};
pub(crate) use crate::vsi::listing::ObjectEntry;
use crate::vsi::listing::{Api, MAX_PAGES, add_implied_directories, html_directory_url, parse_http_date};
use crate::{Error, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// The largest object a single upload request may send: Amazon S3's limit of a `PUT`.
pub(crate) const MAX_SINGLE_UPLOAD: u64 = 5 << 30;

/// The metadata of one remote object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectStat {
    /// The size in bytes, when the response headers provide it.
    pub size: Option<u64>,
    /// The last modification time, in seconds since the Unix epoch.
    pub modified: Option<i64>,
    /// The entity tag, without quotes.
    pub etag: Option<String>,
    /// The media type (`Content-Type`), lowercase, without parameters; `None` when the server
    /// sends none.
    pub content_type: Option<String>,
}

/// How `dir` is listed.
pub(crate) fn listing_api(dir: &Directory) -> Api {
    match dir {
        Directory::S3 { bucket, prefix } => {
            let prefix = if prefix.is_empty() { String::new() } else { format!("{}/", prefix.trim_end_matches('/')) };
            Api::S3 { bucket: bucket.clone(), prefix }
        }
        Directory::Url(url) => Api::for_url(url),
    }
}

/// Whether `dir` is in a store with a listing API (S3, Google Cloud Storage, Azure).
pub(crate) fn is_object_store(dir: &Directory) -> bool {
    listing_api(dir).is_object_store()
}

/// Capped read-only discovery retains its historical warn-and-partial behavior.
/// Mutation callers must use list_objects, which requires a complete listing.
pub(crate) fn list_objects_for_discovery(dir: &Directory, recursive: bool) -> Result<Option<Vec<ObjectEntry>>> {
    list_pages(dir, recursive, None, true)
}

/// Whether a directory of an object store holds anything: one listing request for one key.
/// `None` when the store cannot list.
pub(crate) fn has_entries(dir: &Directory) -> Result<Option<bool>> {
    Ok(list_pages(dir, false, Some(1), false)?.map(|l| !l.is_empty()))
}

/// [`list_objects`]; with `max`, only the first page, of at most `max` entries.
fn list_pages(dir: &Directory, recursive: bool, max: Option<u32>, allow_partial: bool) -> Result<Option<Vec<ObjectEntry>>> {
    list_pages_api(listing_api(dir), recursive, max, allow_partial)
}

/// The entries below a directory of an object store: one level down (objects and common prefixes),
/// or with `recursive` every object below it, with the "directories" their keys imply. `None`
/// when the store cannot list (a plain HTTP server), or refuses to (a bucket whose listing is not
/// public). Names are relative to the directory.
/// The API prefix is a raw object key, encoded only in the request query.
pub(crate) fn list_objects(api: &Api, recursive: bool) -> Result<Option<Vec<ObjectEntry>>> {
    list_pages_api(api.clone(), recursive, None, false)
}

fn list_pages_api(api: Api, recursive: bool, max: Option<u32>, allow_partial: bool) -> Result<Option<Vec<ObjectEntry>>> {
    if !api.is_object_store() {
        return Ok(None);
    }
    let mut out: Vec<ObjectEntry> = Vec::new();
    let mut index = Default::default();
    let mut token: Option<String> = None;
    for page in 0..MAX_PAGES {
        let body = match &api {
            Api::S3 { bucket, prefix } => {
                let remote = Remote::S3(S3Object { bucket: bucket.clone(), key: String::new() });
                let mut client = Client::new(&remote, HttpOptions::from_env());
                client.missing_ok = true;
                client.refused_ok = true;
                client.query = format!("?{}", api.query_limited(token.as_deref(), recursive, max));
                client.url = format!("{}{}", client.url, client.query);
                let fetched = client.fetch_range_limited(None, Some(crate::vsi::RemoteLister::OBJECT_BODY_LIMIT))?;
                // A signed request refused: the credentials may not allow s3:ListBucket. Not
                // listable, rather than failing again.
                if fetched.status == 403 && client.s3.as_ref().is_some_and(|a| a.creds.is_some()) {
                    crate::warnings::warn(format!(
                        "S3: listing s3://{bucket}/{prefix} was refused (403): the credentials may not allow s3:ListBucket"
                    ));
                }
                if matches!(fetched.status, 403 | 404) {
                    return Ok(None);
                }
                fetched.data
            }
            _ => match list_get(
                &api.request_url_limited(token.as_deref(), recursive, max).expect("non-S3 listing APIs always provide a URL"),
            )? {
                Some(b) => b,
                None => return Ok(None),
            },
        };
        if !api.is_listing_page(&body) {
            // Read-only discovery and `has_entries` (stat) keep their historical answer, a store
            // that cannot list; a complete listing, which mutations rely on, fails instead.
            if allow_partial || max.is_some() {
                return Ok(None);
            }
            return Err(Error::InvalidData("not an XML object-store listing".into()));
        }
        let next = api.read_page(&body, &mut out, &mut index);
        if max.is_some() {
            if out.is_empty() && api.has_directory_marker(&body) {
                out.push(ObjectEntry { name: String::new(), dir: true, size: None, modified: None, etag: None });
            }
            break;
        }
        match next {
            Some(_) if page + 1 == MAX_PAGES && allow_partial => {
                crate::warnings::warn(format!("listing stopped after {MAX_PAGES} pages ({} entries)", out.len()));
                break;
            }
            Some(_) if page + 1 == MAX_PAGES => {
                return Err(Error::InvalidData(format!("object listing exceeds {MAX_PAGES} pages; refusing an incomplete operation")));
            }
            Some(t) => token = Some(t),
            None => break,
        }
    }
    if recursive {
        add_implied_directories(&mut out);
    }
    Ok(Some(out))
}

/// A listing page of an unsigned store: `None` when listing is refused or missing (403, 404), or
/// the server answers with something other than XML.
fn list_get(url: &str) -> Result<Option<Vec<u8>>> {
    let mut client = Client::new(&Remote::Url(url.to_string()), HttpOptions::from_env());
    client.missing_ok = true;
    let fetched = client.fetch_range_limited(None, Some(crate::vsi::RemoteLister::OBJECT_BODY_LIMIT))?;
    // Azure starts its XML with a byte order mark.
    let data = fetched.data.strip_prefix(b"\xEF\xBB\xBF").map(<[u8]>::to_vec).unwrap_or(fetched.data);
    if matches!(fetched.status, 403 | 404) || !data.trim_ascii_start().starts_with(b"<") {
        return Ok(None);
    }
    Ok(Some(data))
}

/// The entries of a plain HTTP server's directory, from its HTML index page (Apache, nginx and
/// similar autoindex pages). `None` when the page is not an index.
pub(crate) fn list_html(url: &str) -> Result<Option<Vec<ObjectEntry>>> {
    let api = Api::Html { url: url.to_string() };
    let mut client = Client::new(&Remote::Url(html_directory_url(url)), HttpOptions::from_env());
    client.missing_ok = true;
    let response = client.send_range(None)?;
    super::REQUESTS.fetch_add(1, Ordering::Relaxed);
    if !matches!(response.status().as_u16(), 200 | 206) {
        return Ok(None);
    }
    let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if !content_type.is_empty() && !crate::vsi::listing::is_html_type(&content_type) {
        return Ok(None);
    }
    const MAX_HTML_BYTES: u64 = crate::vsi::RemoteLister::HTML_BODY_LIMIT as u64;
    let mut data = Vec::new();
    let mut reader = response.into_body().into_reader();
    // An untyped binary is not an HTML index; sniff a bounded prefix before buffering it.
    reader.by_ref().take(crate::vsi::RemoteLister::HTML_SNIFF_BYTES as u64).read_to_end(&mut data)?;
    if !crate::vsi::listing::looks_like_index(&data) {
        super::BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
        return Ok(None);
    }
    reader.take(MAX_HTML_BYTES + 1 - data.len() as u64).read_to_end(&mut data)?;
    super::BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
    if data.len() as u64 > MAX_HTML_BYTES {
        return Err(Error::InvalidData("HTTP directory index exceeds 4 MiB".into()));
    }
    if !String::from_utf8_lossy(&data).to_ascii_lowercase().contains("<a ") {
        return Ok(None);
    }
    let mut out = Vec::new();
    api.read_page(&data, &mut out, &mut Default::default());
    Ok(Some(out))
}

/// The size, date and entity tag of a remote object, from a one-byte range request; `None` when
/// it does not exist.
pub(crate) fn stat(remote: &Remote) -> Result<Option<ObjectStat>> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    client.missing_ok = true;
    let response = client.send_range(Some("bytes=0-0".to_string()))?;
    let status = response.status().as_u16();
    let header = |n: &str| response.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    let modified = header("last-modified").and_then(|d| parse_http_date(&d));
    let etag = header("etag").map(|e| e.trim_matches('"').to_string());
    let content_type =
        header("content-type").map(|t| t.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()).filter(|t| !t.is_empty());
    let total = header("content-range").and_then(|v| super::content_range_total(&v));
    let length = header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
    super::REQUESTS.fetch_add(1, Ordering::Relaxed);
    if matches!(status, 403 | 404) {
        return Ok(None);
    }
    // A server may ignore Range and send its entire object. For 200, the
    // headers are enough; do not consume a potentially huge response body.
    if matches!(status, 206 | 416) {
        let mut body = Vec::new();
        // Read to its end (one byte past the range, at most), so that the connection is reused.
        response.into_body().into_reader().take(2).read_to_end(&mut body)?;
        super::BYTES.fetch_add(body.len() as u64, Ordering::Relaxed);
    }
    let size = match status {
        206 => total,
        200 => length,
        204 => Some(0),
        // 416: a request starting at byte zero is past an empty object's end.
        416 => Some(total.unwrap_or(0)),
        _ => total,
    };
    Ok(Some(ObjectStat { size, modified, etag, content_type }))
}

/// What an upload sends.
pub(crate) enum Payload {
    /// Nothing (a `DELETE`, a server-side copy).
    Empty,
    /// The bytes of a local file, with their SHA-256 (hex) when it is already known.
    File(PathBuf, Option<String>),
    /// Bytes in memory.
    Bytes(Vec<u8>),
}

/// The largest file a signed upload reads into memory, to hash and send it from one read, when
/// its hash is not known. Larger files are sent with an unsigned payload over HTTPS (read once,
/// to send them), and over plain HTTP read once to hash them and again to send them.
const SIGNED_BUFFER_LIMIT: u64 = 8 << 20;

/// The `x-amz-content-sha256` of a SigV4 request whose body the signature does not cover.
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

#[cfg(test)]
thread_local! {
    /// Treats plain HTTP URLs as HTTPS in [`signs_payload`], so that tests can send unsigned
    /// payloads to a local server without TLS.
    static ASSUME_HTTPS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether `url` is sent over TLS.
fn is_https(url: &str) -> bool {
    #[cfg(test)]
    if ASSUME_HTTPS.with(|c| c.get()) {
        return true;
    }
    url.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

/// Whether a SigV4-signed request to `url` covers `payload` (prepared by
/// [`Payload::for_signing`]) with its SHA-256. A local file larger than [`SIGNED_BUFFER_LIMIT`]
/// whose hash is not known is sent over HTTPS with `UNSIGNED-PAYLOAD`, as the AWS SDKs send S3
/// uploads over HTTPS, so that it is read once: TLS protects the body in transit. Over plain
/// HTTP, the signature covers it.
fn signs_payload(url: &str, payload: &Payload) -> bool {
    !(matches!(payload, Payload::File(_, None)) && is_https(url))
}

#[cfg(test)]
thread_local! {
    /// The bytes of payload files read on this thread, other than to send them.
    static FILE_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Reads all of `reader`, counting the bytes in tests.
fn read_counted(mut reader: impl Read, out: &mut Vec<u8>) -> std::io::Result<usize> {
    let n = reader.read_to_end(out)?;
    #[cfg(test)]
    FILE_READS.with(|c| c.set(c.get() + n as u64));
    Ok(n)
}

impl Payload {
    fn len(&self) -> Result<u64> {
        Ok(match self {
            Payload::Empty => 0,
            Payload::File(p, _) => std::fs::metadata(p)?.len(),
            Payload::Bytes(b) => b.len() as u64,
        })
    }

    fn sha256(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        match self {
            Payload::Empty => {}
            Payload::Bytes(b) => h.update(b),
            Payload::File(_, Some(hash)) => return Ok(hash.clone()),
            Payload::File(p, None) => {
                let mut f = std::fs::File::open(p)?;
                let mut buf = Vec::with_capacity(1 << 16);
                loop {
                    buf.clear();
                    if read_counted((&mut f).take(1 << 16), &mut buf)? == 0 {
                        break;
                    }
                    h.update(&buf);
                }
            }
        }
        Ok(s3::hex(&h.finalize()))
    }

    /// A small file of a signed upload whose hash is not known, read into memory, so that it is
    /// hashed and sent from one read; other payloads as they are.
    fn for_signing(self) -> Result<Self> {
        let Payload::File(path, None) = &self else { return Ok(self) };
        if std::fs::metadata(path)?.len() > SIGNED_BUFFER_LIMIT {
            return Ok(self);
        }
        let mut bytes = Vec::new();
        read_counted(std::fs::File::open(path)?.take(SIGNED_BUFFER_LIMIT + 1), &mut bytes)?;
        // Grown since: hashed and sent from the file.
        if bytes.len() as u64 > SIGNED_BUFFER_LIMIT {
            return Ok(self);
        }
        Ok(Payload::Bytes(bytes))
    }
}

/// A writer that computes the SHA-256 of the bytes written through it, so that a file written for
/// a signed upload is not read again to hash it.
pub(crate) struct HashingWriter<W> {
    inner: W,
    hasher: sha2::Sha256,
}

impl<W: std::io::Write> HashingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        use sha2::Digest;
        Self { inner, hasher: sha2::Sha256::new() }
    }

    /// The SHA-256 of what was written, in hex.
    pub(crate) fn finish(self) -> String {
        use sha2::Digest;
        s3::hex(&self.hasher.finalize())
    }
}

impl<W: std::io::Write> std::io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A file's size and modification time, to tell whether it changed while it was uploaded.
fn file_stamp(path: &Path) -> Result<(u64, Option<std::time::SystemTime>)> {
    let meta = std::fs::metadata(path)?;
    Ok((meta.len(), meta.modified().ok()))
}

/// A response to a write request.
struct Reply {
    status: u16,
    body: Vec<u8>,
}

/// The final statuses a write request returns rather than failing with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Accept {
    /// 2xx.
    Success,
    /// 2xx, and 404 (a missing object).
    Missing,
    /// 2xx, and the statuses of a request the service does not implement (400, 405, 501).
    Unimplemented,
}

/// Sends a write request (`PUT`, `DELETE`, `POST`) for an object: S3 objects signed when
/// credentials are set (retried in the bucket's region when another was asked), Azure blobs with
/// the SAS token of their URL. Transient errors are retried; only same-origin write
/// redirects are followed. Bucket-region discovery uses the store’s own endpoint. Statuses
/// other than those `accept`ed are errors.
fn write_request(
    method: &str,
    remote: &Remote,
    query: &str,
    headers: &[(String, String)],
    payload: Payload,
    accept: Accept,
) -> Result<Reply> {
    let auth = match remote {
        Remote::S3(_) => (Credentials::from_env(), s3::region_from_env()),
        Remote::Url(_) => (None, None),
    };
    write_request_with(method, remote, query, headers, payload, accept, auth)
}

/// [`write_request`] with the S3 credentials and region given.
fn write_request_with(
    method: &str,
    remote: &Remote,
    query: &str,
    headers: &[(String, String)],
    payload: Payload,
    accept: Accept,
    (creds, mut region): (Option<Credentials>, Option<String>),
) -> Result<Reply> {
    let options = HttpOptions::from_env();
    let agent = agent(&options);
    let base_url = |region: Option<&str>| match remote {
        Remote::S3(o) => format!("{}{query}", o.url(region)),
        Remote::Url(u) => append_query(u, query),
    };
    // A signed request's file whose hash is not known is read into memory when small. A larger one
    // is sent unsigned over HTTPS, and over plain HTTP read once to hash it and again to send it,
    // as docs/cli.md records. Region discovery and same-origin redirects keep the scheme, so the
    // choice holds for every attempt.
    let payload = if creds.is_some() { payload.for_signing()? } else { payload };
    let payload = &payload;
    let mut url = base_url(region.as_deref());
    let payload_hash = match &creds {
        None => String::new(),
        Some(_) if signs_payload(&url, payload) => payload.sha256()?,
        Some(_) => UNSIGNED_PAYLOAD.to_string(),
    };
    let length = payload.len()?;
    // A signed payload hash also made the service refuse a file that changed while it was sent.
    // Without one, the file's size and modification time are compared before and after.
    let unsigned_file = match payload {
        Payload::File(path, _) if payload_hash == UNSIGNED_PAYLOAD => Some((path, file_stamp(path)?)),
        _ => None,
    };
    let (mut attempt, mut redirects, mut discovered) = (0u32, 0u32, false);
    loop {
        let mut builder = ureq::http::Request::builder().method(method).uri(&url);
        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        if !matches!(payload, Payload::Empty) || method == "PUT" || method == "POST" {
            builder = builder.header("content-length", length.to_string());
        }
        let signed = matches!(remote, Remote::S3(_)) && creds.is_some();
        if let Some(c) = &creds {
            let signed_headers: Vec<(&str, &str)> = headers
                .iter()
                .filter(|(k, _)| k.to_ascii_lowercase().starts_with("x-amz-"))
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            for (k, v) in s3::sign_request(
                method,
                &url,
                &signed_headers,
                &payload_hash,
                c,
                region.as_deref().unwrap_or("us-east-1"),
                &s3::amz_date_now(),
            ) {
                builder = builder.header(k, v);
            }
        }
        let result = match payload {
            Payload::Empty => builder.body(()).map_err(|e| Error::Http(e.to_string())).map(|r| agent.run(r)),
            Payload::Bytes(b) => builder.body(b.as_slice()).map_err(|e| Error::Http(e.to_string())).map(|r| agent.run(r)),
            Payload::File(p, _) => {
                let f = std::fs::File::open(p)?;
                builder.body(f).map_err(|e| Error::Http(e.to_string())).map(|r| agent.run(r))
            }
        }?;
        if options.log {
            match &result {
                Ok(r) => eprintln!(
                    "terrio: {method} {} ({length} bytes{}) -> {}",
                    redact(&url),
                    match (signed, payload_hash == UNSIGNED_PAYLOAD) {
                        (true, true) => ", signed, unsigned payload",
                        (true, false) => ", signed",
                        _ => "",
                    },
                    r.status().as_u16()
                ),
                Err(e) => eprintln!("terrio: {method} {} -> {e}", redact(&url)),
            }
        }
        let response = match result {
            Ok(r) => r,
            Err(e) if attempt < options.max_retry && transient(&e) => {
                std::thread::sleep(options.retry_delay.saturating_mul(1 << attempt.min(16)));
                attempt += 1;
                continue;
            }
            Err(e) => return Err(Error::Http(format!("{}: {e}", redact(&url)))),
        };
        let status = response.status().as_u16();
        let header = |n: &str| response.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        if matches!(remote, Remote::S3(_))
            && !discovered
            && matches!(status, 301 | 307 | 400)
            && let Some(r) = header("x-amz-bucket-region")
            && s3::valid_region(&r)
            && region.as_deref() != Some(r.as_str())
        {
            discovered = true;
            region = Some(r);
            url = base_url(region.as_deref());
            continue;
        }
        if matches!(status, 307 | 308)
            && let Some(location) = header("location")
        {
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(Error::Http(format!("{}: too many redirects", redact(&url))));
            }
            let next = resolve_redirect(&url, &location);
            let origin = crate::vfs::url_origin(&url);
            if origin.is_none() || origin != crate::vfs::url_origin(&next) {
                return Err(Error::Http(format!("{}: refusing a cross-origin write redirect", redact(&url))));
            }
            url = next;
            continue;
        }
        if matches!(status, 429 | 500 | 502 | 503 | 504) && attempt < options.max_retry {
            std::thread::sleep(options.retry_delay.saturating_mul(1 << attempt.min(16)));
            attempt += 1;
            continue;
        }
        let mut body = Vec::new();
        let _ = response.into_body().into_reader().take(1 << 20).read_to_end(&mut body);
        super::REQUESTS.fetch_add(1, Ordering::Relaxed);
        super::BYTES.fetch_add(body.len() as u64, Ordering::Relaxed);
        if (200..300).contains(&status)
            || (accept == Accept::Missing && status == 404)
            || (accept == Accept::Unimplemented && matches!(status, 400 | 405 | 501))
        {
            if (200..300).contains(&status)
                && let Some((path, before)) = &unsigned_file
                && file_stamp(path)? != *before
            {
                return Err(Error::Io(std::io::Error::other(format!(
                    "{}: changed while it was uploaded to {}; the object may not match it, upload it again",
                    path.display(),
                    redact(&url)
                ))));
            }
            return Ok(Reply { status, body });
        }
        let text = String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned();
        let hint = if matches!(remote, Remote::S3(_)) && creds.is_none() && status == 403 {
            " (no AWS credentials found: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or a profile)"
        } else {
            ""
        };
        return Err(Error::Http(format!("{method} {}: status {status}{hint}: {}", redact(&url), text.trim())));
    }
}

/// A URL with query parameters added (before any SAS token's own).
fn append_query(url: &str, query: &str) -> String {
    let query = query.trim_start_matches('?');
    if query.is_empty() {
        return url.to_string();
    }
    if url.contains('?') { format!("{url}&{query}") } else { format!("{url}?{query}") }
}

/// Whether a remote can be written: S3 objects, and Azure blobs.
pub(crate) fn writable(remote: &Remote) -> Result<()> {
    match remote {
        Remote::S3(_) => Ok(()),
        Remote::Url(u) if is_azure(u) => Ok(()),
        Remote::Url(u) if host_of(u).eq_ignore_ascii_case("storage.googleapis.com") => Err(Error::Unsupported(format!(
            "{}: writing to Google Cloud Storage is not supported; use its S3-compatible API (s3:// with \
             AWS_S3_ENDPOINT=https://storage.googleapis.com and HMAC keys)",
            redact(u)
        ))),
        Remote::Url(u) => Err(Error::Unsupported(format!("{}: plain HTTP(S) locations are read only", redact(u)))),
    }
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

fn is_azure(url: &str) -> bool {
    host_of(url).to_ascii_lowercase().ends_with(".blob.core.windows.net")
}

/// Uploads a local file (or bytes) to an object.
pub(crate) fn put(remote: &Remote, payload: Payload) -> Result<()> {
    writable(remote)?;
    let len = payload.len()?;
    if len > MAX_SINGLE_UPLOAD {
        return Err(Error::Unsupported(format!("{len} bytes: uploads larger than 5 GiB (multipart uploads) are not supported")));
    }
    let headers: Vec<(String, String)> = match remote {
        Remote::Url(_) => vec![("x-ms-blob-type".into(), "BlockBlob".into()), ("x-ms-version".into(), "2021-08-06".into())],
        Remote::S3(_) => Vec::new(),
    };
    write_request("PUT", remote, "", &headers, payload, Accept::Success)?;
    Ok(())
}

/// Deletes an object. A missing object is not an error.
pub(crate) fn delete(remote: &Remote) -> Result<()> {
    writable(remote)?;
    let headers: Vec<(String, String)> = match remote {
        Remote::Url(_) => vec![("x-ms-version".into(), "2021-08-06".into())],
        Remote::S3(_) => Vec::new(),
    };
    write_request("DELETE", remote, "", &headers, Payload::Empty, Accept::Missing)?;
    Ok(())
}

/// The most keys of one `DeleteObjects` request, S3's limit.
const MAX_DELETE_KEYS: usize = 1000;

/// Deletes objects of an S3 bucket, request after request in order (S3 does not define the order
/// of the keys within one request): up to 1000 keys per `DeleteObjects` request
/// (`POST ?delete`). A key XML cannot carry (a control character) is deleted with a request of its
/// own, between the batches before and after it, as is every key once the service answers that
/// it does not implement multi-object deletion (some S3-compatible services do not). Missing
/// objects are not errors; a key the service fails to delete is, after its batch. The `groups`
/// are deleted one after another: no request mixes keys of two groups, and a group starts once
/// the previous one is deleted.
pub(crate) fn delete_objects(bucket: &str, groups: &[&[String]]) -> Result<()> {
    let one = |key: &str| delete(&Remote::S3(S3Object { bucket: bucket.to_string(), key: key.to_string() }));
    let mut multi = true;
    // The pending keys, with their XML text.
    let mut batch: Vec<(&str, String)> = Vec::new();
    // Sends the pending batch; one request per key from the first refusal on, as before.
    let flush = |batch: &mut Vec<(&str, String)>, multi: &mut bool| -> Result<()> {
        if !batch.is_empty() && !(*multi && delete_batch(bucket, batch)?) {
            *multi = false;
            batch.iter().try_for_each(|(k, _)| one(k))?;
        }
        batch.clear();
        Ok(())
    };
    for group in groups {
        for key in group.iter() {
            if let Some(xml) = xml_key(key).filter(|_| multi) {
                batch.push((key, xml));
                if batch.len() == MAX_DELETE_KEYS {
                    flush(&mut batch, &mut multi)?;
                }
            } else {
                flush(&mut batch, &mut multi)?;
                one(key)?;
            }
        }
        flush(&mut batch, &mut multi)?;
    }
    Ok(())
}

/// One `DeleteObjects` request, in quiet mode (only failures are reported). `false` when the
/// service does not implement it: 405, 501 or a 400 whose code says so, or a success without a
/// `DeleteResult`. Other statuses (after retries) are errors.
fn delete_batch(bucket: &str, keys: &[(&str, String)]) -> Result<bool> {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><Delete xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Quiet>true</Quiet>"#,
    );
    for (_, key) in keys {
        xml += &format!("<Object><Key>{key}</Key></Object>");
    }
    xml += "</Delete>";
    let body = xml.into_bytes();
    let mut md5 = crate::vsi::md5::Md5::new();
    md5.update(&body);
    let headers = [("content-md5".to_string(), base64(&md5.finish())), ("content-type".to_string(), "application/xml".to_string())];
    let bucket_url = Remote::S3(S3Object { bucket: bucket.to_string(), key: String::new() });
    let reply = write_request("POST", &bucket_url, "?delete", &headers, Payload::Bytes(body), Accept::Unimplemented)?;
    let text = String::from_utf8_lossy(&reply.body);
    let code = crate::vsi::listing::xml_elements(&text, "Code").first().map(|c| c.trim().to_string()).unwrap_or_default();
    let unimplemented = match reply.status {
        200..300 => !text.contains("<DeleteResult"),
        400 => matches!(code.as_str(), "NotImplemented" | "MethodNotAllowed"),
        // The only other statuses write_request returns for this request.
        405 | 501 => true,
        _ => false,
    };
    if unimplemented {
        if HttpOptions::from_env().log {
            eprintln!("terrio: DeleteObjects s3://{bucket} -> {} {code}: deleting one key per request", reply.status);
        }
        return Ok(false);
    }
    if !(200..300).contains(&reply.status) {
        let text: String = text.chars().take(300).collect();
        return Err(Error::Http(format!("POST s3://{bucket}/?delete: status {}: {}", reply.status, text.trim())));
    }
    // Missing keys are deleted already (some services report them). The reply is read up to a
    // size limit, so the count is a lower bound; the first key is always reported.
    let failed: Vec<(String, String)> = crate::vsi::listing::xml_elements(&text, "Error")
        .into_iter()
        .map(|e| {
            let field = |name: &str| crate::vsi::listing::xml_elements(e, name).first().map(|v| crate::vsi::listing::xml_unescape(v));
            (field("Key").unwrap_or_default(), format!("{}: {}", field("Code").unwrap_or_default(), field("Message").unwrap_or_default()))
        })
        .filter(|(_, error)| !error.starts_with("NoSuchKey:"))
        .collect();
    match failed.first() {
        None => Ok(true),
        Some((key, error)) => Err(Error::Http(format!(
            "DeleteObjects: at least {} key(s) not deleted, the first s3://{bucket}/{key}: {}",
            failed.len(),
            error.trim_end_matches(": ")
        ))),
    }
}

/// A key as XML text, or `None` when XML 1.0 cannot carry it. Whitespace other than spaces is
/// written as character references, which XML parsers do not normalize.
fn xml_key(key: &str) -> Option<String> {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' | '\r' => out.push_str(&format!("&#{};", c as u32)),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// Standard base64 with padding (for `Content-MD5`).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Copies an S3 object to another key of the same service, server side (no bytes through this
/// process). Objects up to 5 GiB.
pub(crate) fn s3_copy(from: &S3Object, to: &S3Object, size: u64) -> Result<()> {
    if size > MAX_SINGLE_UPLOAD {
        return Err(Error::Unsupported(format!("{size} bytes: copies larger than 5 GiB (multipart copies) are not supported")));
    }
    let source = format!("/{}/{}", from.bucket, s3::uri_encode(&from.key, false));
    let reply =
        write_request("PUT", &Remote::S3(to.clone()), "", &[("x-amz-copy-source".into(), source)], Payload::Empty, Accept::Success)?;
    // S3 may answer 200 with an error in the body.
    let text = String::from_utf8_lossy(&reply.body);
    if reply.status == 200 && text.contains("<Error>") {
        return Err(Error::Http(format!("copy to s3://{}/{}: {}", to.bucket, to.key, text.trim())));
    }
    Ok(())
}

/// A local path's bytes as an upload.
pub(crate) fn file_payload(path: &Path) -> Payload {
    Payload::File(path.to_path_buf(), None)
}

/// A local path's bytes as an upload, with their SHA-256 (hex), computed as they were written.
pub(crate) fn hashed_file_payload(path: &Path, sha256: String) -> Payload {
    Payload::File(path.to_path_buf(), Some(sha256))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    fn reply(mut stream: TcpStream, status: u16, location: &str) -> Vec<u8> {
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut length = 0;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        write!(stream, "HTTP/1.1 {status} Test\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        body
    }

    /// Answers one request with 200: its request line's path, its headers (names in lower case)
    /// and its body.
    fn signed_reply(mut stream: TcpStream) -> (String, Vec<(String, String)>, Vec<u8>) {
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let (mut length, mut headers, mut line) = (0, Vec::new(), String::new());
        reader.read_line(&mut line).unwrap();
        let path = line.split(' ').nth(1).unwrap().to_string();
        line.clear();
        while reader.read_line(&mut line).unwrap() > 2 {
            if let Some((key, value)) = line.split_once(':') {
                let (key, value) = (key.trim().to_ascii_lowercase(), value.trim().to_string());
                if key == "content-length" {
                    length = value.parse().unwrap();
                }
                headers.push((key, value));
            }
            line.clear();
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        (path, headers, body)
    }

    #[test]
    fn signed_file_uploads_read_the_file_once_when_they_can() {
        use sha2::{Digest, Sha256};
        let tmp = tempfile::tempdir().unwrap();
        let creds = Credentials { access_key_id: "TESTKEY".into(), secret_access_key: "secret".into(), session_token: None };
        let limit = SIGNED_BUFFER_LIMIT as usize;
        // (size, hash known, over HTTPS, bytes read besides sending the file, payload signed): a
        // small file is read into memory once and sent from there; a larger one is read to hash
        // it, then sent, over plain HTTP, and only sent, unsigned, over HTTPS; a file hashed as it
        // was written is only sent, signed.
        for (size, known, https, reads, signed) in [
            (100_000, false, false, 100_000, true),
            (100_000, false, true, 100_000, true),
            (limit, false, true, limit, true),
            (limit + 1, false, false, limit + 1, true),
            (limit + 1, false, true, 0, false),
            (100_000, true, false, 0, true),
            (limit + 1, true, true, 0, true),
        ] {
            let case = format!("{size} bytes, hash known: {known}, HTTPS: {https}");
            let path = tmp.path().join(format!("upload-{size}-{known}-{https}"));
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            std::fs::write(&path, &data).unwrap();
            let sha256 = s3::hex(&Sha256::digest(&data));
            let payload = if known { hashed_file_payload(&path, sha256.clone()) } else { file_payload(&path) };
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let remote = Remote::Url(format!("http://{}/bucket/key", listener.local_addr().unwrap()));
            let server = std::thread::spawn(move || signed_reply(listener.accept().unwrap().0));
            FILE_READS.with(|c| c.set(0));
            ASSUME_HTTPS.with(|c| c.set(https));
            let sent = write_request_with("PUT", &remote, "", &[], payload, Accept::Success, (Some(creds.clone()), None));
            ASSUME_HTTPS.with(|c| c.set(false));
            sent.unwrap();
            let (target, headers, body) = server.join().unwrap();
            assert_eq!(FILE_READS.with(|c| c.get()), reads as u64, "{case}");
            assert!(body == data, "{case}");
            let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap();
            let content_sha256 = header("x-amz-content-sha256");
            assert_eq!(content_sha256, if signed { sha256.as_str() } else { UNSIGNED_PAYLOAD }, "{case}");
            // The canonical request has the same payload hash as the header: recomputed from the
            // headers received, the signature is the one sent.
            let authorization = header("authorization");
            let signed_names = authorization.split("SignedHeaders=").nth(1).unwrap().split(',').next().unwrap();
            assert!(signed_names.split(';').any(|n| n == "x-amz-content-sha256"), "{case}: {authorization}");
            let signed_headers: Vec<(String, String)> = signed_names.split(';').map(|n| (n.to_string(), header(n))).collect();
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            let expected =
                s3::authorization("PUT", path, query, &signed_headers, &content_sha256, &creds, "us-east-1", "s3", &header("x-amz-date"));
            assert_eq!(authorization, expected, "{case}");
        }
    }

    #[test]
    fn unsigned_uploads_fail_when_the_file_changes_while_it_is_sent() {
        let tmp = tempfile::tempdir().unwrap();
        let creds = Credentials { access_key_id: "TESTKEY".into(), secret_access_key: "secret".into(), session_token: None };
        let path = tmp.path().join("growing");
        std::fs::write(&path, vec![7u8; SIGNED_BUFFER_LIMIT as usize + 1]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let remote = Remote::Url(format!("http://{}/bucket/key", listener.local_addr().unwrap()));
        let appended = path.clone();
        // The file grows after its body is received, before the service answers.
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut length, mut line) = (0, String::new());
            while reader.read_line(&mut line).unwrap() > 2 {
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
                line.clear();
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            std::fs::OpenOptions::new().append(true).open(&appended).unwrap().write_all(b"more").unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        ASSUME_HTTPS.with(|c| c.set(true));
        let sent = write_request_with("PUT", &remote, "", &[], file_payload(&path), Accept::Success, (Some(creds), None));
        ASSUME_HTTPS.with(|c| c.set(false));
        server.join().unwrap();
        let Err(err) = sent else { panic!("an upload of a file that changed succeeded") };
        assert!(err.to_string().contains("changed while it was uploaded"), "{err}");
    }

    #[test]
    fn only_https_uploads_of_large_unhashed_files_leave_the_payload_unsigned() {
        let file = file_payload(Path::new("large"));
        assert!(!signs_payload("https://bucket.s3.amazonaws.com/key", &file));
        assert!(!signs_payload("HTTPS://bucket.s3.amazonaws.com/key", &file));
        assert!(signs_payload("http://localhost:9000/bucket/key", &file));
        // In memory (small files), hashed as written, or empty: signed over HTTPS too.
        for payload in [Payload::Bytes(vec![1; 10]), hashed_file_payload(Path::new("large"), "00".into()), Payload::Empty] {
            assert!(signs_payload("https://bucket.s3.amazonaws.com/key", &payload));
        }
    }

    #[test]
    fn delete_requests_escape_keys_and_encode_their_digest() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The MD5 of "hello\n", as `openssl md5 -binary | base64` prints it.
        let mut md5 = crate::vsi::md5::Md5::new();
        md5.update(b"hello\n");
        assert_eq!(base64(&md5.finish()), "sZRqySSS0jR8YjW00mERhA==");
        assert_eq!(xml_key("a&b<c>d\"e'f g\th\ni\rj").unwrap(), "a&amp;b&lt;c&gt;d\"e'f g&#9;h&#10;i&#13;j");
        assert_eq!(xml_key("ctl\u{1}"), None);
        assert_eq!(xml_key("\u{FFFF}"), None);
    }

    #[test]
    fn server_side_copies_refuse_objects_over_the_single_request_limit() {
        // Refused before any request is sent: no server is needed.
        let object = S3Object { bucket: "b".into(), key: "k".into() };
        let err = s3_copy(&object, &object, MAX_SINGLE_UPLOAD + 1).unwrap_err();
        assert!(err.to_string().contains("5 GiB"), "{err}");
    }

    #[test]
    fn stat_uses_200_headers_without_waiting_for_the_body() {
        use std::sync::mpsc;
        for size in [Some(8 * 1024 * 1024), None] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let remote = Remote::Url(format!("http://{}/object", listener.local_addr().unwrap()));
            let (ready, headers_ready) = mpsc::channel();
            let (release, close_body) = mpsc::channel();
            let server = std::thread::spawn(move || {
                let mut stream = listener.accept().unwrap().0;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                }
                let length = size.map(|n| format!("Content-Length: {n}\r\n")).unwrap_or_default();
                write!(stream, "HTTP/1.1 200 OK\r\n{length}Connection: close\r\n\r\n").unwrap();
                ready.send(()).unwrap();
                let _ = close_body.recv_timeout(Duration::from_secs(10));
                // No body or EOF is available until the caller has received stat's result.
            });
            let (result, received) = mpsc::channel();
            let request = std::thread::spawn(move || {
                let _ = result.send(stat(&remote));
            });
            headers_ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let metadata = received.recv_timeout(Duration::from_secs(3));
            let _ = release.send(());
            server.join().unwrap();
            request.join().unwrap();
            assert_eq!(metadata.expect("stat must not wait for body/EOF").unwrap().unwrap().size, size.map(|n| n as u64));
        }
    }

    #[test]
    fn uploads_refuse_cross_origin_redirects_without_sending_the_payload() {
        for status in [307, 308] {
            let target = TcpListener::bind("127.0.0.1:0").unwrap();
            target.set_nonblocking(true).unwrap();
            let destination = format!("http://{}/other", target.local_addr().unwrap());
            let (stop, stopped) = std::sync::mpsc::channel();
            let destination_thread = std::thread::spawn(move || {
                loop {
                    match target.accept() {
                        Ok((stream, _)) => return Some(reply(stream, 200, "")),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => panic!("{e}"),
                    }
                    if stopped.recv_timeout(Duration::from_millis(5)).is_ok() {
                        return None;
                    }
                }
            });
            let source = TcpListener::bind("127.0.0.1:0").unwrap();
            let remote = Remote::Url(format!("http://{}/upload", source.local_addr().unwrap()));
            let source_thread = std::thread::spawn(move || reply(source.accept().unwrap().0, status, &destination));
            let payload = b"upload body".to_vec();
            let result = write_request("PUT", &remote, "", &[], Payload::Bytes(payload.clone()), Accept::Success);
            let _ = stop.send(());
            assert_eq!(source_thread.join().unwrap(), payload);
            assert!(destination_thread.join().unwrap().is_none(), "redirect target must receive no request or payload");
            assert!(result.err().expect("cross-origin redirect must fail").to_string().contains("cross-origin write redirect"));
        }
    }

    #[test]
    fn a_page_that_is_not_a_listing_fails_only_complete_listings() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/bucket", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let mut stream = listener.accept().unwrap().0;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 2 {
                    line.clear();
                }
                let body = "<html><body>Not a bucket</body></html>";
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let api = Api::Url { base, prefix: "dir/".into() };
        // Discovery (Zarr children) and stat (`has_entries`): not listable, as before.
        assert!(list_pages_api(api.clone(), false, None, true).unwrap().is_none());
        assert!(list_pages_api(api.clone(), false, Some(1), false).unwrap().is_none());
        // A complete listing for a mutation refuses to read the page as an empty prefix.
        let err = list_pages_api(api, false, None, false).unwrap_err();
        assert!(err.to_string().contains("not an XML object-store listing"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn uploads_follow_same_origin_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let remote = Remote::Url(format!("http://{}/upload", listener.local_addr().unwrap()));
        let server = std::thread::spawn(move || {
            let first = reply(listener.accept().unwrap().0, 307, "/other");
            let second = reply(listener.accept().unwrap().0, 200, "");
            (first, second)
        });
        let payload = b"upload body".to_vec();
        write_request("PUT", &remote, "", &[], Payload::Bytes(payload.clone()), Accept::Success).unwrap();
        assert_eq!(server.join().unwrap(), (payload.clone(), payload));
    }
}
