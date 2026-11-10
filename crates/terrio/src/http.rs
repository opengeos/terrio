// Written to match the behavior of:
//   GDAL v3.13.3:
//     port/cpl_vsil_curl.cpp, port/cpl_http.cpp
// No code is translated from these sources.

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
//! The first request of a file asks for its first 16,384 bytes, GDAL's first read
//! (`CPL_VSIL_CURL_CHUNK_SIZE`'s default in `port/cpl_vsil_curl.cpp`): a file that no driver
//! recognises costs no more than that. What requests return is also kept process-wide, as GDAL's
//! `/vsicurl/` cache keeps it, so that a file opened again is read without requests as far as its
//! cached bytes go (see [`clear_cache`]).
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
//! | `TERRIO_HTTP_CACHE_SIZE` (bytes): the block cache of each open file, and the process-wide cache | `CPL_VSIL_CURL_CACHE_SIZE` | 67108864 |
//! | `TERRIO_HTTP_USERAGENT` | `GDAL_HTTP_USERAGENT` | `terrio/VERSION` |
//! | `TERRIO_HTTP_MULTIRANGE`: `PARALLEL` requests the byte runs of one read up to 8 at once; `SERIAL` one after another; `SINGLE_GET` (one multipart request in GDAL, not supported) as `SERIAL`; any other value is `PARALLEL` | `GDAL_HTTP_MULTIRANGE` | `PARALLEL` |
//! | `TERRIO_HTTP_LOG=YES`: print each request, and each new process-wide connection pool, to standard error | `CPL_CURL_VERBOSE` | off |
//! | `TERRIO_JSON_MAX_SIZE` (bytes, `K`, `M` or `G` suffixes): the largest JSON document fetched whole | `CPL_JSON_MAX_SIZE` | 100 MB |
//!
//! Bodies fetched whole are capped: JSON documents (STAC, a search page) at `CPL_JSON_MAX_SIZE`,
//! as GDAL's `CPLJSONDocument::Load` caps them; a page of an object store's listing at
//! `MAX_LISTING_PAGE_BYTES`; a STAC signing token at `MAX_TOKEN_BYTES`. A body over its
//! cap is an error, read no further than the cap.

use crate::s3::{self, Credentials, S3Object};
use crate::{Error, Result};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport};

pub(crate) mod store;

/// Redirects followed before giving up.
const MAX_REDIRECTS: u32 = 10;

/// The option that keeps the requests of a read on the origin of its URL: any value but `YES`
/// (`TRUE`, `ON`, `1`), the default, refuses a redirect to another origin
/// ([`Redirects::for_reads`]).
pub(crate) const FOLLOW_CROSS_ORIGIN: &str = "TERRIO_HTTP_FOLLOW_CROSS_ORIGIN";

/// The bytes the first request of a remote file asks for: GDAL's `/vsicurl/` chunk
/// (`CPL_VSIL_CURL_CHUNK_SIZE`, 16,384 by default), which is what GDAL reads of a file before its
/// drivers decide whether they recognise it (GDAL refuses a 64 MiB file
/// of noise named `.tif` after 16,384 bytes).
pub(crate) const FIRST_READ: u64 = 16_384;

/// The most bytes one page of an object store's listing may hold. A page lists at most 1,000
/// entries on Amazon S3 and Google Cloud Storage (`max-keys`) and 5,000 on Azure Blob Storage
/// (`maxresults`), each named in at most 1,024 bytes: with every character of the name escaped
/// in XML (6 bytes, `&quot;`) and its properties (about 1.5 KB on Azure), the largest page a
/// compliant store can send is about 38 MB. GDAL reads listing pages without a cap; this one is
/// above any compliant page and bounds what a server can make a listing buffer.
pub(crate) const MAX_LISTING_PAGE_BYTES: u64 = 64 << 20;

/// The most bytes of a STAC signing service's SAS token response: a JSON object of two members
/// (`msft:expiry` and `token`), 400 to 402 bytes for the three public containers measured on
/// 2026-10-24.
pub(crate) const MAX_TOKEN_BYTES: u64 = 64 << 10;

/// GDAL's default `CPL_JSON_MAX_SIZE` ("100MB", 1024-based: `port/cpl_json.cpp`).
const JSON_MAX_SIZE_DEFAULT: u64 = 100 << 20;

/// The largest JSON document fetched whole: `TERRIO_JSON_MAX_SIZE`, else `CPL_JSON_MAX_SIZE`, as
/// GDAL parses it (a number of bytes with an optional `K`, `M` or `G` suffix, 1024-based), else
/// 100 MB, GDAL's default for the JSON files `CPLJSONDocument::Load` reads. An unreadable or zero
/// value is the default.
pub(crate) fn json_max_size() -> u64 {
    crate::config::get_any(&["TERRIO_JSON_MAX_SIZE", "CPL_JSON_MAX_SIZE"])
        .and_then(|v| parse_memory_size(&v))
        .filter(|n| *n > 0)
        .unwrap_or(JSON_MAX_SIZE_DEFAULT)
}

/// A memory size as GDAL's `CPLParseMemorySize` reads one: a number of bytes, with an optional
/// `K`, `M` or `G` (and an optional `B`) multiplying it by powers of 1024.
fn parse_memory_size(v: &str) -> Option<u64> {
    let v = v.trim();
    let digits = v.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(v.len());
    let value: f64 = v[..digits].parse().ok()?;
    let unit = v[digits..].trim().to_ascii_uppercase();
    let factor = match unit.trim_end_matches('B') {
        "" => 1u64,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => return None,
    };
    let bytes = value * factor as f64;
    (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

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
    /// The bytes a remote file's first request fetches (`GDAL_INGESTED_BYTES_AT_OPEN`), when more
    /// than a block: enough for a whole GeoTIFF header in one request.
    pub ingested_bytes_at_open: Option<u64>,
    /// Whether the separate byte runs of one prefetch (the tiles of a window) are fetched in
    /// parallel, as `GDAL_HTTP_MULTIRANGE=PARALLEL` (GDAL's default) fetches a multi-range read;
    /// otherwise one after another.
    pub parallel_ranges: bool,
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
            ingested_bytes_at_open: None,
            parallel_ranges: true,
        }
    }
}

impl HttpOptions {
    /// The options set by environment variables (see the [module documentation](self)).
    pub fn from_env() -> Self {
        let d = Self::default();
        let var = |terrio: &str, gdal: &str| crate::config::get_any(&[terrio, gdal]).map(|v| v.trim().to_string());
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
            // At most 64 MiB: a header, not a download.
            ingested_bytes_at_open: var("TERRIO_INGESTED_BYTES_AT_OPEN", "GDAL_INGESTED_BYTES_AT_OPEN")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .map(|n| n.min(64 << 20)),
            // As GDAL v3.13.3 reads it (`VSICurlHandle::ReadMultiRange`, port/cpl_vsil_curl.cpp):
            // SERIAL is one request at a time, any value but SINGLE_GET parallel. GDAL's SINGLE_GET
            // asks for every range in one multipart request, which terrio does not parse: it is
            // taken as SERIAL.
            parallel_ranges: var("TERRIO_HTTP_MULTIRANGE", "GDAL_HTTP_MULTIRANGE")
                .is_none_or(|v| !matches!(v.to_ascii_uppercase().as_str(), "SERIAL" | "SINGLE_GET")),
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
/// Requests and bytes per host, for [`fetch_stats_by_host`].
static HOST_STATS: Mutex<BTreeMap<String, FetchStats>> = Mutex::new(BTreeMap::new());
/// Requests and bytes per file, for [`fetch_stats_by_file`].
static FILE_STATS: Mutex<BTreeMap<String, FetchStats>> = Mutex::new(BTreeMap::new());

/// A statistics map ([`HOST_STATS`] or [`FILE_STATS`]), also after a panic elsewhere poisoned its
/// lock: counting goes on, as the allow-list's lock does, rather than stopping silently.
fn stats(map: &Mutex<BTreeMap<String, FetchStats>>) -> std::sync::MutexGuard<'_, BTreeMap<String, FetchStats>> {
    crate::par::fork_safe();
    map.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The requests made and bytes received over HTTP by this process so far.
pub fn fetch_stats() -> FetchStats {
    FetchStats { requests: REQUESTS.load(Ordering::Relaxed), bytes: BYTES.load(Ordering::Relaxed) }
}

/// The requests made and bytes received per host: the host of the URL that was asked for (as GDAL's
/// network statistics name the file asked for), not of where a redirect led.
pub fn fetch_stats_by_host() -> Vec<(String, FetchStats)> {
    stats(&HOST_STATS).iter().map(|(k, v)| (k.clone(), *v)).collect()
}

/// The requests made and bytes received per file, named as GDAL's network statistics name the
/// files they report (`VSINetworkStatsGetAsSerializedJSON`): `/vsiaz/container/blob` for an
/// `az://` blob read with a bearer token, `/vsis3/bucket/key` for an S3 object, else
/// `/vsicurl/<url>`. Query strings (SAS tokens, signatures) are left out.
pub fn fetch_stats_by_file() -> Vec<(String, FetchStats)> {
    stats(&FILE_STATS).iter().map(|(k, v)| (k.clone(), *v)).collect()
}

/// Resets the counters of [`fetch_stats`], [`fetch_stats_by_host`] and [`fetch_stats_by_file`]
/// (as GDAL's `VSINetworkStatsReset`).
pub fn reset_fetch_stats() {
    REQUESTS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    for map in [&HOST_STATS, &FILE_STATS] {
        stats(map).clear();
    }
}

/// Counts a response that is not passed on (an error, or one retried), as GDAL's network
/// statistics count it: a request, and the bytes of its body, read up to 1 MiB and dropped.
fn record_discarded(url: &str, name: &str, response: ureq::http::Response<ureq::Body>) {
    let mut body = Vec::new();
    let _ = response.into_body().into_reader().take(1 << 20).read_to_end(&mut body);
    record_fetch(url, name, body.len() as u64);
}

/// Counts a response that returned `bytes` of body for a request to `url`, for the file `name`
/// (see [`fetch_stats_by_file`]).
fn record_fetch(url: &str, name: &str, bytes: u64) {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(bytes, Ordering::Relaxed);
    for (map, key) in [(&HOST_STATS, host_of(url)), (&FILE_STATS, name)] {
        let mut m = stats(map);
        let s = m.entry(key.to_string()).or_default();
        s.requests += 1;
        s.bytes += bytes;
    }
}

/// The `x-ms-version` GDAL sends to Azure Blob Storage (`X_MS_VERSION` in GDAL's `cpl_azure.h`).
const AZURE_X_MS_VERSION: &str = "2019-12-12";

/// Marks the URL of an `az://` (or `/vsiaz/`) blob whose requests carry an Azure bearer token
/// (`AZURE_STORAGE_ACCESS_TOKEN`). It is stripped before any request: [`crate::vfs`] adds it so that
/// tokens go to `az://` blobs only, as GDAL sends them for `/vsiaz/` and never for `/vsicurl/`.
///
/// It holds 128 bits drawn once per process, so that no dataset name from outside can carry it
/// (with a fixed `azure+`, `azure+https://attacker.example/a.tif` would receive
/// the token). The token also goes only to the configured endpoint
/// ([`crate::vfs::azure_token_endpoint`]), whatever the URL says.
pub(crate) fn azure_bearer_prefix() -> &'static str {
    static PREFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PREFIX.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        // `RandomState` keys SipHash with bits from the operating system's generator.
        let bits: String = (0..2u64)
            .map(|i| {
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_u64(i);
                format!("{:016x}", h.finish())
            })
            .collect();
        format!("azure+{bits}+")
    })
}

/// Options bound to path prefixes, as GDAL's `VSISetPathSpecificOption`: prefix, then option name
/// (upper case), then value.
static PATH_OPTIONS: RwLock<BTreeMap<String, BTreeMap<String, String>>> = RwLock::new(BTreeMap::new());

/// Sets the option `key` for the files whose names start with `prefix`, or unsets it when `value`
/// is `None`: GDAL's `VSISetPathSpecificOption`. Requests read `GDAL_HTTP_HEADERS` (and, for
/// `az://` blobs, `AZURE_STORAGE_ACCESS_TOKEN`) this way, so a credential can be bound to one host:
/// a prefix matches a request's URL as given (`https://host/...`) or in its GDAL virtual file
/// system names (`/vsicurl/https://host/...`, `/vsis3/bucket/key`, `/vsiaz/container/blob`).
pub fn set_path_specific_option(prefix: &str, key: &str, value: Option<&str>) {
    let mut map = path_options_write();
    let key = key.to_ascii_uppercase();
    match value {
        Some(v) => {
            map.entry(prefix.to_string()).or_default().insert(key, v.to_string());
        }
        None => {
            if let Some(options) = map.get_mut(prefix) {
                options.remove(&key);
                if options.is_empty() {
                    map.remove(prefix);
                }
            }
        }
    }
}

/// Removes the options set with [`set_path_specific_option`] for `prefix`, or for every prefix:
/// GDAL's `VSIClearPathSpecificOptions`.
pub fn clear_path_specific_options(prefix: Option<&str>) {
    let mut map = path_options_write();
    match prefix {
        Some(p) => {
            map.remove(p);
        }
        None => map.clear(),
    }
}

/// The value of `key` for the file named `path`, from [`set_path_specific_option`]: as GDAL's
/// `VSIGetPathSpecificOption`, the prefixes are tried in reverse lexical order (so a longer prefix
/// comes before a prefix of it) and the first that `path` starts with and that sets `key` wins.
pub fn path_specific_option(path: &str, key: &str) -> Option<String> {
    let map = path_options_read();
    let key = key.to_ascii_uppercase();
    map.iter().rev().filter(|(prefix, _)| path.starts_with(prefix.as_str())).find_map(|(_, o)| o.get(&key).cloned())
}

/// Whether an Azure bearer token is configured, in the environment, the calling thread's options
/// or a path-specific option.
pub(crate) fn azure_token_configured() -> bool {
    crate::config::get_any(&["TERRIO_AZURE_ACCESS_TOKEN", "AZURE_STORAGE_ACCESS_TOKEN"]).is_some()
        || path_options_read().values().any(|o| o.contains_key("AZURE_STORAGE_ACCESS_TOKEN"))
}

/// The header lines of a `GDAL_HTTP_HEADERS` value, as GDAL splits them (`cpl_http.cpp`): at each
/// `\r\n` when the value has one, else at commas outside double quotes, where a backslash escapes
/// the next character and the quotes are removed. Each line is `Name: value`; lines that are not,
/// or whose name or value is not a valid header (a line break, say), are dropped.
pub fn parse_header_lines(value: &str) -> Vec<(String, String)> {
    let lines: Vec<String> = if value.contains("\r\n") {
        value.split("\r\n").map(str::to_string).collect()
    } else {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => quoted = !quoted,
                '\\' if quoted => {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                ',' if !quoted => out.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            }
        }
        out.push(cur);
        out
    };
    lines
        .iter()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            let (name, value) = (name.trim(), value.trim());
            let valid = !name.is_empty()
                && ureq::http::HeaderName::from_bytes(name.as_bytes()).is_ok()
                && ureq::http::HeaderValue::from_str(value).is_ok();
            valid.then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

/// `scheme://host[:port]` of a URL.
fn origin(url: &str) -> &str {
    let rest_at = url.find("://").map_or(0, |i| i + 3);
    let end = url[rest_at..].find(['/', '?', '#']).map_or(url.len(), |i| rest_at + i);
    &url[..end]
}

/// Origin equality with a cheap exact fast path and URL-normalized host/default ports.
fn same_response_origin(left: &str, right: &str) -> bool {
    origin(left) == origin(right)
        || (crate::vfs::url_origin(left).is_some() && crate::vfs::url_origin(left) == crate::vfs::url_origin(right))
}

/// The host (and port) of a URL.
fn host_of(url: &str) -> &str {
    let o = origin(url);
    o.find("://").map_or(o, |i| &o[i + 3..])
}

/// A response that returned data.
struct Fetched {
    /// The HTTP status: 200 (the whole file), 206 (a range) or 416 (the range starts past the end).
    status: u16,
    /// The file's length, from `Content-Range` (or the body's length when the status is 200).
    total_len: Option<u64>,
    /// The `Content-Range` header, for the error of a 206 response that gives no file length.
    content_range: Option<String>,
    data: Vec<u8>,
}

/// Makes requests for one remote file: signing, redirects, region discovery and retries.
#[derive(Clone)]
struct Client {
    agent: ureq::Agent,
    /// The process `agent` was made in (see [`agent_in`]).
    pid: u32,
    options: HttpOptions,
    remote: Remote,
    /// The URL requests go to (after S3 region discovery or a redirect).
    url: String,
    /// The final response location, kept separately so redirect auth stays tied to `url`.
    response_url: String,
    asset_policy: Option<crate::stac::AssetReadPolicy>,
    /// A query added to an S3 object's URL (`?list-type=2&...` for a listing), or empty.
    query: String,
    s3: Option<S3Auth>,
    /// Whether a missing object (status 404, or 403 as anonymous S3 requests get for missing
    /// keys) is an answer rather than an error.
    missing_ok: bool,
    refused_ok: bool,
    service_errors: bool,
    /// The most bytes a response to a request without a range may hold (see
    /// [`Client::fetch_range`]); `None`: as many as it holds.
    body_cap: Option<u64>,
    /// Whether the file is an `az://` blob whose requests carry an Azure bearer token.
    azure: bool,
    /// The options the creating thread had set for itself (see [`crate::config`]): requests may be
    /// sent from other threads, which do not have them.
    config: HashMap<String, Option<String>>,
    /// The headers every request for the file carries ([`Client::resolve_headers`]), resolved
    /// once, when the client is created, as GDAL's `/vsicurl/` handle reads its options when it is
    /// created: an option changed later (a path-specific header, a token) does not change what an
    /// open file sends, so its cache key ([`Client::cache_key`]) always names the credentials its
    /// bytes were fetched with.
    headers: Vec<(String, String)>,
    /// Whether the file's URL is on the endpoint an Azure token may be sent to, resolved with
    /// `headers` (see [`Client::resolve_on_azure_endpoint`]).
    on_azure_endpoint: bool,
    /// Where the requests' redirects may lead.
    redirects: Redirects,
    /// Set when the requests of this copy of the client are no longer wanted (a parallel
    /// prefetch's other run failed): no further request or retry is sent, and a retry's wait ends.
    cancel: Option<Arc<AtomicBool>>,
}

/// Where a request's redirects may lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Redirects {
    /// Anywhere, as GDAL follows them (the file's headers and credentials go only to the origin
    /// asked).
    Any,
    /// Only to the origin of the URL asked: for a request a document chose (a STAC link), which
    /// was checked to lead to that origin. A redirect to another origin is refused before
    /// anything is sent there.
    SameOrigin,
    /// Only to the origin of the URL asked, as [`Redirects::SameOrigin`]: for a read under
    /// `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN=NO`, whose refusal names that option.
    SameOriginRead,
}

impl Redirects {
    /// Whether a request for `asked` may be redirected to `to`.
    fn allow(self, asked: &str, to: &str) -> bool {
        self == Redirects::Any || (crate::vfs::url_origin(to).is_some() && crate::vfs::url_origin(to) == crate::vfs::url_origin(asked))
    }

    /// Where the redirects of a read may lead, as the calling thread's options set
    /// `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN` (see [`Redirects::for_reads_with`]).
    pub(crate) fn for_reads() -> Self {
        Self::for_reads_with(crate::config::get)
    }

    /// Where the redirects of a read may lead, `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN` read with `get`:
    /// anywhere when it is not set, empty or blank, or set to `YES` (`TRUE`, `ON`, `1`); to the
    /// origin of the URL asked for any other value, so that a misspelt `NO` keeps reads on their
    /// origin too.
    pub(crate) fn for_reads_with(get: impl Fn(&str) -> Option<String>) -> Self {
        match get(FOLLOW_CROSS_ORIGIN) {
            Some(v) if !v.trim().is_empty() && !matches!(v.trim().to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1") => {
                Redirects::SameOriginRead
            }
            _ => Redirects::Any,
        }
    }
}

/// The error of a redirect that [`Redirects::allow`] refuses under `policy`: it names the origin it
/// led to, not the URL (which may hold a signature), and the option that refused it.
fn redirect_refused(policy: Redirects, asked: &str, to: &str) -> Error {
    let to = crate::vfs::url_origin(to).map_or_else(|| "?".to_string(), |(scheme, host)| format!("{scheme}://{host}"));
    let why = match policy {
        Redirects::SameOriginRead => format!("reads stay on the origin they were given ({FOLLOW_CROSS_ORIGIN} is not YES)"),
        _ => "a link a document chose is read from its own origin only (TERRIO_STAC_FOLLOW_CROSS_ORIGIN=YES follows it)".to_string(),
    };
    Error::Http(format!("{}: a redirect to another origin ({to}) was not followed: {why}", redact(asked)))
}

