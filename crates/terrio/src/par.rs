//! Data parallelism: rayon's thread pool in native builds and in WebAssembly built with threads
//! (`cfg(threads)`, the pool being started by the host: `initThreadPool` in `terrio-js`),
//! sequential loops in other WebAssembly builds. Results never depend on how the work is split:
//! each item is computed alone, and outputs keep the input order.
//!
//! The `_coarse` functions are for work that computes much and allocates little per item (tiles
//! and chunks to compress or decode, rows of pixels): in WebAssembly with threads, only they run
//! in parallel. Its allocator takes one lock that waiting threads spin on, so that fine-grained
//! work allocating as it goes (a JSON value or a geometry per feature) runs several times slower
//! on more threads; it runs on the calling thread there, as without threads.

#[cfg(threads)]
use rayon::prelude::*;

/// The thread pool parallel work runs on: rayon's global pool natively.
#[cfg(all(threads, not(target_arch = "wasm32")))]
struct Pool;

#[cfg(all(threads, not(target_arch = "wasm32")))]
impl Pool {
    fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        f()
    }

    fn threads(&self) -> usize {
        rayon::current_num_threads()
    }
}

/// The thread pool parallel work runs on in WebAssembly: the one the host started (see
/// [`set_thread_pool`]), never rayon's global pool, which would fall back to the calling thread
/// for good when used before the host's workers exist.
#[cfg(all(threads, target_arch = "wasm32"))]
struct Pool(&'static rayon::ThreadPool);

#[cfg(all(threads, target_arch = "wasm32"))]
impl Pool {
    fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        self.0.install(f)
    }

    fn threads(&self) -> usize {
        self.0.current_num_threads()
    }
}

#[cfg(all(threads, target_arch = "wasm32"))]
static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();

/// Makes `pool` the thread pool of terrio's parallel work, in WebAssembly built with threads
/// (whose workers the host starts, as Web Workers or Node.js worker threads). Until it is set,
/// the work runs on the calling thread. Fails, giving the pool back, when one was set already.
#[cfg(all(threads, target_arch = "wasm32"))]
pub fn set_thread_pool(pool: rayon::ThreadPool) -> Result<(), rayon::ThreadPool> {
    POOL.set(pool)
}

/// The pool to run parallel work on, if any: for `coarse` work, or any work natively.
#[cfg(threads)]
fn pool_for(coarse: bool) -> Option<Pool> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = coarse;
        Some(Pool)
    }
    #[cfg(target_arch = "wasm32")]
    POOL.get().filter(|_| coarse).map(Pool)
}

/// The number of threads parallel work runs on.
pub(crate) fn threads() -> usize {
    #[cfg(threads)]
    if let Some(p) = pool_for(true) {
        return p.threads();
    }
    1
}

/// Maps `f` over `0..n`, in parallel, keeping the order.
fn map_range_on<const COARSE: bool, R: Send>(n: usize, f: impl Fn(usize) -> R + Sync + Send) -> Vec<R> {
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE) {
        return p.install(|| (0..n).into_par_iter().map(f).collect());
    }
    (0..n).map(f).collect()
}

/// Maps `f` over items in parallel, keeping the order, with a scratch value from `init` that
/// each piece of work (several items in a row) starts with.
#[cfg_attr(not(all(threads, target_arch = "wasm32")), allow(dead_code))]
pub(crate) fn map_init<T: Sync, S, R: Send>(
    items: &[T],
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, &T) -> R + Sync + Send,
) -> Vec<R> {
    #[cfg(threads)]
    if let Some(p) = pool_for(true) {
        return p.install(|| items.par_iter().with_min_len(2).map_init(init, |s, t| f(s, t)).collect());
    }
    let mut s = init();
    items.iter().map(|t| f(&mut s, t)).collect()
}

/// Maps `f` over mutable items in parallel, keeping the order.
fn map_mut_on<const COARSE: bool, T: Send, R: Send>(items: &mut [T], f: impl Fn(&mut T) -> R + Sync + Send) -> Vec<R> {
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE) {
        return p.install(|| items.par_iter_mut().map(f).collect());
    }
    items.iter_mut().map(f).collect()
}

