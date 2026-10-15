//! Remote files read by a caller that fetches their bytes itself, for runtimes where reads cannot
//! block on the network (WebAssembly in browsers and Node.js, where `fetch` is asynchronous).
//!
//! A [`SparseFile`] holds the blocks of a file fetched so far. Readers that need bytes it does
//! not hold fail, and the blocks they missed are recorded: the caller fetches them
//! ([`SparseFile::missing`], [`SparseFile::insert`]) and runs the read again, until it needs
//! nothing more. Readers announce the ranges they are about to read (the tiles of a window, the
//! chunks of an octree node) with [`SparseFile::prefetch`], so that a read costs a round trip or
//! two rather than one per block.
//!
//! ```
//! use terrio::sparse::SparseFile;
//! use std::io::{Read, Seek, SeekFrom};
//!
//! let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
//! let file = SparseFile::new(data.len() as u64);
//! let read = |mut f: SparseFile| -> std::io::Result<Vec<u8>> {
//!     f.seek(SeekFrom::Start(150_000))?;
//!     let mut buf = vec![0; 100];
//!     f.read_exact(&mut buf)?;
//!     Ok(buf)
//! };
//! let bytes = loop {
//!     match read(file.clone()) {
//!         Ok(b) if !file.has_missing() => break b,
//!         result => {
//!             // Fetch what was missed (here, from memory), then read again.
//!             let missing = file.missing();
//!             assert!(!missing.is_empty(), "{result:?}");
//!             for (offset, len) in missing {
//!                 file.insert(offset, &data[offset as usize..(offset + len) as usize]);
//!             }
//!         }
//!     }
//! };
//! assert_eq!(bytes, data[150_000..150_100]);
//! ```

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex, MutexGuard};

/// The block size by default: 64 KiB, as GDAL's `/vsicurl/` reads ahead.
pub const DEFAULT_BLOCK_SIZE: u64 = 64 * 1024;

/// Runs of missing blocks separated by at most this many held blocks are fetched together.
const MERGE_GAP: u64 = 2;

/// A function told the byte ranges `(offset, length)` a reader is about to read, which fails
/// when the caller must fetch some first (see [`SparseFile::prefetch`]).
pub type Prefetch<'a> = &'a dyn Fn(&[(u64, u64)]) -> crate::Result<()>;

/// The error of a read that needs bytes not fetched yet.
#[derive(Debug)]
pub struct NotFetched {
    /// The first byte the read needed.
    pub offset: u64,
}

impl std::fmt::Display for NotFetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bytes at {} are not fetched yet", self.offset)
    }
}

impl std::error::Error for NotFetched {}

/// A file of a known length whose blocks are fetched by the caller: see the [module](self)
/// documentation. `Read` and `Seek` read the blocks held, and fail on others, recording them.
///
/// Clones share the blocks, each with its own position.
#[derive(Clone)]
pub struct SparseFile {
    shared: Arc<Mutex<Shared>>,
    len: u64,
    pos: u64,
}

impl std::fmt::Debug for SparseFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SparseFile").field("len", &self.len).field("pos", &self.pos).finish_non_exhaustive()
    }
}

struct Shared {
    block_size: u64,
    blocks: HashMap<u64, Vec<u8>>,
    /// Blocks held, oldest first, for [`SparseFile::trim`].
    order: VecDeque<u64>,
    /// Blocks reads needed and did not find, sorted when taken.
    missing: Vec<u64>,
    /// Bytes inserted, in total.
    bytes: u64,
    /// Calls to [`SparseFile::insert`].
    inserts: u64,
}

impl SparseFile {
    /// A file of `len` bytes, none of them fetched yet, in blocks of [`DEFAULT_BLOCK_SIZE`].
    pub fn new(len: u64) -> Self {
        Self::with_block_size(len, DEFAULT_BLOCK_SIZE)
    }

    /// A file of `len` bytes in blocks of `block_size` bytes.
    pub fn with_block_size(len: u64, block_size: u64) -> Self {
        let shared = Shared {
            block_size: block_size.max(1),
            blocks: HashMap::new(),
            order: VecDeque::new(),
            missing: Vec::new(),
            bytes: 0,
            inserts: 0,
        };
        Self { shared: Arc::new(Mutex::new(shared)), len, pos: 0 }
    }

