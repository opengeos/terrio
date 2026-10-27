//! Files held in memory under virtual paths, for the drivers that read a directory of files (Zarr
//! stores) when the files are in memory: zip archives, and key-value stores given by WebAssembly
//! callers, and the files written under `/vsimem/` by [`crate::vsi`]. Paths under `/vsimem/` read
//! the files in memory; other paths read the file system.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The prefix of the virtual paths of mounts, reserved: [`crate::vsi`] refuses paths below it,
/// and listings above it leave it out, so that `/vsimem/` files and mounts never meet.
const ROOT: &str = "/vsimem/.terrio-mounts";

/// The prefix of every in-memory path: GDAL's `/vsimem/`.
const VSIMEM: &str = "/vsimem";

static FILES: Mutex<BTreeMap<String, Arc<Vec<u8>>>> = Mutex::new(BTreeMap::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

/// A path as a key: `/` separators, without a trailing one.
fn key(p: &Path) -> Option<String> {
    let s = p.to_str()?.replace('\\', "/");
    // Under `/vsimem` only: not `/vsimemory`.
    (s.strip_prefix(VSIMEM).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))).then(|| s.trim_end_matches('/').to_string())
}

fn files() -> std::sync::MutexGuard<'static, BTreeMap<String, Arc<Vec<u8>>>> {
    FILES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Files mounted under a virtual directory, removed when dropped.
pub(crate) struct Mount {
    root: PathBuf,
}

