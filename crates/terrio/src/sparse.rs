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

use std::collections::{HashMap, HashSet, VecDeque};
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
    /// Whether reads of blocks not held fail without recording them (see [`SparseFile::quiet`]).
    quiet: bool,
}

impl std::fmt::Debug for SparseFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SparseFile").field("len", &self.len).field("pos", &self.pos).finish_non_exhaustive()
    }
}

/// Whether block `b` of a file of `len` bytes is held whole, not only the start of it
/// ([`SparseFile::insert_arriving`]).
fn held_whole(s: &Shared, b: u64, len: u64) -> bool {
    s.blocks.get(&b).is_some_and(|block| block.len() as u64 >= (len - b * s.block_size).min(s.block_size))
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
        Self { shared: Arc::new(Mutex::new(shared)), len, pos: 0, quiet: false }
    }

    /// A clone whose reads of blocks not held fail without recording them as missing, so that
    /// the caller does not fetch them: for reads that can do without those bytes, such as the
    /// extra decoders of a GeoTIFF read, which are skipped when they cannot reopen the file.
    /// Missing chunk bytes can be retried through the primary nonquiet decoder.
    pub fn quiet(&self) -> Self {
        Self { quiet: true, ..self.clone() }
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
    /// kept (and the file's last block, which may be short); a partial block at the end of a
    /// short response is dropped, so callers check that they got the bytes they asked for.
    pub fn insert(&self, offset: u64, data: &[u8]) {
        self.store(offset, data, false);
    }

    /// Stores the bytes of a response still arriving, `data` from byte `offset` (a block's start)
    /// on: as [`insert`](Self::insert), and the start of a block not arrived whole, which serves
    /// reads of its bytes; reads past them are missing, as if nothing of that block were held, until
    /// the rest arrives. Call it again with more of the body, and [`insert`](Self::insert) it whole
    /// at the end. It counts no fetch.
    pub fn insert_arriving(&self, offset: u64, data: &[u8]) {
        self.store(offset, data, true);
    }

    fn store(&self, offset: u64, data: &[u8], arriving: bool) {
        let mut s = self.lock();
        let bs = s.block_size;
        if !arriving {
            s.bytes += data.len() as u64;
            s.inserts += 1;
        }
        let end_of_data = offset + data.len() as u64;
        let mut b = offset.div_ceil(bs);
        loop {
            let start = b * bs;
            let end = (start + bs).min(self.len);
            if start >= self.len || start >= end_of_data {
                break;
            }
            let end = if end > end_of_data {
                // The start of a block still arriving: kept only while longer than what is held.
                if !arriving || s.blocks.get(&b).is_some_and(|held| held.len() as u64 >= end_of_data - start) {
                    break;
                }
                end_of_data
            } else {
                end
            };
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
        blocks.retain(|&b| !held_whole(&s, b, self.len));
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
                if !held_whole(&s, b, self.len) {
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
        let end = pos.saturating_add(buf.len() as u64).min(self.len);
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
                if !held_whole(&s, b, self.len) {
                    // Past the bytes of a block still arriving: missing, as below.
                    break;
                }
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
        if !self.quiet {
            let missing: Vec<u64> = (pos / bs..=(end - 1) / bs).filter(|&b| !held_whole(&s, b, self.len)).collect();
            s.missing.extend(missing);
        }
        Err(io::Error::other(NotFetched { offset: pos }))
    }

    /// Reads `len` bytes at `offset`.
    pub fn read_exact_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let n = self.read_at(offset.saturating_add(done as u64), &mut out[done..])?;
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
        self.pos = seek_position(self.pos, self.len, pos)?;
        Ok(self.pos)
    }
}

/// The position a seek from `pos` moves a reader of `len` bytes at `cur` to; an error before the
/// start or past `u64::MAX`.
pub(crate) fn seek_position(cur: u64, len: u64, pos: SeekFrom) -> io::Result<u64> {
    match pos {
        SeekFrom::Start(p) => Some(p),
        SeekFrom::End(d) => len.checked_add_signed(d),
        SeekFrom::Current(d) => cur.checked_add_signed(d),
    }
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek: before the start of the file or past u64::MAX"))
}

// Row groups of remote GeoParquet files are read through the same blocks.
#[cfg(feature = "parquet")]
impl parquet::file::reader::Length for SparseFile {
    fn len(&self) -> u64 {
        self.len
    }
}

#[cfg(feature = "parquet")]
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

/// Grows each range `(offset, length)` of [`SparseFile::missing`] to at least `least` bytes, in
/// whole `block`s within a file of `len` bytes, merging those that then overlap: a header read in
/// steps (a TIFF's IFDs, one after another) takes requests that grow with what it read so far,
/// rather than a request per block or two.
pub fn grow_ranges(ranges: &[(u64, u64)], least: u64, block: u64, len: u64) -> Vec<(u64, u64)> {
    let least = least.div_ceil(block.max(1)) * block.max(1);
    // Ranges starting at or past the end hold nothing to fetch.
    let mut spans: Vec<(u64, u64)> =
        ranges.iter().filter(|&&(o, _)| o < len).map(|&(o, n)| (o, o.saturating_add(n.max(least)).min(len))).collect();
    spans.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match out.last_mut() {
            Some((_, last)) if start <= *last => *last = (*last).max(end),
            _ => out.push((start, end)),
        }
    }
    out.into_iter().map(|(start, end)| (start, end - start)).collect()
}

