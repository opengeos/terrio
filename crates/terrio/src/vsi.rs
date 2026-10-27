//! File management across local, in-memory, remote and archive locations, as GDAL's
//! `gdal vsi list`, `copy`, `delete`, `move` and `sync` do on its virtual file systems.
//!
//! Paths are written as for the readers (see [`crate::vfs`]):
//!
//! | Location | Forms | List | Read | Write, delete |
//! |---|---|---|---|---|
//! | Local files | `dir/file.tif` | yes | yes | yes |
//! | In memory | `/vsimem/dir/file.tif` | yes | yes | yes |
//! | Amazon S3 and S3-compatible services | `s3://bucket/key`, `/vsis3/bucket/key` | yes | yes | yes (signed) |
//! | Google Cloud Storage | `gs://bucket/key`, `/vsigs/bucket/key` | yes (public buckets) | yes | no |
//! | Azure Blob Storage | `az://container/blob`, `/vsiaz/...` | yes | yes | yes, with a SAS token |
//! | HTTP(S) | `https://...`, `/vsicurl/https://...` | from an HTML directory index | yes | no |
//! | Zip archives | `/vsizip/archive.zip/inner/path` | yes (remote ones by range requests) | yes | no |
//! | Tar archives | `/vsitar/archive.tar/inner`, `.tar.gz`, `.tgz` | yes | yes | no |
//!
//! Remote locations need the `remote` feature and a native build; tar archives a native build.
//! Archives are read only.
//!
//! Deleting is guarded: an empty path, a file system root, the home directory and its parents, the current
//! directory or one of its parents, the root of `/vsimem/`, a whole bucket or container, and
//! archive or HTTP locations are refused. Recursive deletion never follows symbolic links (it
//! deletes the links), and deletes on object stores only the keys below `prefix/` (never those of
//! a sibling such as `prefix2/`).
//! Recursive copies refuse destination symlinks that escape the destination tree, for both
//! archive and ordinary sources. Same-device directory renames do not walk the tree and
//! report zero copied files/bytes, since no file payloads were copied.

pub(crate) mod listing;
mod md5;
pub(crate) mod zipdir;

pub use listing::RemoteLister;

use crate::{Error, Result};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// Whether an entry is a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// A file (an object, an archive member).
    File,
    /// A directory (a common prefix of object keys).
    Directory,
}

/// An entry of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The path relative to the listed directory (`/`-separated), or the absolute path when the
    /// listing asks for absolute paths; the path as given for a listed file.
    pub name: String,
    /// File or directory.
    #[serde(rename = "type")]
    pub kind: EntryKind,
    /// The size in bytes, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The last modification time, in seconds since the Unix epoch, when known.
    #[serde(skip)]
    pub modified: Option<i64>,
    /// The permissions, as `ls -l` prints them (`-rw-r--r--`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    /// The entity tag of an object, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl Entry {
    /// Whether the entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }

    /// The modification time as GDAL prints it in JSON (`2024-01-01 01:42:27Z`).
    pub fn modified_string(&self) -> Option<String> {
        self.modified.map(format_time)
    }

    /// The `ls -l`-style line of `gdal vsi list -l`.
    pub fn long_line(&self) -> String {
        let perms = self.permissions.clone().unwrap_or_else(|| if self.is_dir() { "d---------".into() } else { "----------".into() });
        // A date that is not known prints as a placeholder, not the epoch.
        let date = match self.modified {
            Some(secs) => format_time(secs)[..16].to_string(),
            None => format!("{:<16}", "-"),
        };
        format!("{perms} 1 unknown unknown {:>12} {} {}", self.size.unwrap_or(0), date, self.name)
    }
}

/// Formats seconds since the Unix epoch as `YYYY-MM-DD HH:MM:SSZ`.
fn format_time(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Options of [`list`].
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    /// List subdirectories too.
    pub recursive: bool,
    /// With `recursive`, the deepest level listed (1: the directory's own entries).
    pub depth: Option<usize>,
    /// Name entries by their absolute paths (local paths made absolute; other locations as given,
    /// joined with the entry's name).
    pub absolute: bool,
    /// Fetch the sizes and dates that a listing does not give: those of the files of a plain
    /// HTTP server's index page, one request each (as `gdal vsi list -l` does).
    pub details: bool,
}

/// Options of [`copy`].
#[derive(Debug, Clone, Default)]
pub struct CopyOptions {
    /// Copy directories, with their subdirectories.
    pub recursive: bool,
    /// Go on after a file fails to copy (reported in [`Report::errors`]) rather than stop.
    pub skip_errors: bool,
}

/// How [`sync`] decides that a target file is up to date.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncStrategy {
    /// Copy when the target is missing, its size differs, or the source is newer (GDAL's default).
    #[default]
    Timestamp,
    /// Copy when the target is missing or the entity tags (or a local file's MD5 against an S3
    /// object's entity tag) differ; where no entity tag can be compared, as `Timestamp`.
    ETag,
    /// Always copy.
    Overwrite,
}

impl std::str::FromStr for SyncStrategy {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "timestamp" => Ok(Self::Timestamp),
            "etag" => Ok(Self::ETag),
            "overwrite" => Ok(Self::Overwrite),
            _ => Err(Error::InvalidArgument(format!("unknown sync strategy '{s}' (timestamp, ETag or overwrite)"))),
        }
    }
}

/// Options of [`sync`].
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Synchronize subdirectories too.
    pub recursive: bool,
    /// How up-to-date targets are recognized.
    pub strategy: SyncStrategy,
}

/// What an operation did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Files copied (or moved).
    pub copied: u64,
    /// Bytes copied.
    pub bytes: u64,
    /// Files left alone by [`sync`] because the target was up to date.
    pub unchanged: u64,
    /// Files and directories deleted.
    pub deleted: u64,
    /// Files that failed, with the reason, when errors were skipped.
    pub errors: Vec<(String, String)>,
}

/// The kind of an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
}

/// A location, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Local(PathBuf),
    /// A `/vsimem/` path, without a trailing slash.
    Mem(String),
    /// An object (or "directory") of S3.
    S3 {
        bucket: String,
        key: String,
    },
    /// An HTTP(S) URL: a GCS or Azure object, or a plain server's file. `base` has no query (nor
    /// trailing slash); `query` is the query without `?` (an Azure SAS token), or empty.
    Url {
        base: String,
        query: String,
    },
    /// A member (or directory) of an archive; `inner` is empty for the archive's root.
    Archive {
        archive: Box<Node>,
        kind: ArchiveKind,
        inner: String,
    },
}

/// Archive extensions recognized after `/vsizip/` and `/vsitar/`.
const ZIP_EXTENSIONS: &[&str] = &[".zip", ".kmz", ".shz"];
const TAR_EXTENSIONS: &[&str] = &[".tar.gz", ".tgz", ".tar"];

/// The byte offset just past the archive name in `s` (followed by `/` or the end).
fn archive_end(s: &str, extensions: &[&str]) -> Option<(usize, usize)> {
    let lower = s.to_ascii_lowercase();
    let mut best: Option<(usize, usize)> = None;
    for (n, ext) in extensions.iter().enumerate() {
        let mut from = 0;
        while let Some(i) = lower[from..].find(ext) {
            let end = from + i + ext.len();
            if end == s.len() || s[end..].starts_with('/') {
                if best.is_none_or(|(b, _)| end < b) {
                    best = Some((end, n));
                }
                break;
            }
            from = end;
        }
    }
    best
}

impl Node {
    fn parse(input: &str) -> Result<Node> {
        let original = input;
        let input = input.trim();
        let shown = listing::redact_url(input);
        if input.is_empty() {
            return Err(Error::InvalidArgument("empty path".into()));
        }
        for (prefix, exts) in [("/vsizip/", ZIP_EXTENSIONS), ("/vsitar/", TAR_EXTENSIONS)] {
            let Some(rest) = input.strip_prefix(prefix) else { continue };
            // Braces delimit the archive URL, including its query, from the member path.
            let (archive_path, inner, n) = if let Some(braced) = rest.strip_prefix('{') {
                let (archive, suffix) =
                    braced.split_once('}').ok_or_else(|| Error::InvalidArgument(format!("'{shown}': unclosed archive braces")))?;
                if !suffix.is_empty() && !suffix.starts_with('/') {
                    return Err(Error::InvalidArgument(format!("'{shown}': expected a member path after archive braces")));
                }
                let path = archive.split_once('?').map_or(archive, |(path, _)| path);
                let (_, n) = archive_end(path, exts)
                    .filter(|(end, _)| *end == path.len())
                    .ok_or_else(|| Error::InvalidArgument(format!("'{shown}': invalid archive name")))?;
                (archive, suffix.trim_matches('/'), n)
            } else {
                let path = rest.split_once('?').map_or(rest, |(path, _)| path);
                let (end, n) = archive_end(path, exts)
                    .ok_or_else(|| Error::InvalidArgument(format!("'{shown}': no archive name ({}) after {prefix}", exts.join(", "))))?;
                if end == path.len() { (rest, "", n) } else { (&rest[..end], rest[end..].trim_matches('/'), n) }
            };
            let kind = match (prefix, exts[n]) {
                ("/vsizip/", _) => ArchiveKind::Zip,
                (_, ".tar") => ArchiveKind::Tar,
                _ => ArchiveKind::TarGz,
            };
            let archive = Node::parse(archive_path)?;
            let inner = inner.to_string();
            return Ok(Node::Archive { archive: Box::new(archive), kind, inner });
        }
        if input == "/vsimem" || input.starts_with("/vsimem/") {
            // Not resolved: `/vsimem` has no parent to pop to, and a literal `..` key is unreachable.
            // `memfs::key` treats `\` as `/`, so both separators are refused here.
            if input.split(['/', '\\']).any(|p| p == "..") {
                return Err(Error::InvalidArgument(format!("'{shown}': refusing dot segments in a /vsimem path")));
            }
            let key = normalize_mem(input);
            if crate::memfs::is_reserved(Path::new(&key)) {
                return Err(Error::InvalidArgument(format!("'{shown}': reserved for terrio's internal in-memory mounts")));
            }
            return Ok(Node::Mem(key));
        }
        if input.starts_with("/vsi") && !["/vsicurl/", "/vsis3/", "/vsigs/", "/vsiaz/"].iter().any(|p| input.starts_with(p)) {
            return Err(Error::Unsupported(format!("'{shown}': this virtual file system is not supported")));
        }
        Ok(match crate::vfs::Location::parse_unarchived(input) {
            // Trimming only classifies the input; a local path keeps its exact bytes.
            crate::vfs::Location::Local(_) => Node::Local(Path::new(original).components().collect()),
            crate::vfs::Location::S3 { bucket, key } => {
                if bucket.is_empty() {
                    return Err(Error::InvalidArgument(format!("'{shown}': no bucket")));
                }
                Node::S3 { bucket, key: key.trim_end_matches('/').to_string() }
            }
            crate::vfs::Location::Url(u) => {
                let (base, query) = u.split_once('?').map_or((u.as_str(), ""), |(b, q)| (b, q));
                Node::Url { base: base.trim_end_matches('/').to_string(), query: query.to_string() }
            }
            crate::vfs::Location::Zip { .. } => unreachable!("parse_unarchived returns no archive"),
        })
    }