    /// The file's length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The block size in bytes.
    pub fn block_size(&self) -> u64 {
        self.lock().block_size
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        // A panic while holding the lock leaves the blocks consistent: keep using them.
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores fetched bytes, `data` starting at byte `offset`. The blocks it covers whole are
    /// kept (and the file's last block, which may be short).
    pub fn insert(&self, offset: u64, data: &[u8]) {
        let mut s = self.lock();
        let bs = s.block_size;
        s.bytes += data.len() as u64;
        s.inserts += 1;
        let mut b = offset.div_ceil(bs);
        loop {
            let start = b * bs;
            let end = (start + bs).min(self.len);
            if start >= self.len || end > offset + data.len() as u64 {
                break;
            }
            let chunk = data[(start - offset) as usize..(end - offset) as usize].to_vec();
            if s.blocks.insert(b, chunk).is_none() {
                s.order.push_back(b);
            }
            b += 1;
        }
    }

    /// Whether a read missed blocks since [`SparseFile::missing`] was last called.
    pub fn has_missing(&self) -> bool {
        !self.lock().missing.is_empty()
    }

    /// The byte ranges `(offset, length)` reads needed and did not find since the last call, made
    /// of whole blocks, nearby runs merged; the record is cleared. Fetch them, [insert](Self::insert)
    /// them, and read again.
    pub fn missing(&self) -> Vec<(u64, u64)> {
        let mut s = self.lock();
        let mut blocks = std::mem::take(&mut s.missing);
        blocks.sort_unstable();
        blocks.dedup();
        blocks.retain(|b| !s.blocks.contains_key(b));
        let bs = s.block_size;
        let mut out = Vec::new();
        let mut i = 0;
        while i < blocks.len() {
            let (start, mut end) = (blocks[i], blocks[i]);
            while i + 1 < blocks.len() && blocks[i + 1] <= end + 1 + MERGE_GAP {
                i += 1;
                end = blocks[i];
            }
            i += 1;
            let first = start * bs;
            let last = ((end + 1) * bs).min(self.len);
            out.push((first, last - first));
        }
        out
    }

    /// Records the blocks of byte ranges `(offset, length)` not held yet, and fails when there are
    /// some: readers call it before reading ranges, so that they are all fetched in one go.
    pub fn prefetch(&self, ranges: &[(u64, u64)]) -> io::Result<()> {
        let mut s = self.lock();
        let bs = s.block_size;
        let mut first_missing = None;
        for &(off, len) in ranges {
            if len == 0 || off >= self.len {
                continue;
            }
            let end = off.saturating_add(len).min(self.len);
            for b in off / bs..=(end - 1) / bs {
                if !s.blocks.contains_key(&b) {
                    s.missing.push(b);
                    first_missing.get_or_insert(b * bs);
                }
            }
        }
        match first_missing {
            Some(offset) => Err(io::Error::other(NotFetched { offset })),
            None => Ok(()),
        }
    }

    /// The bytes inserted so far, and the number of insertions (fetches).
    pub fn fetched(&self) -> (u64, u64) {
        let s = self.lock();
        (s.bytes, s.inserts)
    }

    /// Drops the oldest blocks until at most `max_bytes` are held. Call it between reads, not
    /// while one is retried.
    pub fn trim(&self, max_bytes: u64) {
        let mut s = self.lock();
        let max_blocks = (max_bytes / s.block_size) as usize;
        while s.order.len() > max_blocks
            && let Some(old) = s.order.pop_front()
        {
            s.blocks.remove(&old);
        }
    }

    /// Copies the bytes at `pos` into `buf`, or records the blocks missing and fails.
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        if pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let end = (pos + buf.len() as u64).min(self.len);
        let mut s = self.lock();
        let bs = s.block_size;
        // Read what is held from `pos` on; stop at the first missing block.
        let mut at = pos;
        while at < end {
            let b = at / bs;
            let Some(block) = s.blocks.get(&b) else { break };
            let off = (at - b * bs) as usize;
            let n = ((end - at) as usize).min(block.len().saturating_sub(off));
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short block"));
            }
            let dst = (at - pos) as usize;
            buf[dst..dst + n].copy_from_slice(&block[off..off + n]);
            at += n as u64;
        }
        if at > pos {
            return Ok((at - pos) as usize);
        }
        // Nothing held at `pos`: record every block of the read, not only the first.
        let missing: Vec<u64> = (pos / bs..=(end - 1) / bs).filter(|b| !s.blocks.contains_key(b)).collect();
        s.missing.extend(missing);
        Err(io::Error::other(NotFetched { offset: pos }))
    }

    /// Reads `len` bytes at `offset`.
    pub fn read_exact_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let n = self.read_at(offset + done as u64, &mut out[done..])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end of the file"));
            }
            done += n;
        }
        Ok(out)
    }
}