/// The least payload worth another server-runtime request. Small windows keep their merged
/// runs; large reads can fill the request window without multiplying requests in saturated rounds.
const MIN_PARALLEL_PIECE: u64 = 256 << 10;

/// Splits a round with fewer runs than request slots, balancing pieces across its largest runs.
/// Boundaries remain on the sparse file's block grid, with the original final partial block.
/// No bytes are added or duplicated. `None` leaves an already saturated or small round intact.
/// Long runs get pieces of at least 256 KiB (or a whole larger block); existing short ranges
/// remain intact. Ranges that overflow or do not start on the block grid remain for the
/// caller's usual range validation.
///
/// The fetching client selects where this is useful: terrio-js uses it after opening, in
/// Node.js, Deno and Bun, where extra requests are not queued behind a browser's connection cap.
/// This request-grouping policy does not change raster decoding or GDAL conformance.
pub fn parallel_ranges(ranges: &[(u64, u64)], block_size: u64) -> Option<Vec<(u64, u64)>> {
    if ranges.is_empty() || ranges.len() >= PARALLEL_REQUESTS {
        return None;
    }
    let block = block_size.max(1);
    let least_blocks = MIN_PARALLEL_PIECE.div_ceil(block).max(1);
    let mut pieces = [1usize; PARALLEL_REQUESTS];
    let mut capacity = [1usize; PARALLEL_REQUESTS];
    for (i, &(offset, len)) in ranges.iter().enumerate() {
        // Leave invalid ranges for the fetcher's existing validation, without overflowing or
        // introducing boundaries outside the block grid.
        if offset.checked_add(len).is_none() || offset % block != 0 {
            continue;
        }
        let full = len / block;
        let tail = len % block;
        capacity[i] = (full / least_blocks + u64::from(tail >= MIN_PARALLEL_PIECE)).clamp(1, PARALLEL_REQUESTS as u64) as usize;
    }
    let mut count = ranges.len();
    while count < PARALLEL_REQUESTS {
        let next = (0..ranges.len()).filter(|&i| pieces[i] < capacity[i]).max_by_key(|&i| ranges[i].1 / pieces[i] as u64);
        let Some(i) = next else { break };
        pieces[i] += 1;
        count += 1;
    }
    if count == ranges.len() {
        return None;
    }
    let mut split = Vec::with_capacity(count);
    for (i, &(offset, len)) in ranges.iter().enumerate() {
        let n = pieces[i] as u64;
        if n == 1 {
            split.push((offset, len));
            continue;
        }
        let full = len / block;
        let tail = len % block;
        let (base, extra) = (full / n, full % n);
        let mut from = offset;
        for part in 0..n {
            let bytes = (base + u64::from(part < extra)) * block + if part + 1 == n { tail } else { 0 };
            split.push((from, bytes));
            from += bytes;
        }
    }
    Some(split)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_pieces_preserve_ranges_and_partial_blocks() {
        let block = 16 << 10;
        let ranges = [(2 * block, (5 << 20) + 17), (20 << 20, 2 << 20)];
        let pieces = parallel_ranges(&ranges, block).unwrap();
        assert_eq!(pieces.len(), PARALLEL_REQUESTS);
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for &(offset, len) in &pieces {
            assert_eq!(offset % block, 0);
            assert!(len >= MIN_PARALLEL_PIECE);
            let &(start, total) = ranges.iter().find(|&&(start, total)| offset >= start && offset < start + total).unwrap();
            assert!(offset + len <= start + total);
            if offset + len < start + total {
                assert_eq!(len % block, 0);
            }
            if let Some(last) = merged.last_mut()
                && last.0 + last.1 == offset
            {
                last.1 += len;
            } else {
                merged.push((offset, len));
            }
        }
        assert_eq!(merged, ranges);
        assert!(pieces.iter().map(|&(_, len)| len).max().unwrap() < ranges[0].1);
    }

    #[test]
    fn parallel_pieces_keep_small_and_saturated_rounds_whole() {
        assert!(parallel_ranges(&[], 16384).is_none());
        assert!(parallel_ranges(&[(0, MIN_PARALLEL_PIECE * 2 - 1)], 16384).is_none());
        let ranges: Vec<_> = (0..PARALLEL_REQUESTS).map(|i| ((i as u64) * (16 << 20), 8 << 20)).collect();
        assert!(parallel_ranges(&ranges, 16384).is_none());
    }

    #[test]
    fn parallel_pieces_respect_large_blocks_and_extreme_offsets() {
        assert_eq!(parallel_ranges(&[(0, 24 << 20)], 16 << 20).unwrap(), [(0, 16 << 20), (16 << 20, 8 << 20)]);
        let block = 16384;
        let from = (u64::MAX / block - 1000) * block;
        let len = 300 * block + 7;
        let pieces = parallel_ranges(&[(from, len)], block).unwrap();
        assert_eq!(pieces[0].0, from);
        assert_eq!(pieces.last().map(|&(off, size)| off + size), Some(from + len));
        assert_eq!(pieces.iter().map(|&(_, size)| size).sum::<u64>(), len);
        assert!(parallel_ranges(&[(u64::MAX - 5, 10)], block).is_none());
        assert!(parallel_ranges(&[(1, 4 << 20)], block).is_none());
        assert!(parallel_ranges(&[(0, 0)], block).is_none());
    }

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

    #[test]
    fn ranges_grow_to_the_least_and_merge() {
        let k = 1024;
        // Grown to 4 KiB (from 3.5 KiB in whole blocks), the first two overlap; the file ends at 20 KiB.
        assert_eq!(grow_ranges(&[(2 * k, k), (0, k), (18 * k, k)], 3 * k + 512, k, 20 * k), [(0, 6 * k), (18 * k, 2 * k)]);
        // Longer than the least: unchanged.
        assert_eq!(grow_ranges(&[(0, 8 * k)], 2 * k, k, 20 * k), [(0, 8 * k)]);
        // Past the end: dropped, not an underflowing length.
        assert_eq!(grow_ranges(&[(25 * k, k), (20 * k, k)], 2 * k, k, 20 * k), []);
    }
}

