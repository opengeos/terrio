//! Reading remote files in place, over HTTP(S) range requests.
//!
//! [`RangeReader`] reads a remote file through `Read` and `Seek` and fetches only the blocks that
//! are read, keeping them in a cache. Adjacent missing blocks are fetched in one request, and
//! [`RangeReader::prefetch`] fetches many ranges (such as the tiles of a window) in as few requests
//! as it can. Servers that ignore range requests get the whole file downloaded once. Requests that
//! fail with a transient error (a connection error, or status 429, 500, 502, 503 or 504) are
//! retried; redirects are followed; S3 objects are signed when credentials are configured (see
//! [`crate::s3`]).
//!
//! Configuration, from environment variables (the GDAL equivalent is read when the terrio one is
//! not set):
//!
//! | Variable | GDAL equivalent | Default |
//! |---|---|---|
//! | `TERRIO_HTTP_TIMEOUT` (seconds, per request) | `GDAL_HTTP_TIMEOUT` | none |
//! | `TERRIO_HTTP_CONNECTTIMEOUT` (seconds) | `GDAL_HTTP_CONNECTTIMEOUT` | 30 |
//! | `TERRIO_HTTP_MAX_RETRY` | `GDAL_HTTP_MAX_RETRY` | 3 |
//! | `TERRIO_HTTP_RETRY_DELAY` (seconds, doubled at each retry) | `GDAL_HTTP_RETRY_DELAY` | 0.5 |
//! | `TERRIO_HTTP_BLOCK_SIZE` (bytes) | `CPL_VSIL_CURL_CHUNK_SIZE` | 65536 |
//! | `TERRIO_HTTP_CACHE_SIZE` (bytes) | `CPL_VSIL_CURL_CACHE_SIZE` | 67108864 |
//! | `TERRIO_HTTP_USERAGENT` | `GDAL_HTTP_USERAGENT` | `terrio/VERSION` |
//! | `TERRIO_HTTP_LOG=YES`: print each request to standard error | `CPL_CURL_VERBOSE` | off |

use crate::s3::{self, Credentials, S3Object};
use crate::{Error, Result};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) mod store;

/// Redirects followed before giving up.
const MAX_REDIRECTS: u32 = 10;

/// Settings of HTTP requests.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// The time limit of a request, body included.
    pub timeout: Option<Duration>,
    /// The time limit to connect.
    pub connect_timeout: Duration,
    /// How many times a request that failed with a transient error is retried.
    pub max_retry: u32,
    /// The wait before the first retry; it doubles at each retry.
    pub retry_delay: Duration,
    /// The size of the blocks fetched and cached, in bytes.
    pub block_size: u64,
    /// The size of the block cache of each file, in bytes.
    pub cache_size: u64,
    /// The `User-Agent` header.
    pub user_agent: String,
    /// Print each request to standard error.
    pub log: bool,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            timeout: None,
            connect_timeout: Duration::from_secs(30),
            max_retry: 3,
            retry_delay: Duration::from_millis(500),
            block_size: 64 * 1024,
            cache_size: 64 * 1024 * 1024,
            user_agent: format!("terrio/{}", crate::VERSION),
            log: false,
        }
    }
}

impl HttpOptions {
    /// The options set by environment variables (see the [module documentation](self)).
    pub fn from_env() -> Self {
        let d = Self::default();
        let var = |terrio: &str, gdal: &str| {
            std::env::var(terrio).ok().or_else(|| std::env::var(gdal).ok()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        };
        let secs = |v: String| v.parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 0.0).map(Duration::from_secs_f64);
        Self {
            timeout: var("TERRIO_HTTP_TIMEOUT", "GDAL_HTTP_TIMEOUT").and_then(secs).filter(|t| !t.is_zero()),
            connect_timeout: var("TERRIO_HTTP_CONNECTTIMEOUT", "GDAL_HTTP_CONNECTTIMEOUT").and_then(secs).unwrap_or(d.connect_timeout),
            max_retry: var("TERRIO_HTTP_MAX_RETRY", "GDAL_HTTP_MAX_RETRY").and_then(|v| v.parse().ok()).unwrap_or(d.max_retry),
            retry_delay: var("TERRIO_HTTP_RETRY_DELAY", "GDAL_HTTP_RETRY_DELAY").and_then(secs).unwrap_or(d.retry_delay),
            block_size: var("TERRIO_HTTP_BLOCK_SIZE", "CPL_VSIL_CURL_CHUNK_SIZE")
                .and_then(|v| v.parse().ok())
                .filter(|b| *b >= 1024)
                .unwrap_or(d.block_size),
            cache_size: var("TERRIO_HTTP_CACHE_SIZE", "CPL_VSIL_CURL_CACHE_SIZE").and_then(|v| v.parse().ok()).unwrap_or(d.cache_size),
            user_agent: var("TERRIO_HTTP_USERAGENT", "GDAL_HTTP_USERAGENT").unwrap_or(d.user_agent),
            log: var("TERRIO_HTTP_LOG", "CPL_CURL_VERBOSE")
                .is_some_and(|v| matches!(v.to_ascii_uppercase().as_str(), "YES" | "ON" | "TRUE" | "1")),
        }
    }
}

/// A remote file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    /// An HTTP(S) URL.
    Url(String),
    /// An object in Amazon S3 (or an S3-compatible service), signed when credentials are set.
    S3(S3Object),
}

/// The requests and bytes fetched over HTTP by this process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// The number of requests that returned data.
    pub requests: u64,
    /// The number of body bytes received.
    pub bytes: u64,
}