impl Read for SparseFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SparseFile {
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

impl parquet::file::reader::Length for SparseFile {
    fn len(&self) -> u64 {
        self.len
    }
}

impl parquet::file::reader::ChunkReader for SparseFile {
    type T = SparseFile;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        let mut r = self.clone();
        r.pos = start;
        Ok(r)
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<bytes::Bytes> {
        self.prefetch(&[(start, length as u64)]).map_err(|e| parquet::errors::ParquetError::External(Box::new(e)))?;
        let b = self.read_exact_at(start, length).map_err(|e| parquet::errors::ParquetError::External(Box::new(e)))?;
        Ok(bytes::Bytes::from(b))
    }
}

/// Runs `read` until it no longer misses blocks, fetching them with `fetch(offset, length)` in
/// between: the synchronous form of what callers with an asynchronous fetch do. At most `rounds`
/// fetch rounds.
pub fn read_with<T>(
    file: &SparseFile,
    rounds: usize,
    mut fetch: impl FnMut(u64, u64) -> io::Result<Vec<u8>>,
    mut read: impl FnMut() -> crate::Result<T>,
) -> crate::Result<T> {
    for _ in 0..rounds {
        let result = read();
        let missing = file.missing();
        if missing.is_empty() {
            return result;
        }
        for (offset, len) in missing {
            let data = fetch(offset, len)?;
            file.insert(offset, &data);
        }
    }
    Err(crate::Error::InvalidData(format!("the read still needed more bytes after {rounds} fetch rounds")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Vec<u8> {
        (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn reads_after_fetching_what_was_missed() {
        let d = data();
        let f = SparseFile::with_block_size(d.len() as u64, 4096);
        let mut r = f.clone();
        r.seek(SeekFrom::Start(10_000)).unwrap();
        let mut buf = vec![0; 10_000];
        let e = r.read_exact(&mut buf).unwrap_err();
        assert!(e.get_ref().is_some_and(|e| e.is::<NotFetched>()));
        // Blocks 2 to 4, in one range.
        assert_eq!(f.missing(), [(8192, 3 * 4096)]);
        assert!(!f.has_missing());
        f.insert(8192, &d[8192..20480]);
        r.seek(SeekFrom::Start(10_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf, d[10_000..20_000]);
        assert_eq!(f.fetched(), (12288, 1));
    }

    #[test]
    fn prefetch_merges_and_clips() {
        let d = data();
        let f = SparseFile::with_block_size(d.len() as u64, 4096);
        f.insert(12 * 4096, &d[12 * 4096..13 * 4096]);
        // Blocks 10-11 and 13 merge across the held block 12; block 40 is apart; the last block is
        // short.
        assert!(f.prefetch(&[(40_960, 5000), (13 * 4096 + 5, 10), (40 * 4096, 100), (299_990, 100)]).is_err());
        let last = 73 * 4096;
        assert_eq!(f.missing(), [(40_960, 4 * 4096), (40 * 4096, 4096), (last, d.len() as u64 - last)]);
        f.insert(last, &d[last as usize..]);
        assert_eq!(f.read_exact_at(299_990, 10).unwrap(), d[299_990..]);
        assert!(f.prefetch(&[(last, 5)]).is_ok());
    }

    #[test]
    fn read_with_fetches_until_done() {
        let d = data();
        let f = SparseFile::with_block_size(d.len() as u64, 1024);
        let mut fetches = 0;
        let out = read_with(
            &f,
            10,
            |o, n| {
                fetches += 1;
                Ok(d[o as usize..(o + n) as usize].to_vec())
            },
            || {
                // Two dependent reads: the second offset comes from the first read.
                let a = f.read_exact_at(100, 4)?;
                let b = f.read_exact_at(200_000 + a[0] as u64, 8)?;
                Ok(b)
            },
        )
        .unwrap();
        assert_eq!(out, d[200_000 + d[100] as usize..][..8]);
        assert_eq!(fetches, 2);
        f.trim(4096);
        assert!(f.lock().blocks.len() <= 4);
    }
}