#[derive(Clone)]
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
        Self::with_auth(remote, options, agent, auth, crate::config::thread_options())
    }

    /// A client with S3 credentials and region already resolved (`auth`, for S3 objects), under
    /// `config`, the options its creating thread had set for itself. The headers and credentials
    /// its requests carry are resolved here, once ([`Client::headers`]).
    fn with_auth(
        remote: &Remote,
        options: HttpOptions,
        agent: ureq::Agent,
        auth: Option<(Option<Credentials>, Option<String>)>,
        config: HashMap<String, Option<String>>,
    ) -> Self {
        let (url, s3, azure) = match remote {
            Remote::Url(u) => match u.strip_prefix(azure_bearer_prefix()) {
                Some(blob) => (blob.to_string(), None, true),
                None => (u.clone(), None, false),
            },
            Remote::S3(o) => {
                let (creds, region) = auth.unwrap_or_else(|| (Credentials::from_env(), s3::region_from_env()));
                (o.url(region.as_deref()), Some(S3Auth { creds, region, discovered: false }), false)
            }
        };
        let mut client = Self {
            agent,
            pid: std::process::id(),
            options,
            remote: remote.clone(),
            response_url: url.clone(),
            asset_policy: crate::stac::AssetReadPolicy::current(),
            url,
            query: String::new(),
            s3,
            missing_ok: false,
            refused_ok: false,
            service_errors: false,
            body_cap: None,
            azure,
            config,
            headers: Vec::new(),
            on_azure_endpoint: false,
            redirects: Redirects::Any,
            cancel: None,
        };
        client.on_azure_endpoint = client.azure && client.resolve_on_azure_endpoint();
        client.headers = client.resolve_headers();
        // A request without a policy of its own (a listing, a service's answer, an object fetched
        // whole) follows redirects as a read under the creating thread's options does.
        client.redirects = Redirects::for_reads_with(|k| client.option(&[k]));
        client
    }

    fn check_asset_request(&mut self, url: &str) -> Result<()> {
        if self.asset_policy.is_none() {
            self.asset_policy = crate::stac::AssetReadPolicy::current();
        }
        if let Some(policy) = &self.asset_policy {
            let headers = origin(url) == origin(&self.url) && !self.headers.is_empty();
            let signed = url == self.url && self.s3.as_ref().is_some_and(|auth| auth.creds.is_some());
            let query = url == self.url && policy.has_ambient_query(url);
            policy.check_client(url, headers || signed || query, matches!(self.remote, Remote::S3(_)) || self.azure)?;
        }
        Ok(())
    }

    fn check_cached_consent(&mut self) -> Result<()> {
        if self.asset_policy.is_none() {
            self.check_asset_request(&self.url.clone())?;
        }
        self.asset_policy.as_ref().map_or(Ok(()), crate::stac::AssetReadPolicy::check_consent)
    }

    fn observe_response(&self, response: &str) {
        if let Some(policy) = &self.asset_policy {
            policy.observe_response(&self.url, response);
        }
    }

    /// The first of `keys` set for this file's requests: by the creating thread's options, else by
    /// the process's options or the environment. Read only while the client is created (by the
    /// creating thread), see [`Client::headers`].
    fn option(&self, keys: &[&str]) -> Option<String> {
        keys.iter().find_map(|k| match self.config.get(*k) {
            Some(v) => v.clone().filter(|v| !v.trim().is_empty()),
            None => crate::config::get(k),
        })
    }

    /// The names of the file for path-specific options: its URL, and its GDAL virtual file system
    /// names.
    fn path_names(&self) -> Vec<String> {
        let mut names = vec![self.url.clone(), format!("/vsicurl/{}", self.url)];
        if let Remote::S3(o) = &self.remote {
            names.push(format!("/vsis3/{}/{}", o.bucket, o.key));
        }
        if self.azure {
            // The blob's path after its endpoint: a custom one (`CPL_AZURE_ENDPOINT`), or Azure's.
            let custom = crate::vfs::custom_azure_endpoint().map(|e| format!("{e}/"));
            let path = custom
                .as_deref()
                .and_then(|e| self.url.strip_prefix(e))
                .or_else(|| self.url.split_once(".blob.core.windows.net/").map(|(_, path)| path));
            if let Some(path) = path {
                names.push(format!("/vsiaz/{}", path.split('?').next().unwrap_or(path)));
            }
        }
        names
    }

    /// The file's name in the network statistics (see [`fetch_stats_by_file`]): its `/vsiaz/` or
    /// `/vsis3/` name when it has one, else its `/vsicurl/` name, without a query string.
    fn stats_name(&self) -> String {
        let names = self.path_names();
        let name = names.iter().find(|n| n.starts_with("/vsiaz/") || n.starts_with("/vsis3/")).unwrap_or(&names[1]);
        name.split('?').next().unwrap_or(name).to_string()
    }

    /// The headers configured for this file's requests: `GDAL_HTTP_HEADERS` (path-specific first,
    /// as GDAL reads it) and, for an `az://` blob, the Azure bearer token with GDAL's `x-ms-version`.
    /// Resolved once, when the client is created: see [`Client::headers`].
    fn resolve_headers(&self) -> Vec<(String, String)> {
        let names = self.path_names();
        let by_path = |key: &str| names.iter().find_map(|n| path_specific_option(n, key));
        let mut headers = by_path("GDAL_HTTP_HEADERS")
            .or_else(|| self.option(&["TERRIO_HTTP_HEADERS", "GDAL_HTTP_HEADERS"]))
            .map(|v| parse_header_lines(&v))
            .unwrap_or_default();
        if self.azure
            && self.on_azure_endpoint
            && let Some(token) =
                by_path("AZURE_STORAGE_ACCESS_TOKEN").or_else(|| self.option(&["TERRIO_AZURE_ACCESS_TOKEN", "AZURE_STORAGE_ACCESS_TOKEN"]))
        {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("x-ms-version"));
            headers.push(("x-ms-version".to_string(), AZURE_X_MS_VERSION.to_string()));
            headers.push(("Authorization".to_string(), format!("Bearer {}", token.trim())));
        }
        headers.retain(|(k, v)| ureq::http::HeaderName::from_bytes(k.as_bytes()).is_ok() && ureq::http::HeaderValue::from_str(v).is_ok());
        headers
    }

    /// Whether the file's URL is on the endpoint an Azure token may be sent to: the configured
    /// one ([`crate::vfs::azure_token_endpoint`]), under this file's options, by origin and path.
    /// Resolved once, when the client is created ([`Client::on_azure_endpoint`]).
    fn resolve_on_azure_endpoint(&self) -> bool {
        let get = |k: &str| self.option(&[k]);
        crate::vfs::azure_token_endpoint(&get).is_some_and(|endpoint| {
            crate::vfs::url_origin(&self.url).is_some()
                && crate::vfs::url_origin(&self.url) == crate::vfs::url_origin(&endpoint)
                && self.url.starts_with(&format!("{endpoint}/"))
        })
    }

    /// The file's key in the process-wide cache ([`FILE_CACHE`]): its URL, and a digest of the
    /// headers and credentials its requests carry (resolved when the client was created, as every
    /// request sends them) and of where its redirects may lead, so that bytes fetched with a
    /// credential are returned only to opens that would send the same one (GDAL keys its cache by
    /// name alone), and bytes a redirect brought from another origin only to opens that would
    /// follow it.
    fn cache_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(format!("{:?}\n{}\n{:?}\n", self.remote, self.azure, self.redirects).as_bytes());
        for (k, v) in &self.headers {
            h.update(format!("{}: {v}\n", k.to_ascii_lowercase()).as_bytes());
        }
        if let Some(S3Auth { creds: Some(c), .. }) = &self.s3 {
            h.update(format!("{}\n{}\n{:?}\n", c.access_key_id, c.secret_access_key, c.session_token).as_bytes());
        }
        let digest: String = h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect();
        format!("{} {digest}", self.url)
    }

    /// Sends a `GET` with a `Range` header value (`bytes=0-99`, `bytes=-16`), if any. With
    /// `missing_ok`, a missing object's response (404, or 403 to an unsigned request) is returned
    /// too; with `refused_ok`, any 403.
    fn send_range(&mut self, range_header: Option<String>) -> Result<ureq::http::Response<ureq::Body>> {
        // An Azure-marked blob goes only to the configured endpoint, with or without its token.
        if self.azure && !self.on_azure_endpoint {
            return Err(Error::InvalidArgument(format!(
                "{}: an Azure blob is read only from the configured endpoint (AZURE_STORAGE_ACCOUNT, \
                 AZURE_STORAGE_CONNECTION_STRING or CPL_AZURE_ENDPOINT)",
                redact(&self.url)
            )));
        }
        let started = Instant::now();
        let mut url = self.url.clone();
        let mut redirects = 0;
        let mut attempt = 0;
        loop {
            self.check_cancelled()?;
            self.check_asset_request(&url)?;
            let mut req = agent_in(self.pid, &self.agent, || own_agent(&self.options)).get(&url);
            if let Some(r) = &range_header {
                req = req.header("Range", r);
            }
            // Configured headers and tokens go only to the origin that was asked for: a redirect to
            // another host does not get them, as it does not get the S3 signature below. They are
            // the ones resolved when the client was created, whatever the options are now.
            if origin(&url) == origin(&self.url) {
                for (k, v) in &self.headers {
                    req = req.header(k, v);
                }
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
                    self.check_cancelled()?;
                    retry_warning(0, &self.url, self.options.retry_delay, attempt);
                    self.backoff(&mut attempt)?;
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
                // The region may become part of the host: only a region name is taken, checked
                // before anything is sent.
                if !s3::usable_region(&region) {
                    record_discarded(&self.url, &self.stats_name(), response);
                    let shown: String = region.chars().take(64).collect();
                    return Err(Error::Http(format!(
                        "{}: status {status} names the bucket's region {shown:?}, which is not a region name: not followed",
                        redact(&url)
                    )));
                }
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
                let to = resolve_redirect(&url, &location);
                // Checked before anything is sent there.
                if !self.redirects.allow(&self.url, &to) {
                    return Err(redirect_refused(self.redirects, &self.url, &to));
                }
                self.observe_response(&to);
                url = to;
                continue;
            }
            self.observe_response(&url);
            self.response_url = url.clone();
            match status {
                200 | 206 | 416 => return Ok(response),
                404 if self.missing_ok => return Ok(response),
                // S3 (and GCS) answer anonymous requests for missing keys with 403; a signed
                // request's 403 is a denied one (expired or insufficient credentials).
                403 if self.missing_ok && !signed => return Ok(response),
                // A refused signed request, returned to a caller that handles it.
                403 if self.refused_ok => return Ok(response),
                429 | 500 | 502 | 503 | 504 if attempt < self.options.max_retry => {
                    record_discarded(&self.url, &self.stats_name(), response);
                    self.check_cancelled()?;
                    retry_warning(status, &self.url, self.options.retry_delay, attempt);
                    self.backoff(&mut attempt)?
                }
                400..=599 if self.service_errors => return Ok(response),
                _ => {
                    record_discarded(&self.url, &self.stats_name(), response);
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

    /// Waits before a retry, for longer at each `attempt`. The wait ends early, with an error,
    /// when the client's requests are cancelled (see [`Client::cancel`]).
    fn backoff(&self, attempt: &mut u32) -> Result<()> {
        let delay = self.options.retry_delay.saturating_mul(1 << (*attempt).min(16));
        match &self.cancel {
            None => std::thread::sleep(delay),
            Some(cancel) => {
                let until = Instant::now() + delay;
                while let Some(left) = until.checked_duration_since(Instant::now()).filter(|left| !left.is_zero()) {
                    if cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(left.min(Duration::from_millis(20)));
                }
            }
        }
        self.check_cancelled()?;
        *attempt += 1;
        Ok(())
    }

    /// An error once the client's requests are cancelled (see [`Client::cancel`]).
    fn check_cancelled(&self) -> Result<()> {
        match &self.cancel {
            Some(cancel) if cancel.load(Ordering::SeqCst) => Err(Error::Http(format!("{}: request cancelled", redact(&self.url)))),
            _ => Ok(()),
        }
    }

    /// Takes the state `copy`, a copy of this client, has after its requests: its response URL
    /// and what it learned. An S3 region this client discovered after the copy was made stays,
    /// with the URL it gives (the region is part of the host), as the copy's requests did not
    /// discover it.
    fn adopt(&mut self, mut copy: Client) {
        let discovered = |c: &Client| c.s3.as_ref().is_some_and(|a| a.discovered);
        if discovered(self) && !discovered(&copy) {
            copy.s3 = self.s3.clone();
            copy.url = self.url.clone();
        }
        copy.cancel = None;
        *self = copy;
    }

    /// Fetches the inclusive byte range `range` (or the whole file), retrying when the transfer
    /// breaks off.
    fn fetch(&mut self, range: Option<(u64, u64)>) -> Result<Fetched> {
        self.fetch_range(range.map(|(a, b)| format!("bytes={a}-{b}")))
    }

    /// Fetches with a `Range` header value, if any: see [`Client::send_range`].
    ///
    /// The body of a `bytes=first-last` request is read only as far as the range justifies: a 206
    /// carries the range asked, and a server that ignores the range (200, no `Content-Range`) is
    /// refused unless the range starts the file and the file is at most ten times the range, as
    /// GDAL refuses it (`VSICurlHandleWriteFunc`: "Range downloading not supported by this
    /// server!"). A server cannot make a range read buffer an unbounded response. A request
    /// without a range reads at most [`Client::body_cap`] bytes, when there is a cap.
    fn fetch_range(&mut self, range: Option<String>) -> Result<Fetched> {
        self.fetch_range_limited(range, None)
    }

    /// [`Client::fetch_range`], a body read without range capped at `page_cap` bytes too (an
    /// object store's listing page), as [`Client::body_cap`] caps it.
    pub(crate) fn fetch_range_limited(&mut self, range: Option<String>, page_cap: Option<usize>) -> Result<Fetched> {
        let mut attempt = 0;
        loop {
            let response = self.send_range(range.clone())?;
            let status = response.status().as_u16();
            let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
            let content_range_header = header("content-range");
            let content_range = content_range_header.as_deref().and_then(content_range_total);
            let declared = header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
            let limit = match explicit_range(range.as_deref()) {
                Some((_, len)) if status == 206 => Some(len),
                Some((first, len)) if status == 200 && content_range.is_none() => {
                    if first != 0 || declared.is_some_and(|n| n > len.saturating_mul(10)) {
                        return Err(Error::Http(format!("{}: range downloading not supported by this server", redact(&self.url))));
                    }
                    Some(len.saturating_mul(10))
                }
                _ => None,
            };
            // A whole body over its cap is refused before it is read when its length is declared
            // (as GDAL's `MAX_FILE_SIZE` refuses it, `CURLOPT_MAXFILESIZE`), else once the cap is
            // passed.
            let cap = if range.is_none() {
                match (self.body_cap, page_cap.map(|n| n as u64)) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            } else {
                None
            };
            if let Some(cap) = cap
                && declared.is_some_and(|n| n > cap)
            {
                // Not read: a request with no body received.
                record_fetch(&self.url, &self.stats_name(), 0);
                return Err(over_cap(&self.url, cap));
            }
            let mut data = Vec::new();
            let reader = response.into_body().into_reader();
            let read = match limit.or(cap) {
                Some(limit) => reader.take(limit.saturating_add(1)).read_to_end(&mut data),
                None => { reader }.read_to_end(&mut data),
            };
            match read {
                Ok(_) => {}
                Err(_) if attempt < self.options.max_retry => {
                    self.check_cancelled()?;
                    retry_warning(status, &self.url, self.options.retry_delay, attempt);
                    self.backoff(&mut attempt)?;
                    continue;
                }
                Err(e) => return Err(Error::Http(format!("{}: {e}", redact(&self.url)))),
            }
            if limit.is_some_and(|limit| data.len() as u64 > limit) {
                return Err(Error::Http(format!("{}: the response is larger than the range asked", redact(&self.url))));
            }
            if let Some(cap) = cap
                && data.len() as u64 > cap
            {
                record_fetch(&self.url, &self.stats_name(), data.len() as u64);
                return Err(over_cap(&self.url, cap));
            }
            record_fetch(&self.url, &self.stats_name(), data.len() as u64);
            if self.options.log {
                eprintln!("terrio:   received {} bytes", data.len());
            }
            let total_len = if status == 200 { Some(data.len() as u64) } else { content_range };
            return Ok(Fetched { status, total_len, content_range: content_range_header, data });
        }
    }

    /// Writes the inclusive byte range `first..=last` of the file to `out`, from one request
    /// whose body is streamed, not held: a 206 must carry exactly that range. A server that
    /// ignores the range is refused unless the range is the whole file from its start and the
    /// body is that long. Returns the bytes written.
    fn stream_range(&mut self, first: u64, last: u64, out: &mut dyn io::Write) -> Result<u64> {
        let range = format!("bytes={first}-{last}");
        let want = last - first + 1;
        let response = self.send_range(Some(range))?;
        let status = response.status().as_u16();
        let has_content_range = response.headers().get("content-range").is_some();
        if status != 206 && !(status == 200 && first == 0 && !has_content_range) {
            record_fetch(&self.url, &self.stats_name(), 0);
            return Err(Error::Http(format!(
                "{}: bytes {first}-{last}: range downloading not supported by this server (status {status})",
                redact(&self.url)
            )));
        }
        let mut body = response.into_body().into_reader().take(want.saturating_add(1));
        let n = io::copy(&mut body, out);
        let n = match n {
            Ok(n) => n,
            Err(e) => return Err(Error::Http(format!("{}: {e}", redact(&self.url)))),
        };
        record_fetch(&self.url, &self.stats_name(), n);
        if self.options.log {
            eprintln!("terrio:   received {n} bytes");
        }
        if n != want {
            return Err(Error::Http(format!("{}: bytes {first}-{last}: {n} bytes received", redact(&self.url))));
        }
        Ok(n)
    }
}

/// Whether an open would send configured headers or S3 credentials, without making a request.
/// All custom headers are conservatively treated as credentials (API keys need not use a
/// standard header name). This uses the same path-specific options and credential profiles as
/// the actual client.
pub(crate) fn uses_configured_credentials(remote: &Remote) -> bool {
    let client = Client::new(remote, HttpOptions::from_env());
    if !client.headers.is_empty() || client.s3.as_ref().is_some_and(|auth| auth.creds.is_some()) {
        return true;
    }
    // A template cannot match a concrete path-specific prefix yet. Treat any overlapping
    // fixed prefix as potentially credentialed, then check the concrete tile again at open.
    client.path_names().iter().any(|name| {
        name.split_once('{').is_some_and(|(fixed, _)| {
            path_options_read().iter().any(|(prefix, options)| {
                (fixed.starts_with(prefix) || prefix.starts_with(fixed))
                    && (options.get("GDAL_HTTP_HEADERS").is_some_and(|v| !v.trim().is_empty())
                        || (client.azure && options.get("AZURE_STORAGE_ACCESS_TOKEN").is_some_and(|v| !v.trim().is_empty())))
            })
        })
    })
}

/// The error of a body over its cap.
pub(crate) fn over_cap(url: &str, cap: u64) -> Error {
    Error::Http(format!("{}: the response is larger than the {cap} bytes allowed", redact(url)))
}

thread_local! {
    /// The agent shared by the requests of the operation in progress on this thread (see
    /// [`AgentScope`]), with the settings it was built with and the process it was built in.
    static SHARED_AGENT: std::cell::RefCell<(u32, Option<ScopeAgent>)> = const { std::cell::RefCell::new((0, None)) };
}

/// The settings an agent is built with.
type AgentSettings = (Option<Duration>, Duration, String);

/// An [`AgentScope`]'s agent, its settings and the id of the process it was built in.
type ScopeAgent = (AgentSettings, ureq::Agent, u32);

/// Ends the life of a scope's agent: dropped in the process it was built in, leaked in a forked
/// child, where its idle connections are the parent's sockets, which the child must neither use
/// nor close.
fn release(agent: Option<ScopeAgent>) {
    if let Some(a) = agent.filter(|a| a.2 != std::process::id()) {
        std::mem::forget(a);
    }
}

/// While alive, the requests made on this thread share one agent, and so its connections (and
/// TLS sessions): a recursive upload, deletion or listing opens one connection per host rather
/// than one per request. Scopes nest; the agent is dropped when the outermost one ends (leaked in
/// a forked child that inherited the scope: see [`release`]).
pub(crate) struct AgentScope(());

impl AgentScope {
    pub(crate) fn new() -> Self {
        SHARED_AGENT.with(|shared| shared.borrow_mut().0 += 1);
        Self(())
    }
}

impl Drop for AgentScope {
    fn drop(&mut self) {
        SHARED_AGENT.with(|shared| {
            let mut shared = shared.borrow_mut();
            shared.0 = shared.0.saturating_sub(1);
            if shared.0 == 0 {
                release(shared.1.take());
            }
        });
    }
}

/// Outside any [`AgentScope`], the agent of the whole process: opening and reading remote files
/// from many threads reuses connections (and TLS sessions) rather than opening one per file.
static PROCESS_AGENT: std::sync::Mutex<Option<ProcessAgent>> = std::sync::Mutex::new(None);

/// [`PROCESS_AGENT`]: the agent, the settings and proxy variables it was built with (`ureq`
/// reads `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` when an agent is built), and
/// the process it was built in.
struct ProcessAgent {
    pid: u32,
    settings: AgentSettings,
    proxy: Vec<Option<std::ffi::OsString>>,
    agent: ureq::Agent,
}

/// The proxy variables `ureq` reads, in both cases.
fn proxy_env() -> Vec<Option<std::ffi::OsString>> {
    ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "NO_PROXY", "no_proxy"]
        .iter()
        .map(std::env::var_os)
        .collect()
}

/// The process's agent for `settings`, rebuilt when they or the proxy variables change. In a
/// forked child (another process id), the parent's agent is leaked, not dropped: its idle
/// connections are the parent's sockets, which the child must neither use nor close.
fn process_agent(settings: AgentSettings, options: &HttpOptions) -> ureq::Agent {
    crate::par::fork_safe();
    cached_agent(&PROCESS_AGENT, std::process::id(), proxy_env(), settings, || {
        if options.log {
            eprintln!("terrio: new HTTP connection pool (first use, or settings, proxy variables or process changed)");
        }
        new_agent(options)
    })
}

/// [`process_agent`] with its cache, process id, proxy variables and agent builder given.
fn cached_agent(
    slot: &std::sync::Mutex<Option<ProcessAgent>>,
    pid: u32,
    proxy: Vec<Option<std::ffi::OsString>>,
    settings: AgentSettings,
    build: impl FnOnce() -> ureq::Agent,
) -> ureq::Agent {
    let mut cached = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match &*cached {
        Some(p) if p.pid == pid && p.settings == settings && p.proxy == proxy => return p.agent.clone(),
        Some(p) if p.pid != pid => std::mem::forget(cached.take()),
        _ => {}
    }
    cached.insert(ProcessAgent { pid, settings, proxy, agent: build() }).agent.clone()
}

/// `agent`, made in process `pid`, in this process: in a forked child, `here` (the child's own
/// agent, [`own_agent`]) instead, since `agent`'s idle connections are the parent's sockets, which
/// the parent and every client sharing its pool may still use.
fn agent_in(pid: u32, agent: &ureq::Agent, here: impl FnOnce() -> ureq::Agent) -> ureq::Agent {
    if pid == std::process::id() { agent.clone() } else { here() }
}

/// The process's agent for `options`, whatever [`AgentScope`] the thread is in: the agent a client
/// made before a fork sends its requests with in the child.
fn own_agent(options: &HttpOptions) -> ureq::Agent {
    process_agent((options.timeout, options.connect_timeout, options.user_agent.clone()), options)
}

/// The agent requests are sent with: no automatic redirects (followed by [`Client`], which
/// signs only the first request), no compression. Within an [`AgentScope`], the scope's agent
/// when its settings and process are the same, otherwise a new one the scope keeps (in a child
/// forked inside a scope, the parent's agent is leaked: see [`release`]); outside any scope, the
/// process's ([`process_agent`]).
fn agent(options: &HttpOptions) -> ureq::Agent {
    let settings: AgentSettings = (options.timeout, options.connect_timeout, options.user_agent.clone());
    SHARED_AGENT.with(|shared| {
        let mut shared = shared.borrow_mut();
        if shared.0 == 0 {
            return process_agent(settings, options);
        }
        let pid = std::process::id();
        match &shared.1 {
            Some((s, agent, made_in)) if *s == settings && *made_in == pid => agent.clone(),
            _ => {
                release(shared.1.take());
                let agent = new_agent(options);
                shared.1 = Some((settings, agent.clone(), pid));
                agent
            }
        }
    })
}

/// HTTPS without a C compiler (`ureq` uses `ring` on the other architectures).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls_graviola::default_provider()))
        .build()
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::default()
}

fn new_agent(options: &HttpOptions) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .tls_config(tls_config())
        .http_status_as_error(false)
        .max_redirects(0)
        .accept_encoding("identity")
        .user_agent(options.user_agent.as_str())
        .timeout_global(options.timeout)
        .timeout_connect(Some(options.connect_timeout))
        // A fixed idle pool sized for parallel reads (ureq keeps 3 per host by default); it could
        // follow the thread count if 64 proves too few or too many.
        .max_idle_connections(64)
        .max_idle_connections_per_host(64)
        .build();
    let connector = DefaultConnector::new().chain(Http10CloseConnector);
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}

/// Wraps each connection in [`Http10Close`], after TLS, where it sees the responses in plain text.
#[derive(Debug)]
struct Http10CloseConnector;

impl Connector<Box<dyn Transport>> for Http10CloseConnector {
    type Out = Http10Close;

    fn connect(&self, _: &ConnectionDetails, chained: Option<Box<dyn Transport>>) -> std::result::Result<Option<Http10Close>, ureq::Error> {
        Ok(chained.map(|inner| Http10Close { inner, response_head: false, closing: false }))
    }
}

/// A connection that is not reused after an HTTP/1.0 response without the `keep-alive`
/// connection option, which, as RFC 9112 section 9.3 says, means "the connection will close after
/// the current response". ureq (ureq-proto 0.6.5) closes it only after `Connection: close` or a
/// close-delimited body, so it would pool such a connection and send a later request on it once
/// the server had closed it, failing with "Peer disconnected" (issue #632).
#[derive(Debug)]
struct Http10Close {
    inner: Box<dyn Transport>,
    /// A request was sent and its final response head has not been seen whole yet.
    response_head: bool,
    /// The connection is not to be reused.
    closing: bool,
}

impl Transport for Http10Close {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> std::result::Result<(), ureq::Error> {
        self.response_head = true;
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
        let progress = match CANCEL_READS.with_borrow(Clone::clone) {
            None => self.inner.await_input(timeout)?,
            Some(cancel) => self.await_input_unless(&cancel, timeout)?,
        };
        // ureq consumes a response's head only once it is whole, so the input holds it from its
        // start until then.
        if self.response_head
            && let Some(close) = closes_after(self.inner.buffers().input())
        {
            self.response_head = false;
            self.closing |= close;
        }
        Ok(progress)
    }

    fn is_open(&mut self) -> bool {
        !self.closing && self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

thread_local! {
    /// Set on a thread whose requests a parallel prefetch cancels ([`Shared::fetch_runs_in_parallel`]):
    /// once it is, a response the thread waits for is abandoned.
    static CANCEL_READS: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
}

/// How soon a wait for a response notices that its request was cancelled.
const CANCEL_POLL: Duration = Duration::from_millis(50);

impl Http10Close {
    /// Waits for input as [`Transport::await_input`] does, in slices of [`CANCEL_POLL`], and
    /// fails once `cancel` is set: a stalled response does not hold a cancelled request. The
    /// connection is then not reused.
    fn await_input_unless(&mut self, cancel: &AtomicBool, timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
        let started = Instant::now();
        // the whole wait's budget, the per-read timeout included, which each slice would restart
        let whole = timeout.for_read();
        loop {
            if cancel.load(Ordering::SeqCst) {
                self.closing = true;
                return Err(ureq::Error::Io(io::Error::other("request cancelled")));
            }
            let left = whole.after.saturating_sub(started.elapsed());
            let slice = left.min(CANCEL_POLL);
            let sliced = NextTimeout { after: slice.into(), per_read: None, ..whole };
            match self.inner.await_input(sliced) {
                Err(ureq::Error::Timeout(_)) if slice < left => continue,
                result => return result,
            }
        }
    }
}

/// Whether the connection closes after the response whose head starts `input`: an HTTP/1.0
/// response without a `Connection` header naming `keep-alive`. Informational heads are skipped;
/// `None` until the final head is whole.
fn closes_after(mut input: &[u8]) -> Option<bool> {
    loop {
        let crlf = input.windows(4).position(|w| w == b"\r\n\r\n").map(|at| (at, 4));
        let lf = input.windows(2).position(|w| w == b"\n\n").map(|at| (at, 2));
        let (end, delimiter) = crlf.into_iter().chain(lf).min_by_key(|(at, _)| *at)?;
        let head = String::from_utf8_lossy(&input[..end]);
        let mut lines = head.lines();
        let mut status = lines.next()?.split_ascii_whitespace();
        let version = status.next()?;
        let code: u16 = status.next()?.parse().ok()?;
        // ureq consumes informational heads before the final one. Inspect every head already
        // buffered here too: no further await_input is needed when the final head arrived with it.
        if (100..200).contains(&code) && code != 101 {
            input = &input[end + delimiter..];
            continue;
        }
        if version != "HTTP/1.0" {
            return Some(false);
        }
        let keep_alive = lines.filter_map(|line| line.split_once(':')).any(|(name, value)| {
            name.trim().eq_ignore_ascii_case("connection") && value.split(',').any(|o| o.trim().eq_ignore_ascii_case("keep-alive"))
        });
        return Some(!keep_alive);
    }
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
    /// The process `agent` was made in (see [`agent_in`]).
    pid: u32,
    options: HttpOptions,
    region: Mutex<Option<String>>,
    /// S3 credentials and region from the environment, read once rather than for every object.
    auth: (Option<Credentials>, Option<String>),
    /// The creating thread's options (see [`crate::config`]): objects are fetched from any thread.
    config: HashMap<String, Option<String>>,
}

impl ObjectFetcher {
    /// A fetcher with the options and S3 credentials from the environment.
    pub(crate) fn new() -> Self {
        let options = HttpOptions::from_env();
        let auth = (Credentials::from_env(), s3::region_from_env());
        Self {
            agent: agent(&options),
            pid: std::process::id(),
            options,
            region: Mutex::new(None),
            auth,
            config: crate::config::thread_options(),
        }
    }

    /// Fetches an object, or part of it. `None` when the object does not exist (status 404, or
    /// 403 as S3 answers anonymous requests for missing keys; a signed request's 403 is an error).
    pub(crate) fn get(&self, remote: &Remote, range: ByteRange) -> Result<Option<Vec<u8>>> {
        // A blocking leaf: resolve endpoint and header options from the fetcher's creator,
        // never from an unrelated task suspended on this pool worker.
        let mut options = self.config.clone();
        // Consent belongs to this read, while credentials and endpoints belong to the
        // fetcher's creator. A redirect can make a previously exempt asset credentialed.
        if crate::stac::AssetReadPolicy::current().is_some() {
            let key = "TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS";
            options.insert(key.into(), crate::config::get(key));
        }
        crate::config::with_thread_options(options, || self.get_with_options(remote, range))
    }

    fn get_with_options(&self, remote: &Remote, range: ByteRange) -> Result<Option<Vec<u8>>> {
        // A region already learned is the client's from the start (its URL is the regional one).
        let known = self.region.lock().ok().and_then(|r| r.clone());
        let auth = (self.auth.0.clone(), known.clone().or_else(|| self.auth.1.clone()));
        let agent = agent_in(self.pid, &self.agent, || own_agent(&self.options));
        let mut client = Client::with_auth(remote, self.options.clone(), agent, Some(auth), self.config.clone());
        client.missing_ok = true;
        if let (Remote::S3(_), Some(auth), Some(_)) = (remote, client.s3.as_mut(), known) {
            auth.discovered = true;
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

/// Whether retry warnings are recorded (see [`record_retry_warnings`]).
static RETRY_WARNINGS_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The retry warnings recorded and not taken yet, at most [`RETRY_WARNINGS_MAX`].
static RETRY_WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
const RETRY_WARNINGS_MAX: usize = 1000;

/// [`RETRY_WARNINGS`], also after a panic elsewhere poisoned its lock; held across a fork as the
/// other locks of this module are (see [`hold_for_fork`]).
fn retry_warnings() -> std::sync::MutexGuard<'static, Vec<String>> {
    crate::par::fork_safe();
    RETRY_WARNINGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts (`true`) or stops recording a warning for each request retried, worded as GDAL's
/// (`HTTP error code: 0 - URL. Retrying again in 0.5 secs`, code 0 for a connection error), for a
/// binding to pass on as GDAL's are passed on (rasterio logs them). Stopping drops those not taken.
pub fn record_retry_warnings(on: bool) {
    RETRY_WARNINGS_ON.store(on, Ordering::Relaxed);
    if !on {
        retry_warnings().clear();
    }
}

/// The retry warnings recorded since the last call, oldest first; none unless recording.
pub fn take_retry_warnings() -> Vec<String> {
    if !RETRY_WARNINGS_ON.load(Ordering::Relaxed) {
        return Vec::new();
    }
    std::mem::take(&mut *retry_warnings())
}

/// Records GDAL's warning for a retry of `url` (redacted) after `code`, an HTTP status or 0 for a
/// connection error (`CPLError(CE_Warning, CPLE_AppDefined, "HTTP error code: %d - %s. Retrying
/// again in %.1f secs")`), with the delay slept before it: `delay` doubled `attempt` times.
fn retry_warning(code: u16, url: &str, delay: Duration, attempt: u32) {
    if !RETRY_WARNINGS_ON.load(Ordering::Relaxed) {
        return;
    }
    let secs = delay.saturating_mul(1 << attempt.min(16)).as_secs_f64();
    let message = format!("HTTP error code: {code} - {}. Retrying again in {secs:.1} secs", redact(url));
    let mut warnings = retry_warnings();
    if warnings.len() < RETRY_WARNINGS_MAX {
        warnings.push(message);
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

/// The first byte and the length of a `bytes=first-last` range.
fn explicit_range(range: Option<&str>) -> Option<(u64, u64)> {
    let (a, b) = range?.strip_prefix("bytes=")?.split_once('-')?;
    let (a, b) = (a.parse::<u64>().ok()?, b.parse::<u64>().ok()?);
    (b >= a).then(|| (a, b - a + 1))
}

/// The total length in a `Content-Range` header (`bytes 0-99/1234` or `bytes */1234`).
fn content_range_total(v: &str) -> Option<u64> {
    v.rsplit_once('/')?.1.trim().parse().ok()
}

/// Why a 206 response gives no file length, for its error: its `Content-Range` gives `*` (a
/// length the server does not know) or none that can be read, or it has no `Content-Range`.
fn no_total_length(content_range: Option<&str>) -> &'static str {
    match content_range.map(|v| v.rsplit_once('/').map(|(_, total)| total.trim())) {
        None => "no Content-Range in a 206 response",
        Some(Some("*")) => "Content-Range gives no total length (`*`)",
        Some(_) => "Content-Range gives no readable total length",
    }
}

/// Downloads a remote file to a local path: see [`download_checked`], with no check.
pub(crate) fn download(remote: &Remote, path: &std::path::Path) -> Result<()> {
    download_checked(remote, path, &|_, _| Ok(()))
}

/// Downloads a remote file to a local path, by range, as GDAL's `/vsicurl/` reads one: its first
/// bytes as a [`RangeReader`] reads them (from the process-wide cache, or one request of
/// [`FIRST_READ`] bytes), which `check` sees before anything more is requested, with the file's
/// length that request gave (`Content-Range`'s), then the rest in one request streamed to the
/// file. A server that ignores ranges is refused at the first request for a file more than ten
/// times its range, as GDAL refuses it ("Range downloading not supported by this server!"), and a
/// smaller file is taken from that request's body.
pub(crate) fn download_checked(remote: &Remote, path: &std::path::Path, check: &dyn Fn(&[u8], u64) -> Result<()>) -> Result<()> {
    let reader = RangeReader::open(remote)?;
    let mut s = reader.lock()?;
    let head = s.head(FIRST_READ)?;
    check(&head, s.len)?;
    let mut file = std::fs::File::create(path)?;
    s.copy_to(&mut file)
}

/// The first `n` bytes of a remote file (fewer when it is shorter), read as a [`RangeReader`]
/// reads them: from the process-wide cache, or by range.
pub(crate) fn head(remote: &Remote, n: u64) -> Result<Vec<u8>> {
    Ok(head_and_len(remote, n)?.0)
}

/// [`head`], with the file's length.
pub(crate) fn head_and_len(remote: &Remote, n: u64) -> Result<(Vec<u8>, u64)> {
    let reader = RangeReader::open(remote)?;
    let mut s = reader.lock()?;
    Ok((s.head(n)?, s.len))
}

/// The first `maximum` bytes of a remote file (fewer when it is shorter) and its length, for format
/// identification (`inspection::identify`): read as [`head_and_len`] reads them, through a
/// [`RangeReader`], so from the process-wide cache or by range (one request of [`FIRST_READ`] bytes
/// for a prefix of that size, which a later open of the file finds in the cache). A server that
/// ignores ranges is refused at that request for a file more than ten times its range, as GDAL
/// refuses it ("Range downloading not supported by this server!"), and a smaller file is taken from
/// its body. The length is known from the first response, so it is always `Some`.
pub(crate) fn read_prefix(remote: &Remote, maximum: usize) -> Result<(Vec<u8>, Option<u64>)> {
    let (head, len) = head_and_len(remote, maximum as u64)?;
    Ok((head, Some(len)))
}

/// Streams a remote file into `out`, read by range as [`download_checked`] reads it (from the
/// process-wide cache, or by range; a server that ignores ranges is refused for a file more than
/// ten times the first request): `out` is told the file's length, which the first response gave,
/// before anything more is requested. Returns the bytes written.
pub(crate) fn download_to(remote: &Remote, out: &mut dyn crate::vsi::Sink) -> Result<u64> {
    let reader = RangeReader::open(remote)?;
    let mut s = reader.lock()?;
    out.expect(s.len)?;
    let mut counted = CountingWriter { inner: out, bytes: 0 };
    s.copy_to(&mut counted)?;
    Ok(counted.bytes)
}

/// A writer that counts the bytes written through it.
struct CountingWriter<'a> {
    inner: &'a mut dyn io::Write,
    bytes: u64,
}
impl io::Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes = self.bytes.saturating_add(n as u64);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Run one sequential archive pass against a streamed response, without range reads.
pub(crate) fn with_reader<T>(remote: &Remote, read: impl FnOnce(&mut dyn Read) -> Result<T>) -> Result<T> {
    with_reader_status(remote, false, |_, reader| read(reader))
}

/// Streams a protocol response, optionally returning HTTP error bodies so a service
/// can decode its structured exception. The caller must cap all body reads and
/// treat a non-success status as an error even when its body is not recognized.
pub(crate) fn with_reader_status<T>(
    remote: &Remote,
    service_errors: bool,
    read: impl FnOnce(u16, &mut dyn Read) -> Result<T>,
) -> Result<T> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    client.service_errors = service_errors;
    let response = client.send_range(None)?;
    let status = response.status().as_u16();
    let mut reader = MeasuredReader { inner: response.into_body().into_reader(), bytes: 0 };
    let result = read(status, &mut reader);
    record_fetch(&client.url, &client.stats_name(), reader.bytes);
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

/// Fetches a whole remote document into memory (a STAC API response, for instance), with one
/// request and no range, capped at [`json_max_size`].
pub(crate) fn get(remote: &Remote) -> Result<Vec<u8>> {
    get_capped(remote, json_max_size())
}

/// [`get`] with a cap of `cap` bytes.
pub(crate) fn get_capped(remote: &Remote, cap: u64) -> Result<Vec<u8>> {
    get_capped_following(remote, cap, Redirects::for_reads())
}

/// [`get_capped`], its redirects followed as `redirects` allows.
pub(crate) fn get_capped_following(remote: &Remote, cap: u64, redirects: Redirects) -> Result<Vec<u8>> {
    get_capped_response(remote, cap, redirects).map(|response| response.body)
}

pub(crate) fn get_capped_response(remote: &Remote, cap: u64, redirects: Redirects) -> Result<JsonResponse> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    client.body_cap = Some(cap);
    client.redirects = redirects;
    let body = client.fetch(None)?.data;
    Ok(JsonResponse { body, url: client.response_url })
}

/// Reads a whole remote file (a STAC item, a GeoJSON document) into memory by range, as
/// [`download_checked`] reads it, capped at [`json_max_size`]: a file over the cap is refused
/// once its length is known, before its body is fetched. Its redirects are followed as
/// `redirects` allows.
pub(crate) fn get_file(remote: &Remote, redirects: Redirects) -> Result<Vec<u8>> {
    get_file_response(remote, redirects).map(|response| response.body)
}

pub(crate) fn get_file_response(remote: &Remote, redirects: Redirects) -> Result<JsonResponse> {
    let reader = RangeReader::open_following(remote, HttpOptions::from_env(), redirects)?;
    let mut s = reader.lock()?;
    let cap = json_max_size();
    if s.len > cap {
        return Err(over_cap(&s.client.url, cap));
    }
    let refuse_mixed = |mixed| {
        if mixed { Err(Error::InvalidArgument("remote document bytes came from different response origins".into())) } else { Ok(()) }
    };
    refuse_mixed(s.mixed_origins)?;
    let mut out = Vec::new();
    s.copy_to(&mut out)?;
    refuse_mixed(s.mixed_origins)?;
    Ok(JsonResponse { body: out, url: s.client.response_url.clone() })
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
/// redirects as `redirects` allows and retrying transient errors.
#[cfg(test)]
pub(crate) fn get_with_headers(url: &str, headers: &[(String, String)], redirects: Redirects) -> Result<Vec<u8>> {
    get_with_headers_capped(url, headers, json_max_size(), redirects)
}

/// [`get_with_headers`] with a cap of `cap` bytes on the body.
pub(crate) fn get_with_headers_capped(url: &str, headers: &[(String, String)], cap: u64, redirects: Redirects) -> Result<Vec<u8>> {
    if headers.is_empty() {
        return get_capped_following(&Remote::Url(url.to_string()), cap, redirects);
    }
    request("GET", url, None, headers, cap, redirects)
}

/// A bounded response body and the effective URL after same-origin redirects.
pub(crate) struct JsonResponse {
    pub body: Vec<u8>,
    pub url: String,
}

/// A bounded JSON response and its effective same-origin URL, for relative service links.
pub(crate) fn get_json_response(url: &str, accept: &str, cap: u64) -> Result<JsonResponse> {
    request_response("GET", url, None, &[("Accept".into(), accept.into())], cap.min(json_max_size()), Redirects::SameOrigin)
}

/// Sends a `GET` or a `POST` (of a JSON `body`) with extra `headers`, and returns the response's
/// body: redirects followed as `redirects` allows (for a `POST`, only those keeping the method,
/// 307 and 308), each checked before anything is sent to where it leads, the extra headers
/// dropped when a redirect leads to another origin (they may hold keys), and transient errors and
/// statuses (429, 500, 502, 503, 504) retried. A body over `cap` bytes is an error, read no
/// further than the cap.
fn request(method: &str, url: &str, body: Option<&[u8]>, headers: &[(String, String)], cap: u64, policy: Redirects) -> Result<Vec<u8>> {
    request_response(method, url, body, headers, cap, policy).map(|response| response.body)
}

pub(crate) fn request_response(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(String, String)],
    cap: u64,
    policy: Redirects,
) -> Result<JsonResponse> {
    let options = HttpOptions::from_env();
    let agent = agent(&options);
    let first_origin = crate::vfs::url_origin(url);
    let asked = url;
    let mut url = url.to_string();
    let (mut redirects, mut attempt) = (0, 0);
    let backoff = |attempt: &mut u32| {
        std::thread::sleep(options.retry_delay.saturating_mul(1 << (*attempt).min(16)));
        *attempt += 1;
    };
    loop {
        let extra: &[(String, String)] = if crate::vfs::url_origin(&url) == first_origin { headers } else { &[] };
        let accept = extra
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("accept"))
            .map_or("application/geo+json, application/json", |(_, value)| value.as_str());
        let result = match body {
            Some(body) => {
                let mut req = agent.post(&url).header("Content-Type", "application/json").header("Accept", accept);
                for (k, v) in extra.iter().filter(|(key, _)| !key.eq_ignore_ascii_case("accept")) {
                    req = req.header(k, v);
                }
                req.send(body)
            }
            None => {
                let mut req = agent.get(&url).header("Accept", accept);
                for (k, v) in extra.iter().filter(|(key, _)| !key.eq_ignore_ascii_case("accept")) {
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
                retry_warning(0, &url, options.retry_delay, attempt);
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
            let to = resolve_redirect(&url, location);
            if !policy.allow(asked, &to) {
                return Err(redirect_refused(policy, asked, &to));
            }
            url = to;
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
                    retry_warning(status, &url, Duration::from_secs(secs.min(60)), 0);
                    std::thread::sleep(std::time::Duration::from_secs(secs.min(60)));
                    attempt += 1;
                }
                None => {
                    retry_warning(status, &url, options.retry_delay, attempt);
                    backoff(&mut attempt)
                }
            }
            continue;
        }
        let mut data = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(cap.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|e| Error::Http(format!("{}: {e}", redact(&url))))?;
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
        if data.len() as u64 > cap {
            return Err(over_cap(&url, cap));
        }
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&data[..data.len().min(300)]).into_owned();
            return Err(Error::Http(format!("{}: status {status}: {}", redact(&url), text.trim())));
        }
        return Ok(JsonResponse { body: data, url });
    }
}

/// GDAL's `/vsicurl/` cache: what remote files' requests returned (their lengths, first bytes and
/// blocks), kept process-wide so that a file opened again is read without requests as far as its
/// cached bytes go, as GDAL keeps file properties and regions until `VSICurlClearCache` (see
/// [`clear_cache`]). GDAL keys its cache by the file's name alone; an entry here is also keyed by
/// the headers and credentials its requests carried ([`Client::cache_key`]), so bytes fetched
/// with a credential are never returned to an open that would not send it. The oldest entries are
/// dropped beyond `TERRIO_HTTP_CACHE_SIZE` bytes.
static FILE_CACHE: Mutex<FileCache> = Mutex::new(FileCache::new());

/// [`FILE_CACHE`], also after a panic elsewhere poisoned its lock.
fn file_cache() -> std::sync::MutexGuard<'static, FileCache> {
    crate::par::fork_safe();
    FILE_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether a test holds [`cache_test_guard`]: the process-wide cache then drops nothing.
#[cfg(test)]
static EVICTION_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The guard of the tests whose request counts rely on the process-wide cache (a file read by
/// one open and again by the next, as an identification read and the open after it are): it
/// serializes them against each other and against the test that clears the cache, and holds the
/// cache's eviction while they run. Other tests' readers have budgets as small as 64 KiB, and a
/// storing reader's budget bounds the whole cache, so their stores would evict the entries these
/// tests rely on.
#[cfg(test)]
pub(crate) fn cache_test_guard() -> CacheTestGuard {
    let serial = CACHE_TEST_SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    EVICTION_HELD.store(true, Ordering::SeqCst);
    CacheTestGuard { _serial: serial }
}

/// Serializes the holders of [`cache_test_guard`] and of [`cache_eviction_test_guard`].
#[cfg(test)]
static CACHE_TEST_SERIAL: Mutex<()> = Mutex::new(());

/// The guard of the tests that rely on the process-wide cache dropping blocks (a block read
/// again once evicted is fetched again): it keeps them from running while a
/// [`cache_test_guard`] holds the cache's eviction, without holding it itself.
#[cfg(test)]
pub(crate) fn cache_eviction_test_guard() -> std::sync::MutexGuard<'static, ()> {
    CACHE_TEST_SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// See [`cache_test_guard`].
#[cfg(test)]
pub(crate) struct CacheTestGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for CacheTestGuard {
    fn drop(&mut self) {
        // Before the serializing lock is released (fields drop after this).
        EVICTION_HELD.store(false, Ordering::SeqCst);
    }
}

/// Holds this module's process-wide locks (the cache of remote files, the request statistics,
/// the path-specific options, the retry warnings and the process's agent) across a fork, for
/// [`crate::par`]: the forking thread takes them before the process is copied and lets them go
/// after, so that the child never starts with one held by a thread it does not have.
#[cfg(unix)]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    held.push(Box::new(file_cache()));
    held.push(Box::new(stats(&HOST_STATS)));
    held.push(Box::new(stats(&FILE_STATS)));
    held.push(Box::new(path_options_write()));
    held.push(Box::new(retry_warnings()));
    held.push(Box::new(PROCESS_AGENT.lock().unwrap_or_else(std::sync::PoisonError::into_inner)));
}

/// [`PATH_OPTIONS`] to change, also after a panic elsewhere poisoned its lock.
fn path_options_write() -> std::sync::RwLockWriteGuard<'static, BTreeMap<String, BTreeMap<String, String>>> {
    crate::par::fork_safe();
    PATH_OPTIONS.write().unwrap_or_else(|e| e.into_inner())
}

