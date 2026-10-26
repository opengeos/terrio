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
    /// The size in bytes.
    pub size: u64,
    /// The last modification time, in seconds since the Unix epoch.
    pub modified: Option<i64>,
    /// The entity tag, without quotes.
    pub etag: Option<String>,
}

/// How `dir` is listed.
fn listing_api(dir: &Directory) -> Api {
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

/// The entries below a directory of an object store: one level down (objects and common prefixes),
/// or with `recursive` every object below it, with the "directories" their keys imply. `None`
/// when the store cannot list (a plain HTTP server), or refuses to (a bucket whose listing is not
/// public). Names are relative to the directory.
pub(crate) fn list_objects(dir: &Directory, recursive: bool) -> Result<Option<Vec<ObjectEntry>>> {
    list_pages(dir, recursive, None)
}

/// Whether a directory of an object store holds anything: one listing request for one key.
/// `None` when the store cannot list.
pub(crate) fn has_entries(dir: &Directory) -> Result<Option<bool>> {
    Ok(list_pages(dir, false, Some(1))?.map(|l| !l.is_empty()))
}

/// [`list_objects`]; with `max`, only the first page, of at most `max` entries.
fn list_pages(dir: &Directory, recursive: bool, max: Option<u32>) -> Result<Option<Vec<ObjectEntry>>> {
    let api = listing_api(dir);
    if !api.is_object_store() {
        return Ok(None);
    }
    let mut out: Vec<ObjectEntry> = Vec::new();
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
                let fetched = client.fetch(None)?;
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
            _ => match list_get(&api.request_url_limited(token.as_deref(), recursive, max).unwrap_or_default())? {
                Some(b) => b,
                None => return Ok(None),
            },
        };
        let next = api.read_page(&body, &mut out);
        if max.is_some() {
            break;
        }
        match next {
            Some(_) if page + 1 == MAX_PAGES => {
                crate::warnings::warn(format!("listing stopped after {MAX_PAGES} pages ({} entries)", out.len()));
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
    let fetched = client.fetch(None)?;
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
    let fetched = client.fetch(None)?;
    if !matches!(fetched.status, 200 | 206) || !String::from_utf8_lossy(&fetched.data).to_ascii_lowercase().contains("<a ") {
        return Ok(None);
    }
    let mut out = Vec::new();
    api.read_page(&fetched.data, &mut out);
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
    let total = header("content-range").and_then(|v| super::content_range_total(&v));
    let length = header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
    let mut body = Vec::new();
    let _ = response.into_body().into_reader().take(1 << 20).read_to_end(&mut body);
    super::REQUESTS.fetch_add(1, Ordering::Relaxed);
    super::BYTES.fetch_add(body.len() as u64, Ordering::Relaxed);
    if matches!(status, 403 | 404) {
        return Ok(None);
    }
    let size = match status {
        206 => total.unwrap_or(body.len() as u64),
        200 => length.unwrap_or(body.len() as u64),
        // 416: the range starts past the end, an empty object.
        _ => total.unwrap_or(0),
    };
    Ok(Some(ObjectStat { size, modified, etag }))
}

/// What an upload sends.
pub(crate) enum Payload {
    /// Nothing (a `DELETE`, a server-side copy).
    Empty,
    /// The bytes of a local file.
    File(PathBuf),
    /// Bytes in memory.
    Bytes(Vec<u8>),
}

impl Payload {
    fn len(&self) -> Result<u64> {
        Ok(match self {
            Payload::Empty => 0,
            Payload::File(p) => std::fs::metadata(p)?.len(),
            Payload::Bytes(b) => b.len() as u64,
        })
    }

    fn sha256(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        match self {
            Payload::Empty => {}
            Payload::Bytes(b) => h.update(b),
            Payload::File(p) => {
                let mut f = std::fs::File::open(p)?;
                let mut buf = vec![0u8; 1 << 16];
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
            }
        }
        Ok(s3::hex(&h.finalize()))
    }
}

/// A response to a write request.
struct Reply {
    status: u16,
    body: Vec<u8>,
}

/// Sends a write request (`PUT`, `DELETE`, `POST`) for an object: S3 objects signed when
/// credentials are set (retried in the bucket's region when another was asked), Azure blobs with
/// the SAS token of their URL. Transient errors are retried and redirects followed. Statuses
/// other than 2xx (and 404 when `missing_ok`) are errors.
fn write_request(
    method: &str,
    remote: &Remote,
    query: &str,
    headers: &[(String, String)],
    payload: &Payload,
    missing_ok: bool,
) -> Result<Reply> {
    let options = HttpOptions::from_env();
    let agent = agent(&options);
    let (creds, mut region) = match remote {
        Remote::S3(_) => (Credentials::from_env(), s3::region_from_env()),
        Remote::Url(_) => (None, None),
    };
    let base_url = |region: Option<&str>| match remote {
        Remote::S3(o) => format!("{}{query}", o.url(region)),
        Remote::Url(u) => append_query(u, query),
    };
    let payload_hash = if creds.is_some() { payload.sha256()? } else { String::new() };
    let length = payload.len()?;
    let mut url = base_url(region.as_deref());
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
            Payload::File(p) => {
                let f = std::fs::File::open(p)?;
                builder.body(f).map_err(|e| Error::Http(e.to_string())).map(|r| agent.run(r))
            }
        }?;
        if options.log {
            match &result {
                Ok(r) => eprintln!(
                    "terrio: {method} {} ({length} bytes{}) -> {}",
                    redact(&url),
                    if signed { ", signed" } else { "" },
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
            url = resolve_redirect(&url, &location);
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
        if (200..300).contains(&status) || (missing_ok && status == 404) {
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
    write_request("PUT", remote, "", &headers, &payload, false)?;
    Ok(())
}

/// Deletes an object. A missing object is not an error.
pub(crate) fn delete(remote: &Remote) -> Result<()> {
    writable(remote)?;
    let headers: Vec<(String, String)> = match remote {
        Remote::Url(_) => vec![("x-ms-version".into(), "2021-08-06".into())],
        Remote::S3(_) => Vec::new(),
    };
    write_request("DELETE", remote, "", &headers, &Payload::Empty, true)?;
    Ok(())
}

/// Copies an S3 object to another key of the same service, server side (no bytes through this
/// process). Objects up to 5 GiB.
pub(crate) fn s3_copy(from: &S3Object, to: &S3Object) -> Result<()> {
    let source = format!("/{}/{}", from.bucket, s3::uri_encode(&from.key, false));
    let reply = write_request("PUT", &Remote::S3(to.clone()), "", &[("x-amz-copy-source".into(), source)], &Payload::Empty, false)?;
    // S3 may answer 200 with an error in the body.
    let text = String::from_utf8_lossy(&reply.body);
    if reply.status == 200 && text.contains("<Error>") {
        return Err(Error::Http(format!("copy to s3://{}/{}: {}", to.bucket, to.key, text.trim())));
    }
    Ok(())
}

/// A local path's bytes as an upload.
pub(crate) fn file_payload(path: &Path) -> Payload {
    Payload::File(path.to_path_buf())
}
