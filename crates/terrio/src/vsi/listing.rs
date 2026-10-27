//! Listing object stores and HTTP directory index pages, without doing any I/O: what to request
//! and how to read the answers. Native builds send the requests with [`crate::http`] (signing S3
//! ones); WebAssembly hosts send them with `fetch` (see [`RemoteLister`]).
//!
//! Amazon S3's `ListObjectsV2` (which Google Cloud Storage's XML API answers too), Azure's List
//! Blobs, and the `<a href>` links of Apache, nginx and similar autoindex pages, as GDAL's
//! `/vsis3/`, `/vsigs/`, `/vsiaz/` and `/vsicurl/` list them.

use super::{Entry, EntryKind};
use crate::{Error, Result};
use std::collections::HashMap;

type EntryIndex = HashMap<(String, bool), usize>;

/// Removes credential-bearing query strings from diagnostics and public names.
pub(crate) fn redact_url(url: &str) -> std::borrow::Cow<'_, str> {
    if let Some((base, query)) = url.split_once('?') {
        let lower = query.to_ascii_lowercase();
        if ["sig", "token=", "credential=", "api_key=", "apikey=", "access_key="].iter().any(|key| lower.contains(key)) {
            return format!("{base}?<redacted>").into();
        }
    }
    url.into()
}

/// An entry of a remote listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectEntry {
    /// The path relative to the listed directory, `/`-separated, without a trailing slash.
    pub name: String,
    /// Whether the entry is a "directory" (a common prefix of keys, or a directory marker).
    pub dir: bool,
    /// The object's size in bytes (0 for an S3 directory marker).
    pub size: Option<u64>,
    /// The last modification time, in seconds since the Unix epoch.
    pub modified: Option<i64>,
    /// The entity tag, without quotes.
    pub etag: Option<String>,
}

impl ObjectEntry {
    pub(crate) fn into_entry(self) -> Entry {
        Entry {
            name: self.name,
            kind: if self.dir { EntryKind::Directory } else { EntryKind::File },
            size: self.size,
            modified: self.modified,
            permissions: None,
            etag: self.etag,
        }
    }
}

/// How a directory is listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Api {
    /// An S3 bucket, listed through the bucket's URL (signed natively when credentials are set):
    /// [`Api::query`] is the query to add to it.
    S3 { bucket: String, prefix: String },
    /// An unsigned `ListObjectsV2` at `base` (`https://host/bucket`, or a virtual-hosted S3 URL):
    /// Google Cloud Storage, public S3 buckets.
    Url { base: String, prefix: String },
    /// Azure Blob Storage's List Blobs at `base` (`https://account.blob.core.windows.net/c`).
    Azure { base: String, prefix: String, sas: String },
    /// A plain HTTP server's HTML index page.
    Html { url: String },
}

fn with_slash(p: &str) -> String {
    if p.is_empty() { String::new() } else { format!("{}/", p.trim_end_matches('/')) }
}

pub(crate) fn looks_like_index(data: &[u8]) -> bool {
    let head = String::from_utf8_lossy(data).to_ascii_lowercase();
    ["<title>index of", "<h1>index of", "directory listing for", "<!-- autoindex"].iter().any(|tag| head.contains(tag))
}

/// Percent-encodes everything but `A-Z a-z 0-9 - _ . ~` (and `/` unless `encode_slash`).
pub(crate) fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decodes a URL path once; malformed escapes or non-UTF-8 keys retain their spelling.
fn uri_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(value) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

impl Api {
    /// The listing API of an HTTP(S) URL: an object store (GCS, Azure, S3 URLs) or else an HTML
    /// index page.
    pub(crate) fn for_url(url: &str) -> Api {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let html = || Api::Html { url: url.to_string() };
        let Some((scheme, rest)) = path.split_once("://") else { return html() };
        let (host, key) = rest.split_once('/').unwrap_or((rest, ""));
        let lower = host.to_ascii_lowercase();
        if lower == "storage.googleapis.com" {
            let (bucket, prefix) = key.split_once('/').unwrap_or((key, ""));
            Api::Url { base: format!("{scheme}://{host}/{bucket}"), prefix: with_slash(&uri_decode(prefix)) }
        } else if lower.ends_with(".blob.core.windows.net") {
            let (container, prefix) = key.split_once('/').unwrap_or((key, ""));
            Api::Azure { base: format!("{scheme}://{host}/{container}"), prefix: with_slash(&uri_decode(prefix)), sas: query.to_string() }
        } else if lower.ends_with(".amazonaws.com") && (lower.contains(".s3") || lower.starts_with("s3.") || lower.starts_with("s3-")) {
            // Virtual-hosted S3 URLs; path-style ones (`s3.amazonaws.com/bucket/...`) too.
            if lower.starts_with("s3.") || lower.starts_with("s3-") {
                let (bucket, prefix) = key.split_once('/').unwrap_or((key, ""));
                Api::Url { base: format!("{scheme}://{host}/{bucket}"), prefix: with_slash(&uri_decode(prefix)) }
            } else {
                Api::Url { base: format!("{scheme}://{host}"), prefix: with_slash(&uri_decode(key)) }
            }
        } else {
            html()
        }
    }

    /// Whether the store lists through an API (not an HTML page).
    pub(crate) fn is_root(&self) -> bool {
        match self {
            Self::S3 { prefix, .. } | Self::Url { prefix, .. } | Self::Azure { prefix, .. } => prefix.trim_matches('/').is_empty(),
            Self::Html { .. } => false,
        }
    }

