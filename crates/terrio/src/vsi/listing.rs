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
            Api::Url { base: format!("{scheme}://{host}/{bucket}"), prefix: with_slash(prefix) }
        } else if lower.ends_with(".blob.core.windows.net") {
            let (container, prefix) = key.split_once('/').unwrap_or((key, ""));
            Api::Azure { base: format!("{scheme}://{host}/{container}"), prefix: with_slash(prefix), sas: query.to_string() }
        } else if lower.ends_with(".amazonaws.com") && (lower.contains(".s3") || lower.starts_with("s3.") || lower.starts_with("s3-")) {
            // Virtual-hosted S3 URLs; path-style ones (`s3.amazonaws.com/bucket/...`) too.
            if lower.starts_with("s3.") || lower.starts_with("s3-") {
                let (bucket, prefix) = key.split_once('/').unwrap_or((key, ""));
                Api::Url { base: format!("{scheme}://{host}/{bucket}"), prefix: with_slash(prefix) }
            } else {
                Api::Url { base: format!("{scheme}://{host}"), prefix: with_slash(key) }
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

/// XML text with its predefined entities replaced.
pub(crate) fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
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
    while let Some(i) = lower[at..].find("href=") {
        let start = at + i + 5;
        let tag_start = lower[..at + i].rfind('<');
        let anchor = tag_start.is_some_and(|tag| {
            let tag = &lower[tag + 1..at + i];
            tag.starts_with('a') && tag.as_bytes().get(1).is_some_and(u8::is_ascii_whitespace) && !tag.contains('>')
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
        if href.is_empty() || href.starts_with("..") || href.contains("://") || href.starts_with("mailto:") {
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
    Some(super::civil_seconds(num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?))
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
    Some(super::civil_seconds(y, m, d, h, mi, sec))
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
    /// HTML index pages still to read: `(directory URL, path relative to the root, level)`.
    html_pending: Vec<(String, String, usize)>,
    /// The page being read: `(relative path, level)`.
    html_current: Option<(String, usize)>,
    done: bool,
}

impl RemoteLister {
    /// A listing of `url` (`https://...`, `gs://bucket/dir`, `az://account/container/dir`, or
    /// `s3://bucket/dir` as its unsigned virtual-hosted URL), recursive or not, to `depth` levels.
    pub fn new(url: &str, recursive: bool, depth: Option<usize>) -> Result<Self> {
        let url = url.trim();
        let url = url.strip_prefix("/vsicurl/").unwrap_or(url);
        let url = if let Some(rest) = url.strip_prefix("s3://").or_else(|| url.strip_prefix("/vsis3/")) {
            let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
            format!("https://{bucket}.s3.amazonaws.com/{key}")
        } else if let Some(rest) = url.strip_prefix("gs://").or_else(|| url.strip_prefix("/vsigs/")) {
            format!("https://storage.googleapis.com/{rest}")
        } else if let Some(rest) = url.strip_prefix("az://").or_else(|| url.strip_prefix("/vsiaz/")) {
            let (account, path) = rest.split_once('/').unwrap_or((rest, ""));
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
        self.api.request_url(self.token.as_deref(), self.recursive && self.max_depth > 1)
    }

    /// Reads the response to the last [`RemoteLister::next_url`]: its status and body.
    pub fn feed(&mut self, status: u16, body: &[u8]) -> Result<()> {
        if !(200..300).contains(&status) {
            self.done = true;
            return Err(Error::Http(format!("listing refused or missing (status {status})")));
        }
        self.pages += 1;
        if let Api::Html { url } = &self.api {
            let (rel, level) = self.html_current.take().unwrap_or_default();
            let page_url = if rel.is_empty() { url.clone() } else { format!("{}/{rel}", url.trim_end_matches('/')) };
            let mut page = Vec::new();
            let html = String::from_utf8_lossy(body);
            page.extend(parse_html_index(&html, &url_path(&page_url)));
            for e in page {
                let name = if rel.is_empty() { e.name.clone() } else { format!("{rel}/{}", e.name) };
                if e.dir && level < self.max_depth {
                    self.html_pending.push((format!("{}/{}", page_url.trim_end_matches('/'), e.name), name.clone(), level + 1));
                }
                self.out.push(ObjectEntry { name, ..e });
            }
            self.done = self.html_pending.is_empty() || self.pages >= MAX_PAGES;
            return Ok(());
        }
        self.token = self.api.read_page(body, &mut self.out, &mut self.index);
        self.done = self.token.is_none() || self.pages >= MAX_PAGES;
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
        let html = r#"<a href="?C=N&amp;O=A">Name</a><a href="../">Parent</a><a href="byte%20x.tif" title="x">byte x.tif</a>
                      <a href='sub/'>sub/</a><a href="/data/gtiff/abs.tif">abs</a><a href="/elsewhere/n.tif">n</a>
                      <a href="https://other.org/">o</a><A HREF=plain.txt>p</A><a href="sub/deeper/">no</a>"#;
        let names: Vec<(String, bool)> = parse_html_index(html, "/data/gtiff").into_iter().map(|e| (e.name, e.dir)).collect();
        assert_eq!(
            names,
            [("byte%20x.tif".to_string(), false), ("sub".into(), true), ("abs.tif".into(), false), ("plain.txt".into(), false)]
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
        l.feed(200, br#"<a href="x.tif">x</a><a href="sub/">sub</a>"#).unwrap();
        assert_eq!(l.next_url().unwrap(), "https://example.com/data/sub/");
        l.feed(200, br#"<a href="y.tif">y</a><a href="deeper/">d</a>"#).unwrap();
        assert_eq!(l.next_url(), None);
        let names: Vec<String> = l.finish().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["sub", "sub/deeper", "sub/y.tif", "x.tif"]);
        assert!(RemoteLister::new("local/dir", false, None).is_err());
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:01:00 GMT"), Some(60));
        assert_eq!(parse_iso8601("garbage"), None);
    }
}