    /// The node of a child.
    fn join(&self, name: &str) -> Node {
        match self {
            Node::Local(p) => Node::Local(name.split('/').fold(p.clone(), |p, c| p.join(c))),
            Node::Mem(k) => Node::Mem(format!("{k}/{name}")),
            Node::S3 { bucket, key } => Node::S3 { bucket: bucket.clone(), key: join_key(key, name) },
            Node::Url { base, query } => {
                let encoded;
                let name = if listing::Api::for_url(base).is_object_store() {
                    encoded = listing::uri_encode(name, false);
                    &encoded
                } else {
                    name
                };
                Node::Url { base: format!("{base}/{name}"), query: query.clone() }
            }
            Node::Archive { archive, kind, inner } => Node::Archive { archive: archive.clone(), kind: *kind, inner: join_key(inner, name) },
        }
    }

    /// The last component of the path.
    fn file_name(&self) -> String {
        let s = match self {
            Node::Local(p) => return p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string(),
            Node::Mem(k) => k.as_str(),
            Node::S3 { bucket, key } => {
                if key.is_empty() {
                    bucket.as_str()
                } else {
                    key.as_str()
                }
            }
            Node::Url { base, .. } => base.as_str(),
            Node::Archive { archive, inner, .. } => {
                if inner.is_empty() {
                    return archive.file_name();
                }
                inner.as_str()
            }
        };
        s.rsplit('/').next().unwrap_or_default().to_string()
    }

    /// Whether the node is in an archive or on a plain HTTP server (read only).
    fn read_only_reason(&self) -> Option<String> {
        match self {
            Node::Archive { .. } => Some("archives are read only".into()),
            #[cfg(remote)]
            Node::Url { base, query } => crate::http::store::writable(&url_remote(base, query)).err().map(|e| e.to_string()),
            #[cfg(not(remote))]
            Node::Url { .. } | Node::S3 { .. } => Some(remote_disabled_message()),
            _ => None,
        }
    }

    #[cfg(remote)]
    fn remote(&self) -> Option<crate::http::Remote> {
        match self {
            Node::S3 { bucket, key } => Some(crate::http::Remote::S3(crate::s3::S3Object { bucket: bucket.clone(), key: key.clone() })),
            Node::Url { base, query } => Some(url_remote(base, query)),
            _ => None,
        }
    }

    /// The node as an object store "directory".
    #[cfg(remote)]
    fn directory(&self) -> Option<crate::http::Directory> {
        match self {
            Node::S3 { bucket, key } => Some(crate::http::Directory::S3 { bucket: bucket.clone(), prefix: key.clone() }),
            Node::Url { base, query } => {
                Some(crate::http::Directory::Url(if query.is_empty() { base.clone() } else { format!("{base}?{query}") }))
            }
            _ => None,
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self, Node::S3 { .. } | Node::Url { .. })
    }
}

#[cfg(not(remote))]
fn remote_disabled_message() -> String {
    if cfg!(target_family = "wasm") {
        "remote locations are not supported in WebAssembly builds".into()
    } else {
        "built without the `remote` feature".into()
    }
}

/// An HTTP(S) location with its query.
#[cfg(remote)]
fn url_remote(base: &str, query: &str) -> crate::http::Remote {
    crate::http::Remote::Url(if query.is_empty() { base.to_string() } else { format!("{base}?{query}") })
}

fn join_key(key: &str, name: &str) -> String {
    if key.is_empty() { name.to_string() } else { format!("{key}/{name}") }
}

/// A `/vsimem/` path without `.` components, doubled or trailing slashes.
fn normalize_mem(input: &str) -> String {
    let parts: Vec<&str> = input.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    format!("/{}", parts.join("/"))
}

/// What is at a node.
#[derive(Debug, Clone)]
struct Stat {
    dir: bool,
    size: Option<u64>,
    modified: Option<i64>,
    permissions: Option<String>,
    etag: Option<String>,
}

impl Stat {
    fn file(size: u64) -> Self {
        Stat { dir: false, size: Some(size), modified: None, permissions: None, etag: None }
    }

    fn directory() -> Self {
        Stat { dir: true, size: None, modified: None, permissions: None, etag: None }
    }

    fn entry(self, name: String) -> Entry {
        Entry {
            name,
            kind: if self.dir { EntryKind::Directory } else { EntryKind::File },
            size: self.size,
            modified: self.modified,
            permissions: self.permissions,
            etag: self.etag,
        }
    }
}

fn local_stat(meta: &std::fs::Metadata) -> Stat {
    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
    Stat { dir: meta.is_dir(), size: Some(meta.len()), modified, permissions: Some(permissions(meta)), etag: None }
}

#[cfg(unix)]
fn permissions(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    let kind = if meta.is_dir() {
        'd'
    } else if meta.file_type().is_symlink() {
        'l'
    } else {
        '-'
    };
    let mut s = String::from(kind);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[cfg(not(unix))]
fn permissions(meta: &std::fs::Metadata) -> String {
    let w = if meta.permissions().readonly() { '-' } else { 'w' };
    if meta.is_dir() { format!("dr{w}xr-xr-x") } else { format!("-r{w}-r--r--") }
}

fn not_found(what: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("'{}' does not exist or cannot be accessed", listing::redact_url(what)),
    ))
}

/// What is at a node, or `None` when nothing is.
fn stat_node(node: &Node) -> Result<Option<Stat>> {
    match node {
        Node::Local(p) => Ok(std::fs::metadata(p).ok().map(|m| local_stat(&m))),
        Node::Mem(k) => {
            let p = Path::new(k);
            if let Some(size) = crate::memfs::file_size(p) {
                Ok(Some(Stat::file(size)))
            } else if k == "/vsimem" || crate::memfs::is_dir(p) {
                Ok(Some(Stat::directory()))
            } else {
                Ok(None)
            }
        }
        #[cfg(remote)]
        Node::S3 { key, .. } if key.is_empty() => Ok(Some(Stat::directory())),
        #[cfg(remote)]
        Node::S3 { .. } | Node::Url { .. } => remote_stat(node),
        #[cfg(not(remote))]
        Node::S3 { .. } | Node::Url { .. } => Err(Error::Unsupported(remote_disabled_message())),
        Node::Archive { .. } => {
            let (archive, kind, inner) = archive_parts(node);
            if inner.is_empty() {
                // The archive's root: a directory when the archive exists.
                return Ok(stat_node(archive)?.map(|_| Stat::directory()));
            }
            #[cfg(not(target_family = "wasm"))]
            if matches!(kind, ArchiveKind::Tar | ArchiveKind::TarGz) {
                return tar_stat(archive, kind, inner);
            }
            let entries = archive_entries(archive, kind)?;
            if let Some(e) = entries.iter().find(|e| e.name == inner) {
                return Ok(Some(Stat { dir: e.is_dir(), size: e.size, modified: e.modified, permissions: None, etag: None }));
            }
            let prefix = format!("{inner}/");
            Ok(entries.iter().any(|e| e.name.starts_with(&prefix)).then(Stat::directory))
        }
    }
}

fn archive_parts(node: &Node) -> (&Node, ArchiveKind, &str) {
    match node {
        Node::Archive { archive, kind, inner } => (archive, *kind, inner),
        _ => unreachable!("not an archive member"),
    }
}

#[cfg(remote)]
thread_local! {
    /// The last HTML index page listed, so that a listing's check that a URL is a directory and
    /// the listing itself fetch it once.
    static HTML_PAGE: std::cell::RefCell<Option<(String, Option<Vec<crate::http::store::ObjectEntry>>)>> = const { std::cell::RefCell::new(None) };
}

/// [`crate::http::store::list_html`], remembering the last page.
#[cfg(remote)]
fn list_html_cached(url: &str) -> Result<Option<Vec<crate::http::store::ObjectEntry>>> {
    if let Some(hit) = HTML_PAGE.with(|c| c.borrow().as_ref().filter(|(u, _)| u == url).map(|(_, l)| l.clone())) {
        return Ok(hit);
    }
    let listed = crate::http::store::list_html(url)?;
    HTML_PAGE.with(|c| *c.borrow_mut() = Some((url.to_string(), listed.clone())));
    Ok(listed)
}

#[cfg(remote)]
fn forget_html_page() {
    HTML_PAGE.with(|c| *c.borrow_mut() = None);
}

#[cfg(not(remote))]
fn forget_html_page() {}

