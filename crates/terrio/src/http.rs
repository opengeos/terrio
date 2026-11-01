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
//! | `TERRIO_HTTP_LOG=YES`: print each request to standard error | `CPL_CURL_VERBOSE` | off |
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub(crate) mod store;

/// Redirects followed before giving up.
const MAX_REDIRECTS: u32 = 10;

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
}

impl Redirects {
    /// Whether a request for `asked` may be redirected to `to`.
    fn allow(self, asked: &str, to: &str) -> bool {
        self == Redirects::Any || (crate::vfs::url_origin(to).is_some() && crate::vfs::url_origin(to) == crate::vfs::url_origin(asked))
    }
}

/// The error of a redirect that [`Redirects::allow`] refuses: it names the origin it led to, not
/// the URL (which may hold a signature).
fn redirect_refused(asked: &str, to: &str) -> Error {
    let to = crate::vfs::url_origin(to).map_or_else(|| "?".to_string(), |(scheme, host)| format!("{scheme}://{host}"));
    Error::Http(format!(
        "{}: a redirect to another origin ({to}) was not followed: a link a document chose is read from its own origin \
         only (TERRIO_STAC_FOLLOW_CROSS_ORIGIN=YES follows it)",
        redact(asked)
    ))
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
            options,
            remote: remote.clone(),
            url,
            query: String::new(),
            s3,
            missing_ok: false,
            refused_ok: false,
            body_cap: None,
            azure,
            config,
            headers: Vec::new(),
            on_azure_endpoint: false,
            redirects: Redirects::Any,
        };
        client.on_azure_endpoint = client.azure && client.resolve_on_azure_endpoint();
        client.headers = client.resolve_headers();
        client
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
            let mut req = self.agent.get(&url);
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
                    retry_warning(0, &self.url, self.options.retry_delay, attempt);
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
                    return Err(redirect_refused(&self.url, &to));
                }
                url = to;
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
                429 | 500 | 502 | 503 | 504 if attempt < self.options.max_retry => {
                    record_discarded(&self.url, &self.stats_name(), response);
                    retry_warning(status, &self.url, self.options.retry_delay, attempt);
                    self.backoff(&mut attempt)
                }
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
                    retry_warning(status, &self.url, self.options.retry_delay, attempt);
                    self.backoff(&mut attempt);
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

/// The error of a body over its cap.
pub(crate) fn over_cap(url: &str, cap: u64) -> Error {
    Error::Http(format!("{}: the response is larger than the {cap} bytes allowed", redact(url)))
}

thread_local! {
    /// The agent shared by the requests of the operation in progress on this thread (see
    /// [`AgentScope`]), with the settings it was built with.
    static SHARED_AGENT: std::cell::RefCell<(u32, Option<(AgentSettings, ureq::Agent)>)> = const { std::cell::RefCell::new((0, None)) };
}

/// The settings an agent is built with.
type AgentSettings = (Option<Duration>, Duration, String);

/// While alive, the requests made on this thread share one agent, and so its connections (and
/// TLS sessions): a recursive upload, deletion or listing opens one connection per host rather
/// than one per request. Scopes nest; the agent is dropped when the outermost one ends.
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
                shared.1 = None;
            }
        });
    }
}