    pub(crate) fn is_object_store(&self) -> bool {
        !matches!(self, Api::Html { .. })
    }

    /// Descends using the raw XML key, encoding it only when making the query.
    pub(crate) fn child_prefix(&self, child: &str) -> Result<Self> {
        let mut api = self.clone();
        match &mut api {
            Api::S3 { prefix, .. } | Api::Url { prefix, .. } | Api::Azure { prefix, .. } => {
                *prefix = format!("{}{}/", prefix, child.trim_end_matches('/'));
            }
            Api::Html { .. } => return Err(Error::Unsupported("not an object-store listing".into())),
        }
        Ok(api)
    }

    /// The query of a listing request (`ListObjectsV2` or List Blobs), from a continuation token,
    /// asking for at most `max` entries a page. Its parameters are in sorted order, as SigV4's
    /// canonical query has them.
    pub(crate) fn query_limited(&self, token: Option<&str>, recursive: bool, max: Option<u32>) -> String {
        let enc = |s: &str| uri_encode(s, true);
        match self {
            Api::Azure { prefix, sas, .. } => {
                let mut q = vec!["restype=container".to_string(), "comp=list".to_string(), format!("prefix={}", enc(prefix))];
                if let Some(m) = max {
                    q.push(format!("maxresults={m}"));
                }
                if !recursive {
                    q.push(format!("delimiter={}", enc("/")));
                }
                if let Some(t) = token {
                    q.push(format!("marker={}", enc(t)));
                }
                if !sas.is_empty() {
                    q.push(sas.clone());
                }
                q.join("&")
            }
            Api::S3 { prefix, .. } | Api::Url { prefix, .. } => {
                let mut q: Vec<String> = token.map(|t| format!("continuation-token={}", enc(t))).into_iter().collect();
                if !recursive {
                    q.push(format!("delimiter={}", enc("/")));
                }
                q.push("list-type=2".to_string());
                if let Some(m) = max {
                    q.push(format!("max-keys={m}"));
                }
                q.push(format!("prefix={}", enc(prefix)));
                q.join("&")
            }
            Api::Html { .. } => String::new(),
        }
    }

    /// The URL of a listing request, for unsigned stores and HTML pages.
    pub(crate) fn request_url(&self, token: Option<&str>, recursive: bool) -> Option<String> {
        self.request_url_limited(token, recursive, None)
    }

    /// [`Api::request_url`], asking for at most `max` entries a page.
    pub(crate) fn request_url_limited(&self, token: Option<&str>, recursive: bool, max: Option<u32>) -> Option<String> {
        Some(match self {
            Api::Url { base, .. } => format!("{base}/?{}", self.query_limited(token, recursive, max)),
            Api::Azure { base, .. } => format!("{base}?{}", self.query_limited(token, recursive, max)),
            Api::Html { url } => html_directory_url(url),
            Api::S3 { .. } => return None,
        })
    }

    /// A placeholder equal to the listed prefix proves that the directory exists.
    pub(crate) fn has_directory_marker(&self, body: &[u8]) -> bool {
        let (prefix, group, key) = match self {
            Self::S3 { prefix, .. } | Self::Url { prefix, .. } => (prefix, "Contents", "Key"),
            Self::Azure { prefix, .. } => (prefix, "Blob", "Name"),
            Self::Html { .. } => return false,
        };
        let xml = String::from_utf8_lossy(body);
        xml_elements(&xml, group).iter().any(|block| xml_elements(block, key).first().is_some_and(|v| xml_unescape(v) == *prefix))
    }

    /// Require the store API's document root, rather than accepting arbitrary HTML/XML.
    pub(crate) fn is_listing_page(&self, body: &[u8]) -> bool {
        use quick_xml::events::Event;
        let expected = match self {
            Self::S3 { .. } | Self::Url { .. } => "ListBucketResult",
            Self::Azure { .. } => "EnumerationResults",
            Self::Html { .. } => return false,
        };
        let mut reader = quick_xml::Reader::from_reader(body);
        loop {
            match reader.read_event() {
                Ok(Event::Start(element) | Event::Empty(element)) => return element.local_name().as_ref() == expected,
                Ok(Event::Decl(_) | Event::Comment(_) | Event::PI(_)) => {}
                Ok(Event::Text(text)) if text.as_ref().bytes().all(|b| b.is_ascii_whitespace()) => {}
                _ => return false,
            }
        }
    }

    /// Reads one listing page into `out`; returns the continuation token, if there are more.
    pub(crate) fn read_page(&self, body: &[u8], out: &mut Vec<ObjectEntry>, index: &mut EntryIndex) -> Option<String> {
        let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
        let xml = String::from_utf8_lossy(body);
        match self {
            Api::Html { url } => {
                out.extend(parse_html_index(&xml, &url_path(url)));
                None
            }
            Api::Azure { prefix, .. } => {
                parse_listing_page_indexed(&xml, prefix, true, out, index);
                xml_elements(&xml, "NextMarker").first().map(|t| xml_unescape(t)).filter(|t| !t.is_empty())
            }
            Api::S3 { prefix, .. } | Api::Url { prefix, .. } => {
                parse_listing_page_indexed(&xml, prefix, false, out, index);
                if xml_elements(&xml, "IsTruncated").first().is_some_and(|t| t.trim() == "true") {
                    xml_elements(&xml, "NextContinuationToken").first().map(|t| xml_unescape(t)).filter(|t| !t.is_empty())
                } else {
                    None
                }
            }
        }
    }
}

