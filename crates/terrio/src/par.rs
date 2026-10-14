//! Data parallelism: rayon's thread pool in native builds, sequential loops in WebAssembly
//! (single-threaded there). Results never depend on how the work is split: each item is computed
//! alone, and outputs keep the input order.

#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;

/// The number of threads parallel work runs on.
pub(crate) fn threads() -> usize {
    #[cfg(not(target_arch = "wasm32"))]
    {
        rayon::current_num_threads()
    }
    #[cfg(target_arch = "wasm32")]
    {
        1
    }
}

/// Maps `f` over `0..n`, in parallel, keeping the order.
pub(crate) fn map_range<R: Send>(n: usize, f: impl Fn(usize) -> R + Sync + Send) -> Vec<R> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        (0..n).into_par_iter().map(f).collect()
    }
    #[cfg(target_arch = "wasm32")]
    {
        (0..n).map(f).collect()
    }
}

/// Calls `f` on each item of an owned list with its index, in parallel.
pub(crate) fn for_each_owned<T: Send>(items: Vec<T>, f: impl Fn(usize, T) + Sync + Send) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        items.into_par_iter().enumerate().for_each(|(i, t)| f(i, t));
    }
    #[cfg(target_arch = "wasm32")]
    {
        items.into_iter().enumerate().for_each(|(i, t)| f(i, t));
    }
}

/// Calls `f` on consecutive chunks of `size` items (the last may be shorter) with the chunk's
/// index, in parallel.
pub(crate) fn chunks_mut<T: Send>(data: &mut [T], size: usize, f: impl Fn(usize, &mut [T]) + Sync + Send) {
    let size = size.max(1);
    #[cfg(not(target_arch = "wasm32"))]
    {
        data.par_chunks_mut(size).enumerate().for_each(|(i, c)| f(i, c));
    }
    #[cfg(target_arch = "wasm32")]
    {
        data.chunks_mut(size).enumerate().for_each(|(i, c)| f(i, c));
    }
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