static REQUESTS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

/// The requests made and bytes received over HTTP by this process so far.
pub fn fetch_stats() -> FetchStats {
    FetchStats { requests: REQUESTS.load(Ordering::Relaxed), bytes: BYTES.load(Ordering::Relaxed) }
}

/// A response that returned data.
struct Fetched {
    /// The HTTP status: 200 (the whole file), 206 (a range) or 416 (the range starts past the end).
    status: u16,
    /// The file's length, from `Content-Range` (or the body's length when the status is 200).
    total_len: Option<u64>,
    data: Vec<u8>,
}

/// Makes requests for one remote file: signing, redirects, region discovery and retries.
struct Client {
    agent: ureq::Agent,
    options: HttpOptions,
    remote: Remote,
    /// The URL requests go to (after S3 region discovery or a redirect).
    url: String,
    /// A query added to an S3 object's URL (`?list-type=2&...` for a listing), or empty.
    query: String,
    s3: Option<S3Auth>,
    /// Whether a missing object (status 404, or 403 as anonymous S3 requests get for missing
    /// keys) is an answer rather than an error.
    missing_ok: bool,
    refused_ok: bool,
}

struct S3Auth {
    creds: Option<Credentials>,
    region: Option<String>,
    /// Whether the region was learned from the server (and so is not retried).
    discovered: bool,
}

impl Client {
    fn new(remote: &Remote, options: HttpOptions) -> Self {
        let agent = agent(&options);
        Self::with_agent(remote, options, agent)
    }

    /// A client sending its requests through `agent` (whose connections it shares).
    fn with_agent(remote: &Remote, options: HttpOptions, agent: ureq::Agent) -> Self {
        let auth = matches!(remote, Remote::S3(_)).then(|| (Credentials::from_env(), s3::region_from_env()));
        Self::with_auth(remote, options, agent, auth)
    }

    /// A client with S3 credentials and region already resolved (`auth`, for S3 objects).
    fn with_auth(remote: &Remote, options: HttpOptions, agent: ureq::Agent, auth: Option<(Option<Credentials>, Option<String>)>) -> Self {
        let (url, s3) = match remote {
            Remote::Url(u) => (u.clone(), None),
            Remote::S3(o) => {
                let (creds, region) = auth.unwrap_or_else(|| (Credentials::from_env(), s3::region_from_env()));
                (o.url(region.as_deref()), Some(S3Auth { creds, region, discovered: false }))
            }
        };
        Self { agent, options, remote: remote.clone(), url, query: String::new(), s3, missing_ok: false, refused_ok: false }
    }

    /// Sends a `GET` (of the inclusive byte range `range`, if any) and returns the response once it
    /// has a final status: 200, 206 or 416. Other statuses are errors.
    fn send(&mut self, range: Option<(u64, u64)>) -> Result<ureq::http::Response<ureq::Body>> {
        self.send_range(range.map(|(a, b)| format!("bytes={a}-{b}")))
    }