impl Mount {
    /// Mounts files given by their paths relative to the mount (`a/.zarray`).
    pub(crate) fn new(entries: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        let root = format!("{ROOT}/{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let mut f = files();
        for (name, bytes) in entries {
            let name = name.replace('\\', "/");
            let name = name.trim_matches('/');
            if !name.is_empty() {
                f.insert(format!("{root}/{name}"), Arc::new(bytes));
            }
        }
        Self { root: PathBuf::from(root) }
    }

    /// The mount's directory.
    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    /// The files under the mount, by their paths relative to it, sorted.
    pub(crate) fn files(&self) -> Vec<(String, Vec<u8>)> {
        let prefix = format!("{}/", self.root.to_string_lossy());
        files()
            .range(prefix.clone()..)
            .take_while(|(f, _)| f.starts_with(&prefix))
            .map(|(f, b)| (f[prefix.len()..].to_string(), b.as_ref().clone()))
            .collect()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let prefix = format!("{}/", self.root.to_string_lossy());
        files().retain(|k, _| !k.starts_with(&prefix));
    }
}

/// Whether a path is the mounts' reserved prefix or below it.
pub(crate) fn is_reserved(p: &Path) -> bool {
    key(p).is_some_and(|k| in_mounts(&k))
}

fn in_mounts(k: &str) -> bool {
    k.strip_prefix(ROOT).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether a path is under the in-memory root (whether or not a file is there).
pub(crate) fn is_virtual(p: &Path) -> bool {
    key(p).is_some()
}

/// Reads a file, in memory or on disk.
pub(crate) fn read(p: &Path) -> std::io::Result<Vec<u8>> {
    match key(p) {
        Some(k) => files()
            .get(&k)
            .map(|b| b.as_ref().clone())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{k}: no such file"))),
        None => std::fs::read(p),
    }
}

/// Writes a file, in memory or on disk.
pub(crate) fn write(p: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    match key(p) {
        Some(k) => {
            files().insert(k, Arc::new(bytes.as_ref().to_vec()));
            Ok(())
        }
        None => std::fs::write(p, bytes),
    }
}

/// Creates a directory and its parents: on disk only, directories in memory being implied by
/// their files.
pub(crate) fn create_dir_all(p: &Path) -> std::io::Result<()> {
    match key(p) {
        Some(_) => Ok(()),
        None => std::fs::create_dir_all(p),
    }
}

/// Whether a path exists, in memory or on disk.
pub(crate) fn exists(p: &Path) -> bool {
    match key(p) {
        Some(_) => is_file(p) || is_dir(p),
        None => p.exists(),
    }
}

/// Whether a path is a file, in memory or on disk.
pub(crate) fn is_file(p: &Path) -> bool {
    match key(p) {
        Some(k) => files().contains_key(&k),
        None => p.is_file(),
    }
}

/// Whether a path is a directory, in memory or on disk.
pub(crate) fn is_dir(p: &Path) -> bool {
    match key(p) {
        Some(k) => {
            let prefix = format!("{k}/");
            files().range(prefix.clone()..).next().is_some_and(|(f, _)| f.starts_with(&prefix))
        }
        None => p.is_dir(),
    }
}

/// The subdirectories of a directory, in memory or on disk: `(name, path)`.
pub(crate) fn subdirs(p: &Path) -> Vec<(String, PathBuf)> {
    match key(p) {
        Some(k) => {
            let prefix = format!("{k}/");
            let mut names: Vec<String> = files()
                .range(prefix.clone()..)
                .take_while(|(f, _)| f.starts_with(&prefix))
                .filter_map(|(f, _)| f[prefix.len()..].split_once('/').map(|(d, _)| d.to_string()))
                .collect();
            names.dedup();
            names.into_iter().map(|n| (n.clone(), p.join(n))).collect()
        }
        None => std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
            .collect(),
    }
}

/// The files under a directory (all of them, at any depth), in memory: `(path relative to the
/// directory, size)`, sorted. `None` for a path outside `/vsimem/`.
pub(crate) fn list_files(p: &Path) -> Option<Vec<(String, u64)>> {
    let k = key(p)?;
    let prefix = format!("{k}/");
    // Mounts are listed only from within their reserved prefix.
    let mounts = in_mounts(&k);
    Some(
        files()
            .range(prefix.clone()..)
            .take_while(|(f, _)| f.starts_with(&prefix))
            .filter(|(f, _)| mounts || !in_mounts(f))
            .map(|(f, b)| (f[prefix.len()..].to_string(), b.len() as u64))
            .collect(),
    )
}

/// The size of a file in memory, if there is one.
pub(crate) fn file_size(p: &Path) -> Option<u64> {
    files().get(&key(p)?).map(|b| b.len() as u64)
}

/// Removes a file in memory; whether there was one.
pub(crate) fn remove_file(p: &Path) -> bool {
    key(p).is_some_and(|k| files().remove(&k).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_paths_end_at_a_separator() {
        assert_eq!(key(Path::new("/vsimem/terrio")).as_deref(), Some("/vsimem/terrio"));
        assert_eq!(key(Path::new("/vsimem/terrio/3/a/")).as_deref(), Some("/vsimem/terrio/3/a"));
        assert_eq!(key(Path::new("/vsimem/terrio_other/a")).as_deref(), Some("/vsimem/terrio_other/a"));
        assert_eq!(key(Path::new("/vsimemory/a")), None);
        assert_eq!(key(Path::new("/tmp/a")), None);
    }

    #[test]
    fn mounts_are_reserved_and_left_out_of_listings() {
        crate::memfs::write(Path::new("/vsimem/memfs-reserved-test/a"), b"user").unwrap();
        let m = Mount::new([("x".to_string(), b"mounted".to_vec())]);
        assert!(m.path().starts_with(ROOT) && is_reserved(&m.path().join("x")) && is_reserved(Path::new(ROOT)));
        assert!(!is_reserved(Path::new("/vsimem/.terrio-mounts2/a")) && !is_reserved(Path::new("/vsimem/terrio/1/a")));
        let all = list_files(Path::new("/vsimem")).unwrap();
        assert!(all.iter().any(|(n, _)| n == "memfs-reserved-test/a"));
        assert!(all.iter().all(|(n, _)| !n.starts_with(".terrio-mounts/")), "{all:?}");
        assert_eq!(list_files(m.path()).unwrap(), [("x".to_string(), 7)]);
        remove_file(Path::new("/vsimem/memfs-reserved-test/a"));
    }

    #[test]
    fn mounts_files() {
        let m = Mount::new([("s/.zgroup".to_string(), b"{}".to_vec()), ("s/a/.zarray".into(), b"x".to_vec()), ("s/b/0.0".into(), vec![1])]);
        let s = m.path().join("s");
        assert!(is_dir(&s) && !is_file(&s));
        assert!(is_file(&s.join(".zgroup")));
        assert_eq!(read(&s.join("a").join(".zarray")).unwrap(), b"x");
        assert_eq!(subdirs(&s).into_iter().map(|(n, _)| n).collect::<Vec<_>>(), ["a", "b"]);
        let root = m.path().to_path_buf();
        drop(m);
        assert!(!is_dir(&root.join("s")));
    }
}