/// The URL of a directory's index page: its path with a trailing slash, its query kept.
pub(crate) fn html_directory_url(url: &str) -> String {
    let (path, query) = url.split_once('?').map_or((url, None), |(p, q)| (p, Some(q)));
    format!("{}/{}", path.trim_end_matches('/'), query.map(|q| format!("?{q}")).unwrap_or_default())
}

/// The path of a URL (`/a/b`), without its query.
fn url_path(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.split_once("://").map_or(path, |(_, r)| r).split_once('/').map_or("/".to_string(), |(_, p)| format!("/{p}"))
}

/// The most pages a listing reads.
pub(crate) const MAX_PAGES: usize = 1000;

/// The contents of the elements named `name` in an XML document (no nesting of the same name).
pub(crate) fn xml_elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

/// XML text with its entities replaced: the predefined ones, and numeric character references
/// (`&#39;`, `&#x27;`) as XML serializers such as Go's `encoding/xml` write them. Each reference
/// is decoded once; an unknown or malformed one is kept as written.
pub(crate) fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        // A reference ends within 10 bytes of its ampersand (`&#x10FFFF;` is the longest usual one).
        let decoded = after.find(';').filter(|&end| end <= 10).and_then(|end| Some((end, decode_entity(&after[1..end])?)));
        match decoded {
            Some((end, c)) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The character of an entity name (`lt`) or a numeric reference (`#39`, `#x27`).
fn decode_entity(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "amp" => Some('&'),
        _ => {
            let n = name.strip_prefix('#')?;
            let (digits, radix) = match n.strip_prefix(['x', 'X']) {
                Some(hex) => (hex, 16),
                None => (n, 10),
            };
            if digits.is_empty() || !digits.chars().all(|d| d.is_digit(radix)) {
                return None;
            }
            char::from_u32(u32::from_str_radix(digits, radix).ok()?)
        }
    }
}

/// Adds the entries of one listing page (S3 `ListObjectsV2`, GCS's XML API, or Azure's List
/// Blobs) to `out`: names relative to `prefix`, common prefixes as directories, keys ending with
/// `/` (directory markers) as directories with a size, so that deleting a tree deletes them too.
#[cfg(test)]
pub(crate) fn parse_listing_page(xml: &str, prefix: &str, azure: bool, out: &mut Vec<ObjectEntry>) {
    let mut index = out.iter().enumerate().map(|(i, e)| ((e.name.clone(), e.dir), i)).collect();
    parse_listing_page_indexed(xml, prefix, azure, out, &mut index);
}

fn parse_listing_page_indexed(xml: &str, prefix: &str, azure: bool, out: &mut Vec<ObjectEntry>, index: &mut EntryIndex) {
    let (group, name) = if azure { ("BlobPrefix", "Name") } else { ("CommonPrefixes", "Prefix") };
    for block in xml_elements(xml, group) {
        if let Some(p) = xml_elements(block, name).first()
            && let Some(child) = xml_unescape(p).strip_prefix(prefix)
        {
            push_entry(out, index, child, true, None, None, None);
        }
    }
    let (group, key, size, modified, etag) = if azure {
        ("Blob", "Name", "Content-Length", "Last-Modified", "Etag")
    } else {
        ("Contents", "Key", "Size", "LastModified", "ETag")
    };
    for block in xml_elements(xml, group) {
        let field = |n: &str| xml_elements(block, n).first().map(|v| xml_unescape(v));
        let Some(k) = field(key) else { continue };
        let Some(child) = k.strip_prefix(prefix) else { continue };
        let dir = child.ends_with('/');
        let modified = field(modified).and_then(|m| if azure { parse_http_date(&m) } else { parse_iso8601(&m) });
        let etag = field(etag).map(|e| e.trim_matches('"').to_string());
        let size = field(size).and_then(|s| s.trim().parse().ok());
        push_entry(out, index, child, dir, size, modified, if dir { None } else { etag });
    }
}

fn push_entry(
    out: &mut Vec<ObjectEntry>,
    index: &mut EntryIndex,
    child: &str,
    dir: bool,
    size: Option<u64>,
    modified: Option<i64>,
    etag: Option<String>,
) {
    let child = child.trim_end_matches('/');
    if child.is_empty() {
        return;
    }
    let key = (child.to_string(), dir);
    if let Some(&i) = index.get(&key) {
        out[i].modified = out[i].modified.or(modified);
        out[i].size = out[i].size.or(size);
    } else {
        index.insert(key, out.len());
        out.push(ObjectEntry { name: child.to_string(), dir, size, modified, etag });
    }
}

/// Adds the directories implied by the names of a recursive listing (`a/b/c` implies `a` and `a/b`).
pub(crate) fn add_implied_directories(out: &mut Vec<ObjectEntry>) {
    let mut dirs: Vec<String> = Vec::new();
    for e in out.iter() {
        let mut at = 0;
        while let Some(i) = e.name[at..].find('/') {
            dirs.push(e.name[..at + i].to_string());
            at += i + 1;
        }
    }
    dirs.sort();
    dirs.dedup();
    let existing: std::collections::HashSet<_> = out.iter().filter(|o| o.dir).map(|o| o.name.clone()).collect();
    for d in dirs {
        if !existing.contains(&d) {
            out.push(ObjectEntry { name: d, dir: true, size: None, modified: None, etag: None });
        }
    }
}