/// The agent requests are sent with: no automatic redirects (followed by [`Client`], which
/// signs only the first request), no compression. Within an [`AgentScope`], the scope's agent
/// when its settings are the same.
fn agent(options: &HttpOptions) -> ureq::Agent {
    let settings: AgentSettings = (options.timeout, options.connect_timeout, options.user_agent.clone());
    SHARED_AGENT.with(|shared| {
        let mut shared = shared.borrow_mut();
        if shared.0 == 0 {
            return new_agent(options);
        }
        match &shared.1 {
            Some((s, agent)) if *s == settings => agent.clone(),
            _ => {
                let agent = new_agent(options);
                shared.1 = Some((settings, agent.clone()));
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
    ureq::Agent::config_builder()
        .tls_config(tls_config())
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
    /// The creating thread's options (see [`crate::config`]): objects are fetched from any thread.
    config: HashMap<String, Option<String>>,
}

impl ObjectFetcher {
    /// A fetcher with the options and S3 credentials from the environment.
    pub(crate) fn new() -> Self {
        let options = HttpOptions::from_env();
        let auth = (Credentials::from_env(), s3::region_from_env());
        Self { agent: agent(&options), options, region: Mutex::new(None), auth, config: crate::config::thread_options() }
    }

    /// Fetches an object, or part of it. `None` when the object does not exist (status 404, or
    /// 403 as S3 answers anonymous requests for missing keys; a signed request's 403 is an error).
    pub(crate) fn get(&self, remote: &Remote, range: ByteRange) -> Result<Option<Vec<u8>>> {
        // A region already learned is the client's from the start (its URL is the regional one).
        let known = self.region.lock().ok().and_then(|r| r.clone());
        let auth = (self.auth.0.clone(), known.clone().or_else(|| self.auth.1.clone()));
        let mut client = Client::with_auth(remote, self.options.clone(), self.agent.clone(), Some(auth), self.config.clone());
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
    let mut client = Client::new(remote, HttpOptions::from_env());
    let response = client.send_range(None)?;
    let mut reader = MeasuredReader { inner: response.into_body().into_reader(), bytes: 0 };
    let result = read(&mut reader);
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
    get_capped_following(remote, cap, Redirects::Any)
}

/// [`get_capped`], its redirects followed as `redirects` allows.
pub(crate) fn get_capped_following(remote: &Remote, cap: u64, redirects: Redirects) -> Result<Vec<u8>> {
    let mut client = Client::new(remote, HttpOptions::from_env());
    client.body_cap = Some(cap);
    client.redirects = redirects;
    Ok(client.fetch(None)?.data)
}

/// Reads a whole remote file (a STAC item, a GeoJSON document) into memory by range, as
/// [`download_checked`] reads it, capped at [`json_max_size`]: a file over the cap is refused
/// once its length is known, before its body is fetched. Its redirects are followed as
/// `redirects` allows.
pub(crate) fn get_file(remote: &Remote, redirects: Redirects) -> Result<Vec<u8>> {
    let reader = RangeReader::open_following(remote, HttpOptions::from_env(), redirects)?;
    let mut s = reader.lock()?;
    let cap = json_max_size();
    if s.len > cap {
        return Err(over_cap(&s.client.url, cap));
    }
    let mut out = Vec::new();
    s.copy_to(&mut out)?;
    Ok(out)
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
/// body. Redirects that keep the method (307 and 308) are followed as `redirects` allows, and
/// transient errors retried.
pub(crate) fn post_json(url: &str, body: &[u8], headers: &[(String, String)], redirects: Redirects) -> Result<Vec<u8>> {
    request("POST", url, Some(body), headers, json_max_size(), redirects)
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

/// Sends a `GET` or a `POST` (of a JSON `body`) with extra `headers`, and returns the response's
/// body: redirects followed as `redirects` allows (for a `POST`, only those keeping the method,
/// 307 and 308), each checked before anything is sent to where it leads, the extra headers
/// dropped when a redirect leads to another origin (they may hold keys), and transient errors and
/// statuses (429, 500, 502, 503, 504) retried. A body over `cap` bytes is an error, read no
/// further than the cap.
fn request(method: &str, url: &str, body: Option<&[u8]>, headers: &[(String, String)], cap: u64, policy: Redirects) -> Result<Vec<u8>> {
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
                return Err(redirect_refused(asked, &to));
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
        return Ok(data);
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
    static SERIAL: Mutex<()> = Mutex::new(());
    let serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    EVICTION_HELD.store(true, Ordering::SeqCst);
    CacheTestGuard { _serial: serial }
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
/// the path-specific options and the retry warnings) across a fork, for [`crate::par`]: the
/// forking thread takes them before the process is copied and lets them go after, so that the
/// child never starts with one held by a thread it does not have.
#[cfg(unix)]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    held.push(Box::new(file_cache()));
    held.push(Box::new(stats(&HOST_STATS)));
    held.push(Box::new(stats(&FILE_STATS)));
    held.push(Box::new(path_options_write()));
    held.push(Box::new(retry_warnings()));
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

    /// Stores a part of the file `key` of length `len`, then drops the oldest parts beyond
    /// `budget` bytes. A part larger than the budget is not kept.
    fn store(&mut self, key: &str, len: u64, slot: Slot, data: Arc<[u8]>, budget: u64) {
        if data.len() as u64 > budget {
            return;
        }
        let file = self.files.entry(key.to_string()).or_default();
        file.len = len;
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
}

impl RangeReader {
    /// Opens a remote file with the options from the environment, fetching its first bytes.
    pub fn open(remote: &Remote) -> Result<Self> {
        Self::open_with(remote, HttpOptions::from_env())
    }

    /// Opens a remote file, fetching its first bytes: `FIRST_READ` of them (a block, when
    /// blocks are smaller), or `ingested_bytes_at_open` in whole blocks. A file in the
    /// process-wide cache is opened without a request.
    pub fn open_with(remote: &Remote, options: HttpOptions) -> Result<Self> {
        Self::open_following(remote, options, Redirects::Any)
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
        };
        if !shared.load_cached(first_len) {
            let first = shared.client.fetch(Some((0, first_len - 1)))?;
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
            .filter(|(off, len)| s.prefix.as_ref().is_none_or(|p| off + len > p.len() as u64))
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
    /// Takes the file's length and first bytes from the process-wide cache. Returns whether it
    /// holds them: its whole content, or at least `first_len` bytes from its start.
    fn load_cached(&mut self, first_len: u64) -> bool {
        let cache = file_cache();
        let Some(file) = cache.files.get(&self.key) else { return false };
        let first: u64 =
            file.whole.as_ref().or(file.prefix.as_ref()).or(file.blocks.get(&(self.block_size, 0))).map_or(0, |d| d.len() as u64);
        if first < first_len.min(file.len) {
            return false;
        }
        self.len = file.len;
        self.whole = file.whole.clone();
        self.prefix = file.prefix.clone();
        if let Some(b) = file.blocks.get(&(self.block_size, 0)) {
            self.blocks.insert(0, b.clone());
            self.order.push_back(0);
        }
        true
    }

    /// Takes block `b` from the process-wide cache. Returns whether it held it.
    fn load_cached_block(&mut self, b: u64) -> bool {
        let data = file_cache().files.get(&self.key).and_then(|f| f.blocks.get(&(self.block_size, b)).cloned());
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
        file_cache().store(&self.key, self.len, Slot::Whole, data.clone(), self.shared_budget);
        self.whole = Some(data);
    }

    fn store_prefix(&mut self, data: Arc<[u8]>) {
        file_cache().store(&self.key, self.len, Slot::Prefix, data.clone(), self.shared_budget);
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

    fn store(&mut self, first_block: u64, data: Vec<u8>) {
        let bs = self.block_size as usize;
        let mut cache = file_cache();
        for (i, chunk) in data.chunks(bs).enumerate() {
            let b = first_block + i as u64;
            let chunk: Arc<[u8]> = chunk.into();
            cache.store(&self.key, self.len, Slot::Block(self.block_size, b), chunk.clone(), self.shared_budget);
            if self.blocks.insert(b, chunk).is_none() {
                self.order.push_back(b);
            }
        }
    }

    /// Fetches the given blocks (sorted, not cached), merging runs separated by at most `gap`
    /// blocks into one request. Blocks the process-wide cache holds are taken from it, and bytes
    /// the first request returned are not asked again.
    fn fetch_blocks(&mut self, blocks: &[u64], gap: u64) -> io::Result<()> {
        let blocks: Vec<u64> = blocks.iter().copied().filter(|&b| !self.load_cached_block(b)).collect();
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
            // The first bytes, already held, start the run when it starts the file.
            let held: &[u8] = match &self.prefix {
                Some(p) if (first as usize) < p.len() => &p[first as usize..p.len().min(last as usize + 1)],
                _ => &[],
            };
            let from = first + held.len() as u64;
            let mut data = held.to_vec();
            if from <= last {
                let fetched = self.client.fetch(Some((from, last))).map_err(io::Error::other)?;
                match fetched.status {
                    206 if fetched.data.len() as u64 == last - from + 1 => data.extend_from_slice(&fetched.data),
                    200 if fetched.data.len() as u64 == self.len => {
                        self.store_whole(fetched.data.into());
                        return Ok(());
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
        }
        Ok(())
    }

    /// Copies the bytes at `pos` into `buf`, fetching what is missing. Returns the count copied.
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
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
        let new = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = new.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the file"))?;
        Ok(self.pos)
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
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
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

    /// A child forked while other threads read remote files reads remote files too: the locks of
    /// remote reads that every thread shares (the cache of remote files, the request statistics,
    /// the path-specific options) are held by the forking thread across the fork, never left held
    /// in the child by a thread it does not have. In a process of its own (`par::alone`).
    #[test]
    #[cfg(unix)]
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
        cache.store("guard-test://first", 5, Slot::Whole, Arc::from(&b"first"[..]), 1 << 20);
        // A budget the cache is already over: without the guard, this store evicts the oldest
        // entries, the one above among them.
        cache.store("guard-test://second", 6, Slot::Whole, Arc::from(&b"second"[..]), 6);
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
    fn info_of_a_remote_file_named_for_no_driver_is_refused_after_its_first_bytes() {
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
    /// the lazy open, in two requests: its first 16,384 bytes, then the rest.
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
                let last = tile.len() - 1;
                assert_eq!(
                    header_values(&seen, "range"),
                    ["bytes=0-16383".to_string(), format!("bytes=16384-{last}")],
                    "{name} (lazy {lazy})"
                );
                assert_eq!(seen.lock().unwrap().len(), 2, "{name} (lazy {lazy})");
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
        let last = srtm_tile().len() - 1;
        let two = ["bytes=0-16383".to_string(), format!("bytes=16384-{last}")];
        assert_eq!(header_values(&seen, "range"), [two.clone(), two].concat());
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
}
