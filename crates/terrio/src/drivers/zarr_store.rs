//! Where the keys of a Zarr store are read from: a local directory, or a prefix of remote objects
//! (HTTP(S) URLs, `s3://`, `gs://` and `az://` objects, GDAL's `/vsicurl/`, `/vsis3/`, `/vsigs/`
//! and `/vsiaz/` names), read in place: metadata documents when first needed (all at once when
//! the store has consolidated metadata), chunks one by one, or byte ranges of them (the index and
//! inner chunks of a shard).

use crate::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A part of an object: the whole object, `length` bytes from `offset`, or the last `n` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Range {
    Whole,
    At(u64, u64),
    Suffix(u64),
}

enum Backend {
    Local(PathBuf),
    #[cfg(remote)]
    Remote {
        base: RemoteBase,
        fetcher: Box<crate::http::ObjectFetcher>,
    },
}

/// The prefix remote keys are appended to.
#[cfg(remote)]
#[derive(Debug, Clone)]
enum RemoteBase {
    /// An URL without its query, and the query (with its `?`, or empty) added to every key.
    Url {
        prefix: String,
        query: String,
    },
    S3 {
        bucket: String,
        prefix: String,
    },
}

/// The metadata documents a store's consolidated metadata holds.
const METADATA_FILES: [&str; 4] = [".zarray", ".zattrs", ".zgroup", "zarr.json"];

/// A Zarr store.
pub(crate) struct Store {
    backend: Backend,
    /// The store's name, for messages: its path or URL.
    name: String,
    /// The metadata documents by key (`a/b/.zarray`, `a/b/zarr.json`), when the store has
    /// consolidated metadata: they are not looked for elsewhere.
    consolidated: Option<HashMap<String, Value>>,
    /// Metadata documents read so far (`None` for missing ones).
    cache: Mutex<HashMap<String, Option<Value>>>,
    /// The child names listed so far of remote keys.
    #[cfg(remote)]
    listings: Mutex<HashMap<String, Vec<String>>>,
}

/// A node (group, array or object) of a store: its key, `""` for the store's root.
#[derive(Clone)]
pub(crate) struct Node {
    store: Arc<Store>,
    key: String,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display())
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.store, &other.store) && self.key == other.key
    }
}