/// The children linked from an HTML directory index whose path is `dir_path` (`/a/b/`): names
/// as they appear in the links (percent-encoded, as GDAL keeps them), directories ending with `/`.
pub(crate) fn parse_html_index(html: &str, dir_path: &str) -> Vec<ObjectEntry> {
    let dir_path = format!("{}/", dir_path.trim_end_matches('/'));
    let lower = html.to_ascii_lowercase();
    let mut out: Vec<ObjectEntry> = Vec::new();
    let mut index = EntryIndex::new();
    let mut at = 0;
    let mut scanned = 0;
    let mut tag_start = None;
    let mut tag_end = None;
    while let Some(i) = lower[at..].find("href=") {
        let attribute = at + i;
        let start = attribute + 5;
        // Scan each segment once, retaining context even for arbitrarily long tags.
        let segment = &lower[scanned..attribute];
        if let Some(offset) = segment.rfind('<') {
            tag_start = Some(scanned + offset);
        }
        if let Some(offset) = segment.rfind('>') {
            tag_end = Some(scanned + offset);
        }
        scanned = start;
        let anchor = tag_start.is_some_and(|tag| {
            let prefix = &lower[tag + 1..attribute];
            prefix.starts_with('a') && prefix.as_bytes().get(1).is_some_and(u8::is_ascii_whitespace) && tag_end.is_none_or(|end| end < tag)
        });
        if !anchor {
            at = start;
            continue;
        }
        let rest = &html[start..];
        let (href, used) = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => match rest[1..].find(q) {
                Some(j) => (&rest[1..1 + j], j + 2),
                None => break,
            },
            _ => {
                let j = rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len());
                (&rest[..j], j)
            }
        };
        at = start + used;
        let href = xml_unescape(href);
        let href = href.split(['?', '#']).next().unwrap_or_default();
        // Absolute paths are kept when they lead into the directory.
        let href = if href.starts_with('/') {
            match href.strip_prefix(dir_path.as_str()) {
                Some(rel) => rel.to_string(),
                None => continue,
            }
        } else {
            href.strip_prefix("./").unwrap_or(href).to_string()
        };
        if href == "."
            || href.is_empty()
            || (href == ".." || href.starts_with("../"))
            || href.contains("://")
            || href.starts_with("mailto:")
        {
            continue;
        }
        let dir = href.ends_with('/');
        let name = href.trim_end_matches('/');
        // Only direct children.
        if name.is_empty() || name.contains('/') {
            continue;
        }
        push_entry(&mut out, &mut index, name, dir, None, None, None);
    }
    out
}

/// Seconds since the epoch of an ISO 8601 UTC time (`2024-01-01T01:42:27.000Z`).
pub(crate) fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    if s.len() < 19 {
        return None;
    }
    crate::civil_time::checked_unix_seconds(num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?)
}

/// Seconds since the epoch of an HTTP date (`Mon, 01 Jan 2024 01:42:27 GMT`).
pub(crate) fn parse_http_date(v: &str) -> Option<i64> {
    let f: Vec<&str> = v.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = f[..] else { return None };
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m = months.iter().position(|x| x.eq_ignore_ascii_case(month))? as i64 + 1;
    let (d, y): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let t: Vec<i64> = time.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [h, mi, sec] = t[..] else { return None };
    crate::civil_time::checked_unix_seconds(y, m, d, h, mi, sec)
}

/// A listing of a remote directory driven by the caller, who sends the requests (with `fetch` in
/// WebAssembly, for instance): unsigned, for public buckets and containers, SAS-token URLs and
/// HTML index pages.
///
/// ```
/// let mut lister = terrio::vsi::RemoteLister::new("https://storage.googleapis.com/bucket/dir", true, None)?;
/// while let Some(url) = lister.next_url() {
///     # let body: Vec<u8> = b"<ListBucketResult><Contents><Key>dir/a.tif</Key><Size>3</Size></Contents></ListBucketResult>".to_vec();
///     // let body = GET url ...
///     lister.feed(200, &body)?;
/// }
/// assert_eq!(lister.finish()[0].name, "a.tif");
/// # Ok::<(), terrio::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct RemoteLister {
    api: Api,
    recursive: bool,
    max_depth: usize,
    token: Option<String>,
    pages: usize,
    out: Vec<ObjectEntry>,
    index: EntryIndex,
    /// Finite-depth object-store directories still to list, without recursive scans.
    object_pending: Vec<(Api, String, usize)>,
    object_relative: String,
    object_level: usize,
    /// HTML index pages still to read: `(directory URL, path relative to the root, level)`.
    html_pending: Vec<(String, String, usize)>,
    /// The page being read: `(relative path, level)`.
    html_current: Option<(String, usize)>,
    done: bool,
}

impl RemoteLister {
    /// Maximum buffered XML response for one object-store listing page.
    pub const OBJECT_BODY_LIMIT: usize = 16 * 1024 * 1024;

    /// Maximum HTML directory-index body buffered by native and browser readers.
    pub const HTML_BODY_LIMIT: usize = 4 << 20;
    /// Prefix examined to distinguish an HTML index from binary/ordinary web content.
    pub const HTML_SNIFF_BYTES: usize = 4096;
    /// Whether responses are plain HTTP directory index pages rather than object-store XML.
    pub fn is_html(&self) -> bool {
        matches!(self.api, Api::Html { .. })
    }

    /// Checks a bounded HTML sniff prefix before a host buffers the rest of the response.
    pub fn check_html_prefix(&self, prefix: &[u8]) -> Result<()> {
        if self.is_html() && !looks_like_index(&prefix[..prefix.len().min(Self::HTML_SNIFF_BYTES)]) {
            return Err(Error::Unsupported("not a listable HTML directory index".into()));
        }
        Ok(())
    }