/// A part of an object of a store: the whole object, `length` bytes from `offset`, or its last `n`
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectRange {
    /// The whole object.
    Whole,
    /// `length` bytes from `offset`.
    At(u64, u64),
    /// The last `n` bytes.
    Suffix(u64),
}

/// The requests the caller of a [`SparseFile`] or [`SparseObjects`] in terrio-js keeps in flight at
/// once: browsers queue more, and servers may rate-limit them.
pub const PARALLEL_REQUESTS: usize = 12;

/// A part of an object: its key and range.
type Part = (String, ObjectRange);

/// The parts fetched (`None`: no such object), and the bytes they hold.
type Held = (HashMap<Part, Option<Vec<u8>>>, u64);

/// The objects of a store (a Zarr store's metadata documents, chunks and shard parts) fetched by
/// the caller, as a [`SparseFile`] holds the blocks of one file: a read of a part not fetched
/// yet fails, and the part is recorded. The caller fetches the parts [`SparseObjects::missing`]
/// lists, hands them over with [`SparseObjects::insert`] (`None` for a missing object) and runs
/// the read again.
///
/// ```
/// use terrio::sparse::{ObjectRange, SparseObjects};
///
/// let objects = SparseObjects::new();
/// assert!(objects.get("a/zarr.json", ObjectRange::Whole).is_err());
/// assert_eq!(objects.missing(), [("a/zarr.json".to_string(), ObjectRange::Whole)]);
/// objects.insert("a/zarr.json", ObjectRange::Whole, Some(b"{}".to_vec()));
/// objects.insert("a/.zarray", ObjectRange::Whole, None);
/// assert_eq!(objects.get("a/zarr.json", ObjectRange::Whole).unwrap(), Some(b"{}".to_vec()));
/// assert_eq!(objects.get("a/.zarray", ObjectRange::Whole).unwrap(), None);
/// assert!(!objects.has_missing());
/// ```
#[derive(Debug, Default)]
pub struct SparseObjects {
    held: Mutex<Held>,
    missing: Mutex<(Vec<Part>, HashSet<Part>)>,
    /// The requests the caller keeps in flight, when reads should fill them (0: not asked).
    parallel: std::sync::atomic::AtomicUsize,
}