/// [`PATH_OPTIONS`] to read, also after a panic elsewhere poisoned its lock.
fn path_options_read() -> std::sync::RwLockReadGuard<'static, BTreeMap<String, BTreeMap<String, String>>> {
    crate::par::fork_safe();
    PATH_OPTIONS.read().unwrap_or_else(|e| e.into_inner())
}

/// Empties the process-wide cache of remote files (GDAL's `VSICurlClearCache`): files opened after
/// it are requested again.
pub fn clear_cache() {
    let mut cache = file_cache();
    cache.files.clear();
    cache.order.clear();
    cache.bytes = 0;
}

/// What the process-wide cache holds of one file.
#[derive(Default)]
struct CachedFile {
    len: u64,
    /// Cached metadata keeps the origin that supplied its bytes, not just the requested URL.
    response_url: String,
    /// The whole file, from a server that ignored a range.
    whole: Option<Arc<[u8]>>,
    /// The file's first bytes, from a first request shorter than a block.
    prefix: Option<Arc<[u8]>>,
    /// Blocks, by block size and index.
    blocks: HashMap<(u64, u64), Arc<[u8]>>,
}

/// A part of a cached file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Whole,
    Prefix,
    Block(u64, u64),
}

struct FileCache {
    files: BTreeMap<String, CachedFile>,
    /// The parts stored, oldest first, for eviction.
    order: VecDeque<(String, Slot)>,
    bytes: u64,
}

impl FileCache {
    const fn new() -> Self {
        Self { files: BTreeMap::new(), order: VecDeque::new(), bytes: 0 }
    }

    fn forget(&mut self, key: &str) {
        if let Some(old) = self.files.remove(key) {
            let bytes = old.whole.iter().chain(old.prefix.iter()).map(|data| data.len() as u64).sum::<u64>()
                + old.blocks.values().map(|data| data.len() as u64).sum::<u64>();
            self.bytes = self.bytes.saturating_sub(bytes);
        }
        self.order.retain(|(old, _)| old != key);
    }

    /// Stores bytes with their cache key and effective response URL, then drops parts beyond
    /// `budget` bytes. A part larger than the budget is not kept.
    fn store(&mut self, source: (&str, &str), len: u64, slot: Slot, data: Arc<[u8]>, budget: u64) {
        let (key, response_url) = source;
        if data.len() as u64 > budget {
            return;
        }
        if self.files.get(key).is_some_and(|file| !same_response_origin(&file.response_url, response_url)) {
            self.forget(key);
        }
        let file = self.files.entry(key.to_string()).or_default();
        file.len = len;
        file.response_url = response_url.to_string();
        let old = match slot {
            Slot::Whole => file.whole.replace(data.clone()),
            Slot::Prefix => file.prefix.replace(data.clone()),
            Slot::Block(bs, i) => file.blocks.insert((bs, i), data.clone()),
        };
        match old {
            Some(old) => self.bytes = self.bytes.saturating_sub(old.len() as u64),
            None => self.order.push_back((key.to_string(), slot)),
        }
        self.bytes += data.len() as u64;
        #[cfg(test)]
        if EVICTION_HELD.load(Ordering::SeqCst) {
            return;
        }
        while self.bytes > budget
            && let Some((key, slot)) = self.order.pop_front()
        {
            let Some(file) = self.files.get_mut(&key) else { continue };
            let removed = match slot {
                Slot::Whole => file.whole.take(),
                Slot::Prefix => file.prefix.take(),
                Slot::Block(bs, i) => file.blocks.remove(&(bs, i)),
            };
            self.bytes = self.bytes.saturating_sub(removed.map_or(0, |d| d.len() as u64));
            if file.whole.is_none() && file.prefix.is_none() && file.blocks.is_empty() {
                self.files.remove(&key);
            }
        }
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

/// The most requests a parallel fetch has in flight (and responses it holds) at once.
// ponytail: a thread per request slot, spawned per fetch; a pool of request threads if spawning
// ever shows next to a request's latency
const PARALLEL_RUNS: usize = 8;

/// The least each piece of a run holds when the run is split into requests fetched in parallel
/// (see [`Shared::fetch_blocks`]), so that a run shorter than twice this stays one request:
/// smaller pieces would cost more in requests than they save in transfer.
const SPLIT_RUN_BYTES: u64 = 1 << 20;

/// A run of consecutive blocks that one request fetches (see [`Shared::fetch_blocks`]).
struct Run {
    /// The run's first block.
    start: u64,
    /// How many of the run's first bytes the file's first bytes (`Shared::prefix`) hold: copied
    /// from there when the run is stored, so that no run holds a copy before.
    held: usize,
    /// The first byte to request, past `last` when `held` covers the run.
    from: u64,
    /// The run's last byte.
    last: u64,
}

impl Run {
    /// Whether the run needs a request: its held bytes do not cover it.
    fn requests(&self) -> bool {
        self.from <= self.last
    }
}

/// The requests of a parallel fetch the threads take in order, at most `free` more than the
/// runs stored so far, until the fetch stops them (see [`Shared::fetch_runs_in_parallel`]).
struct RequestWindow {
    state: Mutex<WindowState>,
    changed: std::sync::Condvar,
    /// Set when the fetch stops: no request is taken, and the clients' retries end.
    cancel: Arc<AtomicBool>,
}

/// Where a [`RequestWindow`] is.
struct WindowState {
    /// The index of the next request to take.
    next: usize,
    /// How many more requests may be taken before a run is stored.
    places: usize,
}

/// Stops a [`RequestWindow`] when the thread holding it panics.
struct StopOnPanic<'a>(&'a RequestWindow);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.stop();
        }
    }
}

impl RequestWindow {
    fn new(places: usize) -> Self {
        Self {
            state: Mutex::new(WindowState { next: 0, places }),
            changed: std::sync::Condvar::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The next of `requests` once the window has a place for it, or `None` when they are all
    /// taken or the fetch stopped.
    fn take<T: Copy>(&self, requests: &[T]) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.cancel.load(Ordering::SeqCst) || state.next >= requests.len() {
                return None;
            }
            if state.places > 0 {
                state.places -= 1;
                state.next += 1;
                return Some(requests[state.next - 1]);
            }
            state = self.changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Gives back the place of a request whose run is stored.
    fn release(&self) {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).places += 1;
        self.changed.notify_one();
    }

    /// Stops the fetch: no other request is taken, and the retries of those in flight end.
    fn stop(&self) {
        let _state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cancel.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }

    fn stop_on_panic(&self) -> StopOnPanic<'_> {
        StopOnPanic(self)
    }
}

struct Shared {
    client: Client,
    /// The file's key in the process-wide cache.
    key: String,
    len: u64,
    block_size: u64,
    max_blocks: usize,
    /// The size of the process-wide cache.
    shared_budget: u64,
    blocks: HashMap<u64, Arc<[u8]>>,
    /// Cached blocks, oldest first, for eviction.
    order: VecDeque<u64>,
    /// The whole file, when the server ignores range requests.
    whole: Option<Arc<[u8]>>,
    /// The file's first bytes, when the first request was shorter than a block.
    prefix: Option<Arc<[u8]>>,
    /// The blocks the last prefetch asked for, sorted: fetched a batch at a time
    /// ([`Shared::fetch_planned`]), the next batch when a read reaches one not cached. A deque, so
    /// that taking batches from its front does not shift the rest.
    plan: VecDeque<u64>,
    response_origin: Option<String>,
    mixed_origins: bool,
}

impl RangeReader {
    /// Opens a remote file with the options from the environment, fetching its first bytes.
    pub fn open(remote: &Remote) -> Result<Self> {
        Self::open_with(remote, HttpOptions::from_env())
    }

    /// Opens a remote file, fetching its first bytes: `FIRST_READ` of them (a block, when
    /// blocks are smaller), or `ingested_bytes_at_open` in whole blocks. A file whose first bytes
    /// the process-wide cache holds is opened without a request; of whole blocks of first bytes,
    /// only those the cache no longer holds are requested.
    pub fn open_with(remote: &Remote, options: HttpOptions) -> Result<Self> {
        Self::open_following(remote, options, Redirects::for_reads())
    }

    /// [`RangeReader::open_with`], every request's redirects followed as `redirects` allows.
    pub(crate) fn open_following(remote: &Remote, options: HttpOptions, redirects: Redirects) -> Result<Self> {
        let block_size = options.block_size.max(1);
        let max_blocks = (options.cache_size / block_size).max(16) as usize;
        let shared_budget = options.cache_size;
        // GDAL_INGESTED_BYTES_AT_OPEN: a whole header in the first request, in whole blocks.
        let first_len =
            options.ingested_bytes_at_open.map_or(FIRST_READ.min(block_size), |n| n.max(block_size).div_ceil(block_size) * block_size);
        let mut client = Client::new(remote, options);
        client.redirects = redirects;
        client.check_asset_request(&client.url.clone())?;
        let key = client.cache_key();
        let mut shared = Shared {
            client,
            key,
            len: 0,
            block_size,
            max_blocks,
            shared_budget,
            blocks: HashMap::new(),
            order: VecDeque::new(),
            whole: None,
            prefix: None,
            plan: VecDeque::new(),
            response_origin: None,
            mixed_origins: false,
        };
        if let Some(missing) = shared.load_cached(first_len) {
            // The blocks of the first bytes that the process-wide cache no longer holds, each run
            // of them in one request.
            shared.fetch_blocks(&missing, 0)?;
        } else {
            let first = shared.client.fetch(Some((0, first_len - 1)))?;
            shared.observe_source();
            match first.status {
                206 => {
                    shared.len = first.total_len.ok_or_else(|| {
                        Error::Http(format!("{}: {}", redact(&shared.client.url), no_total_length(first.content_range.as_deref())))
                    })?;
                    if first.data.len() as u64 >= shared.len.min(block_size) {
                        shared.store(0, first.data);
                    } else {
                        shared.store_prefix(first.data.into());
                    }
                }
                200 => {
                    shared.len = first.data.len() as u64;
                    shared.store_whole(first.data.into());
                }
                // The range starts past the end: an empty file.
                _ => shared.len = first.total_len.unwrap_or(0),
            }
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

    /// The most blocks a prefetch plans (64 GiB of the default 64 KiB blocks; the ordered set
    /// holding them takes a few times their 8 MiB of indices). Past it, the plan keeps the first
    /// distinct blocks in the order of the ranges given, and the others are fetched as reads
    /// reach them.
    const MAX_PLANNED_BLOCKS: usize = 1 << 20;

    /// Fetches the blocks covering the byte ranges `(offset, length)` that are not cached yet, in as
    /// few requests as it can: runs of missing blocks separated by at most two cached blocks are
    /// fetched together. Ranges larger than half the cache are fetched a batch at a time, the
    /// first now and the next ones as reads reach them, so that no block is evicted before it is
    /// read and then fetched again.
    pub fn prefetch(&self, ranges: &[(u64, u64)]) -> io::Result<()> {
        let mut s = self.lock()?;
        if s.whole.is_some() {
            return Ok(());
        }
        s.evict();
        let bs = s.block_size;
        let blocks = ranges
            .iter()
            .filter(|(off, len)| *len > 0 && *off < s.len)
            // Lengths come from file metadata (a TIFF's byte counts), so the end saturates.
            .filter(|(off, len)| s.prefix.as_ref().is_none_or(|p| off.saturating_add(*len) > p.len() as u64))
            .flat_map(|(off, len)| *off / bs..=(off.saturating_add(*len).min(s.len) - 1) / bs)
            .filter(|b| !s.blocks.contains_key(b));
        // A prefetch only saves requests; the plan stays bounded whatever the ranges and the
        // file's length (a byte count past a huge file's end would list every block to its end).
        // Distinct blocks count toward the bound; blocks past it are fetched as reads reach them.
        // The walk over the ranges is bounded too, duplicates included (four times the bound), so
        // that many ranges over the same bytes cannot stall the reader; past it they only lose
        // the prefetch, not data.
        let mut planned = std::collections::BTreeSet::new();
        for b in blocks.take(4 * Self::MAX_PLANNED_BLOCKS) {
            if planned.len() >= Self::MAX_PLANNED_BLOCKS {
                break;
            }
            planned.insert(b);
        }
        let wanted: Vec<u64> = planned.into_iter().collect();
        // A prefetch of cached blocks keeps the plan of an earlier one that is still being read.
        if wanted.is_empty() {
            return Ok(());
        }
        s.plan = wanted.into();
        s.fetch_planned(0)
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, Shared>> {
        self.shared.lock().map_err(|_| io::Error::other("the HTTP block cache is poisoned"))
    }
}

impl Shared {
    /// Takes the file's length and its first `first_len` bytes from the process-wide cache, when
    /// it holds the file. A first read shorter than a block (the default one) is taken only when
    /// the cache holds all of it, in the whole file, its prefix or its first block: otherwise it
    /// is requested as a first read is. A first read of whole blocks (`ingested_bytes_at_open`)
    /// is taken block by block. Returns the blocks of the first bytes the cache no longer holds,
    /// or `None` when the first read is to be requested.
    fn load_cached(&mut self, first_len: u64) -> Option<Vec<u64>> {
        let cache = file_cache();
        let file = cache.files.get(&self.key)?;
        let end = first_len.min(file.len);
        let prefix = file.prefix.as_ref().map_or(0, |p| p.len() as u64);
        let first_block = file.blocks.get(&(self.block_size, 0)).map_or(0, |b| b.len() as u64);
        if file.whole.is_none() && first_len < self.block_size && prefix.max(first_block) < end {
            return None;
        }
        self.len = file.len;
        self.whole = file.whole.clone();
        self.prefix = file.prefix.clone();
        if self.whole.is_some() || self.prefix.is_some() {
            self.client.response_url = file.response_url.clone();
            self.response_origin = Some(origin(&file.response_url).to_string());
            self.client.observe_response(&file.response_url);
        }
        if self.whole.is_some() || end <= prefix {
            return Some(Vec::new());
        }
        let mut missing = Vec::new();
        for b in 0..end.div_ceil(self.block_size) {
            match file.blocks.get(&(self.block_size, b)) {
                Some(block) => {
                    if self.blocks.insert(b, block.clone()).is_none() {
                        self.order.push_back(b);
                    }
                }
                None => missing.push(b),
            }
        }
        if !self.blocks.is_empty() {
            self.client.response_url = file.response_url.clone();
            self.response_origin = Some(origin(&file.response_url).to_string());
            self.client.observe_response(&file.response_url);
        }
        Some(missing)
    }

    fn observe_source(&mut self) {
        let received = origin(&self.client.response_url);
        if self.response_origin.as_deref().is_some_and(|expected| !same_response_origin(expected, received)) {
            self.mixed_origins = true;
            file_cache().forget(&self.key);
        }
        self.response_origin.get_or_insert_with(|| received.to_string());
    }

    /// Takes block `b` from the process-wide cache. Returns whether it held it.
    fn load_cached_block(&mut self, b: u64) -> bool {
        let data = file_cache()
            .files
            .get(&self.key)
            .filter(|file| self.response_origin.as_deref().is_none_or(|expected| same_response_origin(expected, &file.response_url)))
            .and_then(|f| f.blocks.get(&(self.block_size, b)).cloned());
        match data {
            Some(d) => {
                if self.blocks.insert(b, d).is_none() {
                    self.order.push_back(b);
                }
                true
            }
            None => false,
        }
    }

    fn store_whole(&mut self, data: Arc<[u8]>) {
        if !self.mixed_origins {
            file_cache().store((&self.key, &self.client.response_url), self.len, Slot::Whole, data.clone(), self.shared_budget);
        }
        self.whole = Some(data);
    }

    fn store_prefix(&mut self, data: Arc<[u8]>) {
        if !self.mixed_origins {
            file_cache().store((&self.key, &self.client.response_url), self.len, Slot::Prefix, data.clone(), self.shared_budget);
        }
        self.prefix = Some(data);
    }

    /// Drops the oldest blocks until the cache is within its size. Called before a read or a
    /// prefetch, never during one, so that the blocks being read stay.
    fn evict(&mut self) {
        while self.order.len() > self.max_blocks
            && let Some(old) = self.order.pop_front()
        {
            self.blocks.remove(&old);
        }
    }

    /// Fetches the next batch of the prefetch's blocks from its `from`th: those not cached within
    /// half the cache's blocks of the first (the gaps the batch's requests span included), so that
    /// the batch being read stays while the next one is fetched. They leave the plan, so that a
    /// block read again once evicted is fetched alone.
    fn fetch_planned(&mut self, from: usize) -> io::Result<()> {
        let batch = (self.max_blocks / 2).max(1) as u64;
        let (mut next, mut to) = (Vec::<u64>::new(), from);
        while let Some(&b) = self.plan.get(to) {
            if next.first().is_some_and(|&first| b >= first + batch) {
                break;
            }
            if !self.blocks.contains_key(&b) {
                next.push(b);
            }
            to += 1;
        }
        self.plan.drain(from..to);
        self.fetch_blocks(&next, 2)
    }

    fn store(&mut self, first_block: u64, data: Vec<u8>) {
        let bs = self.block_size as usize;
        let mut cache = file_cache();
        for (i, chunk) in data.chunks(bs).enumerate() {
            let b = first_block + i as u64;
            let chunk: Arc<[u8]> = chunk.into();
            if !self.mixed_origins {
                cache.store(
                    (&self.key, &self.client.response_url),
                    self.len,
                    Slot::Block(self.block_size, b),
                    chunk.clone(),
                    self.shared_budget,
                );
            }
            if self.blocks.insert(b, chunk).is_none() {
                self.order.push_back(b);
            }
        }
    }

    /// Fetches the given blocks (sorted, not cached), merging runs separated by at most `gap`
    /// blocks into one request. Blocks the process-wide cache holds are taken from it, and bytes
    /// the first request returned are not asked again. Separate runs are requested in parallel
    /// (see [`HttpOptions::parallel_ranges`] and [`Shared::fetch_runs_in_parallel`]), and stored
    /// in order as if fetched one after another.
    fn fetch_blocks(&mut self, blocks: &[u64], gap: u64) -> io::Result<()> {
        let blocks: Vec<u64> = blocks.iter().copied().filter(|&b| !self.load_cached_block(b)).collect();
        // Runs of consecutive missing blocks (or separated by at most `gap` cached ones).
        let mut spans = Vec::new();
        let mut i = 0;
        while i < blocks.len() {
            let start = blocks[i];
            let mut end = start;
            while i + 1 < blocks.len() && blocks[i + 1] <= end + 1 + gap {
                i += 1;
                end = blocks[i];
            }
            i += 1;
            spans.push((start, end));
        }
        // Fetched in parallel, a long run is split into near-equal pieces fetched together, as a
        // server sends each response at the pace of one connection: as many as there are request
        // slots, each of SPLIT_RUN_BYTES at least, so a run under twice that stays whole.
        if self.client.options.parallel_ranges {
            let least = SPLIT_RUN_BYTES.div_ceil(self.block_size).max(1);
            spans = spans
                .into_iter()
                .flat_map(|(start, end)| {
                    let len = end - start + 1;
                    let pieces = (len / least).clamp(1, PARALLEL_RUNS as u64);
                    let (size, longer) = (len / pieces, len % pieces);
                    // the first `longer` pieces take one block more
                    (0..pieces).map(move |k| {
                        let first = start + k * size + k.min(longer);
                        (first, first + size + u64::from(k < longer) - 1)
                    })
                })
                .collect();
        }
        let mut runs = Vec::new();
        for (start, end) in spans {
            let first = start * self.block_size;
            let last = ((end + 1) * self.block_size).min(self.len) - 1;
            // The first bytes, already held, start the run when it starts the file.
            let held = match &self.prefix {
                Some(p) if (first as usize) < p.len() => p.len().min(last as usize + 1) - first as usize,
                _ => 0,
            };
            let from = first + held as u64;
            runs.push(Run { start, held, from, last });
        }
        if runs.iter().filter(|run| run.requests()).count() > 1 && self.client.options.parallel_ranges {
            return self.fetch_runs_in_parallel(runs);
        }
        self.fetch_runs_serially(runs)
    }

    /// Fetches and stores `runs` one after another.
    fn fetch_runs_serially(&mut self, runs: Vec<Run>) -> io::Result<()> {
        for run in runs {
            let fetched = run.requests().then(|| self.client.fetch(Some((run.from, run.last))));
            if self.store_run(run, fetched)? {
                break;
            }
        }
        Ok(())
    }

    /// Fetches `runs` with up to [`PARALLEL_RUNS`] requests in flight, each sent by a thread of
    /// its own through a copy of the client (and a connection of the shared pool), with the
    /// caller's options and STAC asset policy. The runs are stored in order, each as soon as it
    /// and the runs before it are in, and a new request starts only when a run is stored: at most
    /// [`PARALLEL_RUNS`] responses are held at once, outside the cache. A fetch's runs span at
    /// most half the cache's blocks ([`Shared::fetch_planned`]), so they are within the cache's
    /// size together.
    ///
    /// A failed run (or a whole file a server sent instead) ends the fetch as the serial one
    /// would: no request starts after it, and those in flight are abandoned, their retries
    /// cancelled and a wait for a response ended within [`CANCEL_POLL`] (see [`CANCEL_READS`]).
    /// No request outlives the fetch.
    fn fetch_runs_in_parallel(&mut self, runs: Vec<Run>) -> io::Result<()> {
        // (index of the run, first byte, last byte) of each request
        let requests: Vec<(usize, u64, u64)> =
            runs.iter().enumerate().filter(|(_, run)| run.requests()).map(|(i, run)| (i, run.from, run.last)).collect();
        let context = crate::stac::AssetReadContext::capture(crate::stac::AssetReadPolicy::current());
        let window = RequestWindow::new(PARALLEL_RUNS);
        let (sender, responses) = std::sync::mpsc::channel::<(usize, Client, Result<Fetched>)>();
        std::thread::scope(|scope| {
            let mut threads = 0;
            for _ in 0..PARALLEL_RUNS.min(requests.len()) {
                let (sender, window, context, requests) = (sender.clone(), &window, &context, &requests);
                let mut client = self.client.clone();
                client.cancel = Some(window.cancel.clone());
                let spawned = std::thread::Builder::new().name("terrio-range".into()).spawn_scoped(scope, move || {
                    // A panicking request stops the others, so that the fetch ends.
                    let _stop = window.stop_on_panic();
                    // A response in flight is abandoned too when the fetch stops.
                    CANCEL_READS.set(Some(window.cancel.clone()));
                    while let Some((run, from, last)) = window.take(requests) {
                        let fetched = context.enter().and_then(|_scope| client.fetch(Some((from, last))));
                        // A copy of the client for the storing thread to take back (its URLs and
                        // S3 state): a small cost next to the request's.
                        if sender.send((run, client.clone(), fetched)).is_err() {
                            break;
                        }
                    }
                });
                // A thread that cannot be had: the others send the requests.
                threads += usize::from(spawned.is_ok());
            }
            drop(sender);
            if threads == 0 {
                return self.fetch_runs_serially(runs);
            }
            // A panic here stops the threads too, which would otherwise wait for a place.
            let _stop = window.stop_on_panic();
            let result = self.store_runs_in_order(runs, &responses, &window);
            // Threads still sending a request take no other and end with it, joined here.
            window.stop();
            result
        })
    }

    /// Stores `runs` in order as their `responses` come in, giving `window` back a request's place
    /// as each run is stored. See [`Shared::fetch_runs_in_parallel`].
    fn store_runs_in_order(
        &mut self,
        runs: Vec<Run>,
        responses: &std::sync::mpsc::Receiver<(usize, Client, Result<Fetched>)>,
        window: &RequestWindow,
    ) -> io::Result<()> {
        let mut arrived = HashMap::new();
        for (i, run) in runs.into_iter().enumerate() {
            let fetched = if run.requests() {
                let (client, fetched) = loop {
                    if let Some(response) = arrived.remove(&i) {
                        break response;
                    }
                    let (at, client, fetched) =
                        responses.recv().map_err(|_| io::Error::other(format!("{}: a request thread ended", redact(&self.client.url))))?;
                    arrived.insert(at, (client, fetched));
                };
                // the client of the last run in order is kept, as serially (its response URL)
                self.client.adopt(client);
                Some(fetched)
            } else {
                None
            };
            let requested = run.requests();
            if self.store_run(run, fetched)? {
                return Ok(());
            }
            if requested {
                window.release();
            }
        }
        Ok(())
    }

    /// Stores `run`: its held bytes, then the response for the rest (`None` when the held bytes
    /// cover it). Returns whether the server sent the whole file instead, which is then stored and
    /// ends the fetch.
    fn store_run(&mut self, run: Run, fetched: Option<Result<Fetched>>) -> io::Result<bool> {
        let Run { start, held, from, last } = run;
        let first = (start * self.block_size) as usize;
        let mut data = match &self.prefix {
            Some(p) if held > 0 => p[first..first + held].to_vec(),
            _ => Vec::new(),
        };
        if let Some(fetched) = fetched {
            let fetched = fetched.map_err(io::Error::other)?;
            self.observe_source();
            match fetched.status {
                // the response itself when nothing was held: one copy of the run less
                206 if fetched.data.len() as u64 == last - from + 1 && data.is_empty() => data = fetched.data,
                206 if fetched.data.len() as u64 == last - from + 1 => data.extend_from_slice(&fetched.data),
                200 if fetched.data.len() as u64 == self.len => {
                    self.store_whole(fetched.data.into());
                    return Ok(true);
                }
                status => {
                    return Err(io::Error::other(format!(
                        "{}: bytes {from}-{last}: unexpected response (status {status}, {} bytes)",
                        redact(&self.client.url),
                        fetched.data.len()
                    )));
                }
            }
        }
        self.store(start, data);
        Ok(false)
    }

    /// Copies the bytes at `pos` into `buf`, fetching what is missing. Returns the count copied.
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.client.check_cached_consent().map_err(io::Error::other)?;
        if pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let end = (pos + buf.len() as u64).min(self.len);
        if let Some(p) = &self.prefix
            && end <= p.len() as u64
            && self.whole.is_none()
        {
            let n = (end - pos) as usize;
            buf[..n].copy_from_slice(&p[pos as usize..end as usize]);
            return Ok(n);
        }
        if self.whole.is_none() {
            self.evict();
            let bs = self.block_size;
            let mut missing: Vec<u64> = (pos / bs..=(end - 1) / bs).filter(|b| !self.blocks.contains_key(b)).collect();
            // A block the prefetch asked for: its next batch, from there.
            if let Some(at) = missing.iter().find_map(|b| self.plan.binary_search(b).ok()) {
                self.fetch_planned(at)?;
                missing.retain(|b| !self.blocks.contains_key(b));
            }
            if self.whole.is_none() && !missing.is_empty() {
                // Requests of at most half the cache's blocks, as a prefetch's batches are.
                let batch = (self.max_blocks / 2).max(1);
                for chunk in missing.chunks(batch) {
                    self.fetch_blocks(chunk, 0)?;
                    if self.whole.is_some() {
                        break;
                    }
                }
                // Planned blocks past the batch, fetched with the read, leave the plan too.
                if !self.plan.is_empty() && missing.iter().any(|b| self.plan.binary_search(b).is_ok()) {
                    self.plan.retain(|b| missing.binary_search(b).is_err());
                }
            }
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

    /// The first `n` bytes of the file (fewer when it is shorter).
    fn head(&mut self, n: u64) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; n.min(self.len) as usize];
        let mut at = 0;
        while at < out.len() {
            let got = self.read_at(at as u64, &mut out[at..])?;
            if got == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
            }
            at += got;
        }
        Ok(out)
    }

    /// Writes the whole file to `out`: the bytes held from its start, then the rest in one
    /// request (see [`Client::stream_range`]).
    fn copy_to(&mut self, out: &mut dyn io::Write) -> Result<()> {
        self.client.check_cached_consent()?;
        if let Some(whole) = &self.whole {
            out.write_all(whole)?;
            return Ok(());
        }
        // The bytes held from the start: the first bytes, or the first blocks.
        let mut at = 0u64;
        if let Some(p) = &self.prefix {
            out.write_all(p)?;
            at = p.len() as u64;
        }
        while at < self.len {
            let b = at / self.block_size;
            let Some(block) = self.blocks.get(&b) else { break };
            let off = (at - b * self.block_size) as usize;
            if off >= block.len() {
                break;
            }
            out.write_all(&block[off..])?;
            at += (block.len() - off) as u64;
        }
        if at < self.len {
            self.client.stream_range(at, self.len - 1, out)?;
            self.observe_source();
        }
        Ok(())
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
        self.pos = crate::sparse::seek_position(self.pos, self.len, pos)?;
        Ok(self.pos)
    }
}

// GeoParquet files read in place: their footer, then the column chunks asked for.
#[cfg(feature = "parquet")]
impl parquet::file::reader::Length for RangeReader {
    fn len(&self) -> u64 {
        self.len
    }
}

#[cfg(feature = "parquet")]
impl parquet::file::reader::ChunkReader for RangeReader {
    type T = RangeReader;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        let mut r = self.clone();
        r.pos = start;
        Ok(r)
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<bytes::Bytes> {
        // A malformed file may declare more bytes than it has: refused before allocating them.
        if start.checked_add(length as u64).is_none_or(|end| end > self.len) {
            return Err(parquet::errors::ParquetError::EOF(format!(
                "{length} bytes at {start} are past the end of a {}-byte file",
                self.len
            )));
        }
        let mut buf = vec![0; length];
        let mut r = self.clone();
        r.pos = start;
        r.read_exact(&mut buf).map_err(|e| parquet::errors::ParquetError::External(Box::new(e)))?;
        Ok(bytes::Bytes::from(buf))
    }
}

/// A loopback server for the tests of other modules: serves `data` at every path, honouring
/// ranges, and records each request's `Range` header (`None` without one). Returns its base URL.
#[cfg(test)]
pub(crate) fn test_server(data: Vec<u8>) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut range = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("range:") {
                    range = Some(v.trim().to_string());
                }
            }
            record.lock().unwrap().push(range.clone());
            let parsed = range.as_deref().and_then(|r| {
                let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
                Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
            });
            let response = match parsed {
                Some((a, b)) if a < data.len() => {
                    let b = b.min(data.len() - 1);
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{}\r\nConnection: close\r\n\r\n",
                        b - a + 1,
                        data.len()
                    );
                    [head.into_bytes(), data[a..=b].to_vec()].concat()
                }
                _ => [format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len()).into_bytes(), data.clone()]
                    .concat(),
            };
            let _ = stream.write_all(&response);
        }
    });
    (base, seen)
}