    /// Sends a `GET` with a `Range` header value (`bytes=0-99`, `bytes=-16`), if any. With
    /// `missing_ok`, a missing object's response (404, or 403 to an unsigned request) is returned
    /// too; with `refused_ok`, any 403.
    fn send_range(&mut self, range_header: Option<String>) -> Result<ureq::http::Response<ureq::Body>> {
        let started = Instant::now();
        let mut url = self.url.clone();
        let mut redirects = 0;
        let mut attempt = 0;
        loop {
            let mut req = self.agent.get(&url);
            if let Some(r) = &range_header {
                req = req.header("Range", r);
            }
            // Only the object's own URL is signed, not where a redirect leads.
            let mut signed = false;
            if let Some(S3Auth { creds: Some(creds), region, .. }) = &self.s3
                && url == self.url
            {
                signed = true;
                let signed: Vec<(&str, &str)> = range_header.iter().map(|r| ("range", r.as_str())).collect();
                let region = region.as_deref().unwrap_or("us-east-1");
                for (k, v) in s3::sign_get(&url, &signed, creds, region, &s3::amz_date_now()) {
                    req = req.header(k, v);
                }
            }
            let result = req.call();
            let status = result.as_ref().map(|r| r.status().as_u16()).ok();
            if self.options.log {
                let range = range_header.as_deref().unwrap_or("whole file");
                let signed = if signed { ", signed" } else { "" };
                match &result {
                    Ok(r) => {
                        eprintln!(
                            "terrio: GET {} ({range}{signed}) -> {} in {} ms",
                            redact(&url),
                            r.status().as_u16(),
                            started.elapsed().as_millis()
                        )
                    }
                    Err(e) => eprintln!("terrio: GET {} ({range}{signed}) -> {e}", redact(&url)),
                }
            }
            let response = match result {
                Ok(r) => r,
                Err(e) if attempt < self.options.max_retry && transient(&e) => {
                    self.backoff(&mut attempt);
                    continue;
                }
                Err(e) => return Err(Error::Http(format!("{}: {e}", redact(&url)))),
            };
            let status = status.unwrap_or_default();
            let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
            // An S3 bucket in another region than the one asked: retry there.
            if let (Remote::S3(object), Some(auth)) = (&self.remote, self.s3.as_mut())
                && !auth.discovered
                && matches!(status, 301 | 307 | 400)
                && let Some(region) = header("x-amz-bucket-region")
                && s3::valid_region(&region)
                && auth.region.as_deref() != Some(region.as_str())
            {
                auth.discovered = true;
                auth.region = Some(region);
                self.url = format!("{}{}", object.url(auth.region.as_deref()), self.query);
                url = self.url.clone();
                continue;
            }
            if (300..400).contains(&status)
                && let Some(location) = header("location")
            {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(Error::Http(format!("{}: too many redirects", redact(&self.url))));
                }
                url = resolve_redirect(&url, &location);
                continue;
            }
            match status {
                200 | 206 | 416 => return Ok(response),
                404 if self.missing_ok => return Ok(response),
                // S3 (and GCS) answer anonymous requests for missing keys with 403; a signed
                // request's 403 is a denied one (expired or insufficient credentials).
                403 if self.missing_ok && !signed => return Ok(response),
                // A refused signed request, returned to a caller that handles it.
                403 if self.refused_ok => return Ok(response),
                429 | 500 | 502 | 503 | 504 if attempt < self.options.max_retry => self.backoff(&mut attempt),
                _ => {
                    let hint = match (&self.s3, status) {
                        (Some(S3Auth { creds: None, .. }), 403) => {
                            " (no AWS credentials found: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or a profile)"
                        }
                        _ => "",
                    };
                    return Err(Error::Http(format!("{}: status {status}{hint}", redact(&url))));
                }
            }
        }
    }

    fn backoff(&self, attempt: &mut u32) {
        std::thread::sleep(self.options.retry_delay.saturating_mul(1 << (*attempt).min(16)));
        *attempt += 1;
    }

    /// Fetches the inclusive byte range `range` (or the whole file), retrying when the transfer
    /// breaks off.
    fn fetch(&mut self, range: Option<(u64, u64)>) -> Result<Fetched> {
        self.fetch_range(range.map(|(a, b)| format!("bytes={a}-{b}")))
    }

    /// Fetches with a `Range` header value, if any: see [`Client::send_range`].
    fn fetch_range(&mut self, range: Option<String>) -> Result<Fetched> {
        self.fetch_range_limited(range, None)
    }

    fn fetch_range_limited(&mut self, range: Option<String>, limit: Option<usize>) -> Result<Fetched> {
        let mut attempt = 0;
        loop {
            let response = self.send_range(range.clone())?;
            let status = response.status().as_u16();
            let content_range = response.headers().get("content-range").and_then(|v| v.to_str().ok()).and_then(content_range_total);
            let mut data = Vec::new();
            let reader = response.into_body().into_reader();
            match reader.take(limit.map_or(u64::MAX, |n| n as u64 + 1)).read_to_end(&mut data) {
                Ok(_) => {}
                Err(_) if attempt < self.options.max_retry => {
                    self.backoff(&mut attempt);
                    continue;
                }
                Err(e) => return Err(Error::Http(format!("{}: {e}", redact(&self.url)))),
            }
            REQUESTS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
            if limit.is_some_and(|n| data.len() > n) {
                return Err(Error::InvalidData("object-store listing page exceeds 16 MiB".into()));
            }
            if self.options.log {
                eprintln!("terrio:   received {} bytes", data.len());
            }
            let total_len = if status == 200 { Some(data.len() as u64) } else { content_range };
            return Ok(Fetched { status, total_len, data });
        }
    }
}

/// The agent requests are sent with: no automatic redirects (followed by [`Client`], which
/// signs only the first request), no compression.
fn agent(options: &HttpOptions) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .accept_encoding("identity")
        .user_agent(options.user_agent.as_str())
        .timeout_global(options.timeout)
        .timeout_connect(Some(options.connect_timeout))
        .build()
        .into()
}

/// The part of an object to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ByteRange {
    /// The whole object.
    Whole,
    /// `length` bytes from `offset`.
    At(u64, u64),
    /// The last `n` bytes.
    Suffix(u64),
}

/// Fetches many small objects (such as the chunks of a Zarr store) through one pool of
/// connections, from any number of threads. An S3 bucket's region, once learned, is reused.
pub(crate) struct ObjectFetcher {
    agent: ureq::Agent,
    options: HttpOptions,
    region: Mutex<Option<String>>,
    /// S3 credentials and region from the environment, read once rather than for every object.
    auth: (Option<Credentials>, Option<String>),
}

impl ObjectFetcher {
    /// A fetcher with the options and S3 credentials from the environment.
    pub(crate) fn new() -> Self {
        let options = HttpOptions::from_env();
        let auth = (Credentials::from_env(), s3::region_from_env());
        Self { agent: agent(&options), options, region: Mutex::new(None), auth }
    }

