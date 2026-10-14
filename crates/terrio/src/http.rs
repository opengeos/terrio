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
    s3: Option<S3Auth>,
}

struct S3Auth {
    creds: Option<Credentials>,
    region: Option<String>,
    /// Whether the region was learned from the server (and so is not retried).
    discovered: bool,
}

impl Client {
    fn new(remote: &Remote, options: HttpOptions) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .accept_encoding("identity")
            .user_agent(options.user_agent.as_str())
            .timeout_global(options.timeout)
            .timeout_connect(Some(options.connect_timeout))
            .build()
            .into();
        let (url, s3) = match remote {
            Remote::Url(u) => (u.clone(), None),
            Remote::S3(o) => {
                let region = s3::region_from_env();
                (o.url(region.as_deref()), Some(S3Auth { creds: Credentials::from_env(), region, discovered: false }))
            }
        };
        Self { agent, options, remote: remote.clone(), url, s3 }
    }

    /// Sends a `GET` (of the inclusive byte range `range`, if any) and returns the response once it
    /// has a final status: 200, 206 or 416. Other statuses are errors.
    fn send(&mut self, range: Option<(u64, u64)>) -> Result<ureq::http::Response<ureq::Body>> {
        let started = Instant::now();
        let mut url = self.url.clone();
        let mut redirects = 0;
        let mut attempt = 0;
        loop {
            let range_header = range.map(|(a, b)| format!("bytes={a}-{b}"));
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
                && auth.region.as_deref() != Some(region.as_str())
            {
                auth.discovered = true;
                auth.region = Some(region);
                self.url = object.url(auth.region.as_deref());
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
        let mut attempt = 0;
        loop {
            let response = self.send(range)?;
            let status = response.status().as_u16();
            let content_range = response.headers().get("content-range").and_then(|v| v.to_str().ok()).and_then(content_range_total);
            let mut data = Vec::new();
            match response.into_body().into_reader().read_to_end(&mut data) {
                Ok(_) => {}
                Err(_) if attempt < self.options.max_retry => {
                    self.backoff(&mut attempt);
                    continue;
                }
                Err(e) => return Err(Error::Http(format!("{}: {e}", redact(&self.url)))),
            }
            REQUESTS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
            if self.options.log {
                eprintln!("terrio:   received {} bytes", data.len());
            }
            let total_len = if status == 200 { Some(data.len() as u64) } else { content_range };
            return Ok(Fetched { status, total_len, data });
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
    match url.split_once('?') {
        Some((base, query)) if query.to_ascii_lowercase().contains("sig") => format!("{base}?<redacted>").into(),
        _ => url.into(),
    }
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

/// Fetches a whole remote file into memory (a STAC document, for instance).
pub(crate) fn get(remote: &Remote) -> Result<Vec<u8>> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    Ok(client.fetch(None)?.data)
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[derive(Clone, Copy)]
    enum Mode {
        Ranges,
        NoRanges,
        /// The first request of each connection fails with 503.
        Flaky,
        /// `/old` redirects to `/data`.
        Redirect,
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