/// Calls `f` on each item of an owned list with its index, in parallel.
fn for_each_owned_on<const COARSE: bool, T: Send>(items: Vec<T>, f: impl Fn(usize, T) + Sync + Send) {
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE) {
        return p.install(|| items.into_par_iter().enumerate().for_each(|(i, t)| f(i, t)));
    }
    items.into_iter().enumerate().for_each(|(i, t)| f(i, t));
}

/// Calls `f` on consecutive chunks of `size` items (the last may be shorter) with the chunk's
/// index, in parallel.
fn chunks_mut_on<const COARSE: bool, T: Send>(data: &mut [T], size: usize, f: impl Fn(usize, &mut [T]) + Sync + Send) {
    let size = size.max(1);
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE) {
        return p.install(|| data.par_chunks_mut(size).enumerate().for_each(|(i, c)| f(i, c)));
    }
    data.chunks_mut(size).enumerate().for_each(|(i, c)| f(i, c));
}

/// Maps `f` over `0..n`, in parallel (fine-grained work: see the module's documentation),
/// keeping the order.
pub(crate) fn map_range<R: Send>(n: usize, f: impl Fn(usize) -> R + Sync + Send) -> Vec<R> {
    map_range_on::<false, R>(n, f)
}

/// [`map_range`] for coarse work, in parallel in WebAssembly with threads too.
pub(crate) fn map_range_coarse<R: Send>(n: usize, f: impl Fn(usize) -> R + Sync + Send) -> Vec<R> {
    map_range_on::<true, R>(n, f)
}

/// Maps `f` over mutable items in parallel (fine-grained work), keeping the order.
#[allow(dead_code)]
pub(crate) fn map_mut<T: Send, R: Send>(items: &mut [T], f: impl Fn(&mut T) -> R + Sync + Send) -> Vec<R> {
    map_mut_on::<false, T, R>(items, f)
}

/// [`map_mut`] for coarse work.
pub(crate) fn map_mut_coarse<T: Send, R: Send>(items: &mut [T], f: impl Fn(&mut T) -> R + Sync + Send) -> Vec<R> {
    map_mut_on::<true, T, R>(items, f)
}

/// Calls `f` on each item of an owned list with its index, in parallel (fine-grained work).
pub(crate) fn for_each_owned<T: Send>(items: Vec<T>, f: impl Fn(usize, T) + Sync + Send) {
    for_each_owned_on::<false, T>(items, f)
}

/// [`for_each_owned`] for coarse work.
pub(crate) fn for_each_owned_coarse<T: Send>(items: Vec<T>, f: impl Fn(usize, T) + Sync + Send) {
    for_each_owned_on::<true, T>(items, f)
}

/// Calls `f` on consecutive chunks of `size` items (the last may be shorter) with the chunk's
/// index, in parallel (fine-grained work).
pub(crate) fn chunks_mut<T: Send>(data: &mut [T], size: usize, f: impl Fn(usize, &mut [T]) + Sync + Send) {
    chunks_mut_on::<false, T>(data, size, f)
}

/// [`chunks_mut`] for coarse work.
pub(crate) fn chunks_mut_coarse<T: Send>(data: &mut [T], size: usize, f: impl Fn(usize, &mut [T]) + Sync + Send) {
    chunks_mut_on::<true, T>(data, size, f)
}

/// A chunk length that splits `n` items into a few chunks per thread, of at least `min` items.
pub(crate) fn chunk_len(n: usize, min: usize) -> usize {
    n.div_ceil(threads() * 4).max(min).max(1)
}

/// Reads a whole file, as `std::fs::read` does. Large files are read in parts in parallel (on
/// Unix), which mostly spreads the cost of mapping the buffer's fresh memory over the threads.
pub(crate) fn read_file(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        use std::os::unix::fs::FileExt;
        const PART: usize = 1 << 21;
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len >= 4 * PART && threads() > 1 {
            let mut buf = vec![0u8; len];
            let parts: Vec<&mut [u8]> = buf.chunks_mut(PART).collect();
            let failed = std::sync::atomic::AtomicBool::new(false);
            for_each_owned(parts, |i, part| {
                if file.read_exact_at(part, (i * PART) as u64).is_err() {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            });
            // A file that changed while being read is read again, simply.
            if !failed.into_inner() && file.metadata()?.len() as usize == len {
                return Ok(buf);
            }
        }
    }
    std::fs::read(path)
}