    /// A listing of `url` (`https://...`, `gs://bucket/dir`, `az://account/container/dir`, or
    /// `s3://bucket/dir` as its unsigned virtual-hosted URL), recursive or not, to `depth` levels.
    pub fn new(url: &str, recursive: bool, depth: Option<usize>) -> Result<Self> {
        let url = url.trim();
        let url = url.strip_prefix("/vsicurl/").unwrap_or(url);
        let url = if let Some(rest) = url.strip_prefix("s3://").or_else(|| url.strip_prefix("/vsis3/")) {
            let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
            if !(3..=63).contains(&bucket.len())
                || !bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
                || bucket.split('.').any(|label| {
                    label.is_empty()
                        || !label.as_bytes()[0].is_ascii_alphanumeric()
                        || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                })
            {
                return Err(Error::InvalidArgument("invalid S3 bucket name in remote listing URL".into()));
            }
            // Native `s3://` keys are raw (never URL-decoded): encoded here, the single decode in
            // `Api::for_url` gives the same key back.
            format!("https://{bucket}.s3.amazonaws.com/{}", uri_encode(key, false))
        } else if let Some(rest) = url.strip_prefix("gs://").or_else(|| url.strip_prefix("/vsigs/")) {
            format!("https://storage.googleapis.com/{}", rest.replace('?', "%3F"))
        } else if let Some(rest) = url.strip_prefix("az://").or_else(|| url.strip_prefix("/vsiaz/")) {
            let (account, path) = rest.split_once('/').unwrap_or((rest, ""));
            if !(3..=24).contains(&account.len()) || !account.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
                return Err(Error::InvalidArgument("invalid Azure account name in remote listing URL".into()));
            }
            format!("https://{account}.blob.core.windows.net/{path}")
        } else if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            return Err(Error::InvalidArgument(format!("'{}': not a remote URL", redact_url(url))));
        };
        let api = Api::for_url(&url);
        let max_depth = if recursive { depth.unwrap_or(usize::MAX) } else { 1 };
        let html_pending = match &api {
            Api::Html { url } => vec![(url.clone(), String::new(), 1)],
            _ => Vec::new(),
        };
        Ok(Self {
            api,
            recursive,
            max_depth,
            token: None,
            pages: 0,
            out: Vec::new(),
            index: EntryIndex::new(),
            object_pending: Vec::new(),
            object_relative: String::new(),
            object_level: 1,
            html_pending,
            html_current: None,
            done: false,
        })
    }

    /// The URL to fetch next (a `GET`), or `None` when the listing is complete.
    pub fn next_url(&mut self) -> Option<String> {
        if self.done {
            return None;
        }
        if let Api::Html { .. } = self.api {
            let (url, rel, level) = self.html_pending.pop()?;
            self.html_current = Some((rel, level));
            return Some(html_directory_url(&url));
        }
        self.api.request_url(self.token.as_deref(), self.recursive && self.max_depth == usize::MAX)
    }

    /// Reads the response to the last [`RemoteLister::next_url`]: its status and body.
    pub fn feed(&mut self, status: u16, body: &[u8]) -> Result<()> {
        if !(200..300).contains(&status) {
            self.done = true;
            return Err(Error::Http(format!("listing refused or missing (status {status})")));
        }
        self.pages += 1;
        if self.is_html() {
            if body.len() > Self::HTML_BODY_LIMIT {
                return Err(Error::InvalidData("HTTP directory index exceeds 4 MiB".into()));
            }
            self.check_html_prefix(body)?;
            if !String::from_utf8_lossy(body).to_ascii_lowercase().contains("<a ") {
                return Err(Error::Unsupported("not a listable HTML directory index".into()));
            }
        }
        if let Api::Html { url } = &self.api {
            let (rel, level) = self.html_current.take().unwrap_or_default();
            // The query belongs after the directory, so it is kept out of the walked path and
            // re-appended to each subdirectory URL.
            let (base, query) = url.split_once('?').map_or((url.as_str(), ""), |(b, q)| (b, q));
            let base = base.trim_end_matches('/');
            let query = if query.is_empty() { String::new() } else { format!("?{query}") };
            let page_url = if rel.is_empty() { base.to_string() } else { format!("{base}/{rel}") };
            let mut page = Vec::new();
            let html = String::from_utf8_lossy(body);
            page.extend(parse_html_index(&html, &url_path(&page_url)));
            for e in page {
                let name = if rel.is_empty() { e.name.clone() } else { format!("{rel}/{}", e.name) };
                if e.dir && level < self.max_depth {
                    self.html_pending.push((format!("{page_url}/{}{query}", e.name), name.clone(), level + 1));
                }
                self.out.push(ObjectEntry { name, ..e });
            }
            if self.pages >= MAX_PAGES && !self.html_pending.is_empty() {
                self.done = true;
                return Err(Error::InvalidData("remote listing exceeds the page budget".into()));
            }
            self.done = self.html_pending.is_empty();
            return Ok(());
        }
        if body.len() > Self::OBJECT_BODY_LIMIT {
            return Err(Error::InvalidData("object-store listing page exceeds 16 MiB".into()));
        }
        if !self.api.is_listing_page(body) {
            return Err(Error::InvalidData("not an XML object-store listing".into()));
        }
        if self.max_depth == usize::MAX {
            self.token = self.api.read_page(body, &mut self.out, &mut self.index);
        } else {
            let mut page = Vec::new();
            self.token = self.api.read_page(body, &mut page, &mut EntryIndex::new());
            for entry in page {
                let name = super::join_key(&self.object_relative, &entry.name);
                if self.index.contains_key(&(name.clone(), entry.dir)) {
                    continue;
                }
                if entry.dir && self.object_level < self.max_depth {
                    let child = self.api.child_prefix(&entry.name)?;
                    self.object_pending.push((child, name.clone(), self.object_level + 1));
                }
                self.index.insert((name.clone(), entry.dir), self.out.len());
                self.out.push(ObjectEntry { name, ..entry });
            }
        }
        if self.pages >= MAX_PAGES && (self.token.is_some() || !self.object_pending.is_empty()) {
            self.done = true;
            return Err(Error::InvalidData("remote listing exceeds the page budget".into()));
        }
        if self.token.is_none() {
            if let Some((api, relative, level)) = self.object_pending.pop() {
                self.api = api;
                self.object_relative = relative;
                self.object_level = level;
            } else {
                self.done = true;
            }
        }
        Ok(())
    }

    /// The entries, a directory before what it holds, siblings by name.
    pub fn finish(mut self) -> Vec<Entry> {
        if self.recursive && self.api.is_object_store() {
            add_implied_directories(&mut self.out);
        }
        let mut entries: Vec<Entry> =
            self.out.into_iter().map(ObjectEntry::into_entry).filter(|e| e.name.split('/').count() <= self.max_depth).collect();
        super::sort_entries(&mut entries);
        entries
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn url_prefixes_are_decoded_once_before_query_encoding() {
        for base in [
            "https://storage.googleapis.com/b",
            "https://acc.blob.core.windows.net/c",
            "https://s3.amazonaws.com/b",
            "https://b.s3.amazonaws.com",
        ] {
            let api = Api::for_url(&format!("{base}/a%20b/%252F"));
            let url = api.request_url(None, false).unwrap();
            assert!(url.contains("prefix=a%20b%2F%252F%2F"), "{url}");
            assert!(!url.contains("%2520"), "{url}");
        }
        assert_eq!(uri_decode("a%oops"), "a%oops");
        assert_eq!(uri_decode("a%FF"), "a%FF");
    }

    #[test]
    fn directory_markers_and_page_caps_are_not_silent() {
        let api = super::Api::for_url("https://bucket.s3.amazonaws.com/dir");
        assert!(api.has_directory_marker(b"<ListBucketResult><Contents><Key>dir/</Key><Size>0</Size></Contents></ListBucketResult>"));
        let mut lister = super::RemoteLister::new("s3://bucket/dir", true, None).unwrap();
        lister.pages = super::MAX_PAGES - 1;
        assert!(lister.next_url().is_some());
        assert!(lister.feed(200, b"<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>more</NextContinuationToken></ListBucketResult>").is_err());
    }

    #[test]
    fn question_mark_in_a_cloud_key_stays_in_the_prefix() {
        for url in ["s3://bucket/a?b", "gs://bucket/a?b"] {
            let lister = super::RemoteLister::new(url, false, None).unwrap();
            let request = lister.api.request_url(None, false).unwrap();
            assert!(request.contains("prefix=a%3Fb%2F"), "{url}: {request}");
        }
    }

    #[test]
    fn s3_keys_are_listed_raw_as_natively() {
        // `%20` is part of the key, as `vfs` keeps `s3://` keys: the prefix is `a%20b/`, not `a b/`.
        let lister = super::RemoteLister::new("s3://bucket/a%20b", false, None).unwrap();
        let request = lister.api.request_url(None, false).unwrap();
        assert!(request.contains("prefix=a%2520b%2F"), "{request}");
    }

    use super::*;

    #[test]
    fn repeated_keys_across_pages_merge_metadata() {
        let api = Api::for_url("https://storage.googleapis.com/bucket/p");
        let mut out = Vec::new();
        let mut index = EntryIndex::new();
        api.read_page(b"<ListBucketResult><CommonPrefixes><Prefix>p/a/</Prefix></CommonPrefixes></ListBucketResult>", &mut out, &mut index);
        api.read_page(b"<ListBucketResult><Contents><Key>p/a/</Key><Size>0</Size><LastModified>2024-01-01T00:00:00Z</LastModified></Contents></ListBucketResult>", &mut out, &mut index);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].size, Some(0));
        assert!(out[0].modified.is_some());
    }

    #[test]
    fn decodes_xml_entities_once() {
        assert_eq!(xml_unescape("it&apos;s &amp; &lt;b&gt; &quot;q&quot;"), "it's & <b> \"q\"");
        assert_eq!(xml_unescape("a&#39;b&#x27;c&#X41;&#x1F600;"), "a'b'cA\u{1F600}");
        // One level only: an escaped ampersand stays a reference.
        assert_eq!(xml_unescape("&amp;lt;&amp;#39;"), "&lt;&#39;");
        // Malformed or invalid references are kept as written.
        for s in ["&#;", "&#x;", "&#xD800;", "&#x110000;", "&#1a;", "&bogus; &", "&#99999999999999;"] {
            assert_eq!(xml_unescape(s), s);
        }
        // A key with a numeric reference addresses the object it names.
        let xml = "<ListBucketResult><Contents><Key>p/it&#39;s.tif</Key><Size>1</Size></Contents></ListBucketResult>";
        let mut out = Vec::new();
        parse_listing_page(xml, "p/", false, &mut out);
        assert_eq!(out[0].name, "it's.tif");
    }

    #[test]
    fn parses_s3_and_azure_pages() {
        let xml = "<ListBucketResult><Contents><Key>p/a.tif</Key><LastModified>2024-01-01T01:42:27.000Z</LastModified>\
                   <ETag>&quot;abc&quot;</ETag><Size>12</Size></Contents><Contents><Key>p/</Key><Size>0</Size></Contents>\
                   <CommonPrefixes><Prefix>p/sub/</Prefix></CommonPrefixes></ListBucketResult>";
        let mut out = Vec::new();
        parse_listing_page(xml, "p/", false, &mut out);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], ObjectEntry { name: "sub".into(), dir: true, size: None, modified: None, etag: None });
        assert_eq!(
            out[1],
            ObjectEntry { name: "a.tif".into(), dir: false, size: Some(12), modified: Some(1_704_073_347), etag: Some("abc".into()) }
        );
        let xml = "<EnumerationResults><Blobs><Blob><Name>c/x.nc</Name><Properties><Last-Modified>Mon, 01 Jan 2024 01:42:27 GMT</Last-Modified>\
                   <Etag>0x8D</Etag><Content-Length>7</Content-Length></Properties></Blob><BlobPrefix><Name>c/d/</Name></BlobPrefix></Blobs></EnumerationResults>";
        let mut out = Vec::new();
        parse_listing_page(xml, "c/", true, &mut out);
        assert_eq!(out[0].name, "d");
        assert_eq!(
            out[1],
            ObjectEntry { name: "x.nc".into(), dir: false, size: Some(7), modified: Some(1_704_073_347), etag: Some("0x8D".into()) }
        );
        let mut implied = vec![ObjectEntry { name: "a/b/c.txt".into(), dir: false, size: Some(1), modified: None, etag: None }];
        add_implied_directories(&mut implied);
        let names: Vec<_> = implied.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("a/b/c.txt", false), ("a", true), ("a/b", true)]);
    }

    #[test]
    fn parses_html_indexes() {
        let html = r#"<a href="..hidden.tif">hidden</a><a href=".">Self</a><a href="?C=N&amp;O=A">Name</a><a href="../">Parent</a><a href="byte%20x.tif" title="x">byte x.tif</a>
                      <a href='sub/'>sub/</a><a href="/data/gtiff/abs.tif">abs</a><a href="/elsewhere/n.tif">n</a>
                      <a href="https://other.org/">o</a><A HREF=plain.txt>p</A><a href="sub/deeper/">no</a>"#;
        let names: Vec<(String, bool)> = parse_html_index(html, "/data/gtiff").into_iter().map(|e| (e.name, e.dir)).collect();
        assert_eq!(
            names,
            [
                ("..hidden.tif".to_string(), false),
                ("byte%20x.tif".into(), false),
                ("sub".into(), true),
                ("abs.tif".into(), false),
                ("plain.txt".into(), false)
            ]
        );
    }

    #[test]
    fn plans_requests() {
        assert_eq!(
            Api::for_url("https://storage.googleapis.com/b/p/q"),
            Api::Url { base: "https://storage.googleapis.com/b".into(), prefix: "p/q/".into() }
        );
        assert_eq!(
            Api::for_url("https://acc.blob.core.windows.net/c/d?sv=1&sig=x"),
            Api::Azure { base: "https://acc.blob.core.windows.net/c".into(), prefix: "d/".into(), sas: "sv=1&sig=x".into() }
        );
        assert!(!Api::for_url("https://example.com/d").is_object_store());
        let api = Api::Url { base: "https://h/b".into(), prefix: "a b/".into() };
        assert_eq!(
            api.request_url(Some("t+1"), false).unwrap(),
            "https://h/b/?continuation-token=t%2B1&delimiter=%2F&list-type=2&prefix=a%20b%2F"
        );
        assert_eq!(html_directory_url("https://h/d?x=1"), "https://h/d/?x=1");
    }

    #[test]
    fn finite_depth_object_listing_uses_delimiters_and_stops_at_the_boundary() {
        for root in
            ["s3://bkt/dir", "https://storage.googleapis.com/bkt/dir", "https://account.blob.core.windows.net/container/dir?sig=example"]
        {
            let mut l = RemoteLister::new(root, true, Some(2)).unwrap();
            let azure = root.contains("blob.core");
            assert!(l.next_url().unwrap().contains("delimiter="));
            let page = if azure {
                b"<EnumerationResults><BlobPrefix><Name>dir/a b/</Name></BlobPrefix><NextMarker>next</NextMarker></EnumerationResults>"
                    .as_slice()
            } else {
                b"<ListBucketResult><CommonPrefixes><Prefix>dir/a b/</Prefix></CommonPrefixes><IsTruncated>true</IsTruncated><NextContinuationToken>next</NextContinuationToken></ListBucketResult>".as_slice()
            };
            l.feed(200, page).unwrap();
            let second = l.next_url().unwrap();
            assert!(second.contains("delimiter=") && second.contains("next"));
            l.feed(200, if azure { b"<EnumerationResults/>" } else { b"<ListBucketResult/>" }).unwrap();
            let child = l.next_url().unwrap();
            assert!(child.contains("prefix=dir%2Fa%20b%2F") && child.contains("delimiter="), "{child}");
            if azure {
                assert!(child.contains("sig=example"));
            }
            let child_page = if azure {
                b"<EnumerationResults><Blob><Name>dir/a b/a.txt</Name><Properties><Content-Length>1</Content-Length></Properties></Blob><BlobPrefix><Name>dir/a b/deeper/</Name></BlobPrefix></EnumerationResults>".as_slice()
            } else {
                b"<ListBucketResult><Contents><Key>dir/a b/a.txt</Key><Size>1</Size></Contents><CommonPrefixes><Prefix>dir/a b/deeper/</Prefix></CommonPrefixes></ListBucketResult>".as_slice()
            };
            l.feed(200, child_page).unwrap();
            assert_eq!(l.next_url(), None);
            let names: Vec<_> = l.finish().into_iter().map(|e| e.name).collect();
            assert_eq!(names, ["a b", "a b/a.txt", "a b/deeper"]);
        }
    }

    #[test]
    fn drives_a_paged_listing() {
        let mut l = RemoteLister::new("s3://bkt/dir", true, None).unwrap();
        let first = l.next_url().unwrap();
        assert_eq!(first, "https://bkt.s3.amazonaws.com/?list-type=2&prefix=dir%2F");
        l.feed(200, b"<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>dir/a/b.txt</Key><Size>1</Size></Contents><NextContinuationToken>n</NextContinuationToken></ListBucketResult>").unwrap();
        assert!(l.next_url().unwrap().contains("continuation-token=n"));
        l.feed(200, b"<ListBucketResult><Contents><Key>dir/c.txt</Key><Size>2</Size></Contents></ListBucketResult>").unwrap();
        assert_eq!(l.next_url(), None);
        let names: Vec<String> = l.finish().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["a", "a/b.txt", "c.txt"]);

        let mut l = RemoteLister::new("https://example.com/data", true, Some(2)).unwrap();
        assert_eq!(l.next_url().unwrap(), "https://example.com/data/");
        l.feed(200, br#"<title>Index of /</title><a href="x.tif">x</a><a href="sub/">sub</a>"#).unwrap();
        assert_eq!(l.next_url().unwrap(), "https://example.com/data/sub/");
        l.feed(200, br#"<title>Index of /sub</title><a href="y.tif">y</a><a href="deeper/">d</a>"#).unwrap();
        assert_eq!(l.next_url(), None);
        let names: Vec<String> = l.finish().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["sub", "sub/deeper", "sub/y.tif", "x.tif"]);
        assert!(RemoteLister::new("local/dir", false, None).is_err());
    }

    #[test]
    fn html_context_survives_long_anchors_and_repeated_non_anchor_attributes() {
        let html = format!("<a data-padding='{}' href='long.txt'>long</a>{}", "x".repeat(4096), " href='ignored'".repeat(100_000));
        let names: Vec<_> = parse_html_index(&html, "/").into_iter().map(|entry| entry.name).collect();
        assert_eq!(names, ["long.txt"]);
        let html = "<a href='one' href='two'>both</a><span href='ignored'></span><a href='three'>three</a>";
        let names: Vec<_> = parse_html_index(html, "/").into_iter().map(|entry| entry.name).collect();
        assert_eq!(names, ["one", "two", "three"]);
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:01:00 GMT"), Some(60));
        assert_eq!(parse_iso8601("garbage"), None);
        for value in ["2024-13-01T00:00:00Z", "2024-02-31T00:00:00Z", "2023-02-29T00:00:00Z", "2024-01-01T24:00:00Z"] {
            assert_eq!(parse_iso8601(value), None, "{value}");
        }
        assert!(parse_iso8601("2024-02-29T00:00:00Z").is_some());
        for value in [
            "Mon, 99999999999999 Jan 2024 01:00:00 GMT",
            "Mon, 01 Jan 9223372036854775807 01:00:00 GMT",
            "Mon, 01 Jan 2024 9223372036854775807:00:00 GMT",
            "Mon, 31 Feb 2024 01:00:00 GMT",
            "Mon, 29 Feb 2023 01:00:00 GMT",
            "Mon, 01 Jan 2024 24:00:00 GMT",
            "Mon, 01 Jan 2024 01:60:00 GMT",
        ] {
            assert_eq!(parse_http_date(value), None, "{value}");
        }
        assert!(parse_http_date("Thu, 29 Feb 2024 01:00:00 GMT").is_some());
    }
}

