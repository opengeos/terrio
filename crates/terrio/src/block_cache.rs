// Written to match the behavior of:
//   GDAL v3.13.3:
//     gcore/gdalrasterblock.cpp (GDAL_CACHEMAX, GDALSetCacheMax64, GDALGetCacheMax64)
// No code is translated from these sources.

//! A process-wide cache of the decoded chunks (tiles or strips) of GeoTIFFs and COGs read lazily,
//! as GDAL's raster block cache (`GDAL_CACHEMAX`) keeps the blocks it decodes.
//!
//! Without it, every [`crate::LazyRaster::read_window`] decodes its chunks again, whatever
//! was read before: reading one pixel of a file with one-row LZW strips costs a strip's
//! decompression on each read. With it, a chunk decoded by one window read is placed from memory by
//! the next one that needs it. What is cached is the decoded samples of the chunks of lazy
//! GeoTIFF reads, whether the TIFF is a local file, bytes in memory, a member of a zip archive or a
//! remote object (HTTP, S3), and of the images of external `.ovr` overviews. Rasters read whole
//! (formats other than GeoTIFF, [`crate::open_raster`]) and the other lazy drivers are not cached.
//!
//! The cache holds at most [`max_bytes`] bytes of decoded samples (and a small fixed overhead per
//! chunk), dropping the least recently used chunks first. A chunk larger than the cache is not
//! kept. The size is read once, at first use, from the process environment (the GDAL equivalent is
//! read when the terrio variable is not set, or cannot be read). Options set for a thread
//! ([`crate::config::push_scope`], Python's `terrio.Env`) do not change it; use [`set_max_bytes`]:
//!
//! | Variable | GDAL equivalent | Default |
//! |---|---|---|
//! | `TERRIO_CACHEMAX` (bytes, with optional `K`, `M` or `G` suffixes; 0 turns the cache off) | `GDAL_CACHEMAX` | 64 MiB |
//!
//! `GDAL_CACHEMAX` is read as GDAL reads it: a plain integer below 100,000 is a number of
//! megabytes, any other plain integer a number of bytes, and a suffixed value (`512MB`) has its unit.
//! A percentage of the memory (`5%`) is not supported: it is ignored, as is any value that cannot be
//! read, and the default is used. [`set_max_bytes`] (GDAL's `GDALSetCacheMax64`) changes the size
//! at run time, and [`clear`] empties the cache.
//!
//! The chunks are keyed by the open reader that decoded them, not by the file's path: a reader
//! drops its chunks when it is closed, and a file that is replaced (written anew and renamed over
//! the original) while a reader holds it keeps being read as it was, as GDAL does, while a reader
//! opened after the replacement never sees a chunk of the old file.
//!
//! ```no_run
//! use terrio::{Window, WindowOptions};
//!
//! // Keep up to 256 MiB of decoded chunks.
//! terrio::block_cache::set_max_bytes(256 << 20);
//! let mut r = terrio::open_raster_lazy("big.tif")?;
//! let opts = WindowOptions { bands: vec![1], overview: None };
//! // The second read of the same pixels decodes no chunk.
//! let first = r.read_window(Window::new(0, 0, 64, 64), &opts)?;
//! let again = r.read_window(Window::new(0, 0, 64, 64), &opts)?;
//! assert_eq!(first.raster.data.to_f64_vec(), again.raster.data.to_f64_vec());
//! // Memory can be given back at any time.
//! terrio::block_cache::clear();
//! # Ok::<(), terrio::Error>(())
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tiff::decoder::DecodingResult;

/// The default size of the cache: 64 MiB.
const DEFAULT_MAX_BYTES: u64 = 64 << 20;

/// The bytes counted for each chunk besides its samples: its entry's own memory, so that a file of
/// tiny chunks cannot make the cache take far more than its size.
const ENTRY_OVERHEAD: u64 = 64;

/// GDAL's `CPLParseMemorySize` threshold: a plain number below it is megabytes, else bytes.
const GDAL_MEGABYTES_BELOW: f64 = 100_000.0;

/// A number with an optional `K`, `M` or `G` (and an optional `B`) unit: its value and the bytes of
/// its unit, 1 when there is none.
fn parse_number_and_unit(v: &str) -> Option<(f64, u64)> {
    let v = v.trim();
    let digits = v.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(v.len());
    let value: f64 = v[..digits].parse().ok()?;
    let unit = v[digits..].trim().to_ascii_uppercase();
    let factor = match unit.trim_end_matches('B') {
        "" => 1u64,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => return None,
    };
    Some((value, factor))
}