#[cfg(remote)]
fn remote_stat(node: &Node) -> Result<Option<Stat>> {
    use crate::http::store;
    let remote = node.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?;
    let dir = node.directory().ok_or_else(|| Error::InvalidArgument("not remote".into()))?;
    let object_store = store::is_object_store(&dir);
    if object_store {
        if let Node::Url { base, .. } = node
            && listing::Api::for_url(base).is_root()
        {
            return Ok(Some(Stat::directory()));
        }
        if let Some(s) = store::stat(&remote)? {
            return Ok(Some(Stat { dir: false, size: s.size, modified: s.modified, permissions: None, etag: s.etag }));
        }
        if let Node::S3 { bucket, key } = node {
            let marker =
                crate::http::Remote::S3(crate::s3::S3Object { bucket: bucket.clone(), key: format!("{}/", key.trim_end_matches('/')) });
            if store::stat(&marker)?.is_some() {
                return Ok(Some(Stat::directory()));
            }
        }
        // A "directory": a prefix with keys below it.
        return Ok(store::has_entries(&dir)?.filter(|&any| any).map(|_| Stat::directory()));
    }
    // A plain HTTP server: a directory when its index page lists entries.
    if let Node::Url { base, query } = node
        && let Some(l) = list_html_cached(&if query.is_empty() { base.clone() } else { format!("{base}?{query}") })?
        && !l.is_empty()
    {
        return Ok(Some(Stat::directory()));
    }
    Ok(store::stat(&remote)?.map(|s| Stat { dir: false, size: s.size, modified: s.modified, permissions: None, etag: s.etag }))
}

/// The entries below a directory node (`recursive`: at any depth), named relative to it.
fn read_dir(node: &Node, recursive: bool, depth: Option<usize>) -> Result<Vec<Entry>> {
    let max_depth = if recursive { depth.unwrap_or(usize::MAX) } else { 1 };
    let mut out = Vec::new();
    match node {
        Node::Local(p) => walk_local(p, "", 1, max_depth, &mut out)?,
        Node::Mem(k) => {
            let files = crate::memfs::list_files(Path::new(k)).unwrap_or_default();
            let mut dirs: Vec<String> = Vec::new();
            for (name, size) in files {
                let mut at = 0;
                while let Some(i) = name[at..].find('/') {
                    dirs.push(name[..at + i].to_string());
                    at += i + 1;
                }
                out.push(Stat::file(size).entry(name));
            }
            dirs.sort();
            dirs.dedup();
            out.extend(dirs.into_iter().map(|d| Stat::directory().entry(d)));
        }
        #[cfg(remote)]
        Node::S3 { .. } | Node::Url { .. } => out = remote_read_dir(node, max_depth)?,
        #[cfg(not(remote))]
        Node::S3 { .. } | Node::Url { .. } => return Err(Error::Unsupported(remote_disabled_message())),
        Node::Archive { .. } => {
            let (archive, kind, inner) = archive_parts(node);
            let prefix = if inner.is_empty() { String::new() } else { format!("{inner}/") };
            for e in archive_entries(archive, kind)? {
                if let Some(rel) = e.name.strip_prefix(&prefix)
                    && !rel.is_empty()
                {
                    out.push(Entry { name: rel.to_string(), ..e });
                }
            }
        }
    }
    out.retain(|e| e.name.split('/').count() <= max_depth);
    sort_entries(&mut out);
    Ok(out)
}

/// Sorts entries in pre-order: a directory, then what it holds, siblings by name.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| a.name.split('/').cmp(b.name.split('/')));
}

fn walk_local(dir: &Path, rel: &str, level: usize, max_depth: usize, out: &mut Vec<Entry>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::InvalidArgument("local entry name is not UTF-8; refusing to skip it".into()))?;
        let path = entry.path();
        // Symbolic links are listed with what they point to, but not descended into.
        let link = entry.file_type().is_ok_and(|t| t.is_symlink());
        let meta = std::fs::metadata(&path).or_else(|_| std::fs::symlink_metadata(&path))?;
        let child = join_key(rel, &name);
        let stat = local_stat(&meta);
        let descend = stat.dir && !link && level < max_depth;
        out.push(stat.entry(child.clone()));
        if descend {
            walk_local(&path, &child, level + 1, max_depth, out)?;
        }
    }
    Ok(())
}

#[cfg(remote)]
fn remote_read_dir(node: &Node, max_depth: usize) -> Result<Vec<Entry>> {
    use crate::http::store;
    let dir = node.directory().ok_or_else(|| Error::InvalidArgument("not remote".into()))?;
    let to_entry = |e: store::ObjectEntry| Entry {
        name: e.name,
        kind: if e.dir { EntryKind::Directory } else { EntryKind::File },
        size: e.size,
        modified: e.modified,
        permissions: None,
        etag: e.etag,
    };
    if store::is_object_store(&dir) {
        return remote_read_dir_api(store::listing_api(&dir), max_depth);
    }
    // A plain HTTP server: one index page per directory.
    let Node::Url { base, query } = node else { unreachable!("S3 is an object store") };
    let mut out = Vec::new();
    let mut pending: Vec<(String, String, usize)> = vec![(base.clone(), String::new(), 1)];
    let mut pages = 0;
    while let Some((url, rel, level)) = pending.pop() {
        if pages == listing::MAX_PAGES {
            return Err(Error::InvalidData(format!("HTTP directory listing exceeds {} pages", listing::MAX_PAGES)));
        }
        pages += 1;
        let page = if query.is_empty() { url.clone() } else { format!("{url}?{query}") };
        let listed = list_html_cached(&page)?
            .ok_or_else(|| Error::Unsupported(format!("'{}': not a listable directory", listing::redact_url(&page))))?;
        for e in listed {
            let name = join_key(&rel, &e.name);
            if e.dir && level < max_depth {
                pending.push((format!("{url}/{}", e.name), name.clone(), level + 1));
            }
            out.push(to_entry(store::ObjectEntry { name, ..e }));
        }
    }
    Ok(out)
}

/// Native finite-depth traversal retains raw object-store prefixes between pages.
#[cfg(remote)]
fn remote_read_dir_api(api: listing::Api, max_depth: usize) -> Result<Vec<Entry>> {
    let list_level = |api: &listing::Api, recursive| -> Result<Vec<listing::ObjectEntry>> {
        crate::http::store::list_objects(api, recursive)?
            .ok_or_else(|| Error::Unsupported("the store refused to list this directory".into()))
    };
    if max_depth == usize::MAX {
        return Ok(list_level(&api, true)?.into_iter().map(listing::ObjectEntry::into_entry).collect());
    }
    let mut out = Vec::new();
    let mut pending = vec![(api, String::new(), 1usize)];
    while let Some((api, relative, level)) = pending.pop() {
        for entry in list_level(&api, false)? {
            let name = join_key(&relative, &entry.name);
            if entry.dir && level < max_depth {
                pending.push((api.child_prefix(&entry.name)?, name.clone(), level + 1));
            }
            out.push(listing::ObjectEntry { name, ..entry }.into_entry());
        }
    }
    Ok(out)
}

fn display_node(node: &Node) -> String {
    match node {
        Node::Local(p) => p.display().to_string(),
        Node::Mem(k) => k.clone(),
        Node::S3 { bucket, key } => format!("s3://{bucket}/{key}"),
        Node::Url { base, .. } => base.clone(),
        Node::Archive { archive, kind, inner } => {
            let prefix = if *kind == ArchiveKind::Zip { "/vsizip/" } else { "/vsitar/" };
            format!("{prefix}{}/{inner}", display_node(archive))
        }
    }
}

// --- Archives ---------------------------------------------------------------------------------

/// The entries of an archive, with the directories its member paths imply.
fn archive_entries(archive: &Node, kind: ArchiveKind) -> Result<Vec<Entry>> {
    let mut out = match kind {
        ArchiveKind::Zip => zip_entries(archive)?,
        #[cfg(not(target_family = "wasm"))]
        ArchiveKind::Tar | ArchiveKind::TarGz => tar_entries(archive, kind)?,
        #[cfg(target_family = "wasm")]
        ArchiveKind::Tar | ArchiveKind::TarGz => {
            return Err(Error::Unsupported("tar archives are not supported in WebAssembly builds".into()));
        }
    };
    let mut dirs: Vec<String> = Vec::new();
    for e in &out {
        let mut at = 0;
        while let Some(i) = e.name[at..].find('/') {
            dirs.push(e.name[..at + i].to_string());
            at += i + 1;
        }
    }
    dirs.sort();
    dirs.dedup();
    let existing: std::collections::HashSet<&str> = out.iter().map(|entry| entry.name.as_str()).collect();
    let missing: Vec<_> = dirs.into_iter().filter(|name| !existing.contains(name.as_str())).collect();
    out.extend(missing.into_iter().map(|name| Stat::directory().entry(name)));
    Ok(out)
}

/// A member path cleaned as GDAL shows it: `/`-separated, without a leading `./` or `/`.
fn clean_member(name: &str) -> String {
    let name = name.replace('\\', "/");
    let parts: Vec<&str> = name.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    parts.join("/")
}

/// Runs `f` on the archive's bytes as a seekable reader: a local file, a file in memory, or a
/// remote file read by range requests.
fn with_archive_reader<T>(archive: &Node, metadata_only: bool, f: impl FnOnce(&mut dyn ReadSeek) -> Result<T>) -> Result<T> {
    #[cfg(not(remote))]
    let _ = metadata_only;
    match archive {
        Node::Local(p) => {
            let mut file = std::io::BufReader::new(std::fs::File::open(p).map_err(|e| io_context(e, &p.display().to_string()))?);
            f(&mut file)
        }
        Node::Mem(k) => {
            let bytes = crate::memfs::read(Path::new(k))?;
            f(&mut std::io::Cursor::new(bytes))
        }
        #[cfg(remote)]
        Node::S3 { .. } | Node::Url { .. } => {
            let remote = archive.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?;
            // Small blocks for metadata discovery; payload reads use the default
            // larger blocks to avoid one HTTP request per 16 KiB of member data.
            let mut options = crate::http::HttpOptions::from_env();
            if metadata_only
                && std::env::var_os("TERRIO_HTTP_BLOCK_SIZE").is_none()
                && std::env::var_os("CPL_VSIL_CURL_CHUNK_SIZE").is_none()
            {
                options.block_size = 16 * 1024;
            }
            let mut reader = crate::http::RangeReader::open_with(&remote, options)?;
            f(&mut reader)
        }
        #[cfg(not(remote))]
        Node::S3 { .. } | Node::Url { .. } => Err(Error::Unsupported(remote_disabled_message())),
        Node::Archive { .. } => Err(Error::Unsupported("archives inside archives are not supported".into())),
    }
}