    /// Fetches an object, or part of it. `None` when the object does not exist (status 404, or
    /// 403 as S3 answers anonymous requests for missing keys; a signed request's 403 is an error).
    pub(crate) fn get(&self, remote: &Remote, range: ByteRange) -> Result<Option<Vec<u8>>> {
        let mut client = Client::with_auth(remote, self.options.clone(), self.agent.clone(), Some(self.auth.clone()));
        client.missing_ok = true;
        let known = self.region.lock().ok().and_then(|r| r.clone());
        if let (Remote::S3(object), Some(auth), Some(region)) = (remote, client.s3.as_mut(), known) {
            auth.discovered = true;
            client.url = object.url(Some(&region));
            auth.region = Some(region);
        }
        let header = match range {
            ByteRange::Whole => None,
            ByteRange::At(_, 0) | ByteRange::Suffix(0) => return Ok(Some(Vec::new())),
            ByteRange::At(offset, len) => {
                let last = offset.checked_add(len - 1).ok_or_else(|| Error::Http(format!("bytes {offset}+{len}: out of range")))?;
                Some(format!("bytes={offset}-{last}"))
            }
            ByteRange::Suffix(n) => Some(format!("bytes=-{n}")),
        };
        let fetched = client.fetch_range(header)?;
        if let Some(S3Auth { region: Some(r), discovered: true, .. }) = &client.s3
            && let Ok(mut known) = self.region.lock()
        {
            *known = Some(r.clone());
        }
        match (fetched.status, range) {
            (403 | 404, _) => Ok(None),
            (200 | 206, ByteRange::Whole) => Ok(Some(fetched.data)),
            // A server that ignores ranges sends the whole object: cut the part asked.
            (200, ByteRange::At(offset, len)) => {
                let past = || Error::Http(format!("{}: bytes {offset}+{len} past the end", redact(&client.url)));
                let start = usize::try_from(offset).map_err(|_| past())?;
                let end = offset.checked_add(len).and_then(|e| usize::try_from(e).ok()).ok_or_else(past)?;
                Ok(Some(fetched.data.get(start..end).ok_or_else(past)?.to_vec()))
            }
            (200, ByteRange::Suffix(n)) => {
                let d = fetched.data;
                Ok(Some(d[d.len().saturating_sub(n as usize)..].to_vec()))
            }
            (206, _) => Ok(Some(fetched.data)),
            (status, _) => Err(Error::Http(format!("{}: unexpected status {status}", redact(&client.url)))),
        }
    }
}

/// Whether a request error is worth retrying.
fn transient(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed | ureq::Error::Protocol(_))
}

/// The absolute URL a `Location` header points to.
fn resolve_redirect(from: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }
    let (scheme, rest) = from.split_once("://").unwrap_or(("https", from));
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if location.starts_with('/') {
        format!("{scheme}://{host}{location}")
    } else {
        let base = from.split(['?', '#']).next().unwrap_or(from);
        format!("{}/{location}", base.rsplit_once('/').map_or(base, |(b, _)| b))
    }
}

/// A URL for messages: without its query when the query holds a signature (such as an Azure SAS
/// token or a presigned URL's).
fn redact(url: &str) -> std::borrow::Cow<'_, str> {
    crate::vsi::listing::redact_url(url)
}

/// The total length in a `Content-Range` header (`bytes 0-99/1234` or `bytes */1234`).
fn content_range_total(v: &str) -> Option<u64> {
    v.rsplit_once('/')?.1.trim().parse().ok()
}

/// Downloads a remote file to a local path.
pub(crate) fn download(remote: &Remote, path: &std::path::Path) -> Result<()> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    let response = client.send(None)?;
    let mut file = std::fs::File::create(path)?;
    let n = io::copy(&mut response.into_body().into_reader(), &mut file)?;
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(n, Ordering::Relaxed);
    if client.options.log {
        eprintln!("terrio:   received {n} bytes");
    }
    Ok(())
}

/// Streams a remote file into `out`; returns the bytes written.
pub(crate) fn download_to(remote: &Remote, out: &mut dyn io::Write) -> Result<u64> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    let response = client.send(None)?;
    let n = io::copy(&mut response.into_body().into_reader(), out).map_err(|e| Error::Http(format!("{}: {e}", redact(&client.url))))?;
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(n, Ordering::Relaxed);
    if client.options.log {
        eprintln!("terrio:   received {n} bytes");
    }
    Ok(n)
}

/// Run one sequential archive pass against a streamed response, without range reads.
pub(crate) fn with_reader<T>(remote: &Remote, read: impl FnOnce(&mut dyn Read) -> Result<T>) -> Result<T> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    let response = client.send(None)?;
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    let mut reader = MeasuredReader { inner: response.into_body().into_reader(), bytes: 0 };
    let result = read(&mut reader);
    BYTES.fetch_add(reader.bytes, Ordering::Relaxed);
    if client.options.log {
        eprintln!("terrio:   received {} bytes", reader.bytes);
    }
    result
}

struct MeasuredReader<R> {
    inner: R,
    bytes: u64,
}
impl<R: Read> Read for MeasuredReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes = self.bytes.saturating_add(n as u64);
        Ok(n)
    }
}

/// Fetches a whole remote file into memory (a STAC document, for instance).
pub(crate) fn get(remote: &Remote) -> Result<Vec<u8>> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    Ok(client.fetch(None)?.data)
}

/// A "directory" of an object store: the objects below a prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Directory {
    /// A prefix of an S3 bucket (`a/b`, without a trailing slash; empty for the bucket's root),
    /// listed with signed requests when credentials are set.
    S3 { bucket: String, prefix: String },
    /// An HTTP(S) URL of a directory in a store that lists objects: Amazon S3
    /// (`bucket.s3.amazonaws.com`), Google Cloud Storage (`storage.googleapis.com/bucket`) or
    /// Azure Blob Storage (`account.blob.core.windows.net/container`, with a SAS token in its
    /// query).
    Url(String),
}

/// The names of the "subdirectories" of a directory of an object store (the common prefixes of
/// its keys, one level down), as GDAL's `VSIReadDir` lists them on `/vsis3/`, `/vsigs/` and
/// `/vsiaz/`. `None` when the store cannot list (a plain HTTP server), or refuses to (a bucket
/// whose listing is not public).
pub(crate) fn list_subdirectories(dir: &Directory) -> Result<Option<Vec<String>>> {
    let Some(entries) = store::list_objects_for_discovery(dir, false)? else { return Ok(None) };
    let mut out: Vec<String> = entries.into_iter().filter(|e| e.dir).map(|e| e.name).collect();
    out.sort();
    out.dedup();
    Ok(Some(out))
}