fn join_key(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if rel.starts_with('/') { Vec::new() } else { base.split('/').filter(|s| !s.is_empty()).collect() };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Whether a local directory is a Zarr node.
fn local_node(p: &Path) -> bool {
    crate::memfs::is_dir(p) && METADATA_FILES.iter().any(|f| crate::memfs::is_file(&p.join(f)))
}

impl Store {
    fn new(backend: Backend, name: String) -> Self {
        Self {
            backend,
            name,
            consolidated: None,
            cache: Mutex::new(HashMap::new()),
            #[cfg(remote)]
            listings: Mutex::new(HashMap::new()),
        }
    }

    /// Reads a key: `None` when there is no such object.
    pub(crate) fn get(&self, key: &str, range: Range) -> Result<Option<Vec<u8>>> {
        match &self.backend {
            Backend::Local(root) => {
                use std::io::{Read, Seek, SeekFrom};
                let p = if key.is_empty() { root.clone() } else { root.join(key) };
                if crate::memfs::is_virtual(&p) {
                    // In memory: the whole file, then the range of it.
                    let Ok(all) = crate::memfs::read(&p) else { return Ok(None) };
                    let len = all.len() as u64;
                    let (start, end) = match range {
                        Range::Whole => (0, len),
                        Range::At(offset, n) => (offset.min(len), offset.saturating_add(n).min(len)),
                        Range::Suffix(n) => (len.saturating_sub(n), len),
                    };
                    return Ok(Some(all[start as usize..end as usize].to_vec()));
                }
                let mut f = match std::fs::File::open(&p) {
                    Ok(f) if p.is_file() => f,
                    Ok(_) => return Ok(None),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
                let mut out = Vec::new();
                match range {
                    Range::Whole => {
                        f.read_to_end(&mut out)?;
                    }
                    Range::At(offset, len) => {
                        f.seek(SeekFrom::Start(offset))?;
                        f.take(len).read_to_end(&mut out)?;
                    }
                    Range::Suffix(n) => {
                        let size = f.metadata()?.len();
                        f.seek(SeekFrom::Start(size.saturating_sub(n)))?;
                        f.read_to_end(&mut out)?;
                    }
                }
                Ok(Some(out))
            }
            #[cfg(remote)]
            Backend::Remote { base, fetcher } => {
                use crate::http::{ByteRange, Remote};
                let remote = match base {
                    RemoteBase::Url { prefix, query } => {
                        Remote::Url(if key.is_empty() { format!("{prefix}{query}") } else { format!("{prefix}/{key}{query}") })
                    }
                    RemoteBase::S3 { bucket, prefix } => Remote::S3(crate::s3::S3Object {
                        bucket: bucket.clone(),
                        key: if prefix.is_empty() { key.to_string() } else { format!("{prefix}/{key}") },
                    }),
                };
                let range = match range {
                    Range::Whole => ByteRange::Whole,
                    Range::At(o, l) => ByteRange::At(o, l),
                    Range::Suffix(n) => ByteRange::Suffix(n),
                };
                fetcher.get(&remote, range)
            }
        }
    }

    /// The "subdirectories" of a key of a remote store, as its object store lists them (cached).
    #[cfg(remote)]
    fn list(&self, base: &RemoteBase, key: &str) -> Vec<String> {
        use crate::http::Directory;
        if let Some(v) = self.listings.lock().ok().and_then(|l| l.get(key).cloned()) {
            return v;
        }
        let join = |prefix: &str| match (prefix.is_empty(), key.is_empty()) {
            (_, true) => prefix.to_string(),
            (true, false) => key.to_string(),
            (false, false) => format!("{prefix}/{key}"),
        };
        let dir = match base {
            RemoteBase::Url { prefix, query } => Directory::Url(format!("{}{query}", join(prefix))),
            RemoteBase::S3 { bucket, prefix } => Directory::S3 { bucket: bucket.clone(), prefix: join(prefix) },
        };
        // A store that cannot be listed has no children found this way.
        let names = crate::http::list_subdirectories(&dir).ok().flatten().unwrap_or_default();
        if let Ok(mut l) = self.listings.lock() {
            l.insert(key.to_string(), names.clone());
        }
        names
    }

    fn is_local(&self) -> bool {
        matches!(self.backend, Backend::Local(_))
    }

    /// A metadata document (JSON), from the consolidated metadata or read and cached. A read
    /// that fails (a network error) or a document that is not JSON is an error, and is not
    /// cached: only missing documents are.
    fn json(&self, key: &str) -> Result<Option<Value>> {
        if let Some(c) = &self.consolidated
            && let Some(v) = c.get(key)
        {
            return Ok(Some(v.clone()));
        }
        // Consolidated metadata lists every node of a remote store: the rest is not looked for.
        if self.consolidated.is_some() && !self.is_local() {
            return Ok(None);
        }
        if let Some(v) = self.cache.lock().ok().and_then(|c| c.get(key).cloned()) {
            return Ok(v);
        }
        let v = match self.get(key, Range::Whole)? {
            Some(b) => Some(
                serde_json::from_slice::<Value>(&b)
                    .map_err(|e| crate::Error::InvalidData(format!("Zarr: {key} of {} is not valid JSON: {e}", self.name)))?,
            ),
            None => None,
        };
        if let Ok(mut c) = self.cache.lock() {
            c.insert(key.to_string(), v.clone());
        }
        Ok(v)
    }

    /// A metadata document looked for (that may not be there): one that is not JSON counts as
    /// missing, as some servers answer unknown paths with a page; read failures are errors.
    fn probe(&self, key: &str) -> Result<Option<Value>> {
        match self.json(key) {
            Err(crate::Error::InvalidData(_)) => Ok(None),
            r => r,
        }
    }

    /// Reads the consolidated metadata at the root, when there is some: v2's `.zmetadata`, or the
    /// `consolidated_metadata` of a v3 root group.
    fn load_consolidated(&mut self) -> Result<()> {
        let mut map = HashMap::new();
        if let Some(m) = self.probe(".zmetadata")?
            && let Some(meta) = m["metadata"].as_object()
        {
            for (k, v) in meta {
                map.insert(k.trim_start_matches('/').to_string(), v.clone());
            }
        } else if let Some(root) = self.probe("zarr.json")?
            && let Some(meta) = root["consolidated_metadata"]["metadata"].as_object()
            && !meta.is_empty()
        {
            let mut r = root.clone();
            if let Some(o) = r.as_object_mut() {
                o.remove("consolidated_metadata");
            }
            map.insert("zarr.json".to_string(), r);
            for (k, v) in meta {
                map.insert(join_key(k, "zarr.json"), v.clone());
            }
        }
        if !map.is_empty() {
            self.consolidated = Some(map);
        }
        Ok(())
    }
}

impl Node {
    /// Opens a local directory: the store is rooted at its outermost ancestor that is a Zarr node,
    /// so that the groups above it are found.
    pub(crate) fn local(path: &Path) -> Node {
        // Paths in memory stay as they are: on Windows, `absolute` would prefix the drive.
        let path = if crate::memfs::is_virtual(path) {
            path.to_path_buf()
        } else {
            std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
        };
        let mut root = path.clone();
        while let Some(parent) = root.parent()
            && local_node(parent)
        {
            root = parent.to_path_buf();
        }
        let key = path.strip_prefix(&root).map(|k| k.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        let mut store = Store::new(Backend::Local(root.clone()), root.display().to_string());
        // Local files are read as they are found: unreadable metadata is reported when used.
        let _ = store.load_consolidated();
        Node { store: Arc::new(store), key }
    }

    /// Opens a remote location. The store is rooted at the last path component ending in
    /// `.zarr` (or at the location itself), and the node is the rest of the path.
    #[cfg(remote)]
    pub(crate) fn remote(loc: &crate::vfs::Location) -> Result<Node> {
        use crate::vfs::Location;
        let split = |path: &str| -> (String, String) {
            let path = path.trim_end_matches('/');
            let segs: Vec<&str> = path.split('/').collect();
            match segs.iter().rposition(|s| s.to_ascii_lowercase().ends_with(".zarr")) {
                Some(i) => (segs[..=i].join("/"), segs[i + 1..].join("/")),
                None => (path.to_string(), String::new()),
            }
        };
        let (base, key, name) = match loc {
            Location::Url(u) => {
                let (path, query) = u.split_once('?').map_or((u.as_str(), String::new()), |(p, q)| (p, format!("?{q}")));
                let (prefix, key) = split(path);
                (RemoteBase::Url { prefix: prefix.clone(), query }, key, prefix)
            }
            Location::S3 { bucket, key } => {
                let (prefix, rest) = split(key);
                let name = format!("s3://{bucket}/{prefix}");
                (RemoteBase::S3 { bucket: bucket.clone(), prefix }, rest, name)
            }
            _ => return Err(crate::Error::InvalidArgument("not a remote location".into())),
        };
        let mut store = Store::new(Backend::Remote { base, fetcher: Box::new(crate::http::ObjectFetcher::new()) }, name);
        store.load_consolidated()?;
        let node = Node { store: Arc::new(store), key };
        let mut found = node.store.consolidated.is_some();
        for f in METADATA_FILES {
            if found {
                break;
            }
            found = node.store.probe(&join_key(&node.key, f))?.is_some();
        }
        if !found {
            return Err(crate::Error::InvalidData(format!("Zarr: no Zarr metadata at {}", node.display())));
        }
        Ok(node)
    }

    /// The node at a path relative to this one (`..` going up; a leading `/` from the root).
    pub(crate) fn join(&self, rel: &str) -> Node {
        Node { store: self.store.clone(), key: join_key(&self.key, rel) }
    }

    /// The node above, when this is not the store's root.
    pub(crate) fn parent(&self) -> Option<Node> {
        if self.key.is_empty() {
            return None;
        }
        Some(Node { store: self.store.clone(), key: self.key.rsplit_once('/').map_or("", |(p, _)| p).to_string() })
    }

    /// The node's name: the last component of its key (of the store's name at the root).
    pub(crate) fn name(&self) -> String {
        let k = if self.key.is_empty() { self.store.name.trim_end_matches('/') } else { &self.key };
        k.rsplit(['/', '\\']).next().unwrap_or_default().to_string()
    }

    /// The store's path or URL, and the node's key in it (`""` at the root).
    pub(crate) fn store_and_key(&self) -> (&str, &str) {
        (&self.store.name, &self.key)
    }

    /// The node's full path or URL, for messages.
    pub(crate) fn display(&self) -> String {
        if self.key.is_empty() { self.store.name.clone() } else { format!("{}/{}", self.store.name.trim_end_matches('/'), self.key) }
    }

    /// A metadata document of the node (`.zarray`, `zarr.json`...), `None` when missing; an
    /// error when it cannot be read.
    pub(crate) fn json(&self, file: &str) -> Result<Option<Value>> {
        self.store.json(&join_key(&self.key, file))
    }

    /// Reads an object below the node (a chunk), or part of it. `None` when it is missing.
    pub(crate) fn get(&self, rel: &str, range: Range) -> Result<Option<Vec<u8>>> {
        self.store.get(&join_key(&self.key, rel), range)
    }

    /// The names of the child nodes: the subdirectories of a local store, the nodes the
    /// consolidated metadata lists, or else the "subdirectories" an object store lists (Amazon
    /// S3, Google Cloud Storage, Azure Blob Storage; none for a plain HTTP server).
    pub(crate) fn children(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(c) = &self.store.consolidated {
            let prefix = if self.key.is_empty() { String::new() } else { format!("{}/", self.key) };
            for k in c.keys() {
                if let Some(rest) = k.strip_prefix(&prefix)
                    && let Some((child, _)) = rest.split_once('/')
                    && !out.iter().any(|o| o == child)
                {
                    out.push(child.to_string());
                }
            }
        }
        #[cfg(remote)]
        if self.store.consolidated.is_none()
            && let Backend::Remote { base, .. } = &self.store.backend
        {
            out.extend(self.store.list(base, &self.key));
        }
        // Irrefutable in WebAssembly builds, which read local stores only.
        #[allow(irrefutable_let_patterns)]
        if let Backend::Local(root) = &self.store.backend {
            let dir = if self.key.is_empty() { root.clone() } else { root.join(&self.key) };
            for (n, _) in crate::memfs::subdirs(&dir) {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        out.sort();
        out
    }

    /// Whether the store is remote.
    pub(crate) fn is_remote(&self) -> bool {
        !self.store.is_local()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_keys() {
        assert_eq!(join_key("a/b", "c"), "a/b/c");
        assert_eq!(join_key("a/b", "../c"), "a/c");
        assert_eq!(join_key("a/b", "/c/./d"), "c/d");
        assert_eq!(join_key("", "c/.zarray"), "c/.zarray");
        assert_eq!(join_key("a", ".."), "");
    }
}