/// `bytes` as a size, when it is a finite number that fits.
fn to_size(bytes: f64) -> Option<u64> {
    (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

/// A `TERRIO_CACHEMAX` value: bytes, with optional `K`, `M` or `G` suffixes.
fn parse_terrio_cachemax(v: &str) -> Option<u64> {
    let (value, factor) = parse_number_and_unit(v)?;
    to_size(value * factor as f64)
}

/// A `GDAL_CACHEMAX` value as GDAL reads it: a plain number below 100,000 is megabytes, another
/// plain number is bytes, a number with a unit has it. A percentage is not supported (`None`).
fn parse_gdal_cachemax(v: &str) -> Option<u64> {
    let (value, factor) = parse_number_and_unit(v)?;
    if factor == 1 && !v.trim().to_ascii_uppercase().ends_with('B') && value < GDAL_MEGABYTES_BELOW {
        return to_size(value * (1u64 << 20) as f64);
    }
    to_size(value * factor as f64)
}

/// A variable of the process environment, unset when empty. The thread scopes of
/// [`crate::config`] are not consulted: the size is process-wide.
fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// The size from the process environment, else the default.
fn default_max_bytes() -> u64 {
    process_env("TERRIO_CACHEMAX")
        .and_then(|v| parse_terrio_cachemax(&v))
        .or_else(|| process_env("GDAL_CACHEMAX").and_then(|v| parse_gdal_cachemax(&v)))
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// The most bytes of decoded samples the cache holds (GDAL's `GDALGetCacheMax64`): from
/// `TERRIO_CACHEMAX`, else `GDAL_CACHEMAX`, else 64 MiB, unless [`set_max_bytes`] changed it. 0
/// means that nothing is cached. The size is read from the process environment at first use;
/// options set for a thread (`config::push_scope`, Python's `terrio.Env`) do not change it, use
/// [`set_max_bytes`]. The size lives in the cache, under its lock, which a fork holds; the
/// environment is read outside the lock, and a size set meanwhile by [`set_max_bytes`] wins.
pub fn max_bytes() -> u64 {
    if let Some(max) = cache().max {
        return max;
    }
    let value = default_max_bytes();
    *cache().max.get_or_insert(value)
}

/// Sets the most bytes of decoded samples the cache holds (GDAL's `GDALSetCacheMax64`). A smaller
/// size evicts the least recently used chunks down to it and gives back the memory of the readers'
/// emptied tables; 0 drops them all and turns the cache off.
pub fn set_max_bytes(bytes: u64) {
    cache().set_max(bytes);
}

/// Drops every cached chunk, giving their memory back. Readers decode the chunks they need again.
pub fn clear() {
    cache().clear();
}

/// A decoded chunk: its samples as decoded (a half-float chunk is converted when it is placed)
/// and the size `(width, height)` of the image data it holds.
pub(crate) struct CachedChunk {
    pub samples: DecodingResult,
    pub dims: (usize, usize),
}

impl CachedChunk {
    /// The bytes the chunk takes in the cache.
    fn cost(&self) -> u64 {
        use std::mem::size_of;
        let samples = match &self.samples {
            DecodingResult::U8(v) => v.capacity() * size_of::<u8>(),
            DecodingResult::I8(v) => v.capacity() * size_of::<i8>(),
            DecodingResult::U16(v) => v.capacity() * size_of::<u16>(),
            DecodingResult::I16(v) => v.capacity() * size_of::<i16>(),
            DecodingResult::F16(v) => v.capacity() * size_of::<half::f16>(),
            DecodingResult::U32(v) => v.capacity() * size_of::<u32>(),
            DecodingResult::I32(v) => v.capacity() * size_of::<i32>(),
            DecodingResult::F32(v) => v.capacity() * size_of::<f32>(),
            DecodingResult::U64(v) => v.capacity() * size_of::<u64>(),
            DecodingResult::I64(v) => v.capacity() * size_of::<i64>(),
            DecodingResult::F64(v) => v.capacity() * size_of::<f64>(),
        };
        samples as u64 + ENTRY_OVERHEAD
    }
}

/// One cached chunk: its samples, its cost and its place in the order of use.
struct Entry {
    chunk: Arc<CachedChunk>,
    cost: u64,
    tick: u64,
}

/// The chunks of an open reader, by `(ifd, chunk)`.
type HandleChunks = HashMap<(usize, u32), Entry>;

/// The least recently used chunks, by `(handle, ifd, chunk)`.
struct Cache {
    handles: BTreeMap<u64, HandleChunks>,
    /// The entries by the tick of their last use: the first is the least recently used.
    order: BTreeMap<u64, (u64, usize, u32)>,
    bytes: u64,
    tick: u64,
    /// The most bytes held; `None` until it is read from the environment.
    max: Option<u64>,
}

impl Cache {
    const fn new() -> Self {
        Self { handles: BTreeMap::new(), order: BTreeMap::new(), bytes: 0, tick: 0, max: None }
    }

    /// Sets the size, evicts down to it and gives back the spare memory of the tables that survive.
    fn set_max(&mut self, bytes: u64) {
        self.max = Some(bytes);
        self.shrink(bytes);
        for chunks in self.handles.values_mut() {
            chunks.shrink_to_fit();
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn get(&mut self, handle: u64, ifd: usize, chunk: u32) -> Option<Arc<CachedChunk>> {
        let tick = self.next_tick();
        let entry = self.handles.get_mut(&handle)?.get_mut(&(ifd, chunk))?;
        let old = std::mem::replace(&mut entry.tick, tick);
        let found = entry.chunk.clone();
        self.order.remove(&old);
        self.order.insert(tick, (handle, ifd, chunk));
        Some(found)
    }

    fn remove(&mut self, handle: u64, ifd: usize, chunk: u32) {
        let Some(chunks) = self.handles.get_mut(&handle) else { return };
        if let Some(entry) = chunks.remove(&(ifd, chunk)) {
            self.bytes -= entry.cost;
            self.order.remove(&entry.tick);
        }
        if chunks.is_empty() {
            self.handles.remove(&handle);
        }
    }

    /// Drops the least recently used chunks until the cache holds at most `limit` bytes.
    fn shrink(&mut self, limit: u64) {
        while self.bytes > limit {
            let Some((_, (handle, ifd, chunk))) = self.order.pop_first() else {
                self.bytes = 0;
                break;
            };
            // Out of the order already; `remove` finds no tick to take out of it.
            self.remove(handle, ifd, chunk);
        }
    }

    /// Stores a chunk within the size held by the cache; nothing when the size is not set yet.
    fn insert(&mut self, handle: u64, ifd: usize, chunk: u32, value: CachedChunk) {
        let Some(max) = self.max else { return };
        let cost = value.cost();
        if cost > max {
            return;
        }
        self.remove(handle, ifd, chunk);
        self.shrink(max - cost);
        let tick = self.next_tick();
        self.bytes += cost;
        self.order.insert(tick, (handle, ifd, chunk));
        self.handles.entry(handle).or_default().insert((ifd, chunk), Entry { chunk: Arc::new(value), cost, tick });
    }

    fn remove_handle(&mut self, handle: u64) {
        if let Some(chunks) = self.handles.remove(&handle) {
            for entry in chunks.into_values() {
                self.bytes -= entry.cost;
                self.order.remove(&entry.tick);
            }
        }
    }

    fn clear(&mut self) {
        self.handles.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

static CACHE: Mutex<Cache> = Mutex::new(Cache::new());

/// [`CACHE`], also after a panic elsewhere poisoned its lock.
fn cache() -> MutexGuard<'static, Cache> {
    crate::par::fork_safe();
    CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Holds the cache's lock across a fork, for [`crate::par`]: the forking thread takes it before the
/// process is copied and lets it go after, so that the child never starts with it held by a thread
/// it does not have.
#[cfg(all(unix, not(target_family = "wasm")))]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    held.push(Box::new(cache()));
}

/// A new identity for a reader's chunks (see [`remove_handle`]).
pub(crate) fn new_handle() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The cached chunk `chunk` of image `ifd` of the reader `handle`, marked as the most recently used.
pub(crate) fn get(handle: u64, ifd: usize, chunk: u32) -> Option<Arc<CachedChunk>> {
    let found = cache().get(handle, ifd, chunk);
    #[cfg(test)]
    tests::count_lookup(found.is_some());
    found
}

/// Caches a decoded chunk, evicting the least recently used ones to stay within [`max_bytes`]. A
/// chunk larger than the cache is dropped. The size is read under the same lock that stores the
/// chunk, so no insert follows a completed [`set_max_bytes`] with a stale size.
pub(crate) fn insert(handle: u64, ifd: usize, chunk: u32, samples: DecodingResult, dims: (usize, usize)) {
    // Reads the environment, if it was not yet, outside the lock.
    if max_bytes() == 0 {
        return;
    }
    cache().insert(handle, ifd, chunk, CachedChunk { samples, dims });
}

/// Drops the chunks of a reader that is closed.
pub(crate) fn remove_handle(handle: u64) {
    cache().remove_handle(handle);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static LOOKUPS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    }

    pub(super) fn count_lookup(hit: bool) {
        LOOKUPS.with(|c| {
            let (hits, misses) = c.get();
            c.set(if hit { (hits + 1, misses) } else { (hits, misses + 1) });
        });
    }

    /// The `(hits, misses)` of the lookups the calling thread made since it last asked.
    pub(crate) fn take_lookups() -> (usize, usize) {
        LOOKUPS.with(|c| c.replace((0, 0)))
    }

    /// The guard of the tests that change the cache's size or rely on what it holds: it serializes
    /// them, empties the cache and gives it a size of `size` bytes; the default size is back when
    /// it drops.
    pub(crate) fn cache_test_guard(size: u64) -> CacheTestGuard {
        static SERIAL: Mutex<()> = Mutex::new(());
        let serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear();
        set_max_bytes(size);
        take_lookups();
        CacheTestGuard { _serial: serial }
    }

    /// See [`cache_test_guard`].
    pub(crate) struct CacheTestGuard {
        _serial: MutexGuard<'static, ()>,
    }

    impl Drop for CacheTestGuard {
        fn drop(&mut self) {
            clear();
            set_max_bytes(DEFAULT_MAX_BYTES);
        }
    }

    /// A chunk of `n` bytes, each `fill`.
    fn chunk(n: usize, fill: u8) -> DecodingResult {
        DecodingResult::U8(vec![fill; n])
    }

    /// What a chunk of `n` bytes costs.
    const fn cost(n: u64) -> u64 {
        n + ENTRY_OVERHEAD
    }

    fn first_byte(c: &CachedChunk) -> u8 {
        let DecodingResult::U8(v) = &c.samples else { panic!("not bytes") };
        v[0]
    }

    /// A private cache of `max` bytes, apart from the process-wide one that other tests use.
    fn private_cache(max: u64) -> Cache {
        let mut c = Cache::new();
        c.set_max(max);
        c
    }

    /// Stores a chunk of `n` bytes, each `fill`, in a private cache.
    fn put(c: &mut Cache, handle: u64, ifd: usize, index: u32, n: usize, fill: u8, dims: (usize, usize)) {
        c.insert(handle, ifd, index, CachedChunk { samples: chunk(n, fill), dims });
    }

    #[test]
    fn eviction_drops_the_least_recently_used_chunk_first() {
        let mut c = private_cache(3 * cost(100));
        let h = new_handle();
        for i in 0..3u32 {
            put(&mut c, h, 0, i, 100, i as u8, (10, 10));
        }
        assert_eq!(c.bytes, 3 * cost(100));
        // Chunk 0 is the oldest, until it is used.
        assert!(c.get(h, 0, 0).is_some());
        put(&mut c, h, 0, 3, 100, 3, (10, 10));
        assert!(c.bytes <= 3 * cost(100));
        assert!(c.get(h, 0, 1).is_none(), "chunk 1 was the least recently used");
        assert!(c.get(h, 0, 0).is_some());
        assert!(c.get(h, 0, 2).is_some());
        assert!(c.get(h, 0, 3).is_some());
        assert_eq!(first_byte(&c.get(h, 0, 2).unwrap()), 2);
        assert_eq!(c.get(h, 0, 2).unwrap().dims, (10, 10));
    }

    #[test]
    fn the_total_never_passes_the_maximum() {
        let mut c = private_cache(1000);
        let h = new_handle();
        for i in 0..200u32 {
            put(&mut c, h, 0, i, 37 + (i as usize % 5) * 20, 0, (1, 1));
            assert!(c.bytes <= 1000);
        }
        assert!(c.bytes > 0);
    }

    #[test]
    fn a_chunk_larger_than_the_maximum_is_not_stored() {
        let mut c = private_cache(cost(100));
        let h = new_handle();
        put(&mut c, h, 0, 0, 100, 1, (1, 1));
        put(&mut c, h, 0, 1, 101, 2, (1, 1));
        assert!(c.get(h, 0, 1).is_none());
        assert!(c.get(h, 0, 0).is_some(), "an oversized chunk evicts nothing");
    }

    #[test]
    fn a_maximum_of_zero_empties_and_disables_the_cache() {
        let _serial = cache_test_guard(10_000);
        let h = new_handle();
        insert(h, 0, 0, chunk(100, 1), (1, 1));
        assert!(cache().bytes > 0);
        set_max_bytes(0);
        assert_eq!(cache().bytes, 0);
        assert_eq!(max_bytes(), 0);
        insert(h, 0, 0, chunk(0, 1), (1, 1));
        assert_eq!(cache().bytes, 0);
        assert!(get(h, 0, 0).is_none());
    }

    #[test]
    fn lowering_the_maximum_evicts_down_to_it() {
        let mut c = private_cache(100 * cost(100));
        let h = new_handle();
        for i in 0..100u32 {
            put(&mut c, h, 0, i, 100, 0, (1, 1));
        }
        let before = c.handles[&h].capacity();
        c.set_max(2 * cost(100));
        assert_eq!(c.bytes, 2 * cost(100));
        // The most recent two are kept.
        assert!(c.get(h, 0, 99).is_some() && c.get(h, 0, 98).is_some());
        assert!(c.handles[&h].capacity() < before, "the reader's table gave memory back");
    }

    #[test]
    fn removing_a_handle_removes_only_its_chunks() {
        let mut c = private_cache(10_000);
        let (a, b) = (new_handle(), new_handle());
        put(&mut c, a, 0, 0, 100, 1, (1, 1));
        put(&mut c, a, 1, 0, 100, 1, (1, 1));
        put(&mut c, b, 0, 0, 100, 2, (1, 1));
        c.remove_handle(a);
        assert!(c.get(a, 0, 0).is_none() && c.get(a, 1, 0).is_none());
        assert_eq!(first_byte(&c.get(b, 0, 0).unwrap()), 2);
        assert_eq!(c.bytes, cost(100));
    }

    #[test]
    fn clearing_drops_every_chunk() {
        let mut c = private_cache(10_000);
        let h = new_handle();
        put(&mut c, h, 0, 0, 100, 1, (1, 1));
        c.clear();
        assert_eq!(c.bytes, 0);
        assert!(c.get(h, 0, 0).is_none());
    }

    #[test]
    fn inserting_a_chunk_again_replaces_it_without_double_counting() {
        let mut c = private_cache(10_000);
        let h = new_handle();
        put(&mut c, h, 0, 0, 100, 1, (1, 1));
        put(&mut c, h, 0, 0, 50, 2, (1, 1));
        assert_eq!(c.bytes, cost(50));
        assert_eq!(first_byte(&c.get(h, 0, 0).unwrap()), 2);
    }

    #[test]
    fn a_thread_scope_does_not_set_the_cache_size() {
        let without = default_max_bytes();
        let scope = ["TERRIO_CACHEMAX", "GDAL_CACHEMAX"].into_iter().map(|k| (k.to_string(), Some("1".to_string()))).collect();
        crate::config::push_scope(scope);
        let with = default_max_bytes();
        crate::config::pop_scope();
        assert_eq!(with, without);
        assert_eq!(default_max_bytes(), without);
    }

    #[test]
    fn terrio_cachemax_reads_bytes_and_suffixes() {
        assert_eq!(parse_terrio_cachemax("1000"), Some(1000));
        assert_eq!(parse_terrio_cachemax("512K"), Some(512 << 10));
        assert_eq!(parse_terrio_cachemax("64M"), Some(64 << 20));
        assert_eq!(parse_terrio_cachemax("2g"), Some(2 << 30));
        assert_eq!(parse_terrio_cachemax("2GB"), Some(2 << 30));
        assert_eq!(parse_terrio_cachemax("0"), Some(0));
        assert_eq!(parse_terrio_cachemax("lots"), None);
        assert_eq!(parse_terrio_cachemax("5%"), None);
    }

    #[test]
    fn gdal_cachemax_reads_small_numbers_as_megabytes() {
        assert_eq!(parse_gdal_cachemax("512"), Some(512 << 20));
        assert_eq!(parse_gdal_cachemax("99999"), Some(99_999 << 20));
        assert_eq!(parse_gdal_cachemax("100000"), Some(100_000));
        assert_eq!(parse_gdal_cachemax("200000000"), Some(200_000_000));
        assert_eq!(parse_gdal_cachemax("512MB"), Some(512 << 20));
        assert_eq!(parse_gdal_cachemax("1G"), Some(1 << 30));
        assert_eq!(parse_gdal_cachemax("5%"), None);
        assert_eq!(parse_gdal_cachemax("x"), None);
    }
}
