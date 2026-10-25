//! Optional grid transport supplied by the embedding application.
//!
//! The engine only requests ordinary CDN URLs. Readers must support seeking without
//! materializing the entire resource, so TIFF grids retain lazy block decoding.

use std::io::{self, Read, Seek};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

/// A seekable grid stream. Native implementations may issue HTTP range requests.
pub trait GridStream: Read + Seek + Send {}
impl<T: Read + Seek + Send> GridStream for T {}

/// Network I/O for remote datum grids. A missing URL should return `NotFound`.
pub trait GridTransport: Send + Sync {
    /// Open a seekable resource without downloading all its pixel blocks.
    fn open(&self, url: &str) -> io::Result<Box<dyn GridStream>>;

    /// Download a whole grid when explicitly requested or when range opening fails.
    /// The destination is a caller-owned scratch path and may contain partial bytes
    /// on error. The engine removes it on failure and renames it only after success.
    fn download(&self, url: &str, path: &Path) -> io::Result<()> {
        let mut source = self.open(url)?;
        let mut output = std::fs::File::create(path)?;
        io::copy(&mut source, &mut output)?;
        Ok(())
    }
}

fn transport() -> &'static RwLock<Option<Arc<dyn GridTransport>>> {
    static TRANSPORT: OnceLock<RwLock<Option<Arc<dyn GridTransport>>>> = OnceLock::new();
    TRANSPORT.get_or_init(|| RwLock::new(None))
}

/// Set the process-wide grid transport. Existing open streams remain valid.
/// Search paths, registered grids and the decoded grid cache are shared by all callers.
pub fn set_grid_transport(value: Arc<dyn GridTransport>) {
    *transport().write().unwrap_or_else(|e| e.into_inner()) = Some(value);
    crate::reset_grid_cache();
}

/// Install an embedding application's default without replacing caller configuration.
pub fn set_default_grid_transport(value: Arc<dyn GridTransport>) {
    let inserted = {
        let mut current = transport().write().unwrap_or_else(|e| e.into_inner());
        if current.is_some() {
            false
        } else {
            *current = Some(value);
            true
        }
    };
    if inserted {
        crate::reset_grid_cache();
    }
}

fn current() -> io::Result<Arc<dyn GridTransport>> {
    transport()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no remote grid transport configured"))
}

pub(crate) fn open(url: &str) -> io::Result<Box<dyn GridStream>> {
    current()?.open(url)
}

pub(crate) fn download(url: &str, path: &Path) -> io::Result<()> {
    current()?.download(url, path)
}
