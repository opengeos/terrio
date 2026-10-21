//! Where a tile archive's bytes come from: memory, a local file, or a remote file read by HTTP
//! range requests.

use crate::Result;
use std::io::{Read, Seek, SeekFrom};

/// A source of bytes read at offsets.
pub(crate) enum ByteSource {
    /// Bytes in memory.
    Memory(Vec<u8>),
    /// A local file, and its length.
    File(std::fs::File, u64),
    /// A remote file.
    #[cfg(remote)]
    Http(crate::http::RangeReader),
    /// A remote file whose bytes the caller fetches (see [`crate::sparse`]).
    Sparse(crate::sparse::SparseFile),
}

impl ByteSource {
    /// Opens a location: a local file, or (in native builds) a URL or `s3://`, `gs://` or `az://`
    /// object read in place. Other locations (archive members) are resolved to a local copy.
    pub(crate) fn open(path: &str) -> Result<(Self, Option<crate::vfs::LocalPath>)> {
        use crate::vfs::Location;
        let loc = Location::parse(path);
        match &loc {
            #[cfg(remote)]
            Location::Url(_) | Location::S3 { .. } => {
                let remote = loc.remote().ok_or_else(|| crate::Error::InvalidArgument(format!("'{path}' is not remote")))?;
                Ok((ByteSource::Http(crate::http::RangeReader::open(&remote)?), None))
            }
            Location::Local(p) => {
                let f = std::fs::File::open(p)?;
                let len = f.metadata()?.len();
                Ok((ByteSource::File(f, len), None))
            }
            _ => {
                let local = crate::vfs::resolve(path)?;
                let f = std::fs::File::open(local.path())?;
                let len = f.metadata()?.len();
                Ok((ByteSource::File(f, len), Some(local)))
            }
        }
    }

    /// The length in bytes.
    pub(crate) fn len(&self) -> u64 {
        match self {
            ByteSource::Memory(b) => b.len() as u64,
            ByteSource::File(_, len) => *len,
            #[cfg(remote)]
            ByteSource::Http(r) => r.len(),
            ByteSource::Sparse(s) => s.len(),
        }
    }

    /// Whether the bytes come over the network.
    pub(crate) fn is_remote(&self) -> bool {
        #[cfg(remote)]
        if matches!(self, ByteSource::Http(_)) {
            return true;
        }
        matches!(self, ByteSource::Sparse(_))
    }

    /// Reads `len` bytes at `offset`.
    pub(crate) fn read_at(&mut self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let bad = || crate::Error::InvalidData(format!("read of {len} bytes at {offset} is past the end of the file"));
        match self {
            ByteSource::Memory(b) => {
                let end = offset.checked_add(len).filter(|e| *e <= b.len() as u64).ok_or_else(bad)?;
                Ok(b[offset as usize..end as usize].to_vec())
            }
            ByteSource::File(f, size) => {
                if offset.checked_add(len).is_none_or(|e| e > *size) {
                    return Err(bad());
                }
                f.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; len as usize];
                f.read_exact(&mut buf)?;
                Ok(buf)
            }
            #[cfg(remote)]
            ByteSource::Http(r) => {
                if offset.checked_add(len).is_none_or(|e| e > r.len()) {
                    return Err(bad());
                }
                r.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; len as usize];
                r.read_exact(&mut buf)?;
                Ok(buf)
            }
            ByteSource::Sparse(s) => {
                if offset.checked_add(len).is_none_or(|e| e > s.len()) {
                    return Err(bad());
                }
                Ok(s.read_exact_at(offset, len as usize)?)
            }
        }
    }

    /// Fetches byte ranges `(offset, length)` ahead of reading them, merging nearby ones.
    pub(crate) fn prefetch(&mut self, ranges: &[(u64, u64)]) -> Result<()> {
        #[cfg(remote)]
        if let ByteSource::Http(r) = self {
            r.prefetch(ranges)?;
        }
        if let ByteSource::Sparse(s) = self {
            s.prefetch(ranges)?;
        }
        Ok(())
    }
}