#[cfg(test)]
mod response_limit_tests {
    use super::RemoteLister;

    #[test]
    fn object_store_rejects_wrong_document_roots() {
        for url in ["s3://bucket/dir", "az://account/container/dir"] {
            let mut lister = RemoteLister::new(url, true, None).unwrap();
            lister.next_url().unwrap();
            assert!(lister.feed(200, b"<html><body>Sign in</body></html>").is_err());
        }
        let api = super::Api::for_url("https://storage.googleapis.com/bucket/dir");
        assert!(api.is_listing_page(b"\xEF\xBB\xBF<?xml version=\"1.0\"?><!--ok--><s3:ListBucketResult xmlns:s3=\"test\"/>"));
        assert!(!api.is_listing_page(b"<Error><Message>Denied</Message></Error>"));
    }

    #[test]
    fn cloud_authority_components_cannot_change_the_request_host() {
        for scheme in ["s3", "az"] {
            for authority in ["evil.example#", "evil?", "user@host", "host:443", "has space", "UPPER", "-leading", "trailing-"] {
                assert!(RemoteLister::new(&format!("{scheme}://{authority}/dir"), true, None).is_err());
            }
        }
        assert!(RemoteLister::new("s3://valid.bucket/dir", false, None).is_ok());
        assert!(RemoteLister::new("az://validaccount/container/dir", false, None).is_ok());
    }

    #[test]
    fn object_store_rejects_non_xml_and_oversized_pages() {
        for body in [b"not XML".to_vec(), vec![b' '; RemoteLister::OBJECT_BODY_LIMIT + 1]] {
            let mut lister = RemoteLister::new("s3://bucket/dir", true, None).unwrap();
            lister.next_url().unwrap();
            assert!(lister.feed(200, &body).is_err());
        }
    }
}