/// The requests a [`test_routes`] server saw: each one's request line and headers (names lower
/// case).
#[cfg(test)]
pub(crate) type TestRequests = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

/// A route of a [`test_routes`] server: a path, and the status, headers and body of its answer.
#[cfg(test)]
pub(crate) type TestRoute = (String, u16, Vec<(String, String)>, Vec<u8>);

/// A loopback server for the tests of other modules: answers a request whose path (without its
/// query) is a route's with the route's status, headers and body (whatever the method), and any
/// other with a 404, recording every request. Returns its base URL.
#[cfg(test)]
pub(crate) fn test_routes(routes: Vec<TestRoute>) -> (String, TestRequests) {
    test_routes_with(|_| routes)
}

#[cfg(test)]
pub(crate) fn test_routes_with(make_routes: impl FnOnce(&str) -> Vec<TestRoute>) -> (String, TestRequests) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = make_routes(&base);
    let seen: TestRequests = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("").split('?').next().unwrap_or("").to_string();
            // A request's body is read before the answer, so that closing the connection does not
            // reset it.
            let length = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
            let mut body = vec![0u8; length];
            let _ = std::io::Read::read_exact(&mut reader, &mut body);
            record.lock().unwrap().push((request_line.trim().to_string(), headers));
            let (status, extra, body) = match routes.iter().find(|(p, ..)| *p == path) {
                Some((_, status, extra, body)) => (*status, extra.clone(), body.clone()),
                None => (404, Vec::new(), Vec::new()),
            };
            let extra: String = extra.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
            let head = format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n", body.len());
            let _ = stream.write_all(&[head.into_bytes(), body].concat());
        }
    });
    (base, seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vsi::listing::{xml_elements, xml_unescape};

    #[test]
    fn object_fetcher_uses_creation_options_without_inheriting_another_scope() {
        const CHILD: &str = "TERRIO_TEST_OBJECT_CREATION_OPTIONS";
        if std::env::var_os(CHILD).is_none() {
            // Keep ambient credentials/proxies out of this loopback regression without
            // changing the parent process's environment or exposing option values.
            let module = module_path!().split_once("::").unwrap().1;
            let name = format!("{module}::{}", stringify!(object_fetcher_uses_creation_options_without_inheriting_another_scope));
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", &name]).env(CHILD, "1").env("AWS_NO_SIGN_REQUEST", "YES");
            for key in [
                "GDAL_HTTP_HEADERS",
                "TERRIO_HTTP_HEADERS",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                command.env_remove(key);
            }
            let output = command.output().unwrap();
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let (url, seen) = serve_recording_by(
            |headers| {
                if headers.iter().any(|(name, _)| name == "authorization") { b"unexpected header".to_vec() } else { b"isolated".to_vec() }
            },
            None,
        );
        let fetcher = ObjectFetcher::new();
        let data = crate::config::with_options(&[("GDAL_HTTP_HEADERS", "Authorization: synthetic-other-scope")], || {
            fetcher.get(&Remote::Url(format!("{url}/plain")), ByteRange::Whole).unwrap().unwrap()
        });
        assert_eq!(data, b"isolated");
        let expected = format!("{url}/expected");
        let other = format!("{url}/other");
        let fetcher = crate::config::with_options(&[("AWS_S3_ENDPOINT", &expected), ("AWS_VIRTUAL_HOSTING", "NO")], ObjectFetcher::new);
        crate::config::with_options(&[("AWS_S3_ENDPOINT", &other), ("AWS_VIRTUAL_HOSTING", "NO")], || {
            fetcher.get(&Remote::S3(S3Object { bucket: "scope-test".into(), key: "key".into() }), ByteRange::Whole).unwrap();
        });
        assert!(seen.lock().unwrap().iter().any(|(request, _)| request.starts_with("GET /expected/scope-test/key ")));
    }

    #[test]
    fn response_origin_changes_invalidate_all_cached_parts() {
        let mut cache = FileCache::new();
        cache.store(("source", "https://b.example/item.json"), 128, Slot::Block(64, 0), Arc::from(&b"bbbb"[..]), 1024);
        cache.store(("source", "https://a.example/item.json"), 128, Slot::Block(32, 0), Arc::from(&b"aaaa"[..]), 1024);
        let file = cache.files.get("source").unwrap();
        assert!(!file.blocks.contains_key(&(64, 0)));
        assert_eq!(file.blocks[&(32, 0)].as_ref(), b"aaaa");
        assert_eq!(cache.bytes, 4);
        cache.store(("source", "https://A.example:443/other.json"), 128, Slot::Block(64, 1), Arc::from(&b"same"[..]), 1024);
        assert!(cache.files["source"].blocks.contains_key(&(32, 0)), "equivalent normalized origins keep their parts");
        assert_eq!(cache.bytes, 8);
    }

    #[test]
    fn document_fetch_refuses_a_cached_prefix_from_another_origin() {
        let _serial = cache_test_guard();
        let (base, _) = test_server(vec![b'a'; 64 * 1024]);
        let remote = Remote::Url(format!("{base}/item.json"));
        let key = Client::new(&remote, HttpOptions::from_env()).cache_key();
        file_cache().store(
            (&key, "https://other.example/item.json"),
            64 * 1024,
            Slot::Prefix,
            Arc::from(vec![b'b'; FIRST_READ as usize]),
            1 << 20,
        );
        let error = get_file_response(&remote, Redirects::Any).err().unwrap();
        assert!(matches!(error, Error::InvalidArgument(ref message) if message.contains("different response origins")), "{error}");
        let fresh = get_file_response(&remote, Redirects::Any).unwrap();
        assert!(fresh.body.iter().all(|byte| *byte == b'a'), "mixed cached parts were discarded");
    }

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
        /// The first response closes before its declared body length.
        BrokenBody,
        /// The first request of each connection fails with 503.
        Flaky,
        /// The first request fails with 503 and a 7-byte body, as an ingestion pipeline's
        /// network-statistics test server answers; every request after it, and every one with
        /// `/denied`, is answered as `Ranges`, but `/denied` with 403 and a 6-byte body.
        FlakyWithBody,
        /// `/old` redirects to `/data`.
        Redirect,
        /// Every request is answered 403, as S3 answers for a missing key or denied access.
        Forbidden,
        /// Range requests are answered 206 with the range in `Content-Range`, but the whole file.
        Oversized,
        /// Range requests are answered 206 with the range, its `Content-Range` giving `*` as the
        /// total length (`bytes 0-4095/*`).
        UnknownLength,
        /// Range requests are answered 206 with the range, without `Content-Range`.
        NoContentRange,
        /// The first request is answered with its range; every later one with the whole file (200).
        FirstRangeOnly,
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
                    (Mode::BrokenBody, _) if !failed => {
                        failed = true;
                        [head("200 OK", "", data.len()).into_bytes(), data[..1024].to_vec()].concat()
                    }
                    (Mode::Flaky, _) if !failed => {
                        failed = true;
                        head("503 Service Unavailable", "", 0).into_bytes()
                    }
                    (Mode::FlakyWithBody, _) if request_line.contains("/denied") => {
                        [head("403 Forbidden", "", 6).into_bytes(), b"denied".to_vec()].concat()
                    }
                    (Mode::FlakyWithBody, _) if !failed => {
                        failed = true;
                        [head("503 Service Unavailable", "", 7).into_bytes(), b"retry!!".to_vec()].concat()
                    }
                    (Mode::Redirect, _) if request_line.contains("/old") => head("302 Found", "Location: /data\r\n", 0).into_bytes(),
                    (Mode::NoRanges, _) | (_, None) => [head("200 OK", "", data.len()).into_bytes(), data.clone()].concat(),
                    (Mode::FirstRangeOnly, Some(_)) if failed => [head("200 OK", "", data.len()).into_bytes(), data.clone()].concat(),
                    (Mode::Oversized, Some((a, b))) => {
                        let extra = format!("Content-Range: bytes {a}-{b}/{}\r\n", data.len());
                        [head("206 Partial Content", &extra, data.len()).into_bytes(), data.clone()].concat()
                    }
                    (Mode::UnknownLength | Mode::NoContentRange, Some((a, b))) => {
                        let b = b.min(data.len() - 1);
                        let extra =
                            if matches!(mode, Mode::UnknownLength) { format!("Content-Range: bytes {a}-{b}/*\r\n") } else { String::new() };
                        [head("206 Partial Content", &extra, b - a + 1).into_bytes(), data[a..=b].to_vec()].concat()
                    }
                    (_, Some((a, b))) => {
                        let b = b.min(data.len() - 1);
                        let extra = format!("Content-Range: bytes {a}-{b}/{}\r\n", data.len());
                        [head("206 Partial Content", &extra, b - a + 1).into_bytes(), data[a..=b].to_vec()].concat()
                    }
                };
                let _ = stream.write_all(&response);
                if matches!(mode, Mode::FirstRangeOnly) {
                    failed = true;
                }
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

    // A prefix is read through the range reader: a broken first answer (here a 200 of a file
    // within ten times the range, which the reader takes whole) is retried as any first read is.
    #[test]
    fn bounded_prefix_retries_a_broken_body() {
        let data = test_data()[..100_000].to_vec();
        let (url, count) = serve(data.clone(), Mode::BrokenBody);
        let (prefix, length) = read_prefix(&Remote::Url(url), 16_384).unwrap();
        assert_eq!(prefix, data[..16_384]);
        assert_eq!(length, Some(data.len() as u64));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    // A prefix is read through the range reader, as GDAL's `/vsicurl/` reads a header: a server
    // that ignores ranges is refused at the first request for a file over ten times the range
    // ("Range downloading not supported by this server!"), its body unread, and a smaller file is
    // taken from that body.
    #[test]
    fn bounded_prefixes_stop_even_when_ranges_are_ignored() {
        let data = test_data();
        for (mode, len) in [(Mode::Ranges, data.len()), (Mode::NoRanges, 100_000)] {
            let (url, count) = serve(data[..len].to_vec(), mode);
            let (prefix, length) = read_prefix(&Remote::Url(url), 16_384).unwrap();
            assert_eq!(prefix, &data[..16_384]);
            assert_eq!(length, Some(len as u64));
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
        let (url, count) = serve(data.clone(), Mode::NoRanges);
        let error = read_prefix(&Remote::Url(url), 16_384).unwrap_err().to_string();
        assert!(error.contains("range downloading not supported by this server"), "{error}");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        // A prefix read by identification is in the process-wide cache: the open that follows it
        // makes no request for the same bytes.
        let (url, count) = serve(data.clone(), Mode::Ranges);
        read_prefix(&Remote::Url(url.clone()), 16_384).unwrap();
        let mut reader = RangeReader::open(&Remote::Url(url)).unwrap();
        assert_eq!(read_range(&mut reader, 0, 16_384), &data[..16_384]);
        assert_eq!(count.load(Ordering::SeqCst), 1);
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

    /// What a [`serve_gated`] server has seen.
    #[derive(Default)]
    struct Traffic {
        /// The requests being answered.
        in_flight: usize,
        /// The most requests answered at once.
        most: usize,
        /// The first byte of each request received, in order.
        firsts: Vec<usize>,
    }

    /// The requests a [`serve_gated`] server has seen, which its answers can wait on.
    #[derive(Default)]
    struct Gate {
        seen: Mutex<Traffic>,
        changed: std::sync::Condvar,
    }

    impl Gate {
        /// Waits until `done` holds of what the server has seen, at most `timeout`. What `done`
        /// reads should only grow (`most`, not `in_flight`): another request may be answered, and
        /// out of flight, before this one wakes.
        fn wait_until(&self, done: impl Fn(&Traffic) -> bool, timeout: Duration) {
            let seen = self.seen.lock().unwrap();
            drop(self.changed.wait_timeout_while(seen, timeout, |seen| !done(seen)).unwrap());
        }

        fn most(&self) -> usize {
            self.seen.lock().unwrap().most
        }

        /// The first bytes of the requests received, those of the open's first request (byte 0)
        /// left out.
        fn ranges_asked(&self) -> Vec<usize> {
            self.seen.lock().unwrap().firsts.iter().copied().filter(|&first| first > 0).collect()
        }
    }

    /// Serves `data` by range, a connection per thread. Each request is answered with the status
    /// `answer(its first byte, the gate)` returns, once it returns: 206 with the bytes asked,
    /// another status without a body.
    fn serve_gated(data: Vec<u8>, answer: impl Fn(usize, &Gate) -> u16 + Send + Sync + 'static) -> (String, Arc<Gate>) {
        let answer = Arc::new(answer);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/data", listener.local_addr().unwrap());
        let gate = Arc::new(Gate::default());
        let server_gate = gate.clone();
        let data = Arc::new(data);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (Ok(mut stream), data, gate, answer) = (stream, data.clone(), server_gate.clone(), answer.clone()) else {
                    continue;
                };
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut range = None;
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        loop {
                            line.clear();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                                break;
                            }
                            if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                                let (a, b) = v.trim().split_once('-').unwrap();
                                range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap().min(data.len() - 1)));
                            }
                        }
                        let (a, b) = range.unwrap_or((0, data.len() - 1));
                        {
                            let mut seen = gate.seen.lock().unwrap();
                            seen.in_flight += 1;
                            seen.most = seen.most.max(seen.in_flight);
                            seen.firsts.push(a);
                        }
                        gate.changed.notify_all();
                        let status = answer(a, &gate);
                        // Out of flight before the answer is sent, so that a client holding the
                        // answer sees it out.
                        gate.seen.lock().unwrap().in_flight -= 1;
                        gate.changed.notify_all();
                        let sent = if status == 206 {
                            let head = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{}\r\n\r\n",
                                b - a + 1,
                                data.len()
                            );
                            stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&data[a..=b]))
                        } else {
                            stream.write_all(format!("HTTP/1.1 {status} Refused\r\nContent-Length: 0\r\n\r\n").as_bytes())
                        };
                        if sent.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (url, gate)
    }

    /// The prefetch ranges of `n` runs of blocks far apart (none at the start of the file).
    fn separate_runs(n: u64) -> Vec<(u64, u64)> {
        (0..n).map(|i| ((4 + 8 * i) * 4096, 100)).collect()
    }

    #[test]
    fn parallel_range_requests_check_consent_with_the_callers_options() {
        // A credentialed asset's file: each request checks consent, which the caller's scope
        // gives and the environment of a request thread would not.
        let data = test_data();
        let (url, gate) = serve_gated(data.clone(), |first, gate| {
            if first > 0 {
                gate.wait_until(|seen| seen.most >= 2, Duration::from_secs(10));
            }
            206
        });
        crate::config::with_options(&[("TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS", "YES")], || {
            let policy = crate::stac::AssetReadPolicy::for_tests(&url, true);
            let _entered = policy.enter().expect("the caller's scope gives consent");
            let mut r = RangeReader::open_with(&Remote::Url(url.clone()), options()).unwrap();
            r.prefetch(&separate_runs(2)).expect("consent given by the caller");
            assert_eq!(gate.most(), 2, "requested in parallel");
            assert_eq!(read_range(&mut r, 12 * 4096, 100), data[12 * 4096..12 * 4096 + 100]);
        });
    }

    #[test]
    fn parallel_range_requests_take_the_policy_the_caller_entered_after_the_open() {
        // A file opened with an authorization header, then read under a STAC asset policy without
        // consent: the request threads check the caller's policy, and send nothing.
        let (url, gate) = serve_gated(test_data(), |_, _| 206);
        let r = crate::config::with_options(&[("GDAL_HTTP_HEADERS", "Authorization: synthetic")], || {
            RangeReader::open_with(&Remote::Url(url.clone()), options()).unwrap()
        });
        let policy = crate::stac::AssetReadPolicy::for_tests(&url, false);
        let _entered = policy.enter().expect("no credentials seen yet");
        let error = r.prefetch(&separate_runs(2)).unwrap_err().to_string();
        assert!(error.contains("TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS"), "{error}");
        assert_eq!(gate.ranges_asked(), Vec::<usize>::new(), "no request sent without consent");
    }

    #[test]
    fn a_failed_run_ends_the_prefetch_with_no_request_left() {
        // The first of nine runs fails at once; the others sent answer a retryable 503 a little
        // later, and the ninth waits for a place. The error comes back once the requests in flight
        // are abandoned: none of them is retried, and the ninth is never sent.
        let data = test_data();
        let runs = separate_runs(9);
        let failed = runs[0].0 as usize;
        let (url, gate) = serve_gated(data, move |first, _| match first {
            0 => 206,
            first if first == failed => 416,
            _ => {
                std::thread::sleep(Duration::from_millis(200));
                503
            }
        });
        let opts = HttpOptions { max_retry: 5, retry_delay: Duration::from_millis(50), ..options() };
        let r = RangeReader::open_with(&Remote::Url(url), opts).unwrap();
        assert!(r.prefetch(&runs).is_err());
        // Longer than the retries would wait.
        std::thread::sleep(Duration::from_millis(500));
        // The threads that had not taken a request when the run failed take none (the eight
        // places are not all taken on a loaded machine).
        let asked = gate.ranges_asked();
        let mut once = asked.clone();
        once.sort_unstable();
        once.dedup();
        assert_eq!(once.len(), asked.len(), "no request retried: {asked:?}");
        assert!(asked.contains(&failed), "{asked:?}");
        let first_eight: Vec<usize> = runs[..8].iter().map(|&(off, _)| off as usize).collect();
        assert!(asked.iter().all(|first| first_eight.contains(first)), "none sent after the failure: {asked:?}");
    }

    #[test]
    fn a_failed_run_returns_without_waiting_for_a_stalled_request() {
        // The first run fails at once, the second's request stalls: the error comes back as a
        // serial read would return it (it would not have sent the second request), not once the
        // stalled one ends.
        let runs = separate_runs(2);
        let (failed, stalled) = (runs[0].0 as usize, runs[1].0 as usize);
        let (url, gate) = serve_gated(test_data(), move |first, gate| {
            if first == stalled {
                std::thread::sleep(Duration::from_secs(20));
            } else if first == failed {
                // the stalled request is in flight when the run fails
                gate.wait_until(|seen| seen.most >= 2, Duration::from_secs(10));
                return 416;
            }
            206
        });
        let r = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        let started = Instant::now();
        assert!(r.prefetch(&runs).is_err());
        assert!(started.elapsed() < Duration::from_secs(10), "waited {:?} for the stalled request", started.elapsed());
        assert_eq!(gate.most(), 2, "the stalled request was in flight when the run failed");
    }

    #[test]
    fn prefetch_requests_separate_runs_in_parallel() {
        let data = test_data();
        for parallel in [true, false] {
            // Each request waits for three in flight: the three runs together, unless serial.
            let wait = if parallel { Duration::from_secs(10) } else { Duration::from_millis(100) };
            let (url, gate) = serve_gated(data.clone(), move |first, gate| {
                if first > 0 {
                    gate.wait_until(|seen| seen.most >= 3, wait);
                }
                206
            });
            let opts = HttpOptions { parallel_ranges: parallel, ..options() };
            let mut r = RangeReader::open_with(&Remote::Url(url), opts).unwrap();
            r.prefetch(&[(10 * 4096, 100), (30 * 4096, 100), (60 * 4096, 5000)]).unwrap();
            assert_eq!(gate.most(), if parallel { 3 } else { 1 }, "parallel_ranges={parallel}");
            assert_eq!(gate.ranges_asked().len(), 3);
            assert_eq!(read_range(&mut r, 60 * 4096, 5000), data[60 * 4096..60 * 4096 + 5000]);
            assert_eq!(read_range(&mut r, 10 * 4096, 100), data[10 * 4096..10 * 4096 + 100]);
        }
    }

    #[test]
    fn a_long_run_is_fetched_in_pieces_in_parallel() {
        // 2.5 MiB in one range (640 blocks of 4 KiB): two pieces of 1.25 MiB in flight together
        // (each waits for both), one request when serial. Then 1 MiB and a block, under twice
        // SPLIT_RUN_BYTES: one request, without a small piece for the rest.
        let data: Vec<u8> = (0..5u32 << 20).map(|i| (i * 7 % 251) as u8).collect();
        let (from, len) = (4096u64, 5u64 << 19);
        let (short_from, short_len) = (3u64 << 20, (1u64 << 20) + 4096);
        for parallel in [true, false] {
            let wait = if parallel { Duration::from_secs(10) } else { Duration::from_millis(100) };
            let (url, gate) = serve_gated(data.clone(), move |first, gate| {
                if first > 0 && (first as u64) < short_from {
                    gate.wait_until(|seen| seen.most >= 2, wait);
                }
                206
            });
            let opts = HttpOptions { parallel_ranges: parallel, cache_size: 64 << 20, ..options() };
            let mut r = RangeReader::open_with(&Remote::Url(url), opts).unwrap();
            r.prefetch(&[(from, len)]).unwrap();
            let mut asked = gate.ranges_asked();
            asked.sort_unstable();
            let pieces: Vec<usize> = if parallel { vec![4096, 4096 + 320 * 4096] } else { vec![4096] };
            assert_eq!(asked, pieces, "parallel_ranges={parallel}");
            assert_eq!(gate.most(), if parallel { 2 } else { 1 }, "parallel_ranges={parallel}");
            assert_eq!(read_range(&mut r, from, len as usize), data[from as usize..(from + len) as usize]);
            r.prefetch(&[(short_from, short_len)]).unwrap();
            assert_eq!(gate.ranges_asked().len(), pieces.len() + 1, "one request for a run under 2 MiB");
            assert_eq!(read_range(&mut r, short_from, short_len as usize), data[short_from as usize..(short_from + short_len) as usize]);
        }
    }

    #[test]
    fn prefetch_has_at_most_eight_requests_in_flight() {
        // Nine runs: each request is held until eight are in flight, then until the ninth is sent
        // (the open's request and nine others), for a further 300 ms at most. The ninth is sent
        // only once a run is in, after the wait, with eight in flight until then.
        let data = test_data();
        let (url, gate) = serve_gated(data.clone(), |first, gate| {
            if first > 0 {
                gate.wait_until(|seen| seen.most >= 8 || seen.firsts.len() >= 10, Duration::from_secs(10));
                gate.wait_until(|seen| seen.firsts.len() >= 10, Duration::from_millis(300));
            }
            206
        });
        let mut r = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        let runs = separate_runs(9);
        r.prefetch(&runs).unwrap();
        assert_eq!(gate.most(), PARALLEL_RUNS);
        assert_eq!(gate.ranges_asked().len(), 9);
        for (off, len) in runs {
            assert_eq!(read_range(&mut r, off, len as usize), data[off as usize..(off + len) as usize]);
        }
        assert_eq!(gate.ranges_asked().len(), 9, "the runs were all prefetched");
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

    // A TIFF's byte counts reach `prefetch` unchecked: a count near `u64::MAX` must not wrap the
    // range's end (which planned about 2^48 blocks and aborted on the allocation).
    #[test]
    fn a_prefetch_whose_end_overflows_is_clamped_to_the_file() {
        // The read after the prefetch must find its blocks in the process-wide cache too.
        let _guard = cache_test_guard();
        let data = test_data();
        let (url, count) = serve(data.clone(), Mode::Ranges);
        let mut r = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        let opened = count.load(Ordering::SeqCst);
        r.prefetch(&[(16, u64::MAX - 15), (5, u64::MAX)]).unwrap();
        // The prefetch fetched (the blocks to the file's end, or their first batch) ...
        assert!(count.load(Ordering::SeqCst) > opened);
        let prefetched = count.load(Ordering::SeqCst);
        // ... so that a read just past the first block needs no request of its own.
        let at = 4096 + 16;
        assert_eq!(read_range(&mut r, at, 100), data[at as usize..at as usize + 100]);
        assert_eq!(count.load(Ordering::SeqCst), prefetched);
    }

    /// The inclusive byte ranges of the requests a [`test_server`] saw.
    fn requested(seen: &[Option<String>]) -> Vec<(u64, u64)> {
        seen.iter()
            .filter_map(|r| r.as_deref()?.strip_prefix("bytes=")?.split_once('-').map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap())))
            .collect()
    }

    // A prefetch larger than the cache (a whole COG's tiles read as one window) is fetched in
    // batches as it is read, each byte once: fetched whole, it was evicted before it was read and
    // fetched again a block per request.
    #[test]
    fn a_prefetch_larger_than_the_cache_is_fetched_once_in_batches() {
        let data = test_data();
        let (url, seen) = test_server(data.clone());
        let opts = HttpOptions { cache_size: 16 * 4096, ..options() };
        let mut r = RangeReader::open_with(&Remote::Url(format!("{url}/big.bin")), opts).unwrap();
        r.prefetch(&[(0, data.len() as u64)]).unwrap();
        let mut read = Vec::new();
        r.read_to_end(&mut read).unwrap();
        assert!(read == data);
        let ranges = requested(&seen.lock().unwrap());
        let bytes: u64 = ranges.iter().map(|(a, b)| b - a + 1).sum();
        // The first read, then batches of half the cache (8 blocks), merged across the prefix.
        assert!(bytes <= data.len() as u64 + 2 * 4096, "{bytes} bytes in {ranges:?}");
        assert!(ranges.len() <= 2 + data.len().div_ceil(8 * 4096), "{ranges:?}");
        // A block of the prefetch read again once evicted is fetched alone, not with a batch.
        let before = seen.lock().unwrap().len();
        assert_eq!(read_range(&mut r, 5 * 4096 + 100, 50), data[5 * 4096 + 100..5 * 4096 + 150]);
        let again: Vec<Option<String>> = seen.lock().unwrap()[before..].to_vec();
        assert!(again.len() <= 1 && again.iter().flatten().all(|r| r == "bytes=20480-24575"), "{again:?}");
    }

    // A read longer than a batch of a prefetch: the blocks past its batch are fetched with the read,
    // and leave the plan too, so that one of them read again once evicted is fetched alone.
    #[test]
    fn blocks_a_read_fetched_past_its_batch_leave_the_plan() {
        let _guard = cache_eviction_test_guard();
        let data = test_data();
        let (url, seen) = test_server(data.clone());
        let opts = HttpOptions { cache_size: 16 * 4096, ..options() };
        let mut r = RangeReader::open_with(&Remote::Url(format!("{url}/long.bin")), opts).unwrap();
        r.prefetch(&[(0, data.len() as u64)]).unwrap();
        // Blocks 0-19: 1-8 fetched by the prefetch, 9-16 a batch, 17-19 with the read.
        assert_eq!(read_range(&mut r, 0, 20 * 4096), data[..20 * 4096]);
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert!(rest == data[20 * 4096..]);
        let before = seen.lock().unwrap().len();
        assert_eq!(read_range(&mut r, 18 * 4096, 100), data[18 * 4096..18 * 4096 + 100]);
        assert_eq!(requested(&seen.lock().unwrap()[before..]), [(18 * 4096, 19 * 4096 - 1)]);
    }

    // A read longer than half the cache, with nothing prefetched, is fetched in requests of at
    // most half the cache's blocks, not in one request for the whole read.
    #[test]
    fn a_read_longer_than_half_the_cache_is_fetched_in_bounded_requests() {
        let data = test_data();
        let (url, seen) = test_server(data.clone());
        let opts = HttpOptions { cache_size: 16 * 4096, ..options() };
        let mut r = RangeReader::open_with(&Remote::Url(format!("{url}/read.bin")), opts).unwrap();
        let before = seen.lock().unwrap().len();
        assert_eq!(read_range(&mut r, 0, 20 * 4096), data[..20 * 4096]);
        let ranges = requested(&seen.lock().unwrap()[before..]);
        assert!(!ranges.is_empty() && ranges.iter().all(|(a, b)| b - a < 8 * 4096), "{ranges:?}");
    }

    // A sparse prefetch larger than the cache (every third block: batches merge across two-block
    // gaps) read in file order and in reverse: each request spans at most half the cache, gaps
    // included. In file order no block is fetched twice; in reverse, only the first batch, which
    // the prefetch fetched before the reads began at the other end.
    #[test]
    fn a_sparse_prefetch_larger_than_the_cache_is_fetched_in_bounded_batches() {
        let data = test_data();
        let (url, seen) = test_server(data.clone());
        let opts = HttpOptions { cache_size: 16 * 4096, ..options() };
        let blocks: Vec<u64> = (1..70).step_by(3).collect();
        for (name, reverse) in [("forward.bin", false), ("reverse.bin", true)] {
            let before = seen.lock().unwrap().len();
            let mut r = RangeReader::open_with(&Remote::Url(format!("{url}/{name}")), opts.clone()).unwrap();
            r.prefetch(&blocks.iter().map(|b| (b * 4096, 4096)).collect::<Vec<_>>()).unwrap();
            let mut order = blocks.clone();
            if reverse {
                order.reverse();
            }
            for b in order {
                let at = (b * 4096) as usize;
                assert_eq!(read_range(&mut r, b * 4096, 4096), data[at..at + 4096], "{name} block {b}");
            }
            let ranges = requested(&seen.lock().unwrap()[before..]);
            assert!(ranges.iter().all(|(a, b)| b - a < 8 * 4096), "{name}: {ranges:?}");
            let refetched: u64 =
                blocks.iter().filter(|&&b| ranges.iter().filter(|(s, e)| *s <= b * 4096 && b * 4096 <= *e).count() > 1).count() as u64;
            // The first batch: blocks 1, 4 and 7, within half the cache (8 blocks) of block 1.
            assert!(refetched <= if reverse { 3 } else { 0 }, "{name}: {refetched} blocks fetched twice in {ranges:?}");
        }
    }

    /// A child forked while other threads read remote files reads remote files too: the locks of
    /// remote reads that every thread shares (the cache of remote files, the request statistics,
    /// the path-specific options) are held by the forking thread across the fork, never left held
    /// in the child by a thread it does not have. In a process of its own (`par::alone`).
    ///
    /// Not on macOS: there a child forked while another thread was in the middle of a remote
    /// read died of SIGTRAP (exit by signal 5), as forked children of multi-threaded test
    /// processes do in `par`'s tests. The cause is unconfirmed; #696 tracks a macOS-safe check
    /// of the same locks.
    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn a_child_forked_during_remote_reads_reads_remote_files() {
        crate::par::alone(90, || {
            use std::sync::atomic::AtomicBool;
            let (url, _) = serve(test_data(), Mode::Ranges);
            // One read first, which does the one-time initializations of the code a read runs
            // (the dependencies' lazy statics, such as the HTTP client's default headers): no
            // lock can hold those across a fork, and a child forked while another thread was in
            // the middle of one would wait for it forever. Forking in the middle of an operation
            // is what GDAL forbids too; the locks shared by reads that follow are the subject.
            let mut r = RangeReader::open_with(&Remote::Url(url.clone()), options()).unwrap();
            assert_eq!(read_range(&mut r, 0, 1000), test_data()[..1000]);
            let stop = Arc::new(AtomicBool::new(false));
            let mut threads = Vec::new();
            // A thread reading a remote file in a loop, as an application's worker does.
            let (u, s) = (url.clone(), stop.clone());
            threads.push(std::thread::spawn(move || {
                while !s.load(Ordering::Acquire) {
                    let mut r = RangeReader::open_with(&Remote::Url(u.clone()), options()).unwrap();
                    assert_eq!(read_range(&mut r, 100_000, 10_000), test_data()[100_000..110_000]);
                }
            }));
            // A thread holding those locks for a while at a time, so that every fork finds them held.
            let s = stop.clone();
            threads.push(std::thread::spawn(move || {
                while !s.load(Ordering::Acquire) {
                    {
                        let _cache = file_cache();
                        let _hosts = stats(&HOST_STATS);
                        let _files = stats(&FILE_STATS);
                        let _options = path_options_read();
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }));
            std::thread::sleep(Duration::from_millis(50));
            for _ in 0..5 {
                let u = url.clone();
                let exit = crate::par::in_child(30, move || {
                    RangeReader::open_with(&Remote::Url(u), options())
                        .is_ok_and(|mut r| read_range(&mut r, 200_000, 1000) == test_data()[200_000..201_000])
                });
                assert_eq!(exit, Some(0), "the child hung (None) or failed (1)");
            }
            stop.store(true, Ordering::Release);
            for t in threads {
                t.join().unwrap();
            }
            true
        });
    }

    /// A child forked while another thread records a retry warning records its own: the retry
    /// warnings' lock is held by the forking thread across the fork, as the other locks of remote
    /// reads are, never left held in the child by a thread it does not have. In a process of its
    /// own (`par::alone`), so that no other test delays the fork until the lock is free anyway
    /// (one that held the fork's other locks would).
    #[test]
    #[cfg(unix)]
    fn a_child_forked_while_a_retry_warning_is_recorded_records_its_own() {
        use std::sync::atomic::AtomicBool;
        crate::par::alone(60, || {
            let (holding, released) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (h, r) = (holding.clone(), released.clone());
            // A thread recording a warning when the fork starts, holding the lock long enough that
            // the fork finds it held.
            let holder = std::thread::spawn(move || {
                let warnings = retry_warnings();
                h.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(300));
                r.store(true, Ordering::Release);
                drop(warnings);
            });
            while !holding.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            // The grandchild checks that the fork waited for the lock, then records and takes a
            // warning of its own; no other process sees its recording.
            let exit = crate::par::in_child(10, move || {
                record_retry_warnings(true);
                retry_warning(503, "http://127.0.0.1/forked-child.bin", Duration::from_millis(500), 0);
                released.load(Ordering::Acquire) && take_retry_warnings().iter().any(|w| w.contains("forked-child.bin"))
            });
            holder.join().is_ok() && exit == Some(0)
        });
    }

    /// A child forked while another thread holds the process agent's lock gets an agent of its
    /// own: the lock is held by the forking thread across the fork, as the module's other locks
    /// are, never left held in the child by a thread it does not have.
    #[test]
    #[cfg(unix)]
    fn a_child_forked_while_the_process_agent_lock_is_held_gets_its_own() {
        use std::sync::atomic::AtomicBool;
        crate::par::alone(60, || {
            let (holding, released) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (h, r) = (holding.clone(), released.clone());
            let holder = std::thread::spawn(move || {
                crate::par::fork_safe();
                let agent = PROCESS_AGENT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                h.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(300));
                r.store(true, Ordering::Release);
                drop(agent);
            });
            while !holding.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let exit = crate::par::in_child(10, move || {
                let options = HttpOptions::default();
                process_agent((options.timeout, options.connect_timeout, options.user_agent.clone()), &options);
                released.load(Ordering::Acquire)
            });
            holder.join().is_ok() && exit == Some(0)
        });
    }

    /// A server keeping its connections open (HTTP/1.1 keep-alive) answering any range of
    /// `data` at any path, and the number of connections it has accepted.
    #[cfg(unix)]
    fn serve_keep_alive(data: Vec<u8>) -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicU64::new(0));
        let accepted = connections.clone();
        let data = Arc::new(data);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                accepted.fetch_add(1, Ordering::SeqCst);
                let data = data.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let head = read_head(&mut reader);
                        if head.is_empty() {
                            return;
                        }
                        let range = head.iter().find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            let (a, b) = l.strip_prefix("range: bytes=")?.split_once('-')?;
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?.min(data.len() - 1)))
                        });
                        let (a, b) = range.unwrap_or((0, data.len() - 1));
                        let status = if range.is_some() { "206 Partial Content" } else { "200 OK" };
                        let head = format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{}\r\n\r\n",
                            b - a + 1,
                            data.len()
                        );
                        if stream.write_all(&[head.as_bytes(), &data[a..=b]].concat()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (url, connections)
    }

    /// A server answering requests on each connection with successive `heads` (then the last)
    /// and "ok", never closing a
    /// connection itself, and the number of connections it has accepted.
    fn serve_open_connections(heads: Vec<&'static str>, split: bool) -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicU64::new(0));
        let accepted = connections.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                accepted.fetch_add(1, Ordering::SeqCst);
                let heads = heads.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = 0;
                    while !read_head(&mut reader).is_empty() {
                        let head = heads[request.min(heads.len() - 1)];
                        request += 1;
                        let response = format!("{head}\r\nContent-Length: 2\r\n\r\nok");
                        let result = if split {
                            let boundary = response.find("\r\n\r\n").unwrap() + 4;
                            stream.write_all(&response.as_bytes()[..boundary]).and_then(|_| {
                                std::thread::sleep(std::time::Duration::from_millis(10));
                                stream.write_all(&response.as_bytes()[boundary..])
                            })
                        } else {
                            stream.write_all(response.as_bytes())
                        };
                        if result.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (url, connections)
    }

    /// An HTTP/1.0 response without the keep-alive option ends its connection (RFC 9112, section
    /// 9.3): the next request opens another one rather than being sent on a connection the server
    /// may have closed (issue #632). With `Connection: keep-alive`, or in HTTP/1.1, the
    /// connection is reused. The server here leaves every connection open, so a reuse shows.
    #[test]
    fn http10_responses_without_keep_alive_end_their_connection() {
        for (head, expected) in [
            ("HTTP/1.0 200 OK", 3),
            ("HTTP/1.0 200 OK\r\nConnection: Keep-Alive", 1),
            ("HTTP/1.1 200 OK", 1),
            ("HTTP/1.1 200 OK\r\nConnection: close", 3),
        ] {
            let (url, connections) = serve_open_connections(vec![head], false);
            let agent = new_agent(&options());
            for _ in 0..3 {
                let mut response = agent.get(&url).call().unwrap();
                assert_eq!(response.body_mut().read_to_vec().unwrap(), b"ok");
            }
            assert_eq!(connections.load(Ordering::SeqCst), expected, "{head:?}");
        }
    }

    /// Persistence is checked again after each request on a reused connection.
    #[test]
    fn http10_keep_alive_can_end_after_a_reused_request() {
        let (url, connections) = serve_open_connections(vec!["HTTP/1.0 200 OK\r\nConnection: keep-alive", "HTTP/1.0 200 OK"], false);
        let agent = new_agent(&options());
        for expected in [1, 1, 2] {
            let mut response = agent.get(&url).call().unwrap();
            assert_eq!(response.body_mut().read_to_vec().unwrap(), b"ok");
            assert_eq!(connections.load(Ordering::SeqCst), expected);
        }
    }

    /// Informational heads do not determine final-response persistence (RFC 9112 section 9.3).
    #[test]
    fn informational_headers_before_http10_do_not_keep_the_connection_alive() {
        for split in [false, true] {
            let (url, connections) = serve_open_connections(vec!["HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.0 200 OK"], split);
            let agent = new_agent(&options());
            for _ in 0..3 {
                let mut response = agent.get(&url).call().unwrap();
                assert_eq!(response.body_mut().read_to_vec().unwrap(), b"ok");
            }
            assert_eq!(connections.load(Ordering::SeqCst), 3, "split={split}");
        }
    }

    #[test]
    fn reads_whether_a_response_closes_its_connection() {
        assert_eq!(closes_after(b"HTTP/1.0 200 OK\r\nContent-Le"), None);
        assert_eq!(closes_after(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok"), Some(true));
        assert_eq!(closes_after(b"HTTP/1.0 206 Partial\nConnection: foo, keep-alive\n\n"), Some(false));
        assert_eq!(closes_after(b"HTTP/1.0 200 OK\r\nX-Connection: keep-alive\r\n\r\n"), Some(true));
        assert_eq!(closes_after(b"HTTP/1.1 200 OK\r\n\r\n"), Some(false));
        assert_eq!(closes_after(b"HTTP/1.1 103 Early Hints\r\n\r\n"), None);
        assert_eq!(closes_after(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.0 200 OK\r\nContent-Le"), None);
        assert_eq!(closes_after(b"HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.0 200 OK\r\n\r\n"), Some(true));
        assert_eq!(closes_after(b"HTTP/1.0 200 OK\n\nConnection: keep-alive\r\n\r\n"), Some(true));
    }

    /// Remote files opened one after another, each with a client of its own, send their requests
    /// over one connection, and files opened from 4 threads at once over at most 4: the clients
    /// share the process's agent. In a process of its own (`par::alone`), where no other test
    /// rebuilds that agent with other settings.
    #[test]
    #[cfg(unix)]
    fn remote_files_opened_one_after_another_share_one_connection() {
        crate::par::alone(60, || {
            let (url, connections) = serve_keep_alive(test_data());
            let open_and_read = |name: String| {
                let mut r = RangeReader::open_with(&Remote::Url(name), options()).unwrap();
                assert_eq!(read_range(&mut r, 100_000, 1000), test_data()[100_000..101_000]);
            };
            for i in 0..8 {
                open_and_read(format!("{url}one-{i}.bin"));
            }
            let one = connections.load(Ordering::SeqCst);
            std::thread::scope(|s| {
                for t in 0..4 {
                    let (open_and_read, url) = (&open_and_read, &url);
                    s.spawn(move || (0..8).for_each(|i| open_and_read(format!("{url}many-{t}-{i}.bin"))));
                }
            });
            let many = connections.load(Ordering::SeqCst);
            assert!(one == 1 && many <= 4, "{one} connections for 8 files one after another, {many} after 32 more from 4 threads");
            true
        });
    }

    /// A child forked inside an [`AgentScope`] sends the requests it makes itself through an
    /// agent of its own, over connections of its own: the scope's agent, built in the parent,
    /// holds the parent's sockets, which the child must neither use nor close (ending the
    /// inherited scope in the child leaks that agent). The parent's agent keeps its connection.
    #[test]
    #[cfg(unix)]
    fn a_child_forked_inside_an_agent_scope_uses_connections_of_its_own() {
        crate::par::alone(60, || {
            let (url, connections) = serve_keep_alive(test_data());
            let get = move |agent: &ureq::Agent| {
                agent
                    .get(url.as_str())
                    .header("Range", "bytes=0-99")
                    .call()
                    .is_ok_and(|mut r| r.body_mut().read_to_vec().is_ok_and(|body| body == test_data()[..100]))
            };
            let scope = AgentScope::new();
            let parent = agent(&options());
            assert!(get(&parent));
            assert_eq!(connections.load(Ordering::SeqCst), 1);
            let in_child = get.clone();
            let exit = crate::par::in_child(10, move || {
                let own = in_child(&agent(&options()));
                drop(scope);
                own
            });
            assert_eq!(exit, Some(0), "the child hung (None) or failed (1)");
            assert_eq!(connections.load(Ordering::SeqCst), 2, "the child's request opened a connection of its own");
            get(&parent) && connections.load(Ordering::SeqCst) == 2
        });
    }

    /// A remote file's reader and an object fetcher made before a fork send the child's requests
    /// over a connection of the child's own, not over the warm one they hold from the parent; the
    /// parent's requests keep that one.
    #[test]
    #[cfg(unix)]
    fn clients_made_before_a_fork_use_connections_of_the_childs_own() {
        crate::par::alone(60, || {
            let (url, connections) = serve_keep_alive(test_data());
            let count = || connections.load(Ordering::SeqCst);
            let mut reader = RangeReader::open_with(&Remote::Url(format!("{url}reader.bin")), options()).unwrap();
            let fetcher = ObjectFetcher::new();
            let object = Remote::Url(format!("{url}object.bin"));
            let fetch = |at: usize| {
                fetcher.get(&object, ByteRange::At(at as u64, 100)).ok().flatten().as_deref() == Some(&test_data()[at..at + 100])
            };
            assert!(fetch(0));
            assert_eq!(read_range(&mut reader, 0, 100), test_data()[..100]);
            let warm = count();
            // Each child makes one request through one of the two; the parent then makes one more.
            assert_eq!(crate::par::in_child(10, || read_range(&mut reader, 100_000, 100) == test_data()[100_000..100_100]), Some(0));
            assert_eq!(count(), warm + 1, "the reader's request in the child opened a connection of its own");
            assert_eq!(read_range(&mut reader, 200_000, 100), test_data()[200_000..200_100]);
            assert_eq!(count(), warm + 1, "the parent's request kept its connection");
            assert_eq!(crate::par::in_child(10, || fetch(1000)), Some(0));
            assert_eq!(count(), warm + 2, "the fetcher's request in the child opened a connection of its own");
            fetch(2000) && count() == warm + 2
        });
    }

    #[test]
    fn server_without_ranges_downloads_once() {
        // At most ten times the first range (4096 bytes), as GDAL accepts.
        let data = test_data()[..40_000].to_vec();
        let (url, count) = serve(data.clone(), Mode::NoRanges);
        let mut r = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        assert_eq!(r.len(), data.len() as u64);
        assert_eq!(read_range(&mut r, 12_345, 7_000), data[12_345..19_345]);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn server_without_ranges_is_refused_for_a_large_file() {
        // GDAL: "Range downloading not supported by this server!", before the body is read.
        let (url, _) = serve(test_data(), Mode::NoRanges);
        let err = RangeReader::open_with(&Remote::Url(url), options()).err().unwrap().to_string();
        assert!(err.contains("range downloading not supported"), "{err}");
    }

    #[test]
    fn a_partial_response_is_read_no_further_than_the_range() {
        let (url, _) = serve(test_data(), Mode::Oversized);
        let err = RangeReader::open_with(&Remote::Url(url), options()).err().unwrap().to_string();
        assert!(err.contains("larger than the range asked"), "{err}");
    }

    /// A 206 whose `Content-Range` gives `*` as the total length (RFC 9110 §14.4: the server does
    /// not know it) is refused for that reason, its URL redacted, not as a 206 without
    /// `Content-Range`.
    #[test]
    fn a_partial_response_of_unknown_total_length_is_refused_for_that_reason() {
        let (url, _) = serve(test_data(), Mode::UnknownLength);
        let err = RangeReader::open_with(&Remote::Url(format!("{url}?sig=secret")), options()).err().unwrap().to_string();
        assert_eq!(err, format!("HTTP: {url}?<redacted>: Content-Range gives no total length (`*`)"));
    }

    /// A 206 without `Content-Range` is refused for that reason, not as one whose total length is
    /// `*`.
    #[test]
    fn a_partial_response_without_content_range_is_refused_for_that_reason() {
        let (url, _) = serve(test_data(), Mode::NoContentRange);
        let err = RangeReader::open_with(&Remote::Url(url.clone()), options()).err().unwrap().to_string();
        assert_eq!(err, format!("HTTP: {url}: no Content-Range in a 206 response"));
    }

    #[test]
    fn error_responses_count_as_requests_with_their_bodies() {
        // As GDAL's network statistics count them: each response is a request, with its body.
        let _serial = stats_test_guard();
        let data = test_data();
        let (url, count) = serve(data.clone(), Mode::FlakyWithBody);
        let host = host_of(&url).to_string();
        let mut r = RangeReader::open_with(&Remote::Url(url.clone()), options()).unwrap();
        assert_eq!(read_range(&mut r, 0, 10), data[..10]);
        let sent = count.load(Ordering::SeqCst);
        let stats = |host: &str| fetch_stats_by_host().into_iter().find(|(h, _)| h == host).map(|(_, s)| s).unwrap();
        let first = stats(&host);
        assert_eq!(first.requests, sent, "the retried 503 is a request");
        assert_eq!(first.bytes - 7, (first.requests - 1) * 4096, "its 7-byte body and the 4096-byte block");
        assert!(RangeReader::open_with(&Remote::Url(url.replace("/data", "/denied")), options()).is_err());
        let after = stats(&host);
        assert_eq!((after.requests - first.requests, after.bytes - first.bytes), (1, 6), "a refused request and its body");
        let files: BTreeMap<String, FetchStats> = fetch_stats_by_file().into_iter().collect();
        assert_eq!(files[&format!("/vsicurl/{url}")], first, "per file, as per host");
        assert_eq!(files[&format!("/vsicurl/{}", url.replace("/data", "/denied"))], FetchStats { requests: 1, bytes: 6 });
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

    /// Serves "ok" on a local port after closing the first `dropped` connections without a
    /// response and answering the next request 503. Returns the address.
    fn serve_after_failures(dropped: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                    line.clear();
                }
                let response: &[u8] = if i < dropped {
                    continue;
                } else if i == dropped {
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                };
                let _ = stream.write_all(response);
            }
        });
        addr
    }

    /// While recording, each retry leaves GDAL's warning: code 0 for a connection closed without
    /// a response, the status otherwise, and the delay slept, doubled at each retry; a URL holding
    /// a signature is redacted.
    #[test]
    fn records_retry_warnings_as_gdal_words_them() {
        let range_url = format!("http://{}/file.bin", serve_after_failures(2));
        let get_url = format!("http://{}/doc.json?sig=secret", serve_after_failures(1));
        record_retry_warnings(true);
        let opts = HttpOptions { retry_delay: Duration::from_millis(100), ..options() };
        let mut r = RangeReader::open_with(&Remote::Url(range_url.clone()), opts).unwrap();
        assert_eq!(read_range(&mut r, 0, 2), b"ok");
        let body = get_with_headers(&get_url, &[("X-Test".to_string(), "1".to_string())], Redirects::Any).unwrap();
        assert_eq!(body, b"ok");
        // Other tests may retry meanwhile: only these servers' warnings count.
        let warnings = take_retry_warnings();
        record_retry_warnings(false);
        let ours = |base: &str| warnings.iter().filter(|w| w.contains(base)).cloned().collect::<Vec<_>>();
        assert_eq!(
            ours(&range_url),
            [
                format!("HTTP error code: 0 - {range_url}. Retrying again in 0.1 secs"),
                format!("HTTP error code: 0 - {range_url}. Retrying again in 0.2 secs"),
                format!("HTTP error code: 503 - {range_url}. Retrying again in 0.4 secs"),
            ]
        );
        let redacted = get_url.replace("sig=secret", "<redacted>");
        let get_warnings = ours(&redacted);
        assert_eq!(get_warnings.len(), 2, "{warnings:?}");
        assert!(get_warnings[0].starts_with(&format!("HTTP error code: 0 - {redacted}. Retrying again in ")), "{get_warnings:?}");
        assert!(get_warnings[1].starts_with(&format!("HTTP error code: 503 - {redacted}. Retrying again in ")), "{get_warnings:?}");
        assert!(warnings.iter().all(|w| !w.contains("secret")));
        // Not recording: nothing is kept.
        let off_url = format!("http://{}/file.bin", serve_after_failures(1));
        let mut r = RangeReader::open_with(&Remote::Url(off_url.clone()), options()).unwrap();
        assert_eq!(read_range(&mut r, 0, 2), b"ok");
        record_retry_warnings(true);
        let left = take_retry_warnings();
        record_retry_warnings(false);
        assert!(left.iter().all(|w| !w.contains(&off_url)), "{left:?}");
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
        assert_eq!(no_total_length(Some("bytes 0-99/many")), "Content-Range gives no readable total length");
        assert_eq!(redact("https://a.org/b.tif?sv=1&sig=abc"), "https://a.org/b.tif?<redacted>");
        assert_eq!(redact("https://a.org/b.tif?x=1"), "https://a.org/b.tif?x=1");
        assert_eq!(resolve_redirect("https://a.org/x/y.tif?q=1", "/z.tif"), "https://a.org/z.tif");
        assert_eq!(resolve_redirect("https://a.org/x/y.tif", "z.tif"), "https://a.org/x/z.tif");
        assert_eq!(resolve_redirect("https://a.org/x/y.tif", "https://b.org/c"), "https://b.org/c");
    }

    /// A request as a recording server saw it: its request line and headers (names lower case).
    type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

    /// Serves `data` with ranges on a local port, recording every request. Requests for `/old`
    /// are redirected to `redirect_to` when given. Returns the server's base URL and its record.
    fn serve_recording(data: Vec<u8>, redirect_to: Option<String>) -> (String, Seen) {
        serve_recording_by(move |_| data.clone(), redirect_to)
    }

    /// [`serve_recording`] of the data `data_for` returns for a request's headers.
    fn serve_recording_by(
        data_for: impl Fn(&[(String, String)]) -> Vec<u8> + Send + 'static,
        redirect_to: Option<String>,
    ) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                let mut headers = Vec::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                    }
                }
                let range = headers.iter().find(|(k, _)| k == "range").and_then(|(_, v)| {
                    let (a, b) = v.strip_prefix("bytes=")?.split_once('-')?;
                    Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                });
                let data = data_for(&headers);
                record.lock().unwrap().push((request_line.trim().to_string(), headers));
                let head = |status: &str, extra: &str, len: usize| {
                    format!("HTTP/1.1 {status}\r\nContent-Length: {len}\r\nConnection: close\r\n{extra}\r\n")
                };
                let response = match (&redirect_to, range) {
                    (Some(to), _) if request_line.contains("/old") => head("302 Found", &format!("Location: {to}\r\n"), 0).into_bytes(),
                    (_, Some((a, b))) => {
                        let b = b.min(data.len() - 1);
                        let extra = format!("Content-Range: bytes {a}-{b}/{}\r\n", data.len());
                        [head("206 Partial Content", &extra, b - a + 1).into_bytes(), data[a..=b].to_vec()].concat()
                    }
                    _ => [head("200 OK", "", data.len()).into_bytes(), data.clone()].concat(),
                };
                let _ = stream.write_all(&response);
            }
        });
        (base, seen)
    }

    fn header_values(seen: &Seen, name: &str) -> Vec<String> {
        seen.lock().unwrap().iter().flat_map(|(_, h)| h.iter().filter(|(k, _)| k == name).map(|(_, v)| v.clone())).collect()
    }

    /// An open file's requests carry the headers resolved when it was opened, as GDAL's
    /// `/vsicurl/` handle reads its options when it is created: a path-specific credential
    /// replaced while the file is open changes neither what the open file sends nor what its cache
    /// entry holds. An open with credential A never reads bytes fetched with credential B.
    #[test]
    fn an_open_file_keeps_the_credentials_it_was_opened_with() {
        // The reopen reads from the process-wide cache: hence the guard.
        let _serial = cache_test_guard();
        // Every byte of the file is the credential's label: `A` for `Authorization: A`.
        let (base, seen) = serve_recording_by(
            |headers| {
                let label = headers.iter().find(|(k, _)| k == "authorization").map_or("-", |(_, v)| v.as_str());
                label.as_bytes().repeat(16 * 4096 / label.len())
            },
            None,
        );
        let url = format!("{base}/by-credential.bin");
        let prefix = format!("{base}/");
        let read_block = |reader: &mut RangeReader, block: u64| {
            let mut buf = vec![0u8; 4096];
            reader.seek(SeekFrom::Start(block * 4096)).unwrap();
            reader.read_exact(&mut buf).unwrap();
            buf
        };
        set_path_specific_option(&prefix, "GDAL_HTTP_HEADERS", Some("Authorization: A"));
        // The default cache size: a reader's budget bounds the whole process-wide cache when it
        // stores, so a small one would evict other tests' entries.
        let opts = HttpOptions { cache_size: HttpOptions::default().cache_size, ..options() };
        let mut opened_with_a = RangeReader::open_with(&Remote::Url(url.clone()), opts.clone()).unwrap();
        set_path_specific_option(&prefix, "GDAL_HTTP_HEADERS", Some("Authorization: B"));
        let block = read_block(&mut opened_with_a, 5);
        set_path_specific_option(&prefix, "GDAL_HTTP_HEADERS", Some("Authorization: A"));
        let mut reopened_with_a = RangeReader::open_with(&Remote::Url(url.clone()), opts).unwrap();
        let again = read_block(&mut reopened_with_a, 5);
        clear_path_specific_options(Some(prefix.as_str()));
        assert!(block.iter().all(|&b| b == b'A'), "the reader opened with A read bytes fetched with B");
        assert!(again.iter().all(|&b| b == b'A'), "A's cache entry holds bytes fetched with B");
        assert_eq!(header_values(&seen, "authorization"), vec!["A", "A"], "the header block, then block 5, both with A");
    }

    #[test]
    fn same_origin_redirects_are_decided_by_scheme_host_and_port() {
        let same = |asked: &str, to: &str| Redirects::SameOrigin.allow(asked, to);
        assert!(same("http://a.test:81/x.json", "http://a.test:81/y.json"));
        assert!(same("https://a.test/x.json", "HTTPS://A.TEST:443/y.json"));
        assert!(!same("http://a.test:81/x.json", "http://a.test:82/x.json"), "another port");
        assert!(!same("http://a.test/x.json", "https://a.test/x.json"), "another scheme");
        assert!(!same("http://a.test/x.json", "http://a.test@b.test/x.json"), "user information before another host");
        assert!(!same("http://a.test/x.json", "http://b.test/a.test/x.json"));
        assert!(!same("http://a.test/x.json", "no-scheme/x.json"));
        assert!(Redirects::Any.allow("http://a.test/x.json", "http://b.test/x.json"));
    }

    /// The refusal of a redirect under `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN`, to the origin `other`.
    fn kept_on_origin(other: &str) -> String {
        format!(
            "a redirect to another origin ({other}) was not followed: reads stay on the origin they were given \
             (TERRIO_HTTP_FOLLOW_CROSS_ORIGIN is not YES)"
        )
    }

    /// A read of the whole file at a URL.
    type ReadAll = fn(&str) -> Result<Vec<u8>>;

    /// Reads of `url` through the range reader, a whole-body `GET` and an object fetched whole
    /// (a request without a redirect policy of its own), each the whole of the file.
    fn reads() -> [(&'static str, ReadAll); 3] {
        [
            ("range reader", |url| {
                let reader = RangeReader::open(&Remote::Url(url.to_string()))?;
                let mut out = Vec::new();
                reader.lock()?.copy_to(&mut out)?;
                Ok(out)
            }),
            ("whole-body GET", |url| get(&Remote::Url(url.to_string()))),
            ("object fetched whole", |url| {
                Ok(ObjectFetcher::new().get(&Remote::Url(url.to_string()), ByteRange::Whole)?.unwrap_or_default())
            }),
        ]
    }

    /// A read follows a redirect to another origin (another port), unless
    /// `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN` is set to another value than `YES`: the redirect is then
    /// refused before anything is sent to that origin, by an error naming the option.
    #[test]
    fn a_read_follows_a_redirect_to_another_origin_unless_the_option_keeps_it_on_its_own() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let (other, other_seen) = serve_recording(data.clone(), None);
        let (asked, _) = serve_recording(vec![0], Some(format!("{other}/data.bin")));
        for (label, read) in reads() {
            let url = |value: &str| format!("{asked}/old-{}-{value}.bin", label.replace(' ', "-"));
            let reached = other_seen.lock().unwrap().len();
            for value in ["NO", "off", "0", "never"] {
                let refused = crate::config::with_options(&[(FOLLOW_CROSS_ORIGIN, value)], || read(&url(value)));
                let message = refused.as_ref().map_or_else(ToString::to_string, |d| format!("read, {} bytes", d.len()));
                assert!(message.contains(&kept_on_origin(&other)), "{label}, {value}: {message}");
            }
            assert_eq!(other_seen.lock().unwrap().len(), reached, "{label}: a refused redirect reached the other origin");
            assert_eq!(read(&url("default")).unwrap(), data, "{label}, not set");
            for value in ["YES", "true", ""] {
                let followed = crate::config::with_options(&[(FOLLOW_CROSS_ORIGIN, value)], || read(&url(value)));
                assert_eq!(followed.unwrap(), data, "{label}, {value:?}");
            }
            assert!(other_seen.lock().unwrap().len() > reached, "{label}: the redirects were not followed");
        }
    }

    /// Under `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN=NO`, a redirect within the origin is followed.
    #[test]
    fn a_read_kept_on_its_origin_follows_redirects_within_it() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 241) as u8).collect();
        let body = data.clone();
        let (base, seen) = test_routes_with(move |base| {
            vec![
                ("/old.bin".to_string(), 302, vec![("Location".to_string(), format!("{base}/new.bin"))], Vec::new()),
                ("/new.bin".to_string(), 200, Vec::new(), body),
            ]
        });
        for (label, read) in reads() {
            let url = format!("{base}/old.bin?{}", label.replace(' ', "-"));
            let followed = crate::config::with_options(&[(FOLLOW_CROSS_ORIGIN, "NO")], || read(&url));
            assert_eq!(followed.unwrap(), data, "{label}");
        }
        assert!(seen.lock().unwrap().iter().any(|(line, _)| line.contains("/new.bin")));
    }

    /// Serves `data` with ranges to the requests of its first block (a range from 0), and
    /// redirects every other request to the same path on `to`. Returns its base URL.
    fn serve_first_block_then_redirect(data: Vec<u8>, to: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let range = read_head(&mut reader).iter().find_map(|line| {
                    let line = line.to_ascii_lowercase();
                    let (a, b) = line.strip_prefix("range: bytes=")?.split_once('-')?;
                    Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                });
                let response = match range {
                    Some((0, b)) => {
                        let b = b.min(data.len() - 1);
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{b}/{}\r\nConnection: close\r\n\r\n",
                            b + 1,
                            data.len()
                        );
                        [head.into_bytes(), data[..=b].to_vec()].concat()
                    }
                    _ => {
                        format!("HTTP/1.1 302 Found\r\nLocation: {to}{path}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        base
    }

    /// A read kept on its origin keeps its later requests there too: a file whose first block
    /// comes from its own origin and the rest from a redirect to another is refused when the rest
    /// is read, by the range reader and by a remote vector document downloaded whole, and the
    /// other origin is not asked. Without the option, the range reader reads the file whole, and
    /// the document is refused for its bytes' two origins after the other origin was asked.
    #[test]
    fn a_read_kept_on_its_origin_keeps_its_later_requests_there() {
        let features: Vec<String> = (0..400)
            .map(|i| {
                format!(
                    r#"{{"type":"Feature","properties":{{"name":"feature {i:04}"}},"geometry":{{"type":"Point","coordinates":[{i},1]}}}}"#
                )
            })
            .collect();
        let data = format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(",")).into_bytes();
        assert!(data.len() > 2 * FIRST_READ as usize, "{} bytes", data.len());
        let (other, other_seen) = serve_recording(data.clone(), None);
        let asked = serve_first_block_then_redirect(data.clone(), other.clone());
        let ranged = |url: &str| -> Result<Vec<u8>> {
            let reader = RangeReader::open(&Remote::Url(url.to_string()))?;
            let mut out = Vec::new();
            reader.lock()?.copy_to(&mut out)?;
            Ok(out)
        };
        let document = |url: &str| crate::open_vector(url, &crate::VectorOpenOptions::default()).map(|v| v.layers[0].features.len());
        let kept = [(FOLLOW_CROSS_ORIGIN, "NO")];
        let refused = crate::config::with_options(&kept, || ranged(&format!("{asked}/ranged.bin")));
        assert!(refused.as_ref().is_err_and(|e| e.to_string().contains(&kept_on_origin(&other))), "{:?}", refused.map(|d| d.len()));
        let refused = crate::config::with_options(&kept, || document(&format!("{asked}/features")));
        assert!(refused.as_ref().is_err_and(|e| e.to_string().contains(&kept_on_origin(&other))), "{refused:?}");
        assert_eq!(other_seen.lock().unwrap().len(), 0, "the other origin was asked");
        assert_eq!(ranged(&format!("{asked}/ranged-followed.bin")).unwrap(), data);
        let reached = other_seen.lock().unwrap().len();
        let mixed = document(&format!("{asked}/features-followed"));
        assert!(mixed.as_ref().is_err_and(|e| e.to_string().contains("came from different response origins")), "{mixed:?}");
        assert!(other_seen.lock().unwrap().len() > reached, "the document's redirect was not followed");
    }

    /// The header lines of a request or a response, up to the blank line.
    fn read_head(reader: &mut impl BufRead) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                return lines;
            }
            lines.push(line.trim().to_string());
        }
    }

    /// The process's agent is reused while its settings, the proxy variables and the process
    /// stay the same, and rebuilt when one of them changes (a forked child has another id).
    #[test]
    fn process_agent_is_rebuilt_when_settings_proxy_or_process_change() {
        let slot = std::sync::Mutex::new(None);
        let builds = std::cell::Cell::new(0);
        let options = HttpOptions::default();
        let settings = |timeout: Option<Duration>| -> AgentSettings { (timeout, options.connect_timeout, options.user_agent.clone()) };
        let get = |pid: u32, proxy: Option<&str>, timeout: Option<Duration>| {
            let proxy = vec![proxy.map(std::ffi::OsString::from)];
            cached_agent(&slot, pid, proxy, settings(timeout), || {
                builds.set(builds.get() + 1);
                new_agent(&options)
            });
            builds.get()
        };
        assert_eq!(get(1, None, None), 1);
        assert_eq!(get(1, None, None), 1);
        assert_eq!(get(1, None, Some(Duration::from_secs(5))), 2);
        assert_eq!(get(1, Some("http://proxy:3128"), Some(Duration::from_secs(5))), 3);
        assert_eq!(get(2, Some("http://proxy:3128"), Some(Duration::from_secs(5))), 4);
        assert_eq!(get(2, Some("http://proxy:3128"), Some(Duration::from_secs(5))), 4);
    }

    /// `agent_in` keeps an agent made in this process and replaces one from another (a fork).
    #[test]
    fn an_agent_from_another_process_is_replaced() {
        let made = new_agent(&HttpOptions::default());
        let here = std::cell::Cell::new(0);
        let child = || {
            here.set(here.get() + 1);
            new_agent(&HttpOptions::default())
        };
        agent_in(std::process::id(), &made, child);
        assert_eq!(here.get(), 0);
        agent_in(std::process::id().wrapping_add(1), &made, child);
        assert_eq!(here.get(), 1);
    }

    /// A loopback HTTP proxy standing in for every host: it records the target (`host:port`) of
    /// each tunnel a client opens (`CONNECT`), then answers the request inside it with
    /// `answer(target)`, a whole response. Hosts are seen as the client asks for them, without a
    /// name being resolved or the machine being left.
    fn serve_proxy(answer: impl Fn(&str) -> String + Send + 'static) -> (ureq::Agent, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = ureq::Proxy::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let targets: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let record = targets.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let connect = read_head(&mut reader);
                let target = connect.first().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
                record.lock().unwrap().push(target.clone());
                let _ = stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
                let _ = read_head(&mut reader);
                let _ = stream.write_all(answer(&target).as_bytes());
            }
        });
        let agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).proxy(Some(proxy)).build().into();
        (agent, targets)
    }

    /// A client for `s3://bucket/key.tif` on Amazon's endpoints (over plain HTTP, through the
    /// proxy `agent`), without credentials or a configured region.
    fn amazon_client(agent: ureq::Agent) -> Client {
        let remote = Remote::S3(S3Object { bucket: "bucket".to_string(), key: "key.tif".to_string() });
        Client::with_auth(&remote, options(), agent, Some((None, None)), crate::config::thread_options())
    }

    /// The options of [`amazon_client`]'s requests: Amazon's endpoints over plain HTTP, whatever
    /// the environment says.
    fn amazon_scope() -> HashMap<String, Option<String>> {
        HashMap::from([
            ("AWS_HTTPS".to_string(), Some("NO".to_string())),
            ("AWS_S3_ENDPOINT".to_string(), None),
            ("AWS_VIRTUAL_HOSTING".to_string(), None),
        ])
    }

    /// On Amazon's endpoints a bucket's region, as a server names it (`x-amz-bucket-region`),
    /// becomes part of the host (`s3.<region>.amazonaws.com`): a value that is not a region name
    /// is refused before anything is sent, so no request reaches any other host. A region name is
    /// followed to its regional endpoint.
    #[test]
    fn a_bucket_region_that_is_not_a_region_name_reaches_no_other_host() {
        let refused = [
            "evil.example",
            "evil.example/x?",
            "evil.example:8080/x?",
            "x@evil.example/",
            "evil%2Eexample",
            "us-west-2.evil.example",
            "us-west-2?",
            "us-west-2#",
            "us-west-2:80",
            "US-WEST-2",
        ];
        for region in refused {
            let header = region.to_string();
            let (agent, targets) = serve_proxy(move |_| {
                format!("HTTP/1.1 301 Moved Permanently\r\nx-amz-bucket-region: {header}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            });
            crate::config::push_scope(amazon_scope());
            let mut client = amazon_client(agent);
            let got = client.fetch(Some((0, 99))).map(|f| f.status);
            crate::config::pop_scope();
            let err = got.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(err.contains("which is not a region name"), "{region:?}: {err}");
            assert_eq!(*targets.lock().unwrap(), vec!["bucket.s3.amazonaws.com:80".to_string()], "{region:?}");
        }
        let (agent, targets) = serve_proxy(|target| match target {
            "bucket.s3.amazonaws.com:80" => {
                "HTTP/1.1 301 Moved Permanently\r\nx-amz-bucket-region: us-west-2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string()
            }
            _ => "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata"
                .to_string(),
        });
        crate::config::push_scope(amazon_scope());
        let mut client = amazon_client(agent);
        let got = client.fetch(Some((0, 99))).map(|f| f.data);
        crate::config::pop_scope();
        assert_eq!(got.unwrap(), b"data");
        let expected = ["bucket.s3.amazonaws.com:80", "bucket.s3.us-west-2.amazonaws.com:80"];
        assert_eq!(*targets.lock().unwrap(), expected.map(String::from).to_vec());
    }

    #[test]
    fn region_names_are_checked_as_their_use_needs() {
        for name in ["us-east-1", "us-gov-west-1", "cn-northwest-1", "us-isob-east-1", "ap-southeast-5", "eu-central-2"] {
            assert!(s3::is_region_name(name), "{name}");
        }
        for name in [
            "",
            "us",
            "us-east",
            "us-east-",
            "-east-1",
            "useast-1",
            "u-east-1",
            "us-east-1a",
            "us-East-1",
            "us--1",
            "us-east-1.x",
            "fr-par",
            "nyc3",
        ] {
            assert!(!s3::is_region_name(name), "{name}");
        }
        // On Amazon's endpoints, region names only.
        crate::config::push_scope(amazon_scope());
        let amazon = ["us-west-2", "fr-par", "nyc3", "a.b"].map(s3::usable_region);
        crate::config::pop_scope();
        assert_eq!(amazon, [true, false, false, false]);
        // With an endpoint of one's own, where the region only signs: S3-compatible names too.
        crate::config::push_scope(HashMap::from([("AWS_S3_ENDPOINT".to_string(), Some("http://127.0.0.1:9".to_string()))]));
        let own = ["us-west-2", "fr-par", "nyc3", "my_region", "a.b", "a/b", "a:1", "a@b", "a%2E", ""].map(s3::usable_region);
        crate::config::pop_scope();
        assert_eq!(own, [true, true, true, true, false, false, false, false, false, false]);
    }

    #[test]
    fn parses_gdal_http_headers_as_gdal() {
        // Comma-separated, as a tile server builds it.
        assert_eq!(
            parse_header_lines("x-ms-version:2021-08-06,Authorization: Bearer abc"),
            vec![("x-ms-version".into(), "2021-08-06".into()), ("Authorization".into(), "Bearer abc".into())]
        );
        // GDAL's documented example: a quoted value with an escaped comma and double quote.
        assert_eq!(
            parse_header_lines(r#"Foo: Bar,"Baz: escaped backslash \\, escaped double-quote \", end of value",Another: Header"#),
            vec![
                ("Foo".into(), "Bar".into()),
                ("Baz".into(), r#"escaped backslash \, escaped double-quote ", end of value"#.into()),
                ("Another".into(), "Header".into()),
            ]
        );
        // \r\n-separated: commas are part of values.
        assert_eq!(parse_header_lines("A: 1,2\r\nB: 3"), vec![("A".into(), "1,2".into()), ("B".into(), "3".into())]);
        // A line break smuggled inside a quoted value is not a valid header: it is dropped rather
        // than split into a second, injected header.
        assert_eq!(parse_header_lines("\"X-A: ok\nInjected: yes\",B: 2"), vec![("B".into(), "2".into())]);
        assert!(parse_header_lines("no colon here").is_empty());
    }

    #[test]
    fn path_specific_options_follow_gdal_precedence() {
        let p = "/vsicurl/https://precedence.test/";
        set_path_specific_option(p, "gdal_http_headers", Some("A: outer"));
        set_path_specific_option(&format!("{p}inner/"), "GDAL_HTTP_HEADERS", Some("A: inner"));
        assert_eq!(path_specific_option(&format!("{p}inner/x.tif"), "GDAL_HTTP_HEADERS").as_deref(), Some("A: inner"));
        assert_eq!(path_specific_option(&format!("{p}other/x.tif"), "GDAL_HTTP_HEADERS").as_deref(), Some("A: outer"));
        assert_eq!(path_specific_option("/vsicurl/https://elsewhere.test/x.tif", "GDAL_HTTP_HEADERS"), None);
        set_path_specific_option(&format!("{p}inner/"), "GDAL_HTTP_HEADERS", None);
        assert_eq!(path_specific_option(&format!("{p}inner/x.tif"), "GDAL_HTTP_HEADERS").as_deref(), Some("A: outer"));
        clear_path_specific_options(Some(p));
        assert_eq!(path_specific_option(&format!("{p}x.tif"), "GDAL_HTTP_HEADERS"), None);
    }

    #[test]
    fn path_specific_header_reaches_its_host_and_not_a_redirect_target() {
        let data = test_data();
        let (other, other_seen) = serve_recording(data.clone(), None);
        let (base, seen) = serve_recording(data.clone(), Some(format!("{other}/data")));
        set_path_specific_option(&format!("/vsicurl/{base}/"), "GDAL_HTTP_HEADERS", Some("Authorization: Bearer secret-for-a"));
        // Read from the host the token is bound to: every request carries it.
        let mut r = RangeReader::open_with(&Remote::Url(format!("{base}/data")), options()).unwrap();
        assert_eq!(read_range(&mut r, 9000, 10), data[9000..9010]);
        let sent = header_values(&seen, "authorization");
        assert_eq!(sent.len(), seen.lock().unwrap().len(), "every request to the bound host has the header");
        assert!(sent.iter().all(|v| v == "Bearer secret-for-a"));
        // Read a URL on the bound host that redirects to another host: the first request has the
        // token, the redirected ones do not.
        let mut r = RangeReader::open_with(&Remote::Url(format!("{base}/old")), options()).unwrap();
        assert_eq!(read_range(&mut r, 100, 10), data[100..110]);
        assert!(!other_seen.lock().unwrap().is_empty(), "the redirect was followed");
        assert!(header_values(&other_seen, "authorization").is_empty(), "the token never reaches the other host");
        clear_path_specific_options(Some(&format!("/vsicurl/{base}/")));
        // Negative control: without the option, no header is sent.
        let before = seen.lock().unwrap().len();
        let _ = RangeReader::open_with(&Remote::Url(format!("{base}/data")), options()).unwrap();
        let after: Vec<_> = seen.lock().unwrap()[before..].to_vec();
        assert!(after.iter().all(|(_, h)| h.iter().all(|(k, _)| k != "authorization")));
    }

    #[test]
    fn thread_options_set_headers_for_their_thread() {
        let data = test_data();
        let (base, seen) = serve_recording(data, None);
        crate::config::push_scope(HashMap::from([("GDAL_HTTP_HEADERS".to_string(), Some("X-Tenant: one".to_string()))]));
        let _ = RangeReader::open_with(&Remote::Url(format!("{base}/data")), options()).unwrap();
        crate::config::pop_scope();
        // Another thread, without the scope, sends no such header.
        let url = format!("{base}/data");
        std::thread::spawn(move || RangeReader::open_with(&Remote::Url(url), options()).unwrap()).join().unwrap();
        let tenants = header_values(&seen, "x-tenant");
        assert_eq!(tenants, vec!["one".to_string()]);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// An object fetcher's requests carry the headers its creating thread set, from any thread (a
    /// store's chunks are fetched by worker threads): its clients resolve their headers under the
    /// fetcher's options, not the fetching thread's.
    #[test]
    fn an_object_fetcher_sends_its_creating_threads_headers_from_any_thread() {
        let (base, seen) = serve_recording(test_data(), None);
        crate::config::push_scope(HashMap::from([("GDAL_HTTP_HEADERS".to_string(), Some("X-Tenant: one".to_string()))]));
        let fetcher = ObjectFetcher::new();
        crate::config::pop_scope();
        let remote = Remote::Url(format!("{base}/chunk"));
        let got = std::thread::scope(|s| s.spawn(|| fetcher.get(&remote, ByteRange::At(0, 10))).join().unwrap()).unwrap();
        assert_eq!(got.map(|d| d.len()), Some(10));
        assert_eq!(header_values(&seen, "x-tenant"), vec!["one".to_string()]);
    }

    #[test]
    fn azure_token_goes_to_marked_blobs_only() {
        let data = test_data();
        let (base, seen) = serve_recording(data, None);
        crate::config::push_scope(HashMap::from([
            ("AZURE_STORAGE_ACCESS_TOKEN".to_string(), Some("tok".to_string())),
            ("CPL_AZURE_ENDPOINT".to_string(), Some(base.clone())),
        ]));
        let _ = RangeReader::open_with(&Remote::Url(format!("{}{base}/data", azure_bearer_prefix())), options()).unwrap();
        // The same URL read as a plain URL (GDAL's /vsicurl/) gets no Azure token.
        let _ = RangeReader::open_with(&Remote::Url(format!("{base}/data")), options()).unwrap();
        crate::config::pop_scope();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let auth = |i: usize| seen[i].1.iter().find(|(k, _)| k == "authorization").map(|(_, v)| v.clone());
        let version = |i: usize| seen[i].1.iter().find(|(k, _)| k == "x-ms-version").map(|(_, v)| v.clone());
        assert_eq!(auth(0).as_deref(), Some("Bearer tok"));
        assert_eq!(version(0).as_deref(), Some(AZURE_X_MS_VERSION));
        assert!(seen[0].0.starts_with("GET /data"), "the marker is never sent: {}", seen[0].0);
        assert_eq!(auth(1), None);
    }

    #[test]
    fn counts_requests_per_host_and_ingests_header_bytes() {
        let _serial = stats_test_guard();
        let data = test_data();
        let (base, seen) = serve_recording(data.clone(), None);
        let opts = HttpOptions { ingested_bytes_at_open: Some(10_000), ..options() };
        let mut r = RangeReader::open_with(&Remote::Url(format!("{base}/data")), opts).unwrap();
        // 10,000 bytes rounded up to whole 4096-byte blocks, in one request.
        assert_eq!(header_values(&seen, "range"), vec!["bytes=0-12287".to_string()]);
        assert_eq!(read_range(&mut r, 12_000, 200), data[12_000..12_200], "the ingested blocks are cached");
        assert_eq!(seen.lock().unwrap().len(), 1);
        let host = host_of(&base).to_string();
        let stats = fetch_stats_by_host().into_iter().find(|(h, _)| *h == host).map(|(_, s)| s).unwrap();
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.bytes, 12_288);
    }

    /// A panic elsewhere while the statistics were locked poisons their locks: counting per host
    /// and per file, reporting and resetting go on (they stopped silently before).
    #[test]
    fn network_statistics_survive_a_poisoned_lock() {
        let _serial = stats_test_guard();
        let _ = std::thread::spawn(|| {
            let _hosts = HOST_STATS.lock().unwrap();
            let _files = FILE_STATS.lock().unwrap();
            panic!("a panic while the statistics are locked");
        })
        .join();
        assert!(HOST_STATS.is_poisoned() && FILE_STATS.is_poisoned());
        let name = "/vsicurl/http://poisoned.example/a.tif";
        let counted = || {
            let host = fetch_stats_by_host().into_iter().find(|(h, _)| h == "poisoned.example").map(|(_, s)| (s.requests, s.bytes));
            let file = fetch_stats_by_file().into_iter().find(|(f, _)| f == name).map(|(_, s)| (s.requests, s.bytes));
            (host, file)
        };
        record_fetch("http://poisoned.example/a.tif", name, 7);
        assert_eq!(counted(), (Some((1, 7)), Some((1, 7))), "counted per host and per file");
        reset_fetch_stats();
        assert_eq!(counted(), (None, None), "reset per host and per file");
    }

    /// Serializes the tests that check the process-wide statistics against the one that resets them.
    fn stats_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The bytes the recorded requests asked for (each `Range` header's length).
    fn asked_bytes(seen: &Seen) -> u64 {
        header_values(seen, "range")
            .iter()
            .map(|r| {
                let (a, b) = r.trim_start_matches("bytes=").split_once('-').unwrap();
                b.parse::<u64>().unwrap() - a.parse::<u64>().unwrap() + 1
            })
            .sum()
    }

    /// While a test holds `cache_test_guard`, a store does not evict, whatever the storing
    /// reader's budget: what a request-counting test cached stays until it ends.
    #[test]
    fn the_cache_test_guard_holds_the_caches_eviction() {
        let _serial = cache_test_guard();
        let mut cache = file_cache();
        cache.store(("guard-test://first", "guard-test://first"), 5, Slot::Whole, Arc::from(&b"first"[..]), 1 << 20);
        // A budget the cache is already over: without the guard, this store evicts the oldest
        // entries, the one above among them.
        cache.store(("guard-test://second", "guard-test://second"), 6, Slot::Whole, Arc::from(&b"second"[..]), 6);
        assert!(cache.files.get("guard-test://first").is_some_and(|f| f.whole.is_some()), "evicted while the guard was held");
    }

    #[test]
    fn the_first_request_asks_for_gdals_first_read() {
        let _serial = cache_test_guard();
        let data = test_data();
        let (base, seen) = serve_recording(data.clone(), None);
        let opts = HttpOptions { block_size: 65_536, ..options() };
        let mut r = RangeReader::open_with(&Remote::Url(format!("{base}/data")), opts).unwrap();
        assert_eq!(header_values(&seen, "range"), vec!["bytes=0-16383".to_string()]);
        assert_eq!(read_range(&mut r, 100, 16_000), data[100..16_100], "read from the first bytes");
        assert_eq!(seen.lock().unwrap().len(), 1);
        // Past them: the rest of the first block, not its first bytes again.
        assert_eq!(read_range(&mut r, 16_000, 1_000), data[16_000..17_000]);
        assert_eq!(header_values(&seen, "range"), vec!["bytes=0-16383".to_string(), "bytes=16384-65535".to_string()]);
        assert_eq!(read_range(&mut r, 0, 65_536), data[..65_536]);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_file_opened_again_is_read_from_the_process_wide_cache() {
        // As GDAL's /vsicurl/ cache: a reopened file costs no request.
        let _serial = cache_test_guard();
        let data = test_data();
        let (base, seen) = serve_recording(data.clone(), None);
        let url = format!("{base}/data");
        let mut r = RangeReader::open_with(&Remote::Url(url.clone()), options()).unwrap();
        assert_eq!(read_range(&mut r, 10_000, 100), data[10_000..10_100]);
        let n = seen.lock().unwrap().len();
        assert_eq!(n, 2);
        let mut again = RangeReader::open_with(&Remote::Url(url.clone()), options()).unwrap();
        assert_eq!(read_range(&mut again, 10_000, 100), data[10_000..10_100]);
        assert_eq!(seen.lock().unwrap().len(), n, "opened and read again from the cache");
        // With a header (a credential), the same URL is another entry: requested, with the header.
        crate::config::push_scope(HashMap::from([("GDAL_HTTP_HEADERS".to_string(), Some("X-Tenant: one".to_string()))]));
        let _ = RangeReader::open_with(&Remote::Url(url.clone()), options()).unwrap();
        crate::config::pop_scope();
        assert_eq!(seen.lock().unwrap().len(), n + 1, "bytes fetched without the header are not returned to an open with it");
        assert_eq!(header_values(&seen, "x-tenant"), vec!["one".to_string()]);
        // And bytes fetched with it are not returned to an open without it.
        let other = format!("{base}/other");
        crate::config::push_scope(HashMap::from([("GDAL_HTTP_HEADERS".to_string(), Some("X-Tenant: one".to_string()))]));
        let _ = RangeReader::open_with(&Remote::Url(other.clone()), options()).unwrap();
        crate::config::pop_scope();
        let _ = RangeReader::open_with(&Remote::Url(other), options()).unwrap();
        assert_eq!(seen.lock().unwrap().len(), n + 3);
        clear_cache();
        let _ = RangeReader::open_with(&Remote::Url(url), options()).unwrap();
        assert_eq!(seen.lock().unwrap().len(), n + 4, "requested again after clear_cache");
    }

    /// With `GDAL_INGESTED_BYTES_AT_OPEN` over a block, the first open of a GeoTIFF requests the
    /// ingested bytes, in one request, and the opens of the URL after it take them from the
    /// process-wide cache, without a request.
    #[cfg(feature = "geotiff")]
    #[test]
    fn ingested_header_bytes_are_requested_once_and_reopened_from_the_cache() {
        let _serial = cache_test_guard();
        let config = crate::RasterConfig { cols: 1024, rows: 1024, bands: 1, data_type: crate::DataType::U8, ..Default::default() };
        let pixels = crate::raster::RasterData::U8((0..1024 * 1024).map(|i| (i % 251) as u8).collect());
        let source = crate::RasterDataset::new(crate::Raster::from_data_native(config, pixels).unwrap(), false);
        let options = crate::RasterWriteOptions {
            driver: Some(crate::formats::RasterDriver::Cog),
            compression: Some("NONE".into()),
            ..Default::default()
        };
        let bytes = crate::memory::write_raster_bytes(&source, "source.tif", &options).unwrap();
        assert!(bytes.len() > 1_000_000);
        let (base, seen) = serve_recording(bytes, None);
        let url = format!("{base}/a.tif");
        crate::config::push_scope(HashMap::from([("GDAL_INGESTED_BYTES_AT_OPEN".to_string(), Some("131072".to_string()))]));
        let mut per_open = Vec::new();
        for _ in 0..3 {
            let requests = seen.lock().unwrap().len();
            let asked = asked_bytes(&seen);
            assert_eq!(crate::LazyRaster::open(&url).unwrap().width(), 1024);
            let requests = seen.lock().unwrap().len() - requests;
            per_open.push((requests, asked_bytes(&seen) - asked));
        }
        crate::config::pop_scope();
        // (requests, bytes) of each open.
        assert_eq!(per_open, [(1, 131_072), (0, 0), (0, 0)], "{:?}", header_values(&seen, "range"));
    }

    /// An open that ingests more of a file than the process-wide cache holds takes what it holds
    /// and requests only the rest: after an open that read the first 16,384 bytes, an open
    /// ingesting three 65,536-byte blocks asks for the bytes after them, the next open asks for
    /// nothing, and once the first and the last block are dropped from the cache, an open asks
    /// for those two alone.
    #[test]
    fn an_open_requests_only_the_ingested_bytes_the_cache_does_not_hold() {
        let _serial = cache_test_guard();
        let data = test_data();
        let (base, seen) = serve_recording(data.clone(), None);
        let url = format!("{base}/data");
        let first = HttpOptions { block_size: 65_536, ..options() };
        RangeReader::open_with(&Remote::Url(url.clone()), first.clone()).unwrap();
        let ingest = HttpOptions { ingested_bytes_at_open: Some(196_608), ..first };
        let mut r = RangeReader::open_with(&Remote::Url(url.clone()), ingest.clone()).unwrap();
        RangeReader::open_with(&Remote::Url(url.clone()), ingest.clone()).unwrap();
        assert_eq!(header_values(&seen, "range"), ["bytes=0-16383", "bytes=16384-196607"]);
        assert_eq!(read_range(&mut r, 150_000, 1_000), data[150_000..151_000], "read from the blocks taken at open");
        assert_eq!(seen.lock().unwrap().len(), 2);
        let mut cache = file_cache();
        let key = cache.files.keys().find(|key| key.starts_with(&format!("{url} "))).unwrap().clone();
        let blocks = &mut cache.files.get_mut(&key).unwrap().blocks;
        let dropped: u64 = blocks.iter().filter(|((_, b), _)| *b != 1).map(|(_, d)| d.len() as u64).sum();
        blocks.retain(|&(_, b), _| b == 1);
        cache.bytes -= dropped;
        drop(cache);
        let mut r = RangeReader::open_with(&Remote::Url(url), ingest).unwrap();
        // the two runs are requested in parallel, so in either order
        let mut asked = header_values(&seen, "range")[2..].to_vec();
        asked.sort();
        assert_eq!(asked, ["bytes=131072-196607", "bytes=16384-65535"]);
        assert_eq!(read_range(&mut r, 0, 196_608), data[..196_608]);
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn a_small_file_from_a_server_without_ranges_is_reopened_from_the_cache() {
        // GDAL's requests for a reopened 4 x 4 GeoTIFF are [503, 200].
        let _serial = cache_test_guard();
        let data = test_data()[..700].to_vec();
        let (url, count) = serve(data.clone(), Mode::Flaky);
        let opts = HttpOptions { block_size: 65_536, ..options() };
        for _ in 0..4 {
            let mut r = RangeReader::open_with(&Remote::Url(url.clone()), opts.clone()).unwrap();
            assert_eq!(read_range(&mut r, 0, 700), data);
        }
        assert_eq!(count.load(Ordering::SeqCst), 2, "a 503, then the file once");
    }

    #[test]
    fn a_listing_page_over_its_cap_is_refused() {
        // Sync plan guarantee #3 for #199's listings: a page is read no further than its cap.
        let page = b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".to_vec();
        let (url, _) = serve(page.clone(), Mode::NoRanges);
        assert_eq!(store::list_get(&url).unwrap(), Some(page));
        let mut big = b"<ListBucketResult>".to_vec();
        big.resize(MAX_LISTING_PAGE_BYTES as usize + 1, b' ');
        let (url, _) = serve(big, Mode::NoRanges);
        let err = store::list_get(&url).unwrap_err().to_string();
        assert!(err.contains(&format!("larger than the {MAX_LISTING_PAGE_BYTES} bytes allowed")), "{err}");
    }

    #[test]
    fn an_s3_listing_page_over_its_cap_is_refused() {
        let mut big = b"<ListBucketResult>".to_vec();
        big.resize(MAX_LISTING_PAGE_BYTES as usize + 1, b' ');
        let (url, count) = serve(big, Mode::NoRanges);
        let host = url.trim_start_matches("http://").trim_end_matches("/data").to_string();
        let scope = |k: &str, v: &str| (k.to_string(), Some(v.to_string()));
        crate::config::push_scope(HashMap::from([
            scope("AWS_S3_ENDPOINT", &host),
            scope("AWS_HTTPS", "NO"),
            scope("AWS_VIRTUAL_HOSTING", "FALSE"),
            scope("AWS_NO_SIGN_REQUEST", "YES"),
        ]));
        let listed = list_subdirectories(&Directory::S3 { bucket: "bucket".into(), prefix: "store.zarr".into() });
        crate::config::pop_scope();
        let err = listed.unwrap_err().to_string();
        assert!(err.contains(&format!("larger than the {MAX_LISTING_PAGE_BYTES} bytes allowed")), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_token_response_over_its_cap_is_refused() {
        let token = br#"{"msft:expiry":"2026-10-24T18:00:00Z","token":"st=1&se=2&sp=rl&sig=x"}"#.to_vec();
        let (url, _) = serve(token.clone(), Mode::NoRanges);
        assert_eq!(get_with_headers_capped(&url, &[], MAX_TOKEN_BYTES, Redirects::Any).unwrap(), token);
        let (url, _) = serve(vec![b' '; MAX_TOKEN_BYTES as usize + 1], Mode::NoRanges);
        let err = get_with_headers_capped(&url, &[], MAX_TOKEN_BYTES, Redirects::Any).unwrap_err().to_string();
        assert!(err.contains(&format!("larger than the {MAX_TOKEN_BYTES} bytes allowed")), "{err}");
        // With headers (an API key), through the other request path.
        let headers = vec![("Ocp-Apim-Subscription-Key".to_string(), "k".to_string())];
        let err = get_with_headers_capped(&url, &headers, MAX_TOKEN_BYTES, Redirects::Any).unwrap_err().to_string();
        assert!(err.contains("bytes allowed"), "{err}");
    }

    #[test]
    fn json_documents_are_capped_as_gdal_caps_them() {
        assert_eq!(parse_memory_size("100MB"), Some(104_857_600));
        assert_eq!(parse_memory_size("2k"), Some(2048));
        assert_eq!(parse_memory_size("1.5G"), Some(1_610_612_736));
        assert_eq!(parse_memory_size("12"), Some(12));
        assert_eq!(parse_memory_size("12 parsecs"), None);
        assert_eq!(json_max_size(), 104_857_600, "GDAL's CPL_JSON_MAX_SIZE default, 100MB");
        let doc = vec![b' '; 3000];
        let (url, _) = serve(doc.clone(), Mode::NoRanges);
        crate::config::push_scope(HashMap::from([("CPL_JSON_MAX_SIZE".to_string(), Some("2K".to_string()))]));
        let whole = get(&Remote::Url(url.clone()));
        let headers = vec![("X-A".to_string(), "1".to_string())];
        let with_headers = get_with_headers(&url, &headers, Redirects::Any);
        crate::config::pop_scope();
        for r in [whole, with_headers] {
            let err = r.unwrap_err().to_string();
            assert!(err.contains("larger than the 2048 bytes allowed"), "{err}");
        }
        assert_eq!(get(&Remote::Url(url)).unwrap(), doc, "within the default cap");
    }

    #[test]
    fn a_document_file_is_read_by_range_and_capped_before_its_body() {
        let data = test_data();
        let (base, seen) = serve_recording(data.clone(), None);
        assert_eq!(get_file(&Remote::Url(format!("{base}/doc.json")), Redirects::Any).unwrap(), data);
        assert_eq!(
            header_values(&seen, "range"),
            vec!["bytes=0-16383".to_string(), format!("bytes=16384-{}", data.len() - 1)],
            "the first read, then the rest"
        );
        crate::config::push_scope(HashMap::from([("TERRIO_JSON_MAX_SIZE".to_string(), Some("100000".to_string()))]));
        let err = get_file(&Remote::Url(format!("{base}/big.json")), Redirects::Any).unwrap_err().to_string();
        crate::config::pop_scope();
        assert!(err.contains("larger than the 100000 bytes allowed"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 3, "refused after the first read");
        // A server that ignores ranges: refused for a file over ten times the first read.
        let (url, count) = serve(data, Mode::NoRanges);
        let err = get_file(&Remote::Url(url.replace("/data", "/doc.json")), Redirects::Any).unwrap_err().to_string();
        assert!(err.contains("range downloading not supported"), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn a_remote_file_that_is_not_what_its_name_says_is_refused_after_its_first_read() {
        // 64 MiB of noise named .tif, .png or .jpg is not downloaded whole
        // (67,174,400 bytes); GDAL reads 16,384 and refuses. Each name is read twice (identified,
        // then opened), the second time from the process-wide cache: hence the guard.
        let _serial = cache_test_guard();
        let noise = vec![0x55u8; 1_000_000];
        let (base, seen) = serve_recording(noise, None);
        for name in ["junk.tif", "junk.png", "junk.jpg", "junk.jp2"] {
            let err = crate::LazyRaster::open(&format!("{base}/{name}")).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(!err.is_empty(), "{name} opened");
        }
        let ranges = header_values(&seen, "range");
        assert!(ranges.iter().all(|r| r == "bytes=0-16383"), "{ranges:?}");
        assert!(asked_bytes(&seen) <= 4 * FIRST_READ, "{ranges:?}");
        assert_eq!(seen.lock().unwrap().len(), 4, "one request each");
    }

    #[test]
    fn a_remote_file_that_is_what_its_name_says_is_downloaded_by_range() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend((0..200_000u32).map(|i| (i % 251) as u8));
        let (base, seen) = serve_recording(png.clone(), None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        download_checked(&Remote::Url(format!("{base}/a.png")), &path, &|head, len| {
            crate::formats::check_remote_head("a.png", "a.png", head, len)
        })
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), png);
        assert_eq!(header_values(&seen, "range"), vec!["bytes=0-16383".to_string(), format!("bytes=16384-{}", png.len() - 1)]);
    }

    /// Documents a raster open refuses, of about `len` bytes each: GeoJSON, random bytes, CSV
    /// and XML, with the format [`crate::formats::sniff`] finds in each.
    fn documents(len: usize) -> Vec<(Option<&'static str>, Vec<u8>)> {
        let mut geojson = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"s":""#.to_vec();
        geojson.resize(len, b'x');
        geojson.extend_from_slice(br#""},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#);
        let mut state = 5u64;
        let noise = (0..len)
            .map(|_| {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                (state >> 33) as u8
            })
            .collect();
        let mut csv = b"id,name,value\n".to_vec();
        let mut xml = br#"<?xml version="1.0"?><root>"#.to_vec();
        while csv.len() < len {
            csv.extend_from_slice(b"1,a,2\n");
            xml.extend_from_slice(b"<a>1</a>");
        }
        vec![(Some("GEOJSON"), geojson), (None, noise), (None, csv), (Some("XML"), xml)]
    }

    /// Without an allow-list, a raster open of a remote document named for no raster
    /// driver (`x`, `dir.json/x`, with a query, through `/vsicurl/`) or for ENVI (`x.dat`, read
    /// with a `.hdr` a remote file comes without) downloaded it whole, then refused it naming the
    /// local copy: 49,999,930 bytes or more of a 50,000,000-byte GeoJSON, random, CSV or XML
    /// document, where GDAL reads 49,152. Its first bytes show no format the read takes: it is
    /// refused after them (16,384 bytes, one request), by the whole read and the lazy open alike,
    /// naming the dataset.
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn a_remote_document_named_for_no_raster_driver_is_refused_after_its_first_bytes() {
        let _serial = cache_test_guard();
        let scratch = std::env::temp_dir().to_string_lossy().into_owned();
        for (format, doc) in documents(1_000_000) {
            assert_eq!(crate::formats::sniff(&doc[..FIRST_READ as usize]), format);
            let shows = match format {
                Some(format) => format!("its first 16384 bytes hold {format} data"),
                None => "its first 16384 bytes show no known format".to_string(),
            };
            for lazy in [false, true] {
                let (base, seen) = serve_recording(doc.clone(), None);
                for (name, shown) in [
                    (format!("{base}/x"), format!("{base}/x")),
                    (format!("{base}/x.dat"), format!("{base}/x.dat")),
                    (format!("{base}/dir.json/x"), format!("{base}/dir.json/x")),
                    (format!("{base}/q/x?sig=secret"), format!("{base}/q/x")),
                    (format!("/vsicurl/{base}/v/x"), format!("/vsicurl/{base}/v/x")),
                ] {
                    let requests = seen.lock().unwrap().len();
                    let asked = asked_bytes(&seen);
                    let opened = if lazy { crate::LazyRaster::open(&name).map(|_| ()) } else { crate::open_raster(&name).map(|_| ()) };
                    let err = opened.unwrap_err().to_string();
                    let requests = seen.lock().unwrap().len() - requests;
                    let asked = asked_bytes(&seen) - asked;
                    assert!(asked <= FIRST_READ && requests <= 1, "{name} (lazy {lazy}): {asked} bytes in {requests} requests: {err}");
                    assert_eq!(err, format!("'{shown}' not recognized as being in a supported raster format: {shows}"));
                    assert!(!err.contains(&scratch), "{err}");
                }
                assert_eq!(header_values(&seen, "range").len(), seen.lock().unwrap().len(), "every request has a range");
            }
        }
    }

    /// A remote file named for no raster driver whose first bytes show a format the whole read
    /// takes under that name is read as before: a netCDF file or a GRIB file named `x` is read
    /// whole, as its local copy is. A TIFF named `x` opens in place by range. With wbraster's
    /// detection asked for, such a name is not judged: wbraster looks for the format in the whole
    /// file, and the error names the dataset, not the temporary copy.
    #[test]
    fn a_remote_raster_named_for_no_raster_driver_is_read_as_before() {
        let _serial = cache_test_guard();
        let scratch = std::env::temp_dir().to_string_lossy().into_owned();
        let all_ranged = |seen: &Seen| header_values(seen, "range").len() == seen.lock().unwrap().len();
        let tiff = std::fs::read("tests/data/user_crs/merc.tif").unwrap();
        let (base, seen) = serve_recording(tiff, None);
        let lazy = crate::LazyRaster::open(&format!("{base}/x")).unwrap().read().unwrap();
        let local = crate::open_raster("tests/data/user_crs/merc.tif").unwrap();
        assert_eq!((lazy.raster.data.to_f64_vec(), lazy.geo_transform()), (local.raster.data.to_f64_vec(), local.geo_transform()));
        assert!(all_ranged(&seen));
        let whole: &[&str] = &[
            #[cfg(feature = "netcdf-hdf5")]
            "tests/data/rotated_pole/eur11.nc",
            #[cfg(feature = "grib")]
            "tests/data/grib/simple.grib2",
        ];
        let bits = |ds: &crate::RasterDataset| ds.raster.data.to_f64_vec().iter().map(|v| v.to_bits()).collect::<Vec<_>>();
        for path in whole {
            let bytes = std::fs::read(path).unwrap();
            let (base, seen) = serve_recording(bytes.clone(), None);
            let remote = crate::open_raster(&format!("{base}/x")).unwrap();
            let local = crate::open_raster(path).unwrap();
            assert_eq!((bits(&remote), remote.geo_transform()), (bits(&local), local.geo_transform()), "{path}");
            assert!(asked_bytes(&seen) >= bytes.len() as u64 && all_ranged(&seen), "{path}: read whole");
        }
        let (_, geojson) = documents(200_000).swap_remove(0);
        let (base, seen) = serve_recording(geojson.clone(), None);
        crate::config::push_scope(scope(&[("TERRIO_WBRASTER_FALLBACK", "YES")]));
        let err = crate::open_raster(&format!("{base}/x")).map(|_| ()).unwrap_err().to_string();
        crate::config::pop_scope();
        assert!(asked_bytes(&seen) >= geojson.len() as u64, "read whole for wbraster: {err}");
        assert!(err.starts_with(&format!("'{base}/x': ")) && !err.contains(&scratch), "{err}");
    }

    /// A remote file named for no raster driver, or for ENVI (`x.dat`), whose first bytes show a
    /// format that the whole read of a raster does not take under that name was downloaded whole
    /// before that read refused it: 30,000,000 bytes of a TIFF (the whole read's; a lazy open reads
    /// a TIFF by range whatever its name) or of a PNG named `x`, and so for a JPEG, a JPEG 2000, a
    /// WebP, a GeoPackage, a PMTiles archive, a VRT, a STAC item and, named for ENVI, a GRIB,
    /// netCDF or HDF5 file. Each is refused after its first 16,384 bytes, the same failure, the
    /// error naming the dataset and what its first bytes hold.
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn a_remote_raster_the_whole_read_does_not_take_under_its_name_is_refused_after_its_first_bytes() {
        let _serial = cache_test_guard();
        let file = |magic: &[u8], len: usize| {
            let mut v = magic.to_vec();
            v.resize(len, 0);
            v
        };
        let mut stac = serde_json::to_vec(&serde_json::json!({"type": "Feature", "stac_version": "1.0.0",
            "stac_extensions": ["tiled-assets"], "id": "a", "properties": {"pad": "x".repeat(1_000_000)}}))
        .unwrap();
        stac.truncate(1_000_000);
        let both: &[&str] = &["x", "x.dat"];
        let cases: Vec<(&str, Vec<u8>, &[&str])> = vec![
            ("GTIFF", file(b"II*\0", 30_000_000), both),
            ("PNG", file(b"\x89PNG\r\n\x1a\n", 30_000_000), both),
            ("JPEG", file(&[0xFF, 0xD8, 0xFF, 0xE0], 1_000_000), both),
            ("JP2", file(b"\0\0\0\x0cjP  \r\n\x87\n", 1_000_000), both),
            ("WEBP", file(b"RIFF\0\0\0\0WEBP", 1_000_000), both),
            ("GPKG", file(b"SQLite format 3\0", 1_000_000), both),
            ("PMTILES", file(b"PMTiles\x03", 1_000_000), both),
            ("VRT", file(br#"<VRTDataset rasterXSize="1" rasterYSize="1">"#, 1_000_000), both),
            ("STAC", stac, both),
            ("GRIB", file(b"GRIB\0\0\0\x02", 1_000_000), &["x.dat"]),
            ("NETCDF", file(b"CDF\x01", 1_000_000), &["x.dat"]),
            ("HDF5", file(b"\x89HDF\r\n\x1a\n", 1_000_000), &["x.dat"]),
        ];
        for (format, bytes, names) in cases {
            let opens: &[bool] = if format == "GTIFF" { &[false] } else { &[false, true] };
            for &lazy in opens {
                let (base, seen) = serve_recording(bytes.clone(), None);
                for name in names {
                    let url = format!("{base}/{name}");
                    let requests = seen.lock().unwrap().len();
                    let asked = asked_bytes(&seen);
                    let opened = if lazy { crate::LazyRaster::open(&url).map(|_| ()) } else { crate::open_raster(&url).map(|_| ()) };
                    let err = opened.unwrap_err().to_string();
                    let asked = asked_bytes(&seen) - asked;
                    let requests = seen.lock().unwrap().len() - requests;
                    assert!(asked <= FIRST_READ && requests == 1, "{url} (lazy {lazy}): {asked} bytes in {requests} requests: {err}");
                    assert_eq!(
                        err,
                        format!(
                            "'{url}' not recognized as being in a supported raster format: its first 16384 bytes hold {format} data, \
                             which terrio does not read whole under this name"
                        )
                    );
                }
            }
        }
    }

    /// The generic open of a remote file (`info`, `open`) named for no driver, or for ENVI, read
    /// it whole the same way: a 1,000,000-byte GeoJSON, random, CSV, XML or PNG document named `x`,
    /// `x.dat` or `dir.json/x` was downloaded whole and refused, naming the local copy. It is
    /// refused after its first 16,384 bytes, naming the dataset; a netCDF file named `x` is still
    /// read whole, and a CSV document named `x.csv` too (as a vector, by its name).
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn remote_info_reads_known_vectors_and_bounds_unknown_format_probes() {
        let _serial = cache_test_guard();
        let scratch = std::env::temp_dir().to_string_lossy().into_owned();
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(1_000_000, 0);
        let mut files = documents(1_000_000);
        files.push((Some("PNG"), png));
        for (format, bytes) in files {
            let shows = match format {
                Some("XML") => "hold XML data".to_string(),
                Some(format) => format!("hold {format} data, which terrio does not read whole under this name"),
                None => "show no known format".to_string(),
            };
            let (base, seen) = serve_recording(bytes, None);
            for name in ["x", "x.dat", "dir.json/x"] {
                let url = format!("{base}/{name}");
                let asked = asked_bytes(&seen);
                if format == Some("GEOJSON") {
                    let crate::info::Info::Vector(info) = crate::info::info_path(&url, &Default::default()).unwrap() else {
                        panic!("not a vector")
                    };
                    assert_eq!(info.driver, "GeoJSON");
                    assert_eq!(info.layers[0].feature_count, 1);
                    assert!(asked_bytes(&seen) - asked >= 1_000_000, "valid vector read whole");
                    continue;
                }
                let err = crate::info::info_path(&url, &Default::default()).map(|_| ()).unwrap_err().to_string();
                let asked = asked_bytes(&seen) - asked;
                assert!(asked <= FIRST_READ, "{url}: {asked} bytes: {err}");
                assert_eq!(err, format!("'{url}' not recognized as being in a supported file format: its first 16384 bytes {shows}"));
                assert!(!err.contains(&scratch), "{err}");
            }
        }
        #[cfg(feature = "netcdf-hdf5")]
        {
            let bytes = std::fs::read("tests/data/rotated_pole/eur11.nc").unwrap();
            let (base, seen) = serve_recording(bytes.clone(), None);
            assert!(matches!(crate::info::info_path(&format!("{base}/x"), &Default::default()), Ok(crate::info::Info::Raster(_))));
            assert!(asked_bytes(&seen) >= bytes.len() as u64, "read whole");
        }
        let (_, csv) = documents(200_000).swap_remove(2);
        let (base, seen) = serve_recording(csv.clone(), None);
        let read = crate::info::info_path(&format!("{base}/x.csv"), &Default::default());
        assert!(matches!(read, Ok(crate::info::Info::Vector(_))), "{:?}", read.err());
        assert!(asked_bytes(&seen) >= csv.len() as u64, "read whole");
    }

    /// A remote file named for a format terrio reads whole whose first bytes do not show that
    /// format was downloaded whole before the format's reader refused it: 49,999,930 bytes of a
    /// 50,000,000-byte GeoJSON document named `.vrt`, `.nc`, `.webp`, `.h5` or `.gpkg`, where GDAL
    /// reads 49,152, and `.vrt` under an allow-list that has VRT too. It is refused after its first
    /// 16,384 bytes, by the whole read and the lazy open alike, naming the dataset (without its
    /// query), as a file named `.tif` is; so are the other formats read whole whose files start
    /// with a signature (MBTiles, Imagine, DTED, Surfer, Esri ASCII grids).
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn a_remote_file_named_for_a_format_read_whole_is_refused_unless_its_first_bytes_show_it() {
        let _serial = cache_test_guard();
        let (_, geojson) = documents(1_000_000).swap_remove(0);
        let names = [
            ("x.vrt", "VRT"),
            ("x.nc", "NETCDF"),
            ("x.nc4", "NETCDF"),
            ("x.cdf", "NETCDF"),
            ("x.webp", "WEBP"),
            ("x.h5", "HDF5"),
            ("x.hdf5", "HDF5"),
            ("x.he5", "HDF5"),
            ("x.gpkg", "GPKG"),
            ("x.mbtiles", "MBTILES"),
            ("x.img", "HFA"),
            ("x.dt1", "DTED"),
            ("x.grd", "GSBG"),
            ("x.asc", "AAIGRID"),
        ];
        for lazy in [false, true] {
            let (base, seen) = serve_recording(geojson.clone(), None);
            for (name, driver) in names {
                let url = format!("{base}/{name}?sig=secret");
                let requests = seen.lock().unwrap().len();
                let asked = asked_bytes(&seen);
                let opened = if lazy { crate::LazyRaster::open(&url).map(|_| ()) } else { crate::open_raster(&url).map(|_| ()) };
                let err = opened.unwrap_err().to_string();
                let requests = seen.lock().unwrap().len() - requests;
                let asked = asked_bytes(&seen) - asked;
                assert!(asked <= FIRST_READ && requests == 1, "{name} (lazy {lazy}): {asked} bytes in {requests} requests: {err}");
                assert_eq!(
                    err,
                    format!(
                        "'{base}/{name}' not recognized as being in a supported file format: named for {driver}, it holds GEOJSON data"
                    )
                );
            }
        }
        // A tile service's allow-list has VRT: the name passes it, the first bytes do not.
        let (base, seen) = serve_recording(geojson, None);
        crate::config::push_scope(scope(&[("TERRIO_ALLOWED_DRIVERS", "COG,GRIB,GTiff,JP2OpenJPEG,JPEG,MEM,PNG,VRT")]));
        let vrt = crate::LazyRaster::open(&format!("{base}/x.vrt")).map(|_| ()).unwrap_err().to_string();
        let nc = crate::LazyRaster::open(&format!("{base}/x.nc")).map(|_| ()).unwrap_err().to_string();
        crate::config::pop_scope();
        assert!(vrt.ends_with("named for VRT, it holds GEOJSON data"), "{vrt}");
        assert!(nc.contains("the NETCDF driver is not allowed"), "{nc}");
        assert!(asked_bytes(&seen) <= FIRST_READ && seen.lock().unwrap().len() == 1, "{:?}", header_values(&seen, "range"));
    }

    /// A genuine remote file under the name of a format terrio reads whole still opens as its local
    /// copy does, downloaded whole as before: the files terrio writes as Esri ASCII grid, Surfer 6
    /// grid, GeoPackage, netCDF, DTED and WebP, an Esri ASCII grid whose header comes after a blank
    /// line in capitals, and an HDF5 file named `.h5`.
    #[test]
    fn a_genuine_remote_file_named_for_its_format_is_read_whole_as_before() {
        let _serial = cache_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let raster = |bands: usize| {
            let cfg = wbraster::RasterConfig {
                cols: 8,
                rows: 6,
                bands,
                x_min: 10.0,
                y_min: 47.0,
                cell_size: 0.5,
                nodata: -9999.0,
                data_type: wbraster::DataType::U8,
                crs: wbraster::CrsInfo::from_epsg(4326),
                ..Default::default()
            };
            let data = (0..48 * bands).map(|i| (i * 5 % 251) as u8).collect();
            wbraster::Raster::from_data_native(cfg, wbraster::raster::RasterData::U8(data)).unwrap()
        };
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for (name, bands) in [("x.asc", 1), ("x.grd", 1), ("x.gpkg", 1), ("x.nc", 1), ("x.dt1", 1), ("x.webp", 3)] {
            let path = dir.path().join(name);
            crate::write_raster(&raster(bands), path.to_str().unwrap(), &Default::default()).unwrap();
            files.push((name.to_string(), std::fs::read(&path).unwrap()));
        }
        files.push(("y.asc".to_string(), b"\n  NCOLS 2\r\nNROWS 1\r\nxllcorner 0\r\nyllcorner 0\r\ncellsize 1\r\n1 2\r\n".to_vec()));
        files.push(("x.h5".to_string(), std::fs::read("tests/data/netcdf_bands/axes.nc").unwrap()));
        let bits =
            |ds: crate::RasterDataset| (ds.raster.data.to_f64_vec().iter().map(|v| v.to_bits()).collect::<Vec<_>>(), ds.geo_transform());
        for (name, bytes) in files {
            let local = dir.path().join(format!("local-{name}"));
            std::fs::write(&local, &bytes).unwrap();
            let expected = bits(crate::open_raster(local.to_str().unwrap()).unwrap());
            for lazy in [false, true] {
                let (base, seen) = serve_recording(bytes.clone(), None);
                let url = format!("{base}/{name}");
                let remote = if lazy { crate::LazyRaster::open(&url).and_then(|mut r| r.read()) } else { crate::open_raster(&url) };
                assert_eq!(bits(remote.unwrap()), expected, "{name} (lazy {lazy})");
                assert!(asked_bytes(&seen) >= bytes.len() as u64, "{name} (lazy {lazy}): read whole");
                assert_eq!(header_values(&seen, "range").len(), seen.lock().unwrap().len(), "every request has a range");
            }
        }
    }

    /// The names GDAL's SRTMHGT driver and terrio's take a tile under.
    const TILE_NAMES: [&str; 8] =
        ["N37W120.hgt", "N37W120.img", "N37W120.hgts", "N37W120.err", "N37W120.num", "N37W120.swb", "N37W120.raw", "N37W120.SRTMSWBD.raw"];

    /// A 3" SRTM tile: 1201 x 1201 big-endian Int16 samples, the first void.
    fn srtm_tile() -> Vec<u8> {
        (0..1201 * 1201).flat_map(|i: i32| if i == 0 { -32768i16 } else { (i % 3000) as i16 }.to_be_bytes()).collect()
    }

    /// A remote SRTM tile named for Imagine (`N37W120.img`, as NASADEM names its tiles) was refused
    /// after its first 16,384 bytes, which show no Imagine header, and one named `.hgts`, `.err`,
    /// `.num`, `.swb` or `.raw` after them too, as no format the read takes under its name, where
    /// GDAL reads them all: an SRTM tile has no signature, and is known by its name and size, as
    /// GDAL's SRTMHGT driver knows one (`VSIStatL`). Under every one of those names, a tile is read
    /// as its local copy is (driver, samples, georeferencing, nodata, unit), by the whole read and
    /// the lazy open: its first 16,384 bytes, then the rest (the lazy open's in pieces of 1 MiB
    /// fetched in parallel).
    #[test]
    fn a_remote_srtm_tile_is_read_under_every_name_the_driver_takes() {
        let _serial = cache_test_guard();
        let tile = srtm_tile();
        let dir = tempfile::tempdir().unwrap();
        let read = |ds: crate::RasterDataset| {
            let samples: Vec<u64> = ds.raster.data.to_f64_vec().iter().map(|v| v.to_bits()).collect();
            let unit = ds.band_metadata.first().and_then(|b| b.unit.clone());
            (ds.driver.name(), samples, ds.geo_transform(), ds.nodata(), ds.crs().epsg, unit)
        };
        for name in TILE_NAMES {
            let local = dir.path().join(name);
            std::fs::write(&local, &tile).unwrap();
            let expected = read(crate::open_raster(local.to_str().unwrap()).unwrap());
            assert_eq!((expected.0, expected.3, expected.4), ("SRTMHGT", Some(-32768.0), Some(4326)), "{name}");
            for lazy in [false, true] {
                let (base, seen) = serve_recording(tile.clone(), None);
                let url = format!("{base}/{name}");
                let remote = if lazy { crate::LazyRaster::open(&url).and_then(|mut r| r.read()) } else { crate::open_raster(&url) };
                let remote = remote.unwrap_or_else(|e| panic!("{name} (lazy {lazy}): {e}"));
                assert!(read(remote) == expected, "{name} (lazy {lazy}): read otherwise than its local copy");
                let mut asked = header_values(&seen, "range");
                asked.sort();
                let expected =
                    if lazy { tile_requests(tile.len()) } else { vec!["bytes=0-16383".into(), format!("bytes=16384-{}", tile.len() - 1)] };
                assert_eq!(asked, expected, "{name} (lazy {lazy})");
            }
        }
    }

    /// A remote file named as an SRTM tile without a tile's size is no tile, as GDAL's SRTMHGT
    /// driver finds from its size (`VSIStatL`) without reading it: refused after its first request
    /// (at most 16,384 bytes), whatever its content (a tile and one more byte, zeros, a GeoJSON
    /// document), by the whole read and the lazy open, where one named `.hgt` was downloaded whole
    /// before the reader refused it.
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn a_remote_file_named_as_an_srtm_tile_without_its_size_is_refused_after_its_first_request() {
        let _serial = cache_test_guard();
        let mut plus_one = srtm_tile();
        plus_one.push(0);
        let (_, geojson) = documents(1_000_000).swap_remove(0);
        for content in [plus_one, vec![0u8; 3_000_000], geojson] {
            for name in TILE_NAMES {
                for lazy in [false, true] {
                    let (base, seen) = serve_recording(content.clone(), None);
                    let url = format!("{base}/{name}");
                    let opened = if lazy { crate::LazyRaster::open(&url).map(|_| ()) } else { crate::open_raster(&url).map(|_| ()) };
                    let err = opened.unwrap_err().to_string();
                    let requests = seen.lock().unwrap().len();
                    let asked = asked_bytes(&seen);
                    assert!(requests == 1 && asked <= FIRST_READ, "{name} (lazy {lazy}): {asked} bytes in {requests} requests: {err}");
                    assert!(err.starts_with(&format!("'{url}' not recognized as being in a supported")), "{name} (lazy {lazy}): {err}");
                    if name.ends_with(".hgt") {
                        let len = content.len();
                        assert!(err.ends_with(&format!("named for SRTMHGT, it has {len} bytes, which is no SRTM tile's size")), "{err}");
                    }
                }
            }
        }
    }

    /// Under an allow-list without SRTMHGT, a remote tile named for a driver the list has
    /// (`N37W120.img`, HFA's) is refused after its first request as SRTM data, as it was refused
    /// there as no Imagine file, rather than downloaded whole and refused by the reader; named
    /// `.hgt`, it is refused by its name, unrequested; named `.hgts`, after the first request that
    /// identified it. `driver=` (`check_drivers`) refuses the tile named `.img` after that request
    /// too unless it has SRTMHGT. With SRTMHGT allowed, the tiles named `.hgts` and `.img` open, in
    /// the two requests of an open without a list.
    #[test]
    fn a_remote_srtm_tile_under_an_allow_list_without_srtmhgt_is_refused_after_its_first_request() {
        let _serial = cache_test_guard();
        let (base, seen) = serve_recording(srtm_tile(), None);
        crate::config::push_scope(scope(&[("TERRIO_ALLOWED_DRIVERS", "HFA,GTiff")]));
        let whole = crate::open_raster(&format!("{base}/a/N37W120.img")).map(|_| ()).unwrap_err().to_string();
        let lazy = crate::LazyRaster::open(&format!("{base}/b/N37W120.img")).map(|_| ()).unwrap_err().to_string();
        let hgt = crate::LazyRaster::open(&format!("{base}/c/N37W120.hgt")).map(|_| ()).unwrap_err().to_string();
        let hgts = crate::LazyRaster::open(&format!("{base}/d/N37W120.hgts")).map(|_| ()).unwrap_err().to_string();
        crate::config::pop_scope();
        for (dir, name, err) in [("a", "N37W120.img", whole), ("b", "N37W120.img", lazy), ("d", "N37W120.hgts", hgts)] {
            let refusal = format!("'{base}/{dir}/{name}' holds SRTMHGT data, and the SRTMHGT driver is not allowed (allowed: HFA, GTIFF)");
            assert_eq!(err, refusal);
        }
        assert!(hgt.contains("the SRTMHGT driver is not allowed"), "{hgt}");
        assert_eq!(header_values(&seen, "range"), ["bytes=0-16383"; 3]);
        assert_eq!(seen.lock().unwrap().len(), 3);
        let url = format!("{base}/e/N37W120.img");
        let err = crate::formats::check_drivers(&url, &["HFA".to_string()]).unwrap_err().to_string();
        assert_eq!(err, format!("'{url}' holds SRTMHGT data, and the SRTMHGT driver is not allowed (allowed: HFA)"));
        assert_eq!(seen.lock().unwrap().len(), 4, "refused after its first request");
        let (base, seen) = serve_recording(srtm_tile(), None);
        crate::formats::check_drivers(&format!("{base}/N37W120.img"), &["HFA".to_string(), "SRTMHGT".to_string()]).unwrap();
        crate::config::push_scope(scope(&[("TERRIO_ALLOWED_DRIVERS", "HFA,SRTMHGT")]));
        let img = crate::LazyRaster::open(&format!("{base}/N37W120.img")).and_then(|mut r| r.read());
        let hgts = crate::LazyRaster::open(&format!("{base}/N37W120.hgts")).and_then(|mut r| r.read());
        crate::config::pop_scope();
        assert_eq!((img.unwrap().driver.name(), hgts.unwrap().driver.name()), ("SRTMHGT", "SRTMHGT"));
        let mut asked = header_values(&seen, "range");
        asked.sort();
        let mut two = [tile_requests(srtm_tile().len()), tile_requests(srtm_tile().len())].concat();
        two.sort();
        assert_eq!(asked, two);
    }

    /// The requests a lazy read of a whole remote tile of `len` bytes makes, sorted: its first
    /// 16,384 bytes, then the rest of its blocks (of the default size) as one run split into
    /// near-equal pieces of SPLIT_RUN_BYTES at least, at most PARALLEL_RUNS of them.
    fn tile_requests(len: usize) -> Vec<String> {
        let bs = HttpOptions::default().block_size;
        let blocks = (len as u64).div_ceil(bs);
        let pieces = (blocks / SPLIT_RUN_BYTES.div_ceil(bs)).clamp(1, PARALLEL_RUNS as u64);
        let (size, longer) = (blocks / pieces, blocks % pieces);
        let mut out = vec!["bytes=0-16383".to_string()];
        for k in 0..pieces {
            let first = k * size + k.min(longer);
            let end = first + size + u64::from(k < longer);
            out.push(format!("bytes={}-{}", (first * bs).max(16384), (end * bs).min(len as u64) - 1));
        }
        out.sort();
        out
    }

    /// A remote file under a tile's extension whose name is no tile's (`big.hgt`, `N38W120_v2.hgt`),
    /// which GDAL's SRTMHGT driver does not take, was downloaded whole before the reader refused it:
    /// one named `.hgt` by every read, one under another of the extensions by every read but a
    /// raster open's, which judges a name of no driver by its first bytes. It is refused after its
    /// first request (at most 16,384 bytes), a GeoJSON document as a genuine tile so named, by the
    /// whole read, the lazy open and a download alike; named `.img`, by Imagine's signature, as
    /// before. A Surfer 6 grid so named, which the read takes under any name, is read whole.
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn a_remote_file_under_a_tile_extension_not_named_as_a_tile_is_refused_after_its_first_request() {
        let _serial = cache_test_guard();
        let (_, geojson) = documents(1_000_000).swap_remove(0);
        let names = ["big.hgt", "big.HGT", "N38W120_v2.hgt", "big.hgts", "big.err", "big.num", "big.swb", "big.raw", "big.img"];
        for content in [geojson, srtm_tile()] {
            for name in names {
                for how in ["whole", "lazy", "download"] {
                    let (base, seen) = serve_recording(content.clone(), None);
                    let url = format!("{base}/{name}");
                    let opened = match how {
                        "whole" => crate::open_raster(&url).map(|_| ()),
                        "lazy" => crate::LazyRaster::open(&url).map(|_| ()),
                        _ => crate::vfs::resolve(&url).map(|_| ()),
                    };
                    let err = opened.err().map(|e| e.to_string()).unwrap_or_else(|| format!("{name} ({how}): opened"));
                    let requests = seen.lock().unwrap().len();
                    let asked = asked_bytes(&seen);
                    assert!(requests == 1 && asked <= FIRST_READ, "{name} ({how}): {asked} bytes in {requests} requests: {err}");
                    assert!(err.starts_with(&format!("'{url}' not recognized as being in a supported")), "{name} ({how}): {err}");
                    if name.to_ascii_lowercase().ends_with(".hgt") {
                        assert!(err.ends_with("named for SRTMHGT, its name is no SRTM tile's"), "{err}");
                    }
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let grd = dir.path().join("x.grd");
        let cfg = wbraster::RasterConfig {
            cols: 8,
            rows: 6,
            bands: 1,
            x_min: 10.0,
            y_min: 47.0,
            cell_size: 0.5,
            nodata: -9999.0,
            data_type: wbraster::DataType::U8,
            crs: wbraster::CrsInfo::from_epsg(4326),
            ..Default::default()
        };
        let data = wbraster::raster::RasterData::U8((0..48).map(|i| (i * 5 % 251) as u8).collect());
        crate::write_raster(&wbraster::Raster::from_data_native(cfg, data).unwrap(), grd.to_str().unwrap(), &Default::default()).unwrap();
        let surfer = std::fs::read(&grd).unwrap();
        assert!(surfer.starts_with(b"DSBB"));
        let local = crate::open_raster(grd.to_str().unwrap()).unwrap().raster.data.to_f64_vec();
        let (base, seen) = serve_recording(surfer.clone(), None);
        let ds = crate::open_raster(&format!("{base}/big.hgt")).unwrap();
        assert_eq!((ds.driver.name(), ds.raster.data.to_f64_vec()), ("GSBG", local));
        assert!(asked_bytes(&seen) >= surfer.len() as u64, "read whole");
    }

    /// Remotely, a Surfer 6 grid, which the read takes by its content whatever its name, was
    /// downloaded whole and opened as GSBG under an allow-list without GSBG when its name was an
    /// unsigned driver's (`big.hgt` of 50,000,000 bytes under SRTMHGT and GTiff: 2 requests, the
    /// whole file). Under any name a raster read takes, it is refused after its first request (at
    /// most 16,384 bytes), by the whole read, the lazy open and a download; with GSBG allowed, it
    /// opens as before.
    #[test]
    fn a_remote_surfer_6_grid_under_an_allow_list_without_gsbg_is_refused_after_its_first_request() {
        let _serial = cache_test_guard();
        let mut grid = b"DSBB".to_vec();
        grid.extend_from_slice(&[10, 0, 10, 0]);
        for v in [0.0f64, 9.0, 0.0, 9.0, 0.0, 1.0] {
            grid.extend_from_slice(&v.to_le_bytes());
        }
        grid.resize(1_000_000, 0);
        for name in ["big.hgt", "N37W120.hgt", "x.xyz", "x.dat", "x.tif", "noext"] {
            for how in ["whole", "lazy", "download"] {
                let (base, seen) = serve_recording(grid.clone(), None);
                let url = format!("{base}/{name}");
                crate::config::push_scope(scope(&[("TERRIO_ALLOWED_DRIVERS", "SRTMHGT,GTiff,XYZ,ENVI")]));
                let opened = match how {
                    "whole" => crate::open_raster(&url).map(|_| ()),
                    "lazy" => crate::LazyRaster::open(&url).map(|_| ()),
                    _ => crate::vfs::resolve(&url).map(|_| ()),
                };
                crate::config::pop_scope();
                let err = opened.err().map(|e| e.to_string()).unwrap_or_else(|| format!("{name} ({how}): opened"));
                let requests = seen.lock().unwrap().len();
                let asked = asked_bytes(&seen);
                assert!(requests == 1 && asked <= FIRST_READ, "{name} ({how}): {asked} bytes in {requests} requests: {err}");
                let refusal = format!("'{url}' holds GSBG data, and the GSBG driver is not allowed (allowed: SRTMHGT, GTIFF, XYZ, ENVI)");
                assert_eq!(err, refusal, "{name} ({how})");
            }
        }
        let (base, seen) = serve_recording(grid.clone(), None);
        crate::config::push_scope(scope(&[("TERRIO_ALLOWED_DRIVERS", "SRTMHGT,GTiff,GSBG")]));
        let ds = crate::open_raster(&format!("{base}/big.hgt"));
        crate::config::pop_scope();
        assert_eq!(ds.unwrap().driver.name(), "GSBG");
        assert!(asked_bytes(&seen) >= grid.len() as u64, "read whole");
    }

    /// Formats a read finds by their content where the open judged other first bytes, remotely: a
    /// GRIB message after a TIFF's signature, named for no driver, which the whole read would
    /// download to read as GRIB, and a TIFF named for PNG, whose content the open does not judge
    /// and the lazy open read as a TIFF. Each is refused after its first request when the list
    /// leaves out the driver its content shows, and the TIFF opens with GTiff allowed.
    #[test]
    fn a_remote_file_read_by_content_needs_that_driver_allowed() {
        let _serial = cache_test_guard();
        let mut grib = b"II*\0".to_vec();
        grib.resize(100, 0);
        grib.extend_from_slice(b"GRIB\0\0\0\x02");
        grib.resize(1_000_000, 0);
        let (base, seen) = serve_recording(grib, None);
        crate::config::push_scope(scope(&[("TERRIO_ALLOWED_DRIVERS", "GTiff")]));
        let err = crate::open_raster(&format!("{base}/x")).map(|_| ()).unwrap_err().to_string();
        crate::config::pop_scope();
        assert_eq!(err, format!("'{base}/x' holds GRIB data, and the GRIB driver is not allowed (allowed: GTIFF)"));
        let requests = seen.lock().unwrap().len();
        assert!(requests == 1 && asked_bytes(&seen) <= FIRST_READ, "refused after its first request");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.tif");
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 1, data_type: wbraster::DataType::U8, ..Default::default() };
        let raster = wbraster::Raster::from_data_native(cfg, wbraster::raster::RasterData::U8((0..12).collect())).unwrap();
        crate::write_raster(&raster, path.to_str().unwrap(), &Default::default()).unwrap();
        for (drivers, opened) in [("PNG", false), ("PNG,GTiff", true)] {
            let (base, seen) = serve_recording(std::fs::read(&path).unwrap(), None);
            crate::config::push_scope(scope(&[("TERRIO_ALLOWED_DRIVERS", drivers)]));
            let lazy = crate::LazyRaster::open(&format!("{base}/x.png")).map(|r| r.driver().name()).map_err(|e| e.to_string());
            crate::config::pop_scope();
            let refusal = format!("'{base}/x.png' holds GTiff data, and the GTiff driver is not allowed (allowed: PNG)");
            assert_eq!(lazy, if opened { Ok("GTiff") } else { Err(refusal) });
            assert_eq!(seen.lock().unwrap().len(), 1);
        }
    }

    #[cfg(feature = "grib")]
    #[test]
    fn a_remote_grib_without_a_grib_extension_is_read_by_range() {
        // Files with NOAA's names (`gfs.t00z.pgrb2.1p00.f000`) are not downloaded whole
        // (41,329,883 bytes for one GFS band); a named file is read message by message.
        let message = include_bytes!("../tests/data/grib/simple.grib2");
        let bands = crate::LazyRaster::open("tests/data/grib/simple.grib2").unwrap().band_count();
        let file: Vec<u8> = message.iter().copied().cycle().take(message.len() * 200).collect();
        let (base, seen) = serve_recording(file.clone(), None);
        let r = crate::LazyRaster::open(&format!("{base}/gfs.t00z.pgrb2.1p00.f000")).unwrap();
        assert!(r.is_lazy(), "read band by band, by range");
        assert_eq!(r.band_count(), 200 * bands);
        assert_eq!(header_values(&seen, "range").len(), seen.lock().unwrap().len(), "every request has a range");
    }

    #[test]
    fn a_download_is_refused_when_the_server_stops_honouring_ranges() {
        // The rest of a file is one more range request: a whole body in answer is refused, not
        // appended to the bytes already written.
        let data = test_data();
        let (url, count) = serve(data, Mode::FirstRangeOnly);
        let dir = tempfile::tempdir().unwrap();
        let err = download(&Remote::Url(url.replace("/data", "/a.dat")), &dir.path().join("a.dat")).unwrap_err().to_string();
        assert!(err.contains("range downloading not supported"), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    /// Options of a test's thread scope.
    fn scope(pairs: &[(&str, &str)]) -> HashMap<String, Option<String>> {
        pairs.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect()
    }

    /// A dataset name never chooses the host the Azure token goes to
    /// (`azure+https://attacker.example/...`, `az://attacker.example?x=/container/blob`). Names
    /// that would reach another host than the configured endpoint send no request at all.
    #[test]
    fn an_azure_token_never_reaches_a_host_a_dataset_name_chooses() {
        let (base, seen) = serve_recording(test_data(), None);
        let host = base.trim_start_matches("http://").to_string();
        crate::config::push_scope(scope(&[("AZURE_STORAGE_ACCESS_TOKEN", "dummy-token"), ("CPL_AZURE_USE_HTTPS", "NO")]));
        let names = [
            format!("azure+{base}/asset.tif"),
            format!("azure+https://{host}/asset.tif"),
            format!("/vsicurl/azure+{base}/asset.tif"),
            format!("az://{host}?ignored=/container/asset.tif"),
            format!("/vsiaz/{host}?ignored=/container/asset.tif"),
            format!("az://acct@{host}/container/asset.tif"),
            format!("az://{}/container/asset.tif", host.replace(':', "%3A")),
            format!("az://{host}/container/asset.tif"),
            format!("/vsizip//vsiaz/{host}?x=/container/a.zip/a.tif"),
        ];
        let mut errors = Vec::new();
        for name in &names {
            assert!(!matches!(crate::vfs::Location::parse(name), crate::vfs::Location::Url(_)), "{name} parsed as a URL");
            errors.push(crate::LazyRaster::open(name).err().map(|e| e.to_string()).unwrap_or_else(|| format!("{name} opened")));
        }
        crate::config::pop_scope();
        assert!(seen.lock().unwrap().is_empty(), "requests reached the host the names chose: {:?}", seen.lock().unwrap());
        assert!(errors.iter().all(|e| e.contains("not a valid Azure location") || e.contains("not a dataset name")), "{errors:?}");
    }

    /// The token goes to the configured endpoint, and a marked URL elsewhere is not requested.
    #[test]
    fn an_azure_token_reaches_only_the_configured_endpoint() {
        let (base, seen) = serve_recording(test_data(), None);
        let endpoint = format!("{base}/devstoreaccount1");
        crate::config::push_scope(scope(&[
            ("AZURE_STORAGE_ACCESS_TOKEN", "dummy-token"),
            ("AZURE_STORAGE_ACCOUNT", "devstoreaccount1"),
            ("CPL_AZURE_ENDPOINT", &endpoint),
        ]));
        let remote = crate::vfs::Location::parse("/vsiaz/container/blob.bin").remote().unwrap();
        let opened = RangeReader::open_with(&remote, options());
        crate::config::pop_scope();
        assert!(opened.is_ok());
        let first = seen.lock().unwrap()[0].clone();
        assert!(first.0.starts_with("GET /devstoreaccount1/container/blob.bin"), "{}", first.0);
        assert!(first.1.iter().any(|(k, v)| k == "authorization" && v == "Bearer dummy-token"));
        // A marked URL that is not on the configured endpoint (none here): refused, unrequested.
        let (other, other_seen) = serve_recording(test_data(), None);
        crate::config::push_scope(scope(&[("AZURE_STORAGE_ACCESS_TOKEN", "dummy-token")]));
        let marked = RangeReader::open_with(&Remote::Url(format!("{}{other}/data", azure_bearer_prefix())), options());
        crate::config::pop_scope();
        assert!(marked.err().unwrap().to_string().contains("only from the configured endpoint"));
        assert!(other_seen.lock().unwrap().is_empty());
        // An account from the path alone gets no token (GDAL's /vsiaz/ takes it from the
        // configuration); a configured account keeps the host whatever the path holds.
        crate::config::push_scope(scope(&[("AZURE_STORAGE_ACCESS_TOKEN", "dummy-token")]));
        let path_account = crate::vfs::Location::parse("az://acct/container/b.tif");
        crate::config::pop_scope();
        assert_eq!(path_account, crate::vfs::Location::Url("https://acct.blob.core.windows.net/container/b.tif".into()));
        crate::config::push_scope(scope(&[("AZURE_STORAGE_ACCESS_TOKEN", "dummy-token"), ("AZURE_STORAGE_ACCOUNT", "acct")]));
        let configured = crate::vfs::Location::parse("az://evil.example?x=/container/b.tif");
        crate::config::pop_scope();
        let crate::vfs::Location::Url(url) = configured else { panic!("{configured:?}") };
        let url = url.strip_prefix(azure_bearer_prefix()).expect("marked for the token");
        assert_eq!(crate::vfs::url_origin(url), crate::vfs::url_origin("https://acct.blob.core.windows.net/"), "{url}");
    }

    #[test]
    fn a_remote_vector_is_read_by_range_and_a_server_without_ranges_is_refused() {
        // Remote vector reads send a Range, and a server without ranges is refused, as GDAL
        // refuses it ("Range downloading not supported by this server!").
        let features: Vec<String> = (0..4000)
            .map(|i| format!(r#"{{"type":"Feature","properties":{{"i":{i}}},"geometry":{{"type":"Point","coordinates":[{},1]}}}}"#, i % 90))
            .collect();
        let doc = format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(",")).into_bytes();
        assert!(doc.len() as u64 > 10 * FIRST_READ);
        let (url, count) = serve(doc.clone(), Mode::NoRanges);
        let err =
            crate::open_vector(&url.replace("/data", "/big.geojson"), &crate::VectorOpenOptions::default()).err().unwrap().to_string();
        assert!(err.contains("range downloading not supported"), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let (base, seen) = serve_recording(doc.clone(), None);
        let v = crate::open_vector(&format!("{base}/big.geojson"), &crate::VectorOpenOptions::default()).unwrap();
        assert_eq!(v.layers[0].features.len(), 4000);
        let ranges = header_values(&seen, "range");
        assert_eq!(ranges.len(), seen.lock().unwrap().len(), "every request has a range: {ranges:?}");
    }

    #[cfg(feature = "geotiff")]
    #[test]
    fn extensionless_raster_metadata_reuses_the_vector_probe_range() {
        let _serial = cache_test_guard();
        for driver in [crate::formats::RasterDriver::GTiff, crate::formats::RasterDriver::Cog] {
            let config = crate::RasterConfig { cols: 1024, rows: 1024, bands: 1, data_type: crate::DataType::U8, ..Default::default() };
            let raster = crate::Raster::from_data_native(config, crate::raster::RasterData::U8(vec![42; 1024 * 1024])).unwrap();
            let source = crate::RasterDataset::new(raster, false);
            let options = crate::RasterWriteOptions { driver: Some(driver), compression: Some("NONE".into()), ..Default::default() };
            let bytes = crate::memory::write_raster_bytes(&source, "source.tif", &options).unwrap();
            assert!(bytes.len() > 1_000_000);
            let (base, seen) = serve_recording(bytes, None);
            let baseline = crate::LazyRaster::open(&format!("{base}/baseline")).unwrap();
            assert_eq!(baseline.width(), 1024);
            let before_requests = seen.lock().unwrap().len();
            let before_bytes = asked_bytes(&seen);
            let before = (before_requests, before_bytes);
            let url = format!("{base}/info");
            assert!(matches!(crate::info::info_path(&url, &Default::default()), Ok(crate::info::Info::Raster(_))));
            let after_requests = seen.lock().unwrap().len() - before.0;
            let after_bytes = asked_bytes(&seen) - before.1;
            let after = (after_requests, after_bytes);
            assert_eq!(after, before, "{driver:?}: metadata dispatch must reuse the first cached range");
            assert!(after.1 <= FIRST_READ, "metadata must not fetch raster tiles");
            eprintln!("{driver:?}: direct lazy metadata and extensionless info each use {} request(s), {} range bytes", after.0, after.1);
            let count = seen.lock().unwrap().len();
            crate::info::info_path(&url, &Default::default()).unwrap();
            assert_eq!(seen.lock().unwrap().len(), count, "the repeated metadata read stays cached");
        }
    }

    #[test]
    fn wfs_identification_reads_a_bounded_prefix_not_the_capabilities_document() {
        let _serial = stats_test_guard();
        let mut document =
            br#"<wfs:WFS_Capabilities xmlns:wfs="http://www.opengis.net/wfs/2.0" version="2.0.0"><FeatureTypeList/><!--"#.to_vec();
        document.extend(vec![b'x'; 1_000_000]);
        document.extend_from_slice(b"--></wfs:WFS_Capabilities>");
        let (base, _) = serve_recording(document, None);
        let found = crate::inspection::identify(&format!("WFS:{base}/ows?map=demo"), false).unwrap().unwrap();
        assert_eq!(found.driver, "WFS");
        assert!(!found.deep);
        let stats = fetch_stats_by_host().into_iter().find(|(host, _)| host == host_of(&base)).unwrap().1;
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.bytes, crate::inspection::IDENTIFICATION_HEADER_BYTES as u64);
    }
}