#[cfg(not(target_family = "wasm"))]
fn with_archive_stream<T>(archive: &Node, read: impl FnOnce(&mut dyn Read) -> Result<T>) -> Result<T> {
    #[cfg(remote)]
    if let Some(remote) = archive.remote() {
        return crate::http::with_reader(&remote, read);
    }
    with_archive_reader(archive, false, |reader| read(reader))
}

/// A seekable reader.
pub(crate) trait ReadSeek: Read + std::io::Seek {}
impl<T: Read + std::io::Seek> ReadSeek for T {}

fn io_context(e: std::io::Error, what: &str) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound { not_found(what) } else { Error::Io(e) }
}

struct ZipIndex {
    entries: Vec<zipdir::CdEntry>,
    by_name: std::collections::HashMap<String, usize>,
}

thread_local! {
    static ZIP_INDEX: std::cell::RefCell<Option<(Node, std::rc::Rc<ZipIndex>)>> = const { std::cell::RefCell::new(None) };
}

fn zip_index(archive: &Node, reader: &mut dyn ReadSeek) -> Result<std::rc::Rc<ZipIndex>> {
    if let Some(index) = ZIP_INDEX.with(|cache| cache.borrow().as_ref().filter(|(node, _)| node == archive).map(|(_, i)| i.clone())) {
        return Ok(index);
    }
    let entries = zipdir::central_directory(reader)?;
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate().filter(|(_, e)| !e.is_dir()) {
        by_name.entry(clean_member(&entry.name)).or_insert(i);
    }
    let index = std::rc::Rc::new(ZipIndex { entries, by_name });
    ZIP_INDEX.with(|cache| *cache.borrow_mut() = Some((archive.clone(), index.clone())));
    Ok(index)
}

fn zip_entries(archive: &Node) -> Result<Vec<Entry>> {
    with_archive_reader(archive, true, |r| {
        let mut out = Vec::new();
        for f in &zip_index(archive, r)?.entries {
            let name = clean_member(&f.name);
            if name.is_empty() {
                continue;
            }
            let dir = f.is_dir();
            out.push(Entry {
                name,
                kind: if dir { EntryKind::Directory } else { EntryKind::File },
                size: Some(if dir { 0 } else { f.uncompressed_size }),
                modified: f.modified(),
                permissions: None,
                etag: None,
            });
        }
        Ok(out)
    })
}

#[cfg(not(target_family = "wasm"))]
fn tar_entry<R: Read>(entry: &tar::Entry<'_, R>) -> Option<Entry> {
    let name = clean_member(&String::from_utf8_lossy(&entry.path_bytes()));
    let header = entry.header();
    let dir = header.entry_type().is_dir();
    if name.is_empty() || !(dir || header.entry_type().is_file()) {
        return None;
    }
    Some(Entry {
        name,
        kind: if dir { EntryKind::Directory } else { EntryKind::File },
        size: Some(if dir { 0 } else { entry.size() }),
        modified: header.mtime().ok().map(|t| t as i64),
        permissions: None,
        etag: None,
    })
}

#[cfg(not(target_family = "wasm"))]
fn tar_entries(archive: &Node, kind: ArchiveKind) -> Result<Vec<Entry>> {
    if kind == ArchiveKind::TarGz {
        return with_archive_stream(archive, |reader| {
            let mut tar = tar::Archive::new(flate2::read::MultiGzDecoder::new(reader));
            let mut out = Vec::new();
            for entry in tar.entries()? {
                if let Some(entry) = tar_entry(&entry?) {
                    out.push(entry);
                }
            }
            Ok(out)
        });
    }
    with_archive_reader(archive, true, |reader| {
        let mut out = Vec::new();
        // Plain tar can seek over payloads, fetching only headers remotely.
        let mut tar = tar::Archive::new(SeekRead(reader));
        for entry in tar.entries_with_seek()? {
            if let Some(entry) = tar_entry(&entry?) {
                out.push(entry);
            }
        }
        Ok(out)
    })
}

#[cfg(not(target_family = "wasm"))]
fn tar_stat(archive: &Node, kind: ArchiveKind, inner: &str) -> Result<Option<Stat>> {
    if kind == ArchiveKind::TarGz {
        return with_archive_stream(archive, |reader| {
            let mut tar = tar::Archive::new(flate2::read::MultiGzDecoder::new(reader));
            tar_stat_entries(tar.entries()?, inner)
        });
    }
    with_archive_reader(archive, true, |reader| {
        let mut tar = tar::Archive::new(SeekRead(reader));
        tar_stat_entries(tar.entries_with_seek()?, inner)
    })
}

#[cfg(not(target_family = "wasm"))]
fn tar_stat_entries<R: Read>(entries: tar::Entries<'_, R>, inner: &str) -> Result<Option<Stat>> {
    let prefix = format!("{inner}/");
    let mut implied_directory = false;
    for entry in entries {
        let Some(info) = tar_entry(&entry?) else { continue };
        if info.name == inner {
            return Ok(Some(Stat { dir: info.is_dir(), size: info.size, modified: info.modified, permissions: None, etag: None }));
        }
        // Keep looking for an exact member: an implied prefix must not shadow a file.
        implied_directory |= info.name.starts_with(&prefix);
    }
    Ok(implied_directory.then(Stat::directory))
}

/// A `&mut dyn ReadSeek` as a sized reader.
struct SeekRead<'a>(&'a mut dyn ReadSeek);

impl Read for SeekRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl std::io::Seek for SeekRead<'_> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

/// Copies an archive member's bytes into `out`.
fn read_member(archive: &Node, kind: ArchiveKind, inner: &str, out: &mut dyn Write) -> Result<u64> {
    #[cfg(not(target_family = "wasm"))]
    if kind == ArchiveKind::TarGz {
        return with_archive_stream(archive, |reader| read_tar_member(&mut flate2::read::MultiGzDecoder::new(reader), inner, out));
    }
    with_archive_reader(archive, false, |reader| match kind {
        ArchiveKind::Zip => {
            let index = zip_index(archive, reader)?;
            let entry = index.by_name.get(inner).map(|&i| &index.entries[i]).ok_or_else(|| not_found(inner))?;
            zipdir::extract(reader, entry, out)
        }
        #[cfg(not(target_family = "wasm"))]
        ArchiveKind::Tar => {
            let mut tar = tar::Archive::new(SeekRead(reader));
            copy_tar_entries(tar.entries_with_seek()?, inner, out)
        }
        #[cfg(not(target_family = "wasm"))]
        ArchiveKind::TarGz => unreachable!("compressed tar uses a sequential stream"),
        #[cfg(target_family = "wasm")]
        ArchiveKind::Tar | ArchiveKind::TarGz => Err(Error::Unsupported("tar archives are not supported in WebAssembly builds".into())),
    })
}

#[cfg(not(target_family = "wasm"))]
fn read_tar_member(reader: &mut dyn Read, inner: &str, out: &mut dyn Write) -> Result<u64> {
    let mut tar = tar::Archive::new(reader);
    copy_tar_entries(tar.entries()?, inner, out)
}

#[cfg(not(target_family = "wasm"))]
fn copy_tar_entries<R: Read>(entries: tar::Entries<'_, R>, inner: &str, out: &mut dyn Write) -> Result<u64> {
    for entry in entries {
        let mut entry = entry?;
        if clean_member(&String::from_utf8_lossy(&entry.path_bytes())) == inner && entry.header().entry_type().is_file() {
            return Ok(std::io::copy(&mut entry, out)?);
        }
    }
    Err(not_found(inner))
}

/// Restricts the last HTML page cache to one public operation, including error exits.
struct HtmlCacheScope;
impl HtmlCacheScope {
    fn new() -> Self {
        forget_html_page();
        ZIP_INDEX.with(|cache| *cache.borrow_mut() = None);
        Self
    }
}
impl Drop for HtmlCacheScope {
    fn drop(&mut self) {
        forget_html_page();
        ZIP_INDEX.with(|cache| *cache.borrow_mut() = None);
    }
}

// --- Reading and writing files ----------------------------------------------------------------

/// Refuses local pipes, sockets and devices before byte operations open them.
fn check_regular_source(node: &Node) -> Result<()> {
    if let Node::Local(path) = node
        && !std::fs::metadata(path)?.is_file()
    {
        return Err(Error::Unsupported(format!("'{}': only regular files can be copied or read", path.display())));
    }
    Ok(())
}

/// Copies a file's bytes into `out`.
fn read_into(node: &Node, out: &mut dyn Write) -> Result<u64> {
    check_regular_source(node)?;
    match node {
        Node::Local(p) => {
            let mut f = std::fs::File::open(p).map_err(|e| io_context(e, &p.display().to_string()))?;
            Ok(std::io::copy(&mut f, out)?)
        }
        Node::Mem(k) => {
            let bytes = crate::memfs::read(Path::new(k)).map_err(|e| io_context(e, k))?;
            out.write_all(&bytes)?;
            Ok(bytes.len() as u64)
        }
        #[cfg(remote)]
        Node::S3 { .. } | Node::Url { .. } => {
            crate::http::download_to(&node.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?, out)
        }
        #[cfg(not(remote))]
        Node::S3 { .. } | Node::Url { .. } => Err(Error::Unsupported(remote_disabled_message())),
        Node::Archive { archive, kind, inner } => read_member(archive, *kind, inner, out),
    }
}

/// Reads a whole file into memory.
pub fn read(path: &str) -> Result<Vec<u8>> {
    let _cache = HtmlCacheScope::new();
    let mut out = Vec::new();
    read_into(&Node::parse(path)?, &mut out)?;
    Ok(out)
}

/// Writes bytes to a file (local, in memory, or an object), replacing it.
pub fn write(path: &str, bytes: &[u8]) -> Result<()> {
    let _cache = HtmlCacheScope::new();
    let node = Node::parse(path)?;
    if let Some(reason) = node.read_only_reason() {
        return Err(Error::Unsupported(format!("'{}': {reason}", listing::redact_url(path))));
    }
    match &node {
        Node::Local(p) => write_local_atomic(p, |f| Ok(f.write_all(bytes)?)),
        Node::Mem(k) => {
            if k == "/vsimem" {
                return Err(Error::InvalidArgument("cannot write a file at the /vsimem root".into()));
            }
            Ok(crate::memfs::write(Path::new(k), bytes)?)
        }
        #[cfg(remote)]
        _ => crate::http::store::put(
            &node.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?,
            crate::http::store::Payload::Bytes(bytes.to_vec()),
        ),
        #[cfg(not(remote))]
        _ => Err(Error::Unsupported(remote_disabled_message())),
    }
}