/// Sends a `POST` of a JSON body (a STAC API search, for instance) and returns the response's
/// body. Redirects that keep the method (307 and 308) are followed, and transient errors retried.
pub(crate) fn post_json(url: &str, body: &[u8], headers: &[(String, String)]) -> Result<Vec<u8>> {
    request("POST", url, Some(body), headers)
}

/// The seconds a `Retry-After` header asks to wait: a number of seconds, or an HTTP date
/// (`Sun, 06 Nov 1994 08:49:37 GMT`) from now. `None` for a date past or a value unreadable.
fn retry_after_seconds(v: &str) -> Option<u64> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(secs);
    }
    // IMF-fixdate: day name, day, month, year, time, GMT.
    let f: Vec<&str> = v.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = f[..] else { return None };
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m = months.iter().position(|x| x.eq_ignore_ascii_case(month))? as i64 + 1;
    let (d, y): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let t: Vec<i64> = time.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [h, mi, sec] = t[..] else { return None };
    seconds_until_utc(y, m, d, h, mi, sec)
}

/// The seconds from now to a UTC date and time; `None` when it is past.
pub(crate) fn seconds_until_utc(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<u64> {
    let at = crate::civil_time::checked_unix_seconds(y, m, d, h, mi, s)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    u64::try_from(at - now).ok()
}

/// Fetches a whole remote file with extra request headers (an API key, for instance), following
/// redirects and retrying transient errors.
pub(crate) fn get_with_headers(url: &str, headers: &[(String, String)]) -> Result<Vec<u8>> {
    if headers.is_empty() {
        return get(&Remote::Url(url.to_string()));
    }
    request("GET", url, None, headers)
}

/// Sends a `GET` or a `POST` (of a JSON `body`) with extra `headers`, and returns the response's
/// body: redirects followed (for a `POST`, only those keeping the method, 307 and 308), the extra
/// headers dropped when a redirect leads to another origin (they may hold keys), and transient
/// errors and statuses (429, 500, 502, 503, 504) retried.
fn request(method: &str, url: &str, body: Option<&[u8]>, headers: &[(String, String)]) -> Result<Vec<u8>> {
    let options = HttpOptions::from_env();
    let agent = agent(&options);
    let first_origin = crate::vfs::url_origin(url);
    let mut url = url.to_string();
    let (mut redirects, mut attempt) = (0, 0);
    let backoff = |attempt: &mut u32| {
        std::thread::sleep(options.retry_delay.saturating_mul(1 << (*attempt).min(16)));
        *attempt += 1;
    };
    loop {
        let extra: &[(String, String)] = if crate::vfs::url_origin(&url) == first_origin { headers } else { &[] };
        let result = match body {
            Some(body) => {
                let mut req =
                    agent.post(&url).header("Content-Type", "application/json").header("Accept", "application/geo+json, application/json");
                for (k, v) in extra {
                    req = req.header(k, v);
                }
                req.send(body)
            }
            None => {
                let mut req = agent.get(&url).header("Accept", "application/geo+json, application/json");
                for (k, v) in extra {
                    req = req.header(k, v);
                }
                req.call()
            }
        };
        if options.log {
            match &result {
                Ok(r) => eprintln!("terrio: {method} {} -> {}", redact(&url), r.status().as_u16()),
                Err(e) => eprintln!("terrio: {method} {} -> {e}", redact(&url)),
            }
        }
        let response = match result {
            Ok(r) => r,
            Err(e) if attempt < options.max_retry && transient(&e) => {
                backoff(&mut attempt);
                continue;
            }
            Err(e) => return Err(Error::Http(format!("{}: {e}", redact(&url)))),
        };
        let status = response.status().as_u16();
        // A POST follows only 307 and 308, which keep the method and body.
        let follows = if body.is_some() { matches!(status, 307 | 308) } else { matches!(status, 301..=303 | 307 | 308) };
        if follows && let Some(location) = response.headers().get("location").and_then(|v| v.to_str().ok()) {
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(Error::Http(format!("{}: too many redirects", redact(&url))));
            }
            url = resolve_redirect(&url, location);
            continue;
        }
        if body.is_some() && matches!(status, 301..=303) {
            let to = response.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("?");
            return Err(Error::Http(format!(
                "{}: status {status}, a redirect to {} that would turn the POST into a GET: not followed",
                redact(&url),
                redact(to)
            )));
        }
        if matches!(status, 429 | 500 | 502 | 503 | 504) && attempt < options.max_retry {
            // A rate limit's `Retry-After` (seconds or a date, at most a minute), else the backoff.
            match response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(retry_after_seconds) {
                Some(secs) => {
                    std::thread::sleep(std::time::Duration::from_secs(secs.min(60)));
                    attempt += 1;
                }
                None => backoff(&mut attempt),
            }
            continue;
        }
        let mut data = Vec::new();
        response.into_body().into_reader().read_to_end(&mut data).map_err(|e| Error::Http(format!("{}: {e}", redact(&url))))?;
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&data[..data.len().min(300)]).into_owned();
            return Err(Error::Http(format!("{}: status {status}: {}", redact(&url), text.trim())));
        }
        return Ok(data);
    }
}

/// A remote file read in place: `Read` and `Seek` over HTTP range requests, with a block cache.
///
/// Clones share the cache and connection, each with its own position.
#[derive(Clone)]
pub struct RangeReader {
    shared: Arc<Mutex<Shared>>,
    len: u64,
    pos: u64,
}