impl SparseObjects {
    /// No object fetched yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A part of an object: `None` when the object does not exist, an error recording the part
    /// when it is not fetched yet.
    pub fn get(&self, key: &str, range: ObjectRange) -> io::Result<Option<Vec<u8>>> {
        if let Some(held) = lock(&self.held).0.get(&(key.to_string(), range)) {
            return Ok(held.clone());
        }
        let mut missing = lock(&self.missing);
        let part = (key.to_string(), range);
        if missing.1.insert(part.clone()) {
            missing.0.push(part);
        }
        Err(io::Error::other(format!("{key} ({range:?}) is not fetched yet")))
    }

    /// Asks reads to split their parts into at least `n` requests where they can, as the caller
    /// keeps `n` in flight at once: Zarr reads then split the inner chunks of a shard into that
    /// many runs, rather than runs of up to 1 MiB. Worth it where requests at once are not queued:
    /// in Node.js, not in browsers, which keep 6 connections to an HTTP/1.1 server such as S3.
    /// Not asked (0), the default, parts are as reads choose them.
    pub fn set_parallel_requests(&self, n: usize) {
        self.parallel.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// The requests reads should fill ([`SparseObjects::set_parallel_requests`]), when asked.
    pub fn parallel_requests(&self) -> Option<usize> {
        Some(self.parallel.load(std::sync::atomic::Ordering::Relaxed)).filter(|&n| n > 0)
    }

    /// Whether a read missed parts not fetched since.
    pub fn has_missing(&self) -> bool {
        !lock(&self.missing).0.is_empty()
    }

    /// The parts reads missed, in the order they were asked for, forgotten once listed.
    pub fn missing(&self) -> Vec<(String, ObjectRange)> {
        std::mem::take(&mut *lock(&self.missing)).0
    }

    /// Hands over a part fetched: its bytes, or `None` when the object does not exist.
    pub fn insert(&self, key: &str, range: ObjectRange, data: Option<Vec<u8>>) {
        let mut held = lock(&self.held);
        let overhead = key.len() as u64 + std::mem::size_of::<(Part, Option<Vec<u8>>)>() as u64;
        held.1 += overhead + data.as_ref().map_or(0, |d| d.len() as u64);
        if let Some(old) = held.0.insert((key.to_string(), range), data) {
            held.1 -= overhead + old.map_or(0, |d| d.len() as u64);
        }
    }

    /// Forgets every part held once they take more than `max_bytes`; parts a read needs again are
    /// then fetched again. Readers keep what they parsed from them (metadata, shard indexes).
    pub fn trim(&self, max_bytes: u64) {
        let mut held = lock(&self.held);
        if held.1 > max_bytes {
            *held = Default::default();
        }
    }
}

/// A lock, also after a panic elsewhere poisoned it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod object_tests {
    use super::*;

    #[test]
    fn missing_parts_are_ordered_unique_and_can_be_requested_again() {
        let objects = SparseObjects::new();
        for _ in 0..2 {
            for i in 0..1000 {
                assert!(objects.get(&i.to_string(), ObjectRange::Whole).is_err());
                assert!(objects.get(&i.to_string(), ObjectRange::Whole).is_err());
            }
            let missing = objects.missing();
            assert_eq!(missing.len(), 1000);
            assert_eq!(missing[0].0, "0");
            assert_eq!(missing[999].0, "999");
        }
    }

    #[test]
    fn absent_objects_count_toward_cache_budget() {
        let objects = SparseObjects::new();
        objects.insert("missing", ObjectRange::Whole, None);
        objects.trim(0);
        assert!(objects.get("missing", ObjectRange::Whole).is_err());
        objects.insert("missing", ObjectRange::Whole, None);
        objects.insert("missing", ObjectRange::Whole, None);
        let held = lock(&objects.held);
        assert_eq!(held.1, 7 + std::mem::size_of::<(Part, Option<Vec<u8>>)>() as u64);
    }
}