/// Writes a local file through a temporary file in the same directory, renamed into place once
/// complete: a failure leaves any existing file as it was.
fn write_local_atomic(path: &Path, fill: impl FnOnce(&mut std::fs::File) -> Result<()>) -> Result<()> {
    if cfg!(target_family = "wasm") {
        return Err(Error::Unsupported(format!("'{}': WebAssembly builds have no local files (use /vsimem/)", path.display())));
    }
    let resolved = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Some(std::fs::canonicalize(path)?),
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let path = resolved.as_deref().unwrap_or(path);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let permissions = match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::IsADirectory, format!("'{}': is a directory", path.display()))));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(Error::Unsupported(format!("'{}': refusing to replace a non-regular file", path.display())));
        }
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut n = 0u32;
    let (tmp, mut file) = loop {
        let tmp = dir.join(format!(".terrio-partial-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
            Err(e) => return Err(e.into()),
        }
    };
    let result = fill(&mut file).and_then(|()| {
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        Ok(file.sync_all()?)
    });
    drop(file);
    match result.and_then(|()| Ok(std::fs::rename(&tmp, path)?)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Copies one file from `src` to `dst`; returns the bytes copied.
fn copy_file(src: &Node, dst: &Node, known_size: Option<u64>) -> Result<u64> {
    check_regular_source(src)?;
    #[cfg(not(remote))]
    let _ = known_size;
    if let Some(reason) = dst.read_only_reason() {
        return Err(Error::Unsupported(format!("'{}': {reason}", display_node(dst))));
    }
    match dst {
        Node::Local(p) => {
            if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            let mut n = 0;
            write_local_atomic(p, |f| {
                let mut w = std::io::BufWriter::new(f);
                n = read_into(src, &mut w)?;
                w.flush()?;
                Ok(())
            })?;
            Ok(n)
        }
        Node::Mem(k) => {
            let mut buf = Vec::new();
            let n = read_into(src, &mut buf)?;
            crate::memfs::write(Path::new(k), buf)?;
            Ok(n)
        }
        #[cfg(remote)]
        Node::S3 { .. } | Node::Url { .. } => {
            use crate::http::store;
            let remote = dst.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?;
            match (src, &remote) {
                // Within S3: a server-side copy.
                (Node::S3 { bucket, key }, crate::http::Remote::S3(to)) => {
                    let size = match known_size {
                        Some(size) => size,
                        None => stat_node(src)?.and_then(|s| s.size).unwrap_or(0),
                    };
                    store::s3_copy(&crate::s3::S3Object { bucket: bucket.clone(), key: key.clone() }, to, size)?;
                    Ok(size)
                }
                (Node::Local(p), _) => {
                    let size = std::fs::metadata(p).map_err(|e| io_context(e, &p.display().to_string()))?.len();
                    store::put(&remote, store::file_payload(p))?;
                    Ok(size)
                }
                _ => {
                    // Through a temporary file.
                    let mut tmp = tempfile::NamedTempFile::new()?;
                    let n = read_into(src, tmp.as_file_mut())?;
                    tmp.as_file_mut().flush()?;
                    store::put(&remote, store::file_payload(tmp.path()))?;
                    Ok(n)
                }
            }
        }
        #[cfg(not(remote))]
        Node::S3 { .. } | Node::Url { .. } => Err(Error::Unsupported(remote_disabled_message())),
        Node::Archive { .. } => Err(Error::Unsupported("archives are read only".into())),
    }
}

fn make_dir(node: &Node) -> Result<()> {
    match node {
        Node::Local(p) => Ok(std::fs::create_dir_all(p)?),
        // An S3 directory marker (`key/`), as GDAL creates it: empty directories survive.
        #[cfg(remote)]
        Node::S3 { bucket, key } if !key.is_empty() => {
            let marker = crate::http::Remote::S3(crate::s3::S3Object { bucket: bucket.clone(), key: format!("{key}/") });
            crate::http::store::put(&marker, crate::http::store::Payload::Bytes(Vec::new()))
        }
        // Directories in memory and in Azure are implied by the files below them.
        Node::Mem(_) | Node::S3 { .. } | Node::Url { .. } => Ok(()),
        Node::Archive { .. } => Err(Error::Unsupported("archives are read only".into())),
    }
}

// --- Safety -----------------------------------------------------------------------------------

/// Refuses to delete (or move away) locations whose loss would be catastrophic or that are not
/// precise: see the [module documentation](self).
fn check_deletable(input: &str, node: &Node, operation: &str) -> Result<()> {
    let shown = listing::redact_url(input);
    let refuse = |why: &str| Err(Error::InvalidArgument(format!("refusing to {operation} '{shown}': {why}")));
    if input.trim().is_empty() {
        return refuse("empty path");
    }
    match node {
        Node::Local(p) => {
            if p.as_os_str().is_empty()
                || (p.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir)) && p.components().count() == 1)
            {
                return refuse("not a precise path");
            }
            if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
                // delete removes only the link, never the directory it points to.
                return Ok(());
            }
            // A missing/dangling path cannot be an existing protected directory.
            let Ok(canonical) = std::fs::canonicalize(p) else { return Ok(()) };
            if canonical.parent().is_none() {
                return refuse("a file system root");
            }
            if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
                && std::fs::canonicalize(home).is_ok_and(|h| h.starts_with(&canonical))
            {
                return refuse("the home directory or one of its parents");
            }
            if let Ok(cwd) = std::env::current_dir().and_then(std::fs::canonicalize)
                && cwd.starts_with(&canonical)
            {
                return refuse("the current directory or one of its parents");
            }
            Ok(())
        }
        Node::Mem(k) if k == "/vsimem" => refuse("the root of /vsimem/"),
        Node::Mem(_) => Ok(()),
        Node::S3 { key, .. } if key.is_empty() => refuse("a whole bucket"),
        Node::Url { base, .. } => {
            if listing::Api::for_url(base).is_object_store() {
                // `https://account.blob.core.windows.net/container`: a whole container.
                let path = base.split_once("://").map_or("", |(_, r)| r).split_once('/').map_or("", |(_, p)| p);
                let segments = path.split('/').filter(|s| !s.is_empty()).count();
                let host = base.split_once("://").map_or("", |(_, r)| r).split('/').next().unwrap_or_default().to_ascii_lowercase();
                let virtual_hosted_s3 = host.ends_with(".amazonaws.com") && !host.starts_with("s3.") && !host.starts_with("s3-");
                if segments < if virtual_hosted_s3 { 1 } else { 2 } {
                    return refuse("a whole bucket or container");
                }
            }
            node.read_only_reason().map_or(Ok(()), |r| refuse(&r))
        }
        Node::S3 { .. } => node.read_only_reason().map_or(Ok(()), |r| refuse(&r)),
        Node::Archive { .. } => refuse("archives are read only"),
    }
}

/// A local path made absolute through its nearest existing ancestor: that ancestor canonical
/// (links resolved), with the components below it that do not exist yet.
fn absolute_path(p: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut at = p;
    loop {
        let existing = if at.as_os_str().is_empty() { Path::new(".") } else { at };
        if let Ok(c) = std::fs::canonicalize(existing) {
            return Some(missing.iter().rev().fold(c, |c, n: &&std::ffi::OsStr| c.join(n)));
        }
        missing.push(at.file_name()?);
        at = at.parent()?;
    }
}

/// Whether `inner` is `outer` or below it (the same location named the same way).
fn is_within(inner: &Node, outer: &Node) -> bool {
    let below = |a: &str, b: &str| a == b || b.is_empty() || a.starts_with(&format!("{}/", b.trim_end_matches('/')));
    match (inner, outer) {
        (Node::Local(a), Node::Local(b)) => match (absolute_path(a), absolute_path(b)) {
            (Some(a), Some(b)) => a.starts_with(b),
            _ => true,
        },
        (Node::Mem(a), Node::Mem(b)) => below(a, b),
        (Node::S3 { bucket: ba, key: a }, Node::S3 { bucket: bb, key: b }) => ba == bb && below(a, b),
        (Node::Url { base: a, .. }, Node::Url { base: b, .. }) => below(a, b),
        _ => false,
    }
}

// --- Operations -------------------------------------------------------------------------------

/// Lists a directory (its entries, named relative to it), or describes a file (one entry named
/// as given), as `gdal vsi list` does.
pub fn list(path: &str, options: &ListOptions) -> Result<Vec<Entry>> {
    list_inner(path, options).map(|(_, entries)| entries)
}

fn list_inner(path: &str, options: &ListOptions) -> Result<(bool, Vec<Entry>)> {
    let _cache = HtmlCacheScope::new();
    let node = Node::parse(path)?;
    let stat = stat_node(&node)?.ok_or_else(|| not_found(path))?;
    if !stat.dir {
        let name = if options.absolute { absolute_name(path, &node, "") } else { listing::redact_url(path.trim()).into_owned() };
        return Ok((true, vec![stat.entry(name)]));
    }
    let mut entries = read_dir(&node, options.recursive, options.depth)?;
    forget_html_page();
    #[cfg(remote)]
    if options.details
        && let Node::Url { .. } = &node
        && node.directory().is_some_and(|d| !crate::http::store::is_object_store(&d))
    {
        for e in entries.iter_mut().filter(|e| !e.is_dir() && e.size.is_none()) {
            if let Some(remote) = node.join(&e.name).remote()
                && let Ok(Some(s)) = crate::http::store::stat(&remote)
            {
                e.size = s.size;
                e.modified = s.modified;
                e.etag = s.etag;
            }
        }
    }
    if options.absolute {
        for e in &mut entries {
            e.name = absolute_name(path, &node, &e.name);
        }
    }
    Ok((false, entries))
}