impl std::fmt::Debug for RangeReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeReader").field("len", &self.len).field("pos", &self.pos).finish_non_exhaustive()
    }
}

struct Shared {
    client: Client,
    len: u64,
    block_size: u64,
    max_blocks: usize,
    blocks: HashMap<u64, Vec<u8>>,
    /// Cached blocks, oldest first, for eviction.
    order: VecDeque<u64>,
    /// The whole file, when the server ignores range requests.
    whole: Option<Vec<u8>>,
}

impl RangeReader {
    /// Opens a remote file with the options from the environment, fetching its first block.
    pub fn open(remote: &Remote) -> Result<Self> {
        Self::open_with(remote, HttpOptions::from_env())
    }

    /// Opens a remote file, fetching its first block.
    pub fn open_with(remote: &Remote, options: HttpOptions) -> Result<Self> {
        let block_size = options.block_size.max(1);
        let max_blocks = (options.cache_size / block_size).max(16) as usize;
        let mut client = Client::new(remote, options);
        let first = client.fetch(Some((0, block_size - 1)))?;
        let mut shared = Shared { client, len: 0, block_size, max_blocks, blocks: HashMap::new(), order: VecDeque::new(), whole: None };
        match first.status {
            206 => {
                shared.len = first
                    .total_len
                    .ok_or_else(|| Error::Http(format!("{}: no Content-Range in a 206 response", redact(&shared.client.url))))?;
                shared.store(0, first.data);
            }
            200 => {
                shared.len = first.data.len() as u64;
                shared.whole = Some(first.data);
            }
            // The range starts past the end: an empty file.
            _ => shared.len = first.total_len.unwrap_or(0),
        }
        Ok(Self { len: shared.len, shared: Arc::new(Mutex::new(shared)), pos: 0 })
    }

    /// The file's length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The URL requests go to.
    pub fn url(&self) -> String {
        self.lock().map(|s| s.client.url.clone()).unwrap_or_default()
    }

    /// Fetches the blocks covering the byte ranges `(offset, length)` that are not cached yet, in as
    /// few requests as it can: runs of missing blocks separated by at most two cached blocks are
    /// fetched together.
    pub fn prefetch(&self, ranges: &[(u64, u64)]) -> io::Result<()> {
        let mut s = self.lock()?;
        if s.whole.is_some() {
            return Ok(());
        }
        s.evict();
        let bs = s.block_size;
        let mut wanted: Vec<u64> = ranges
            .iter()
            .filter(|(off, len)| *len > 0 && *off < s.len)
            .flat_map(|(off, len)| *off / bs..=((off + len).min(s.len) - 1) / bs)
            .filter(|b| !s.blocks.contains_key(b))
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        s.fetch_blocks(&wanted, 2)
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, Shared>> {
        self.shared.lock().map_err(|_| io::Error::other("the HTTP block cache is poisoned"))
    }
}

impl Shared {
    /// Drops the oldest blocks until the cache is within its size. Called before a read or a
    /// prefetch, never during one, so that the blocks being read stay.
    fn evict(&mut self) {
        while self.order.len() > self.max_blocks
            && let Some(old) = self.order.pop_front()
        {
            self.blocks.remove(&old);
        }
    }

    fn store(&mut self, first_block: u64, data: Vec<u8>) {
        let bs = self.block_size as usize;
        for (i, chunk) in data.chunks(bs).enumerate() {
            let b = first_block + i as u64;
            if self.blocks.insert(b, chunk.to_vec()).is_none() {
                self.order.push_back(b);
            }
        }
    }

