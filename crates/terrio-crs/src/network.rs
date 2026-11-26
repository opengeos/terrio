//! Optional grid transport supplied by the embedding application.
//!
//! The engine only requests ordinary CDN URLs. Readers must support seeking without
//! materializing the entire resource, so TIFF grids retain lazy block decoding.

use std::io::{self, Read, Seek};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

/// The largest grid file the engine downloads or reads, larger than any grid on the PROJ CDN
/// (the largest, `us_nga_egm08_25.tif`, is about 81 MB). A transport overriding
/// [`GridTransport::download`] should stop writing past it; the engine checks the downloaded size
/// again before the file enters the cache.
pub const MAX_GRID_DOWNLOAD_BYTES: u64 = 2 << 30;

/// A seekable grid stream. Native implementations may issue HTTP range requests.
pub trait GridStream: Read + Seek + Send {}
impl<T: Read + Seek + Send> GridStream for T {}

/// Network I/O for remote datum grids. A missing URL should return `NotFound`.
pub trait GridTransport: Send + Sync {
    /// Open a seekable resource without downloading all its pixel blocks.
    fn open(&self, url: &str) -> io::Result<Box<dyn GridStream>>;

    /// Download a whole grid when explicitly requested or when range opening fails.
    /// The destination is a caller-owned scratch path and may contain partial bytes
    /// on error. The engine removes it on failure and renames it into the cache only after
    /// success, once the file is no larger than [`MAX_GRID_DOWNLOAD_BYTES`] and reads as a grid.
    /// The default implementation stops copying a resource larger than that, with an error.
    fn download(&self, url: &str, path: &Path) -> io::Result<()> {
        let source = self.open(url)?;
        let mut output = std::fs::File::create(path)?;
        copy_bounded(source, &mut output)
    }
}

/// Copies a resource to `output`, failing once it exceeds [`MAX_GRID_DOWNLOAD_BYTES`]: no more
/// than one byte past the limit is read.
fn copy_bounded(source: impl Read, output: &mut impl io::Write) -> io::Result<()> {
    let copied = io::copy(&mut source.take(MAX_GRID_DOWNLOAD_BYTES + 1), output)?;
    if copied > MAX_GRID_DOWNLOAD_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("grid download is larger than {MAX_GRID_DOWNLOAD_BYTES} bytes")));
    }
    Ok(())
}

fn transport() -> &'static RwLock<Option<Arc<dyn GridTransport>>> {
    crate::host::before_lock();
    static TRANSPORT: OnceLock<RwLock<Option<Arc<dyn GridTransport>>>> = OnceLock::new();
    TRANSPORT.get_or_init(|| RwLock::new(None))
}

/// Holds the transport's lock across a fork (see [`crate::host::hold_for_fork`]), written: no
/// reader nor writer is left inside it in the child.
#[cfg(all(unix, not(target_family = "wasm")))]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    held.push(Box::new(transport().write().unwrap_or_else(|e| e.into_inner())));
}

/// Set the process-wide grid transport. Existing open streams remain valid.
/// Clears shared decoded-grid and missing-grid caches for subsequent lookups.
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

#[cfg(test)]
mod copy_tests {
    use super::{MAX_GRID_DOWNLOAD_BYTES, copy_bounded};
    use std::io::{self, Read};

    /// An endless stream of zeros that counts the bytes read from it.
    struct Endless(u64);
    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            buf.fill(0);
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
    }

    /// A writer that keeps only the count of the bytes written.
    struct Count(u64);
    impl io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The default download stops an endless response one byte past the limit, with an error,
    /// and copies a response within the limit whole.
    #[test]
    fn grid_downloads_stop_past_the_size_limit() {
        let mut source = Endless(0);
        let mut output = Count(0);
        let error = copy_bounded(&mut source, &mut output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(output.0, MAX_GRID_DOWNLOAD_BYTES + 1);
        assert!(source.0 <= MAX_GRID_DOWNLOAD_BYTES + 1 + (64 << 10), "{}", source.0);
        let mut output = Count(0);
        copy_bounded(&[7u8; 1000][..], &mut output).unwrap();
        assert_eq!(output.0, 1000);
    }
}

#[cfg(all(test, unix, not(target_family = "wasm")))]
mod tests {
    /// The transport's lock is one `host::hold_for_fork` takes across a fork, read or written.
    #[test]
    fn hold_for_fork_takes_the_transport_lock() {
        crate::host::assert_held_for_fork("transport (written)", |wait| {
            let _g = super::transport().write().unwrap_or_else(|e| e.into_inner());
            wait();
        });
        crate::host::assert_held_for_fork("transport (read)", |wait| {
            let _g = super::transport().read().unwrap_or_else(|e| e.into_inner());
            wait();
        });
    }
}