/// Lists a location in GDAL-compatible JSON, flat or nested, with optional details.
/// Absolute tree names are resolved from the listing's file/directory context,
/// without reopening entries or joining redacted URLs as relative names.
pub fn list_json(path: &str, options: &ListOptions, tree: bool) -> Result<serde_json::Value> {
    let read_options = ListOptions { absolute: options.absolute && !tree, ..options.clone() };
    let (single_file, entries) = list_inner(path, &read_options)?;
    if !tree {
        return Ok(serde_json::Value::Array(entries.iter().map(|e| entry_json(e, &e.name, options.details)).collect()));
    }
    let absolute = if options.absolute {
        let node = Node::parse(path)?;
        let mut absolute = entries.clone();
        for e in &mut absolute {
            e.name = absolute_name(path, &node, if single_file { "" } else { &e.name });
        }
        Some(absolute)
    } else {
        None
    };
    Ok(tree_json(&entries, options.details, absolute.as_deref()))
}

/// An entry as GDAL's JSON prints it: its name alone, or with -l an object.
fn entry_json(e: &Entry, name: &str, long: bool) -> serde_json::Value {
    if !long {
        return serde_json::Value::String(name.to_string());
    }
    let mut o = serde_json::Map::new();
    o.insert("name".into(), name.into());
    o.insert("type".into(), (if e.is_dir() { "directory" } else { "file" }).into());
    if let Some(s) = e.size {
        o.insert("size".into(), s.into());
    }
    if let Some(m) = e.modified_string() {
        o.insert("last_modification_date".into(), m.into());
    }
    if let Some(p) = &e.permissions {
        o.insert("permissions".into(), p.clone().into());
    }
    if let Some(t) = &e.etag {
        o.insert("etag".into(), t.clone().into());
    }
    serde_json::Value::Object(o)
}

/// Entries nested by directory, as `gdal vsi list --tree -f json` prints them.
fn tree_json(entries: &[Entry], long: bool, absolute: Option<&[Entry]>) -> serde_json::Value {
    if entries.len() == 1 && !entries[0].is_dir() {
        let e = absolute.map_or(&entries[0], |a| &a[0]);
        return vec![entry_json(e, &e.name, long)].into();
    }
    let mut index: std::collections::HashMap<&str, Vec<usize>> = Default::default();
    for (i, e) in entries.iter().enumerate() {
        let parent = e.name.rsplit_once('/').map_or("", |(parent, _)| parent);
        index.entry(parent).or_default().push(i);
    }
    fn children(
        entries: &[Entry],
        parent: &str,
        long: bool,
        absolute: Option<&[Entry]>,
        index: &std::collections::HashMap<&str, Vec<usize>>,
    ) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for &i in index.get(parent).into_iter().flatten() {
            let e = &entries[i];
            let rel = e.name.rsplit('/').next().unwrap_or(&e.name);
            let shown = absolute.map_or(rel, |a| a[i].name.as_str());
            if e.is_dir() {
                let mut o = match long.then(|| entry_json(e, shown, true)) {
                    Some(serde_json::Value::Object(o)) => o,
                    _ => {
                        let mut o = serde_json::Map::new();
                        o.insert("name".into(), shown.into());
                        o
                    }
                };
                o.insert("entries".into(), children(entries, &e.name, long, absolute, index).into());
                out.push(serde_json::Value::Object(o));
            } else {
                out.push(entry_json(e, shown, long));
            }
        }
        out
    }
    children(entries, "", long, absolute, &index).into()
}

/// Resolves a relative listing entry against its source path without opening the entry.
/// Local paths become absolute; URL credential parameters are redacted.
pub fn absolute_entry_name(path: &str, name: &str) -> Result<String> {
    Ok(absolute_name(path, &Node::parse(path)?, name))
}

/// The absolute name of `name` below the listed `path`.
fn absolute_name(path: &str, node: &Node, name: &str) -> String {
    if let Node::Url { base, query } = node {
        let joined = if name.is_empty() { base.clone() } else { format!("{}/{name}", base.trim_end_matches('/')) };
        return listing::redact_url(&if query.is_empty() { joined } else { format!("{joined}?{query}") }).into_owned();
    }
    let base = match node {
        Node::Local(p) => {
            std::path::absolute(p).map(|p| p.display().to_string()).unwrap_or_else(|_| listing::redact_url(path.trim()).into_owned())
        }
        _ => listing::redact_url(path.trim()).into_owned(),
    };
    let base = base.trim_end_matches('/');
    if name.is_empty() { base.to_string() } else { format!("{base}/{name}") }
}

/// Describes one location, or `None` when nothing is there.
pub fn stat(path: &str) -> Result<Option<Entry>> {
    let _cache = HtmlCacheScope::new();
    let node = Node::parse(path)?;
    Ok(stat_node(&node)?.map(|s| s.entry(listing::redact_url(path.trim()).into_owned())))
}

/// Where a file or directory copied to `dst` goes: into `dst` when it is an existing directory
/// (or ends with `/`), as `dst` otherwise.
fn copy_target(src: &Node, dst_input: &str, dst: &Node) -> Result<Node> {
    let existing_directory = stat_node(dst)?.is_some_and(|s| s.dir);
    let into = dst_input.trim().ends_with('/') || existing_directory;
    let portable = matches!(src, Node::Archive { .. }) || cfg!(windows) && matches!(dst, Node::Local(_));
    Ok(if into { checked_child_target(dst, &src.file_name(), portable)? } else { dst.clone() })
}

/// Copies a file, or with `recursive` a directory, as `gdal vsi copy` does: into `dst` when it is
/// an existing directory, to `dst` otherwise. Archive members can be copied out; object stores
/// copy within S3 server-side.
pub fn copy(src: &str, dst: &str, options: &CopyOptions) -> Result<Report> {
    let _cache = HtmlCacheScope::new();
    let src_node = Node::parse(src)?;
    let dst_node = Node::parse(dst)?;
    let stat = stat_node(&src_node)?.ok_or_else(|| not_found(src))?;
    let target = copy_target(&src_node, dst, &dst_node)?;
    let mut report = Report::default();
    if !stat.dir {
        if is_same(&src_node, &target) {
            return Err(Error::InvalidArgument(format!(
                "'{}' and '{}' are the same file",
                listing::redact_url(src),
                listing::redact_url(dst)
            )));
        }
        report.bytes += copy_file(&src_node, &target, stat.size)?;
        report.copied += 1;
        return Ok(report);
    }
    if !options.recursive {
        return Err(Error::InvalidArgument(format!("'{}' is a directory: use the recursive option", listing::redact_url(src))));
    }
    if is_within(&target, &src_node) {
        return Err(Error::InvalidArgument(format!(
            "cannot copy '{}' into itself ('{}')",
            listing::redact_url(src),
            listing::redact_url(dst)
        )));
    }
    copy_tree(&src_node, &target, options.skip_errors, None, &mut report)?;
    Ok(report)
}

fn is_same(a: &Node, b: &Node) -> bool {
    match (a, b) {
        (Node::Local(x), Node::Local(y)) => std::fs::canonicalize(x).ok().is_some_and(|x| std::fs::canonicalize(y).ok() == Some(x)),
        _ => a == b,
    }
}

/// Archive output must remain below its destination even through existing symlinks.
fn unsafe_relative(name: &str) -> bool {
    name.starts_with(['/', '\\'])
        || name.split(['/', '\\']).any(|part| part == "..")
        || (name.as_bytes().get(1) == Some(&b':') && name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic))
}

fn unsafe_child_name(name: &str, portable: bool) -> bool {
    name.starts_with('/') || name.split('/').any(|part| matches!(part, "." | "..")) || portable && unsafe_relative(name)
}

fn unsafe_destination_name(root: &Node, name: &str, portable: bool) -> bool {
    // memfs canonicalizes backslashes to separators, so they cannot round-trip.
    unsafe_child_name(name, portable) || matches!(root, Node::Mem(_)) && name.contains('\\')
}

fn archive_target(root: &Node, name: &str) -> Result<Node> {
    checked_child_target(root, name, true)
}

fn checked_child_target(root: &Node, name: &str, portable: bool) -> Result<Node> {
    if unsafe_destination_name(root, name, portable) {
        return Err(Error::InvalidArgument("refusing unsafe path traversal in copy source".into()));
    }
    let target = root.join(name);
    if let (Node::Local(root), Node::Local(path)) = (root, &target) {
        let root = std::fs::canonicalize(root).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::InvalidArgument(format!("destination directory '{}' does not exist", root.display()))
            } else {
                io_context(error, &root.display().to_string())
            }
        })?;
        let mut existing = path.as_path();
        loop {
            match std::fs::symlink_metadata(existing) {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    existing = existing.parent().ok_or_else(|| Error::InvalidArgument("copy target has no existing parent".into()))?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        if !std::fs::canonicalize(existing)?.starts_with(root) {
            return Err(Error::InvalidArgument("copy target escapes its output directory through a symbolic link".into()));
        }
    }
    Ok(target)
}

/// Writes one sequential archive member without reopening its archive.
#[cfg(not(target_family = "wasm"))]
fn write_member(dst: &Node, reader: &mut dyn Read) -> Result<u64> {
    if let Some(reason) = dst.read_only_reason() {
        return Err(Error::Unsupported(reason));
    }
    match dst {
        Node::Local(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut copied = 0;
            write_local_atomic(path, |out| {
                copied = std::io::copy(reader, out)?;
                Ok(())
            })?;
            Ok(copied)
        }
        Node::Mem(key) => {
            let mut data = Vec::new();
            let copied = std::io::copy(reader, &mut data)?;
            crate::memfs::write(Path::new(key), data)?;
            Ok(copied)
        }
        #[cfg(remote)]
        Node::S3 { .. } | Node::Url { .. } => {
            let mut tmp = tempfile::NamedTempFile::new()?;
            let copied = std::io::copy(reader, tmp.as_file_mut())?;
            tmp.as_file_mut().flush()?;
            let remote = dst.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?;
            crate::http::store::put(&remote, crate::http::store::file_payload(tmp.path()))?;
            Ok(copied)
        }
        _ => Err(Error::Unsupported("the target does not support streamed archive extraction".into())),
    }
}