    /// Fetches the given blocks (sorted, not cached), merging runs separated by at most `gap`
    /// blocks into one request.
    fn fetch_blocks(&mut self, blocks: &[u64], gap: u64) -> io::Result<()> {
        let mut i = 0;
        while i < blocks.len() {
            let start = blocks[i];
            let mut end = start;
            while i + 1 < blocks.len() && blocks[i + 1] <= end + 1 + gap {
                i += 1;
                end = blocks[i];
            }
            i += 1;
            let first = start * self.block_size;
            let last = ((end + 1) * self.block_size).min(self.len) - 1;
            let fetched = self.client.fetch(Some((first, last))).map_err(io::Error::other)?;
            match fetched.status {
                206 if fetched.data.len() as u64 == last - first + 1 => self.store(start, fetched.data),
                200 if fetched.data.len() as u64 == self.len => {
                    self.whole = Some(fetched.data);
                    return Ok(());
                }
                status => {
                    return Err(io::Error::other(format!(
                        "{}: bytes {first}-{last}: unexpected response (status {status}, {} bytes)",
                        redact(&self.client.url),
                        fetched.data.len()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Copies the bytes at `pos` into `buf`, fetching what is missing. Returns the count copied.
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        if pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let end = (pos + buf.len() as u64).min(self.len);
        if self.whole.is_none() {
            self.evict();
            let bs = self.block_size;
            let missing: Vec<u64> = (pos / bs..=(end - 1) / bs).filter(|b| !self.blocks.contains_key(b)).collect();
            self.fetch_blocks(&missing, 0)?;
        }
        if let Some(whole) = &self.whole {
            let n = (end - pos) as usize;
            buf[..n].copy_from_slice(&whole[pos as usize..end as usize]);
            return Ok(n);
        }
        let mut at = pos;
        while at < end {
            let b = at / self.block_size;
            let block = self.blocks.get(&b).ok_or_else(|| io::Error::other("block missing from the cache"))?;
            let off = (at - b * self.block_size) as usize;
            let n = ((end - at) as usize).min(block.len().saturating_sub(off));
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short block"));
            }
            let dst = (at - pos) as usize;
            buf[dst..dst + n].copy_from_slice(&block[off..off + n]);
            at += n as u64;
        }
        Ok((end - pos) as usize)
    }
}

impl Read for RangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.lock()?.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for RangeReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = new.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the file"))?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vsi::listing::{xml_elements, xml_unescape};

    #[test]
    fn reads_retry_after() {
        assert_eq!(retry_after_seconds(" 120 "), Some(120));
        assert_eq!(retry_after_seconds("Sun, 06 Nov 1994 08:49:37 GMT"), None);
        let soon = retry_after_seconds("Fri, 01 Jan 2100 00:00:00 GMT").expect("a date to come");
        assert!(soon > 2_000_000_000, "{soon}");
        assert_eq!(retry_after_seconds("soon"), None);
    }
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn reads_listing_xml() {
        let xml = "<ListBucketResult><IsTruncated>true</IsTruncated><CommonPrefixes><Prefix>a/b&amp;c/</Prefix></CommonPrefixes>\
                   <CommonPrefixes><Prefix>a/d/</Prefix></CommonPrefixes><NextContinuationToken>t1</NextContinuationToken></ListBucketResult>";
        let prefixes: Vec<String> =
            xml_elements(xml, "CommonPrefixes").iter().flat_map(|b| xml_elements(b, "Prefix")).map(xml_unescape).collect();
        assert_eq!(prefixes, ["a/b&c/", "a/d/"]);
        assert_eq!(xml_elements(xml, "NextContinuationToken"), ["t1"]);
        assert!(xml_elements("<a>", "a").is_empty());
    }

    /// Lists real public directories of Amazon S3, Google Cloud Storage and Azure Blob Storage
    /// (with a Planetary Computer SAS token): `cargo test -p terrio --lib lists_object_stores -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn lists_object_stores() {
        let s3 = list_subdirectories(&Directory::S3 {
            bucket: "hrrrzarr".into(),
            prefix: "sfc/20240101/20240101_00z_anl.zarr/2m_above_ground".into(),
        })
        .expect("S3 listing")
        .expect("listable");
        assert!(s3.contains(&"TMP".to_string()), "{s3:?}");
        let url = "https://hrrrzarr.s3.amazonaws.com/sfc/20240101/20240101_00z_anl.zarr/2m_above_ground";
        assert_eq!(list_subdirectories(&Directory::Url(url.into())).expect("S3 URL listing"), Some(s3.clone()));
        // Path-style S3 URLs.
        let path_style = "https://s3.us-west-1.amazonaws.com/hrrrzarr/sfc/20240101/20240101_00z_anl.zarr/2m_above_ground";
        assert_eq!(list_subdirectories(&Directory::Url(path_style.into())).expect("path-style listing"), Some(s3));
        let gcs = list_subdirectories(&Directory::Url(
            "https://storage.googleapis.com/cmip6/CMIP6/CMIP/NCAR/CESM2/historical/r1i1p1f1/Amon/ta/gn/v20190308".into(),
        ))
        .expect("GCS listing")
        .expect("listable");
        assert!(gcs.contains(&"ta".to_string()) && gcs.contains(&"lat_bnds".to_string()), "{gcs:?}");
        let token: serde_json::Value = serde_json::from_slice(
            &get(&Remote::Url("https://planetarycomputer.microsoft.com/api/sas/v1/token/daymeteuwest/daymet-zarr".into())).expect("token"),
        )
        .expect("JSON");
        let sas = token["token"].as_str().expect("a token");
        let az = list_subdirectories(&Directory::Url(format!("https://daymeteuwest.blob.core.windows.net/daymet-zarr/daily?{sas}")))
            .expect("Azure listing")
            .expect("listable");
        assert!(az.iter().any(|d| d.ends_with(".zarr")), "{az:?}");
        // A plain HTTP server does not list.
        assert_eq!(list_subdirectories(&Directory::Url("https://example.com/a.zarr".into())).expect("no listing"), None);
    }
    use std::net::TcpListener;

    #[derive(Clone, Copy)]
    enum Mode {
        Ranges,
        NoRanges,
        /// The first request of each connection fails with 503.
        Flaky,
        /// `/old` redirects to `/data`.
        Redirect,
        /// Every request is answered 403, as S3 answers for a missing key or denied access.
        Forbidden,
    }

    /// Serves `data` on a local port. Returns the URL and a counter of the requests served.
    fn serve(data: Vec<u8>, mode: Mode) -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/data", listener.local_addr().unwrap());
        let count = Arc::new(AtomicU64::new(0));
        let counter = count.clone();
        std::thread::spawn(move || {
            let mut failed = false;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                let mut range = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        let (a, b) = v.trim().split_once('-').unwrap();
                        range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()));
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let head = |status: &str, extra: &str, len: usize| {
                    format!("HTTP/1.1 {status}\r\nContent-Length: {len}\r\nConnection: close\r\n{extra}\r\n")
                };
                let response = match (mode, range) {
                    (Mode::Forbidden, _) => head("403 Forbidden", "", 0).into_bytes(),
                    (Mode::Flaky, _) if !failed => {
                        failed = true;
                        head("503 Service Unavailable", "", 0).into_bytes()
                    }
                    (Mode::Redirect, _) if request_line.contains("/old") => head("302 Found", "Location: /data\r\n", 0).into_bytes(),
                    (Mode::NoRanges, _) | (_, None) => [head("200 OK", "", data.len()).into_bytes(), data.clone()].concat(),
                    (_, Some((a, b))) => {
                        let b = b.min(data.len() - 1);
                        let extra = format!("Content-Range: bytes {a}-{b}/{}\r\n", data.len());
                        [head("206 Partial Content", &extra, b - a + 1).into_bytes(), data[a..=b].to_vec()].concat()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        (url, count)
    }

    fn test_data() -> Vec<u8> {
        (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn options() -> HttpOptions {
        HttpOptions { block_size: 4096, cache_size: 1 << 20, retry_delay: Duration::from_millis(1), ..Default::default() }
    }

    fn read_range(r: &mut RangeReader, off: u64, len: usize) -> Vec<u8> {
        r.seek(SeekFrom::Start(off)).unwrap();
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).unwrap();
        buf
    }

    #[test]
    fn forbidden_is_missing_only_for_unsigned_requests() {
        let (url, _) = serve(Vec::new(), Mode::Forbidden);
        let mut client = Client::new(&Remote::Url(url), options());
        client.missing_ok = true;
        // Anonymous: a missing key.
        assert_eq!(client.fetch_range(None).unwrap().status, 403);
        // Signed: access denied, an error.
        let creds = Credentials { access_key_id: "AKID".into(), secret_access_key: "secret".into(), session_token: None };
        client.s3 = Some(S3Auth { creds: Some(creds), region: Some("us-east-1".into()), discovered: true });
        assert!(client.fetch_range(None).is_err());
    }

    #[test]
    fn reads_ranges_through_the_cache() {
        let data = test_data();
        let (url, count) = serve(data.clone(), Mode::Ranges);
        let mut r = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        assert_eq!(r.len(), data.len() as u64);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(read_range(&mut r, 100, 50), data[100..150]);
        assert_eq!(count.load(Ordering::SeqCst), 1, "the first block is cached");
        // Spans three missing blocks: one request.
        assert_eq!(read_range(&mut r, 10_000, 10_000), data[10_000..20_000]);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(read_range(&mut r, 12_000, 100), data[12_000..12_100]);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        // The end of the file, and past it.
        assert_eq!(read_range(&mut r, data.len() as u64 - 10, 10), data[data.len() - 10..]);
        let mut buf = [0u8; 8];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn prefetch_merges_ranges() {
        let data = test_data();
        let (url, count) = serve(data.clone(), Mode::Ranges);
        let mut r = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        // Blocks 10-11 and 13 merge (gap of one block); block 40 is fetched apart.
        r.prefetch(&[(40_960, 5000), (13 * 4096 + 5, 10), (40 * 4096, 100)]).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert_eq!(read_range(&mut r, 41_000, 4000), data[41_000..45_000]);
        assert_eq!(read_range(&mut r, 12 * 4096, 4096), data[12 * 4096..13 * 4096]);
        assert_eq!(read_range(&mut r, 40 * 4096, 100), data[40 * 4096..40 * 4096 + 100]);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn server_without_ranges_downloads_once() {
        let data = test_data();
        let (url, count) = serve(data.clone(), Mode::NoRanges);
        let mut r = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        assert_eq!(r.len(), data.len() as u64);
        assert_eq!(read_range(&mut r, 123_456, 70_000), data[123_456..193_456]);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retries_and_follows_redirects() {
        let data = test_data();
        let (url, count) = serve(data.clone(), Mode::Flaky);
        let mut r = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        assert_eq!(read_range(&mut r, 0, 10), data[..10]);
        assert_eq!(count.load(Ordering::SeqCst), 2);

        let (url, _) = serve(data.clone(), Mode::Redirect);
        let mut r = RangeReader::open_with(&Remote::Url(url.replace("/data", "/old")), options()).unwrap();
        assert_eq!(read_range(&mut r, 5000, 10), data[5000..5010]);
    }

    #[test]
    fn evicts_old_blocks() {
        let data = test_data();
        let (url, _) = serve(data.clone(), Mode::Ranges);
        let opts = HttpOptions { cache_size: 16 * 4096, ..options() };
        let mut r = RangeReader::open_with(&Remote::Url(url), opts).unwrap();
        assert_eq!(read_range(&mut r, 0, data.len()), data);
        assert!(r.lock().unwrap().blocks.len() <= 74);
        assert_eq!(read_range(&mut r, 1000, 10), data[1000..1010]);
        assert!(r.lock().unwrap().blocks.len() <= 17);
        assert_eq!(read_range(&mut r, 200_000, 10), data[200_000..200_010]);
    }

    #[test]
    fn parses_headers() {
        assert_eq!(content_range_total("bytes 0-99/1234"), Some(1234));
        assert_eq!(content_range_total("bytes */1234"), Some(1234));
        assert_eq!(redact("https://a.org/b.tif?sv=1&sig=abc"), "https://a.org/b.tif?<redacted>");
        assert_eq!(redact("https://a.org/b.tif?x=1"), "https://a.org/b.tif?x=1");
        assert_eq!(resolve_redirect("https://a.org/x/y.tif?q=1", "/z.tif"), "https://a.org/z.tif");
        assert_eq!(resolve_redirect("https://a.org/x/y.tif", "z.tif"), "https://a.org/x/z.tif");
        assert_eq!(resolve_redirect("https://a.org/x/y.tif", "https://b.org/c"), "https://b.org/c");
    }
}