#[cfg(not(target_family = "wasm"))]
fn copy_tar_reader(
    reader: &mut dyn Read,
    inner: &str,
    dst: &Node,
    skip_errors: bool,
    sync: Option<&SyncOptions>,
    report: &mut Report,
) -> Result<()> {
    make_dir(dst)?;
    let targets: std::collections::HashMap<String, Entry> = if let Some(o) = sync {
        read_dir(dst, o.recursive, None)?.into_iter().map(|e| (e.name.clone(), e)).collect()
    } else {
        Default::default()
    };
    let prefix = format!("{}/", inner.trim_matches('/'));
    let mut archive = tar::Archive::new(reader);
    let mut made = std::collections::HashSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let Some(info) = tar_entry(&entry) else {
            let name = clean_member(&String::from_utf8_lossy(&entry.path_bytes()));
            let relative = if inner.is_empty() {
                name.as_str()
            } else {
                let Some(relative) = name.strip_prefix(&prefix) else {
                    continue;
                };
                relative
            };
            if relative.is_empty() || sync.is_some_and(|o| !o.recursive) && relative.contains('/') {
                continue;
            }
            let error = Error::Unsupported(format!("'{relative}': tar links and non-file entries are not supported"));
            if skip_errors {
                report.errors.push((relative.into(), error.to_string()));
                continue;
            }
            return Err(error);
        };
        let relative = if inner.is_empty() {
            info.name.as_str()
        } else {
            let Some(relative) = info.name.strip_prefix(&prefix) else { continue };
            relative
        };
        if relative.is_empty() {
            continue;
        }
        if unsafe_relative(relative) {
            return Err(Error::InvalidArgument("refusing unsafe path traversal in copy source".into()));
        }
        if sync.is_some_and(|o| !o.recursive) && relative.contains('/') {
            let top = relative.split('/').next().expect("has separator");
            if made.insert(top.to_string()) {
                make_dir(&archive_target(dst, top)?)?;
            }
            continue;
        }
        let target = archive_target(dst, relative)?;
        if !info.is_dir()
            && let Some(o) = sync
            && let Some(t) = targets.get(relative)
        {
            // Archive entries have no single-part object ETag, so only the ordinary
            // size/timestamp fallback is applicable here.
            if o.strategy != SyncStrategy::Overwrite
                && info.size == t.size
                && matches!((info.modified, t.modified), (Some(a), Some(b)) if a <= b)
            {
                report.unchanged += 1;
                continue;
            }
        }
        for (at, _) in relative.match_indices('/') {
            let parent = &relative[..at];
            if made.insert(parent.to_string()) {
                make_dir(&dst.join(parent))?;
            }
        }
        let result = if info.is_dir() {
            make_dir(&target).map(|()| None)
        } else if let Node::Mem(key) = &target
            && sync.is_some_and(|o| o.strategy != SyncStrategy::Overwrite)
            && targets.get(relative).is_some_and(|t| !t.is_dir() && t.size == info.size)
        {
            let result = sync_mem_member(key, &mut entry);
            if matches!(&result, Ok(None)) {
                report.unchanged += 1;
                continue;
            }
            result
        } else {
            write_member(&target, &mut entry).map(Some)
        };
        match result {
            Ok(Some(bytes)) => {
                report.copied += 1;
                report.bytes += bytes;
            }
            Ok(None) => {}
            Err(error) if skip_errors => report.errors.push((relative.into(), error.to_string())),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
fn sync_mem_member(key: &str, input: &mut dyn Read) -> Result<Option<u64>> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    if crate::memfs::read(Path::new(key))?.as_slice() == bytes {
        return Ok(None);
    }
    let n = bytes.len() as u64;
    crate::memfs::write(Path::new(key), bytes)?;
    Ok(Some(n))
}

/// Copies a directory's tree: every file (and directory) below `src` to the same name below
/// `dst`. With `sync`, files whose target is up to date are left.
fn copy_tree(src: &Node, dst: &Node, skip_errors: bool, sync: Option<&SyncOptions>, report: &mut Report) -> Result<()> {
    let recursive = sync.is_none_or(|o| o.recursive);
    #[cfg(not(target_family = "wasm"))]
    if let Node::Archive { archive, kind, inner } = src
        && matches!(kind, ArchiveKind::Tar | ArchiveKind::TarGz)
    {
        if *kind == ArchiveKind::TarGz {
            return with_archive_stream(archive, |reader| {
                copy_tar_reader(&mut flate2::read::MultiGzDecoder::new(reader), inner, dst, skip_errors, sync, report)
            });
        }
        return with_archive_reader(archive, false, |reader| copy_tar_reader(reader, inner, dst, skip_errors, sync, report));
    }
    // A non-recursive sync creates the subdirectories, empty, as GDAL does.
    let entries = read_dir(src, recursive, None)?;
    // Drive prefixes and backslashes are special only for Windows local outputs.
    // Archive extraction retains its stricter guard; slash traversal is refused everywhere.
    let portable = matches!(src, Node::Archive { .. }) || cfg!(windows) && matches!(dst, Node::Local(_));
    if entries.iter().any(|e| unsafe_destination_name(dst, &e.name, portable)) {
        return Err(Error::InvalidArgument("refusing unsafe path traversal in copy source".into()));
    }
    let target_entries = match sync {
        Some(_) => match stat_node(dst)? {
            Some(s) if s.dir => read_dir(dst, recursive, None)?,
            _ => {
                make_dir(dst)?;
                Vec::new()
            }
        },
        None => {
            make_dir(dst)?;
            Vec::new()
        }
    };
    let target_index: std::collections::HashMap<_, _> = target_entries.iter().map(|e| (e.name.as_str(), e)).collect();
    for e in &entries {
        let from = src.join(&e.name);
        if let Node::Local(path) = &from
            && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
            && e.is_dir()
        {
            let message = "recursive copy of a directory symlink is unsupported";
            if skip_errors {
                report.errors.push((display_node(&from), message.into()));
                continue;
            }
            return Err(Error::Unsupported(message.into()));
        }
        let to = checked_child_target(dst, &e.name, portable)?;
        if e.is_dir() {
            // Already there (a sync's target).
            if target_index.get(e.name.as_str()).is_some_and(|t| t.is_dir()) {
                continue;
            }
            if let Err(err) = make_dir(&to) {
                if !skip_errors {
                    return Err(err);
                }
                report.errors.push((display_node(&to), err.to_string()));
            }
            continue;
        }
        if let Err(error) = check_regular_source(&from) {
            if skip_errors {
                report.errors.push((display_node(&from), error.to_string()));
                continue;
            }
            return Err(error);
        }
        if let Some(options) = sync {
            let target = target_index.get(e.name.as_str()).filter(|t| !t.is_dir());
            if let Some(t) = target
                && up_to_date(&from, e, &to, t, options.strategy)?
            {
                report.unchanged += 1;
                continue;
            }
        }
        match copy_file(&from, &to, e.size) {
            Ok(n) => {
                report.copied += 1;
                report.bytes += n;
            }
            Err(err) if skip_errors => report.errors.push((display_node(&from), err.to_string())),
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Whether the target of a sync is up to date with its source.
fn up_to_date(src: &Node, s: &Entry, dst: &Node, t: &Entry, strategy: SyncStrategy) -> Result<bool> {
    let timestamp = || -> Result<bool> {
        if s.size != t.size {
            return Ok(false);
        }
        if let (Some(a), Some(b)) = (s.modified, t.modified) {
            return Ok(a <= b);
        }
        // In-memory files have no portable clock on WebAssembly. Compare content
        // when either endpoint is untimed memory instead of recopying it forever.
        if matches!(src, Node::Mem(_)) || matches!(dst, Node::Mem(_)) {
            return Ok(md5_of(src)? == md5_of(dst)?);
        }
        Ok(false)
    };
    Ok(match strategy {
        SyncStrategy::Overwrite => false,
        SyncStrategy::Timestamp => timestamp()?,
        SyncStrategy::ETag => {
            if matches!((&s.etag, &t.etag), (Some(a), Some(b)) if a != b) {
                return Ok(false);
            }
            let single_part = |e: &Option<String>| e.clone().filter(|e| !e.contains('-'));
            match (single_part(&s.etag), single_part(&t.etag)) {
                (Some(a), Some(b)) => a == b,
                // A local (or in-memory) file against an object: its MD5.
                (None, Some(b))
                    if matches!(src, Node::Local(_) | Node::Mem(_))
                        && s.size == t.size
                        && b.len() == 32
                        && b.bytes().all(|c| c.is_ascii_hexdigit()) =>
                {
                    md5_of(src)? == b.to_ascii_lowercase()
                }
                (Some(a), None)
                    if matches!(dst, Node::Local(_) | Node::Mem(_))
                        && s.size == t.size
                        && a.len() == 32
                        && a.bytes().all(|c| c.is_ascii_hexdigit()) =>
                {
                    md5_of(dst)? == a.to_ascii_lowercase()
                }
                _ => s.size == t.size && (s.etag.is_some() && s.etag == t.etag || timestamp()?),
            }
        }
    })
}

/// The MD5 digest of bytes as lowercase hex: the entity tag S3 gives an object uploaded in one
/// request.
pub fn md5_hex(data: &[u8]) -> String {
    let mut h = md5::Md5::new();
    h.update(data);
    h.finish().iter().map(|b| format!("{b:02x}")).collect()
}

fn md5_of(node: &Node) -> Result<String> {
    struct Hasher(md5::Md5);
    impl Write for Hasher {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut h = Hasher(md5::Md5::new());
    read_into(node, &mut h)?;
    Ok(h.0.finish().iter().map(|b| format!("{b:02x}")).collect())
}

/// Deletes a file, or with `recursive` a directory and everything below it, as `gdal vsi delete`
/// does. Refuses dangerous targets (see the [module documentation](self)).
pub fn delete(path: &str, recursive: bool) -> Result<Report> {
    let _cache = HtmlCacheScope::new();
    let node = Node::parse(path)?;
    check_deletable(path, &node, "delete")?;
    let mut report = Report::default();
    // Not followed: a symbolic link is deleted, not what it points to.
    if let Node::Local(p) = &node {
        let meta = std::fs::symlink_metadata(p).map_err(|e| io_context(e, path))?;
        if meta.is_dir() {
            if !recursive {
                return std::fs::remove_dir(p).map(|()| Report { deleted: 1, ..report }).map_err(|e| {
                    Error::InvalidArgument(format!(
                        "cannot delete '{path}': {e} (a directory: use the recursive option to delete what it holds)"
                    ))
                });
            }
            let root = std::fs::canonicalize(p)?;
            delete_local_tree(p, &root, &mut report)?;
        } else {
            std::fs::remove_file(p)?;
            report.deleted += 1;
        }
        return Ok(report);
    }
    let stat = stat_node(&node)?.ok_or_else(|| not_found(path))?;
    if !stat.dir && !(recursive && node.is_remote()) {
        delete_file(&node)?;
        report.deleted += 1;
        return Ok(report);
    }
    if !recursive {
        return Err(Error::InvalidArgument(format!(
            "cannot delete '{}': a directory (use the recursive option to delete what it holds)",
            listing::redact_url(path)
        )));
    }
    // Every file below `path/`, never a sibling sharing its name as a prefix.
    let entries = read_dir(&node, true, None)?;
    if !stat.dir && entries.is_empty() {
        return Err(Error::InvalidArgument(
            "recursive deletion found no directory entries; delete the exact object without the recursive option".into(),
        ));
    }
    // Validate the whole listing before deleting anything: HTTP clients can normalize
    // dot segments, which would otherwise address a sibling outside the requested prefix.
    if entries.iter().any(|e| unsafe_child_name(&e.name, false)) {
        return Err(Error::InvalidArgument("refusing dot segments or absolute names in recursive deletion".into()));
    }
    for e in &entries {
        if !e.is_dir() {
            delete_file(&node.join(&e.name))?;
            report.deleted += 1;
        }
    }
    // S3 directory markers (zero-byte `prefix/` keys, listed with a size): deepest first, then
    // the directory's own.
    if let Node::S3 { bucket, key } = &node {
        let mut markers: Vec<String> =
            entries.iter().filter(|e| e.is_dir() && e.size.is_some()).map(|e| format!("{key}/{}/", e.name)).collect();
        markers.reverse();
        markers.push(format!("{key}/"));
        for marker in markers {
            delete_file(&Node::S3 { bucket: bucket.clone(), key: marker })?;
        }
    }
    // An exact object that coexists with keys below it is kept, as GDAL keeps it: recursive
    // deletion removes the directory `path/`, and the object is deleted without `-r`.
    Ok(report)
}

fn delete_file(node: &Node) -> Result<()> {
    match node {
        Node::Local(p) => Ok(std::fs::remove_file(p)?),
        Node::Mem(k) => {
            crate::memfs::remove_file(Path::new(k));
            Ok(())
        }
        #[cfg(remote)]
        Node::S3 { .. } | Node::Url { .. } => {
            crate::http::store::delete(&node.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?)
        }
        #[cfg(not(remote))]
        Node::S3 { .. } | Node::Url { .. } => Err(Error::Unsupported(remote_disabled_message())),
        Node::Archive { .. } => Err(Error::Unsupported("archives are read only".into())),
    }
}

/// Counts a stable tree, then lets the standard library remove it using its
/// platform symlink-race protection. The discovery walk itself never deletes.
fn delete_local_tree(dir: &Path, root: &Path, report: &mut Report) -> Result<()> {
    let entries = count_local_tree(dir, root)?;
    std::fs::remove_dir_all(dir)?;
    report.deleted += entries;
    Ok(())
}

fn count_local_tree(dir: &Path, root: &Path) -> Result<u64> {
    let mut count = 1;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            let canonical = std::fs::canonicalize(&path)?;
            if !canonical.starts_with(root) {
                return Err(Error::InvalidArgument(format!("refusing to inspect '{}': outside '{}'", path.display(), root.display())));
            }
            count += count_local_tree(&path, root)?;
        } else {
            // Count the entry itself, including symbolic links; do not open its target.
            count += 1;
        }
    }
    Ok(count)
}

/// Copy-delete moves cannot represent symlinks across all supported stores. Refuse
/// unsupported local entries before copying, so a later recursive delete cannot lose them.
fn check_copy_move_source(node: &Node) -> Result<()> {
    if let Node::Local(path) = node {
        fn check(path: &Path) -> Result<()> {
            let meta = std::fs::symlink_metadata(path)?;
            if meta.file_type().is_symlink() {
                return Err(Error::Unsupported(
                    "copy-delete moves of symbolic links are not supported; use a local filesystem rename".into(),
                ));
            }
            if !meta.is_dir() && !meta.is_file() {
                return Err(Error::Unsupported(format!("'{}': copy-delete moves require regular files and directories", path.display())));
            }
            if meta.is_dir() {
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    if entry.file_name().to_str().is_none() {
                        return Err(Error::InvalidArgument("move source contains a non-UTF-8 name".into()));
                    }
                    check(&entry.path())?;
                }
            }
            Ok(())
        }
        check(path)?;
    }
    Ok(())
}

/// Moves (renames) a file or directory, as `gdal vsi move` does: into `dst` when it is an existing
/// directory, to `dst` otherwise. Local moves on one file system are renames; others copy, then
/// delete the source only once everything was copied. Existing destination files are
/// replaced, as GDAL does; existing destination directories are refused. Copy-delete
/// fallbacks refuse symbolic links and non-UTF-8 names they cannot faithfully preserve.
/// Sources must not be changed concurrently during a copy-delete move.
pub fn rename(src: &str, dst: &str) -> Result<Report> {
    let _cache = HtmlCacheScope::new();
    let src_node = Node::parse(src)?;
    let dst_node = Node::parse(dst)?;
    check_deletable(src, &src_node, "move")?;
    let stat = stat_node(&src_node)?.ok_or_else(|| not_found(src))?;
    let target = copy_target(&src_node, dst, &dst_node)?;
    if is_same(&src_node, &target) {
        return Err(Error::InvalidArgument(format!("'{}' and '{}' are the same", listing::redact_url(src), listing::redact_url(dst))));
    }
    if stat.dir && is_within(&target, &src_node) {
        return Err(Error::InvalidArgument(format!(
            "cannot move '{}' into itself ('{}')",
            listing::redact_url(src),
            listing::redact_url(dst)
        )));
    }
    if stat_node(&target)?.is_some_and(|t| t.dir) {
        return Err(Error::InvalidArgument(format!("'{}' is a directory that exists", display_node(&target))));
    }
    if let (Node::Local(a), Node::Local(b)) = (&src_node, &target) {
        match std::fs::symlink_metadata(b) {
            Ok(meta) if !meta.file_type().is_file() && !meta.file_type().is_symlink() => {
                return Err(Error::Unsupported(format!("'{}': refusing to replace a non-regular file", b.display())));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(parent) = b.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::rename(a, b) {
            Ok(()) => {
                return Ok(Report {
                    copied: u64::from(!stat.dir),
                    bytes: if stat.dir { 0 } else { stat.size.unwrap_or(0) },
                    ..Default::default()
                });
            }
            // Across file systems: copied, then deleted, below.
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(Error::Io(std::io::Error::new(e.kind(), format!("moving '{src}' to '{}': {e}", b.display())))),
        }
    }
    if let (Node::Mem(a), Node::Mem(b)) = (&src_node, &target) {
        let mut report = Report::default();
        if !stat.dir {
            let bytes = crate::memfs::read(Path::new(a))?;
            report.bytes = bytes.len() as u64;
            crate::memfs::write(Path::new(b), bytes)?;
            crate::memfs::remove_file(Path::new(a));
            report.copied = 1;
            return Ok(report);
        }
    }
    // Copy, then delete what was copied. Local rename above preserves native links/names;
    // the fallback must reject anything its portable copy cannot faithfully represent.
    check_copy_move_source(&src_node)?;
    let mut report = Report::default();
    if stat.dir {
        copy_tree(&src_node, &target, false, None, &mut report)?;
    } else {
        report.bytes += copy_file(&src_node, &target, stat.size)?;
        report.copied += 1;
    }
    let deleted = delete(src, stat.dir)?;
    report.deleted = deleted.deleted;
    Ok(report)
}

/// Synchronizes `dst` with `src`, as `gdal vsi sync` does: copies the files that are missing or
/// out of date in the target, and deletes nothing. A source directory named with a trailing `/`
/// has its contents synchronized into `dst`; without one, it is synchronized to `dst/<name>`.
pub fn sync(src: &str, dst: &str, options: &SyncOptions) -> Result<Report> {
    let _cache = HtmlCacheScope::new();
    let src_node = Node::parse(src)?;
    let dst_node = Node::parse(dst)?;
    let stat = stat_node(&src_node)?.ok_or_else(|| not_found(src))?;
    let mut report = Report::default();
    if !stat.dir {
        let target = copy_target(&src_node, dst, &dst_node)?;
        let s = stat.clone().entry(String::new());
        if let Some(t) = stat_node(&target)?.filter(|t| !t.dir).map(|t| t.entry(String::new()))
            && up_to_date(&src_node, &s, &target, &t, options.strategy)?
        {
            report.unchanged += 1;
            return Ok(report);
        }
        report.bytes += copy_file(&src_node, &target, stat.size)?;
        report.copied += 1;
        return Ok(report);
    }
    let target = if src.trim().ends_with('/') { dst_node.clone() } else { dst_node.join(&src_node.file_name()) };
    if is_within(&target, &src_node) {
        return Err(Error::InvalidArgument(format!(
            "cannot synchronize '{}' into itself ('{}')",
            listing::redact_url(src),
            listing::redact_url(dst)
        )));
    }
    copy_tree(&src_node, &target, false, Some(options), &mut report)?;
    Ok(report)
}

/// Whether a location is remote (an object store or HTTP server, possibly inside an archive).
pub fn is_remote(path: &str) -> bool {
    match Node::parse(path) {
        Ok(Node::Archive { archive, .. }) => archive.is_remote(),
        Ok(n) => n.is_remote(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests;
