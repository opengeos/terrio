//! Data parallelism: terrio's rayon thread pool in native builds and in WebAssembly built with
//! threads (`cfg(threads)`, the pool being started by the host: `initThreadPool` in `terrio-js`),
//! sequential loops in other WebAssembly builds. Results never depend on how the work is split:
//! each item is computed alone, and outputs keep the input order.
//!
//! The `_coarse` functions are for work that computes much and allocates little per item (tiles
//! and chunks to compress or decode, rows of pixels): in WebAssembly with threads, only they run
//! in parallel. Its allocator takes one lock that waiting threads spin on, so that fine-grained
//! work allocating as it goes (a JSON value or a geometry per feature) runs several times slower
//! on more threads; it runs on the calling thread there, as without threads.
//!
//! # One pool, which survives `fork()`
//!
//! All of terrio's parallel and background work runs on this module's pool, through its
//! functions:
//!
//! - the `map_*`, `for_each_owned*` and `chunks_mut*` functions, for data parallelism;
//! - `spawn`, for work in the background (native builds);
//! - [`install`], around rayon code that cannot be written with the functions above, and around
//!   a call into a dependency that parallelizes with rayon by itself (`jpeg-decoder`,
//!   `jxl-oxide`): rayon's free functions and parallel iterators run on the pool of the thread
//!   that calls them, so inside [`install`] they run on this one.
//!
//! Nothing else may use rayon's global pool (`rayon::spawn`, `rayon::join`, `rayon::scope`, a
//! parallel iterator outside [`install`]) or start a pool of its own. The reason is `fork()`,
//! which Python's `multiprocessing` uses on Linux: the child has only the thread that called it.
//! A pool started before the fork exists in the child without its threads, so work given to it
//! waits forever; rayon's global pool cannot be started again. This one is per process: a
//! `pthread_atfork` handler counts forks in each child, and the first parallel call in a child
//! starts a new pool there. The parent's pool is left alone in the child, neither used nor
//! dropped, since its threads are not there to stop. A fork also waits for the background work
//! running (`spawn`) to finish, so that a child never copies an initialization half done, which
//! would block it: the EPSG table that `crs::prefetch` builds. Background jobs that would start
//! meanwhile are set aside, off the pool, and queued again when the last fork in progress is done:
//! none of them holds a thread of the pool in the meantime.

// The one place rayon is called directly: the methods clippy.toml disallows elsewhere.
#![allow(clippy::disallowed_methods)]

#[cfg(threads)]
use rayon::prelude::*;

/// The pool of native builds, one per process (see the module's documentation).
#[cfg(not(target_family = "wasm"))]
mod native {
    use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

    /// A pool, with the number of forks of the process that started it.
    struct Started {
        forks: u64,
        pool: rayon::ThreadPool,
        /// The pool's threads that have run the pool's first job (see [`start`]); read by tests.
        #[cfg_attr(not(all(test, unix)), allow(dead_code))]
        warmed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    /// This process's pool, once started. A pool is never freed once published, so references to
    /// it stay valid; the one a child replaces is its parent's, whose threads it does not have.
    static POOL: AtomicPtr<Started> = AtomicPtr::new(std::ptr::null_mut());
    /// The forks this process descends from, counted in each child by `child` (Unix).
    static FORKS: AtomicU64 = AtomicU64::new(0);
    /// Whether [`prepare`], [`parent`] and [`child`] are registered.
    #[cfg(unix)]
    static HANDLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// The background jobs running (the low 32 bits), and [`FORK`] times the forks in progress,
    /// which wait for them to finish: counted, so that the jobs held back by forks from several
    /// threads at once start only once the last is done. Sequentially consistent throughout, as
    /// [`hold_back`] and [`end_fork`] each write one of `GATE` and [`HELD_BACK`] and then read
    /// the other.
    static GATE: AtomicU64 = AtomicU64::new(0);
    const FORK: u64 = 1 << 32;
    /// The jobs running, in [`GATE`].
    #[cfg(unix)]
    const JOBS: u64 = FORK - 1;
    /// How long a fork sleeps between checks for the background jobs running.
    #[cfg(unix)]
    const NAP: std::time::Duration = std::time::Duration::from_micros(50);
    /// The background jobs held back by the forks in progress, newest first (see [`submit`]).
    static HELD_BACK: AtomicPtr<HeldBack> = AtomicPtr::new(std::ptr::null_mut());

    /// A background job in [`HELD_BACK`], with the one held back before it.
    struct HeldBack {
        job: Box<dyn FnOnce() + Send>,
        next: *mut HeldBack,
    }

    /// The pool of this process, started on first use.
    pub(super) fn pool() -> &'static rayon::ThreadPool {
        let p = POOL.load(Ordering::Acquire);
        // SAFETY: a published pool is never freed.
        match unsafe { p.as_ref() } {
            Some(s) if s.forks == FORKS.load(Ordering::Acquire) => &s.pool,
            _ => start(p),
        }
    }

    /// Starts this process's pool, `seen` being the one published, if any, which another process
    /// started (this process's parent), unless another thread starts this process's first.
    #[cold]
    fn start(mut seen: *mut Started) -> &'static rayon::ThreadPool {
        // Registered before the pool starts, so that every fork after it is counted.
        #[cfg(unix)]
        register();
        let forks = FORKS.load(Ordering::Acquire);
        // rayon's defaults, as its global pool has: `RAYON_NUM_THREADS` threads, else one a core.
        let pool = rayon::ThreadPoolBuilder::new().thread_name(|i| format!("terrio-par-{i}")).build().expect("terrio's thread pool starts");
        // Each thread of the pool runs a first job before the pool is used, so that none of them is
        // still starting, nor making a one-time initialization, when the call that started the
        // pool returns: a child forked then would inherit what the thread held, and wait for it
        // forever. A thread starting (or ending) holds the standard library's record of the
        // threads' stack guards (1.95: `stack_overflow::thread_info::LOCK`), without which the
        // child can start no thread. And the process's first thread to look for work initializes
        // crossbeam-epoch's global collector (a `OnceLock`, `COLLECTOR`), on which every thread of
        // the child's pool would then wait. A thread takes this job by stealing it from its own
        // queue of broadcasts, which it does once started, and which uses the collector.
        let warmed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = warmed.clone();
        pool.broadcast(move |_| {
            w.fetch_add(1, Ordering::AcqRel);
        });
        let new = Box::into_raw(Box::new(Started { forks, pool, warmed }));
        loop {
            match POOL.compare_exchange(seen, new, Ordering::AcqRel, Ordering::Acquire) {
                // SAFETY: published, `new` is never freed.
                Ok(_) => return unsafe { &(*new).pool },
                // SAFETY: a published pool is never freed.
                Err(other) => match unsafe { other.as_ref() } {
                    Some(s) if s.forks == forks => {
                        // Another thread started this process's pool first: this one stops.
                        // SAFETY: `new` was never published.
                        drop(unsafe { Box::from_raw(new) });
                        return &s.pool;
                    }
                    _ => seen = other,
                },
            }
        }
    }

    /// Registers [`prepare`], [`parent`] and [`child`] with `pthread_atfork`, once. Threads racing
    /// here may all register: a fork then runs them several times, which serves as well (the
    /// locks are taken by the first [`prepare`], and the others find none to take).
    #[cfg(unix)]
    pub(super) fn register() {
        if !HANDLED.load(Ordering::Acquire) {
            // SAFETY: the handlers are functions without arguments, as `pthread_atfork` wants.
            // It fails only for lack of memory; then the handlers are tried again next time.
            if unsafe { libc::pthread_atfork(Some(prepare), Some(parent), Some(child)) } == 0 {
                HANDLED.store(true, Ordering::Release);
            }
        }
    }

    #[cfg(unix)]
    thread_local! {
        /// The process-wide locks the forking thread holds across a fork (see [`prepare`]).
        static HELD: std::cell::RefCell<Vec<Box<dyn std::any::Any>>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Runs `f` in the background on the pool, unless a fork is waiting for the background work
    /// running, in which case `f` starts after the fork.
    pub(super) fn spawn(f: impl FnOnce() + Send + 'static) {
        submit(Box::new(f));
    }

    /// Queues a background job on the pool. A job that would start while a fork is in progress is
    /// set aside in [`HELD_BACK`] until the last fork in progress ends, and its thread goes on with
    /// other work. That thread may be running, further down its stack, a job the fork waits for
    /// (a thread waiting for part of its parallel work runs queued work meanwhile), so the held
    /// back job must not wait on it. Nor may it go back to the pool's queue: queued from a thread
    /// of the pool, it would go to that thread's own queue, whose newest job the thread runs first,
    /// so the thread would run it again and again for the length of the fork, never helping with
    /// the work of the jobs the fork waits for, nor running other parallel work, nor reaching the
    /// work queued below it (when a running job queued it: that job's own parallel work).
    fn submit(f: Box<dyn FnOnce() + Send>) {
        pool().spawn(move || match Running::enter() {
            Some(_running) => f(),
            None => hold_back(f),
        });
    }

    /// Sets `job` aside in [`HELD_BACK`] (see [`submit`]).
    fn hold_back(job: Box<dyn FnOnce() + Send>) {
        let node = Box::into_raw(Box::new(HeldBack { job, next: std::ptr::null_mut() }));
        let mut head = HELD_BACK.load(Ordering::SeqCst);
        loop {
            // SAFETY: `node` is not published yet, so this thread alone has it.
            unsafe { (*node).next = head };
            match HELD_BACK.compare_exchange_weak(head, node, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(now) => head = now,
            }
        }
        // The last fork in progress may have ended since `Running::enter` saw it, and its
        // `end_fork` taken the jobs held back before this one: then this one is queued here.
        if GATE.load(Ordering::SeqCst) < FORK {
            release_held_back();
        }
    }

    /// Queues the jobs in [`HELD_BACK`] again, oldest first.
    fn release_held_back() {
        let mut node = HELD_BACK.swap(std::ptr::null_mut(), Ordering::SeqCst);
        let mut jobs = Vec::new();
        while !node.is_null() {
            // SAFETY: nodes come from `Box::into_raw` in `hold_back`, and the swap above took
            // these out of the list, whole, for this thread alone.
            let held = unsafe { Box::from_raw(node) };
            node = held.next;
            jobs.push(held.job);
        }
        for job in jobs.into_iter().rev() {
            submit(job);
        }
    }

    /// A background job running, counted in [`GATE`] while it lives.
    struct Running;

    impl Running {
        /// Counts a job as running, unless a fork is in progress.
        fn enter() -> Option<Self> {
            let mut g = GATE.load(Ordering::SeqCst);
            loop {
                if g >= FORK {
                    return None;
                }
                match GATE.compare_exchange_weak(g, g + 1, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => return Some(Self),
                    Err(now) => g = now,
                }
            }
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            GATE.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Counts a fork as in progress, and waits for the background jobs running to finish; those
    /// that would start from then on are held back (see [`submit`]).
    #[cfg(unix)]
    pub(super) fn begin_fork() {
        GATE.fetch_add(FORK, Ordering::SeqCst);
        while GATE.load(Ordering::SeqCst) & JOBS != 0 {
            std::thread::sleep(NAP);
        }
    }

    /// Counts a fork in progress as done, in the parent: the last of them to end queues the jobs
    /// held back again.
    #[cfg(unix)]
    pub(super) fn end_fork() {
        if GATE.fetch_sub(FORK, Ordering::SeqCst) - FORK < FORK {
            release_held_back();
        }
    }

    /// Before a fork, in the forking thread: waits for the background jobs running to finish,
    /// and holds back those about to start; then takes the process-wide locks of terrio's modules
    /// (see [`super::hold_locks`]), so that no other thread holds one when the process is copied.
    #[cfg(unix)]
    extern "C" fn prepare() {
        begin_fork();
        let _ = HELD.try_with(|held| {
            if let Ok(mut held) = held.try_borrow_mut()
                && held.is_empty()
            {
                super::hold_locks(&mut held);
            }
        });
    }

    /// Lets go of the locks [`prepare`] took.
    #[cfg(unix)]
    fn release_locks() {
        let _ = HELD.try_with(|held| {
            if let Ok(mut held) = held.try_borrow_mut() {
                held.clear();
            }
        });
    }

    /// After a fork, in the parent: the locks are let go, and the background jobs held back
    /// start, unless another fork is in progress.
    #[cfg(unix)]
    extern "C" fn parent() {
        release_locks();
        end_fork();
    }

    /// After a fork, in the child, where only the forking thread is left: the locks, which that
    /// thread holds, are let go; no job runs, and the pool is started again on first use. The
    /// jobs held back are the parent's, like its pool: left alone, neither run nor dropped.
    /// Besides unlocking (atomics, a futex wake) and freeing the guards (glibc's malloc is usable
    /// in the child once `fork` returns), only atomics here, as a child of a process with threads
    /// may only call async-signal-safe functions until `exec`.
    #[cfg(unix)]
    extern "C" fn child() {
        release_locks();
        HELD_BACK.store(std::ptr::null_mut(), Ordering::SeqCst);
        GATE.store(0, Ordering::SeqCst);
        FORKS.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether a fork is waiting for the background work running.
    #[cfg(all(test, unix))]
    pub(super) fn fork_in_progress() -> bool {
        GATE.load(Ordering::SeqCst) >= FORK
    }

    /// The threads of this process's pool that have run its first job, and the pool's threads.
    #[cfg(all(test, unix))]
    pub(super) fn threads_warmed() -> (usize, usize) {
        let s = pool();
        // SAFETY: a published pool is never freed; `pool()` published this process's.
        let started = unsafe { &*POOL.load(Ordering::Acquire) };
        (started.warmed.load(Ordering::Acquire), s.current_num_threads())
    }
}

/// Takes the process-wide locks of terrio's modules, for a fork (`native::prepare`): the remote
/// files' cache, the request statistics, path options and retry warnings, the STAC tokens, the
/// allow-list, the files in memory, the grids' caches and the warper's nodata warnings. Each is
/// held for map operations only, never while another is taken, so any order is free of deadlocks.
#[cfg(all(unix, not(target_family = "wasm")))]
fn hold_locks(held: &mut Vec<Box<dyn std::any::Any>>) {
    crate::formats::hold_for_fork(held);
    crate::memfs::hold_for_fork(held);
    crate::crs::hold_for_fork(held);
    crate::ops::warp::hold_for_fork(held);
    #[cfg(remote)]
    {
        crate::stac::hold_for_fork(held);
        crate::http::hold_for_fork(held);
    }
}

/// Makes sure that a fork holds the process-wide locks (see [`hold_locks`]), so that a child
/// never starts with one locked by a thread it does not have: called by the modules that keep
/// state behind such a lock, before taking it. One atomic load once registered.
#[inline]
pub(crate) fn fork_safe() {
    #[cfg(all(unix, not(target_family = "wasm")))]
    native::register();
}

/// A thread pool parallel work runs on: [`native::pool`], or the one the host started in
/// WebAssembly.
#[cfg(threads)]
struct Pool(&'static rayon::ThreadPool);

#[cfg(threads)]
impl Pool {
    fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        self.0.install(f)
    }
}

/// The thread pool parallel work runs on in WebAssembly: the one the host started (see
/// [`set_thread_pool`]), never rayon's global pool, which would fall back to the calling thread
/// for good when used before the host's workers exist.
#[cfg(all(threads, target_family = "wasm"))]
static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();

/// Makes `pool` the thread pool of terrio's parallel work, in WebAssembly built with threads
/// (whose workers the host starts, as Web Workers or Node.js worker threads). Until it is set,
/// the work runs on the calling thread. Fails, giving the pool back, when one was set already.
#[cfg(all(threads, target_family = "wasm"))]
pub fn set_thread_pool(pool: rayon::ThreadPool) -> Result<(), rayon::ThreadPool> {
    POOL.set(pool)
}

/// The pool to run `pieces` pieces of work on, if any: for `coarse` work, or any work natively.
/// Natively, work that rayon would not split, fewer than `split` pieces, runs on the calling
/// thread, as it did on rayon's global pool: in the same thread as the code around it.
#[cfg(threads)]
fn pool_for(coarse: bool, pieces: usize, split: usize) -> Option<Pool> {
    #[cfg(not(target_family = "wasm"))]
    {
        let _ = coarse;
        (pieces >= split).then(|| Pool(native::pool()))
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = (pieces, split);
        POOL.get().filter(|_| coarse).map(Pool)
    }
}

/// The number of threads parallel work runs on.
pub(crate) fn threads() -> usize {
    #[cfg(not(target_family = "wasm"))]
    {
        native::pool().current_num_threads()
    }
    #[cfg(target_family = "wasm")]
    {
        #[cfg(threads)]
        if let Some(p) = POOL.get() {
            return p.current_num_threads();
        }
        1
    }
}

/// Runs `f` on terrio's thread pool natively, and returns its result; in WebAssembly, on the
/// calling thread. For rayon code that the other functions cannot express, and for calls into a
/// dependency that parallelizes with rayon by itself: their parallel work then runs on this pool,
/// never on rayon's global pool (see the module's documentation). `f` runs on a thread of the
/// pool, so it does not see the calling thread's thread-local state (`config`, `warnings`).
pub(crate) fn install<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    #[cfg(not(target_family = "wasm"))]
    {
        native::pool().install(f)
    }
    #[cfg(target_family = "wasm")]
    {
        f()
    }
}

/// Runs `f` in the background on terrio's thread pool (native builds). A fork waits for the
/// background work running to finish (see the module's documentation), so `f` must be short, and
/// must not wait for other background work.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn spawn(f: impl FnOnce() + Send + 'static) {
    native::spawn(f);
}

/// Maps `f` over `0..n`, in parallel, keeping the order.
fn map_range_on<const COARSE: bool, R: Send>(n: usize, f: impl Fn(usize) -> R + Sync + Send) -> Vec<R> {
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE, n, 2) {
        return p.install(|| (0..n).into_par_iter().map(f).collect());
    }
    (0..n).map(f).collect()
}

/// Maps `f` over items in parallel, keeping the order, with a scratch value from `init` that
/// each piece of work (several items in a row) starts with.
#[cfg_attr(not(all(threads, target_family = "wasm")), allow(dead_code))]
pub(crate) fn map_init<T: Sync, S, R: Send>(
    items: &[T],
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, &T) -> R + Sync + Send,
) -> Vec<R> {
    #[cfg(threads)]
    if let Some(p) = pool_for(true, items.len(), 4) {
        return p.install(|| items.par_iter().with_min_len(2).map_init(init, |s, t| f(s, t)).collect());
    }
    let mut s = init();
    items.iter().map(|t| f(&mut s, t)).collect()
}

/// Maps `f` over mutable items in parallel, keeping the order.
fn map_mut_on<const COARSE: bool, T: Send, R: Send>(items: &mut [T], f: impl Fn(&mut T) -> R + Sync + Send) -> Vec<R> {
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE, items.len(), 2) {
        return p.install(|| items.par_iter_mut().map(f).collect());
    }
    items.iter_mut().map(f).collect()
}

/// Calls `f` on each item of an owned list with its index, in parallel.
fn for_each_owned_on<const COARSE: bool, T: Send>(items: Vec<T>, f: impl Fn(usize, T) + Sync + Send) {
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE, items.len(), 2) {
        return p.install(|| items.into_par_iter().enumerate().for_each(|(i, t)| f(i, t)));
    }
    items.into_iter().enumerate().for_each(|(i, t)| f(i, t));
}

/// Calls `f` on consecutive chunks of `size` items (the last may be shorter) with the chunk's
/// index, in parallel.
fn chunks_mut_on<const COARSE: bool, T: Send>(data: &mut [T], size: usize, f: impl Fn(usize, &mut [T]) + Sync + Send) {
    let size = size.max(1);
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE, data.len().div_ceil(size), 2) {
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
// The COG overview reducer of #165 called this; COG overviews are now GDAL's
// (`gdal_overview`), which do not.
#[allow(dead_code)]
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
    #[cfg(all(unix, not(target_family = "wasm")))]
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

/// Runs `child` in a fork()ed child of this process: its exit code (0 when `child` returns
/// true), or `None` when it is still running after `secs` seconds (it is killed then).
#[cfg(all(test, unix))]
pub(crate) fn in_child(secs: u64, child: impl FnOnce() -> bool) -> Option<i32> {
    // SAFETY: the child runs `child` and leaves with `_exit`, never returning to the harness.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(child)).unwrap_or(false);
        // SAFETY: ends the child without running the parent's exit handlers.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut status = 0;
    loop {
        // SAFETY: `pid` is this process's child; `status` is a valid pointer.
        if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
            return Some(if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { -libc::WTERMSIG(status) });
        }
        if std::time::Instant::now() > deadline {
            // SAFETY: as above.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Runs a fork test's `scenario` in a process of its own, where it must return true: this test
/// binary started again for this one test (`--exact`), killed when still running after `secs`
/// seconds. A fork copies every thread's state, and the test binary runs other tests on other
/// threads, which hold locks of their own when the fork comes (the standard library's, for one,
/// while a thread starts or ends): a child forked from the test process may wait for them
/// forever. In a process of its own, a scenario's forks copy only the scenario's threads.
#[cfg(all(test, unix))]
pub(crate) fn alone(secs: u64, scenario: impl FnOnce() -> bool) {
    const ALONE: &str = "TERRIO_TEST_ALONE";
    let name = std::thread::current().name().expect("a test thread is named after its test").to_string();
    if std::env::var_os(ALONE).is_some_and(|v| v == name.as_str()) {
        assert!(scenario(), "the scenario failed");
        return;
    }
    let log = std::env::temp_dir().join(format!("terrio-alone-{}-{}.log", std::process::id(), name.replace("::", "-")));
    let out = std::fs::File::create(&log).expect("a log file for the test's own process");
    let mut process = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args([name.as_str(), "--exact", "--test-threads=1", "--nocapture"])
        .env(ALONE, &name)
        .stdout(out.try_clone().expect("the log file"))
        .stderr(out)
        .spawn()
        .expect("the test binary starts again");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        if let Some(status) = process.try_wait().expect("the test's own process") {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            let _ = process.kill();
            let _ = process.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let ran = output.contains("test result: ok. 1 passed");
    assert!(
        status.is_some_and(|s| s.success()) && ran,
        "{name} in a process of its own: {status:?} (None: killed after {secs} s)\n{output}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn parallel_work_runs_in_a_child_forked_after_parallel_work() {
        alone(60, || {
            // The parent's pool is started, as after any read or write.
            let doubled: Vec<usize> = (0..10_000).map(|i| i * 2).collect();
            assert_eq!(map_range(10_000, |i| i * 2), doubled);
            let (tx, rx) = std::sync::mpsc::channel();
            spawn(move || tx.send(threads()).unwrap());
            assert!(rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok_and(|n| n >= 1));
            // In the child, the parent's pool has no threads: each call below waited forever on it.
            let exit = in_child(30, || {
                let mapped = map_range_coarse(10_000, |i| i * 2) == doubled;
                let mut chunks = vec![0usize; 10_000];
                chunks_mut(&mut chunks, 100, |i, c| c.fill(i));
                let chunked = chunks.iter().enumerate().all(|(k, v)| *v == k / 100);
                let (tx, rx) = std::sync::mpsc::channel();
                spawn(move || tx.send(()).unwrap());
                let spawned = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
                // A dependency's rayon calls (here `rayon::join`) inside `install`.
                let joined = install(|| rayon::join(|| 1, || 2)) == (1, 2);
                mapped && chunked && spawned && joined
            });
            assert_eq!(exit, Some(0), "the child hung (None) or failed (1)");
            true
        });
    }

    #[test]
    #[cfg(unix)]
    fn a_fork_waits_for_background_work_running() {
        alone(60, || {
            static TABLE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
            let (started, wait) = std::sync::mpsc::channel();
            spawn(move || {
                TABLE.get_or_init(|| {
                    started.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    7
                });
            });
            // The initialization has begun on the pool: a fork now copies it half done, and the
            // child waits for it forever, unless the fork waits for it to finish.
            wait.recv().unwrap();
            assert_eq!(in_child(30, || *TABLE.get_or_init(|| 0) == 7), Some(0), "the child hung (None) or failed (1)");
            true
        });
    }

    #[test]
    #[cfg(unix)]
    fn a_dependency_that_uses_rayon_runs_in_a_forked_child() {
        alone(60, || {
            // A progressive JPEG, which `jpeg-decoder` decodes (terrio's own decoder reads baseline
            // ones), with `rayon::in_place_scope` above 128 x 128 pixels: on rayon's global pool
            // when called outside `install`, which the parent's call starts here.
            let (w, h) = (512u16, 512u16);
            let rgb: Vec<u8> = (0..usize::from(w) * usize::from(h) * 3).map(|i| (i * 7 % 251) as u8).collect();
            let mut jpeg = Vec::new();
            let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, 90);
            encoder.set_progressive(true);
            encoder.encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
            let decoded = crate::jpeg::decode_file(&jpeg).unwrap().data;
            assert_eq!(decoded.len(), rgb.len());
            let exit = in_child(30, || crate::jpeg::decode_file(&jpeg).is_ok_and(|i| i.data == decoded));
            assert_eq!(exit, Some(0), "the child hung (None) or failed (1)");
            true
        });
    }

    #[test]
    fn install_runs_on_terrios_pool() {
        let name = install(|| std::thread::current().name().map(str::to_string));
        assert!(name.as_deref().is_some_and(|n| n.starts_with("terrio-par-")), "{name:?}");
        let nested = install(rayon::current_num_threads);
        assert_eq!(nested, threads());
        // Work rayon would not split stays on the calling thread, as on rayon's global pool.
        let caller = std::thread::current().id();
        assert_eq!(map_range(1, |_| std::thread::current().id()), vec![caller]);
        assert!(map_range(64, |_| std::thread::current().id()).iter().all(|id| *id != caller));
    }

    /// Every process-wide lock terrio's modules list for a fork is free in the child, even when
    /// another thread of the parent holds them all when the fork is asked for.
    #[test]
    #[cfg(unix)]
    fn a_child_never_starts_with_a_process_wide_lock_held() {
        alone(60, || {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::time::Duration;
            let (holding, released) = (std::sync::Arc::new(AtomicBool::new(false)), std::sync::Arc::new(AtomicBool::new(false)));
            let (h, r) = (holding.clone(), released.clone());
            let holder = std::thread::spawn(move || {
                let mut held = Vec::new();
                hold_locks(&mut held);
                h.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(300));
                r.store(true, Ordering::Release);
                drop(held);
            });
            while !holding.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            // The child takes every lock, as its first remote read, allow-list check or CRS lookup
            // would.
            let exit = in_child(30, || {
                let mut held = Vec::new();
                hold_locks(&mut held);
                !held.is_empty()
            });
            assert_eq!(exit, Some(0), "the child hung on a lock (None) or failed (1)");
            // The fork waited for the locks: it happened once the other thread was letting them go.
            assert!(released.load(Ordering::Acquire));
            holder.join().unwrap();
            true
        });
    }

    /// A background job running when a fork starts may wait for parallel work of its own (a COG
    /// tile's compression does); while it waits, its thread runs other queued work, among which
    /// background jobs the fork holds back. Those must not wait on that thread for the fork to
    /// end: the fork waits for the job below them on the same stack.
    #[test]
    #[cfg(unix)]
    fn a_fork_ends_while_a_running_job_waits_for_its_parallel_work() {
        alone(60, || {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::time::{Duration, Instant};
            let wait_for = |flag: &AtomicBool| {
                while !flag.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            };
            let (other_started, release) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (s, r) = (other_started.clone(), release.clone());
            // The running job: two halves; the second is taken by another thread, so the job's own
            // thread, once its half is done, waits for it, running queued work meanwhile.
            spawn(move || {
                install(|| {
                    rayon::join(
                        || {
                            wait_for(&s);
                            wait_for(&r);
                        },
                        || {
                            s.store(true, Ordering::Release);
                            wait_for(&r);
                            std::thread::sleep(Duration::from_millis(300));
                        },
                    )
                });
            });
            wait_for(&other_started);
            // The fork, from another thread, so that a fork that never ends fails the test.
            let (forked, fork_result) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = forked.send(in_child(30, || true));
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while !native::fork_in_progress() {
                assert!(Instant::now() < deadline, "the fork did not start");
                std::thread::sleep(Duration::from_millis(1));
            }
            // More jobs than threads, held back by the fork: some stay queued for the running job's
            // thread to take while it waits.
            let ran = Arc::new(AtomicUsize::new(0));
            let jobs = 2 * threads() + 2;
            for _ in 0..jobs {
                let ran = ran.clone();
                spawn(move || {
                    ran.fetch_add(1, Ordering::AcqRel);
                });
            }
            std::thread::sleep(Duration::from_millis(100));
            release.store(true, Ordering::Release);
            let exit = fork_result.recv_timeout(Duration::from_secs(60));
            assert_eq!(exit, Ok(Some(0)), "the fork never ended (Err) or the child hung (None)");
            // The jobs held back run once the fork is done.
            let deadline = Instant::now() + Duration::from_secs(10);
            while ran.load(Ordering::Acquire) < jobs && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(ran.load(Ordering::Acquire), jobs);
            true
        });
    }

    /// Waits for `flag`, for at most `secs` seconds: whether it was set.
    #[cfg(unix)]
    fn wait_for(flag: &std::sync::atomic::AtomicBool, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while !flag.load(std::sync::atomic::Ordering::Acquire) {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        true
    }

    /// Queues `jobs` background jobs that count themselves in `ran` when they run.
    #[cfg(unix)]
    fn count_jobs(jobs: usize, ran: &std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        for _ in 0..jobs {
            let ran = ran.clone();
            spawn(move || {
                ran.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            });
        }
    }

    /// Waits for `ran` to reach `jobs`, for at most 10 seconds: what it reached.
    #[cfg(unix)]
    fn wait_for_jobs(jobs: usize, ran: &std::sync::atomic::AtomicUsize) -> usize {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ran.load(std::sync::atomic::Ordering::Acquire) < jobs && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        ran.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A background job running when a fork starts may queue another one (`spawn` allows it).
    /// Queued from a thread of the pool, that job goes on the thread's own queue, above the
    /// running job's work still queued there (here the second half of a `join`). The fork holds it
    /// back: if it went back to the same queue, the thread would take it again and again, never
    /// reaching the half below it, which the other threads, kept just as busy by the other jobs
    /// held back, never take; the running job, and so the fork, would never finish. In a process
    /// of its own (`alone`), so that a fork that never ends stops only that process.
    #[test]
    #[cfg(unix)]
    fn a_running_job_that_queues_one_during_a_fork_finishes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        alone(60, || {
            let flag = || Arc::new(AtomicBool::new(false));
            let (entered, go, queued_ran, half_ran) = (flag(), flag(), flag(), flag());
            let (e, g, q, h) = (entered.clone(), go.clone(), queued_ran.clone(), half_ran.clone());
            spawn(move || {
                e.store(true, Ordering::Release);
                wait_for(&g, 30);
                install(|| rayon::join(move || spawn(move || q.store(true, Ordering::Release)), move || h.store(true, Ordering::Release)));
            });
            if !wait_for(&entered, 10) {
                return false;
            }
            let (forked, fork_result) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = forked.send(in_child(30, || true));
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while !native::fork_in_progress() {
                if Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            // More jobs held back than threads: each other thread of the pool takes one.
            let ran = Arc::new(AtomicUsize::new(0));
            let jobs = 2 * threads() + 2;
            count_jobs(jobs, &ran);
            std::thread::sleep(Duration::from_millis(100));
            go.store(true, Ordering::Release);
            if fork_result.recv_timeout(Duration::from_secs(20)) != Ok(Some(0)) {
                return false;
            }
            wait_for_jobs(jobs, &ran) == jobs && wait_for(&queued_ran, 10) && half_ran.load(Ordering::Acquire)
        });
    }

    /// Background jobs held back by a fork leave the pool's threads to other work: parallel work
    /// started meanwhile (a read in another thread) runs at once, while those jobs still wait.
    #[test]
    #[cfg(unix)]
    fn parallel_work_runs_while_a_fork_holds_background_jobs_back() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        // A fork in progress, as `prepare` counts one, with more jobs held back than threads.
        native::begin_fork();
        let ran = Arc::new(AtomicUsize::new(0));
        let jobs = 2 * threads() + 2;
        count_jobs(jobs, &ran);
        std::thread::sleep(Duration::from_millis(100));
        let (mapped, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = mapped.send(map_range(64, |i| i * 2));
        });
        let result = result.recv_timeout(Duration::from_secs(10));
        let ran_during_fork = ran.load(Ordering::Acquire);
        native::end_fork();
        assert_eq!(result, Ok((0..64).map(|i| i * 2).collect::<Vec<_>>()), "the parallel work waited for the fork");
        assert_eq!(ran_during_fork, 0, "a job held back ran during the fork");
        assert_eq!(wait_for_jobs(jobs, &ran), jobs, "the jobs held back did not all run after the fork");
    }

    /// Two threads fork at once, while a background job runs that takes the process-wide locks as
    /// it ends: both forks wait for it, take the locks in turn, end, and leave children that work;
    /// the jobs held back meanwhile run once both are done. In a process of its own (`alone`), so
    /// that forks that never end stop only that process.
    #[test]
    #[cfg(unix)]
    fn two_threads_fork_at_once() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        use std::time::Duration;
        alone(90, || {
            let started = Arc::new(AtomicBool::new(false));
            let s = started.clone();
            spawn(move || {
                s.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(200));
                let mut held = Vec::new();
                hold_locks(&mut held);
            });
            if !wait_for(&started, 10) {
                return false;
            }
            let barrier = Arc::new(Barrier::new(3));
            let (forked, fork_results) = std::sync::mpsc::channel();
            for _ in 0..2 {
                let (b, forked) = (barrier.clone(), forked.clone());
                std::thread::spawn(move || {
                    b.wait();
                    let _ = forked.send(in_child(30, || {
                        let mapped = map_range(1000, |i| i * 2) == (0..1000).map(|i| i * 2).collect::<Vec<_>>();
                        let (tx, rx) = std::sync::mpsc::channel();
                        spawn(move || {
                            let _ = tx.send(());
                        });
                        let spawned = rx.recv_timeout(Duration::from_secs(10)).is_ok();
                        let mut held = Vec::new();
                        hold_locks(&mut held);
                        mapped && spawned && !held.is_empty()
                    }));
                });
            }
            barrier.wait();
            std::thread::sleep(Duration::from_millis(50));
            let ran = Arc::new(AtomicUsize::new(0));
            count_jobs(8, &ran);
            let exits: Vec<_> = (0..2).map(|_| fork_results.recv_timeout(Duration::from_secs(40))).collect();
            exits == [Ok(Some(0)), Ok(Some(0))] && wait_for_jobs(8, &ran) == 8 && !native::fork_in_progress()
        });
    }

    /// The forks in progress are counted: when two overlap and the first ends, the jobs held back
    /// still wait for the second. In a process of its own (`alone`), so that no other test's fork
    /// overlaps.
    #[test]
    #[cfg(unix)]
    fn jobs_held_back_wait_for_the_last_of_two_forks() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        alone(60, || {
            // A fork in progress, as `prepare` counts one.
            native::begin_fork();
            let ran = Arc::new(AtomicUsize::new(0));
            count_jobs(4, &ran);
            // Another fork, from another thread, which starts and ends meanwhile.
            let other = std::thread::spawn(|| in_child(30, || true)).join();
            std::thread::sleep(Duration::from_millis(100));
            let held = ran.load(Ordering::Acquire) == 0 && native::fork_in_progress();
            native::end_fork();
            matches!(other, Ok(Some(0))) && held && wait_for_jobs(4, &ran) == 4 && !native::fork_in_progress()
        });
    }

    /// A fork that ends while the pool's threads are setting jobs it held back aside leaves none
    /// of them behind: each job either is in the list the fork's end takes, or finds no fork in
    /// progress once in it. 200 forks, each ending while 2,000 jobs it held back are being set
    /// aside; all of each fork's jobs must run before the next fork begins. In a process of its
    /// own (`alone`), so that no other test's job is held back by these forks.
    #[test]
    #[cfg(unix)]
    fn a_fork_that_ends_while_jobs_are_set_aside_leaves_none_behind() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        alone(60, || {
            let ran = Arc::new(AtomicUsize::new(0));
            let jobs = 2_000;
            (1..=200).all(|fork| {
                native::begin_fork();
                count_jobs(jobs, &ran);
                native::end_fork();
                wait_for_jobs(fork * jobs, &ran) == fork * jobs
            })
        });
    }

    /// When a call that started the pool returns, every thread of the pool has run a job: it has
    /// started, and looked for work once (see `native::start`), so that none of them holds the
    /// standard library's lock of starting threads, or is initializing crossbeam-epoch's
    /// collector, when the process forks. In a process of its own, 20 times: a child queues a
    /// background job, which starts the child's pool and returns at once, and counts the threads.
    #[test]
    #[cfg(unix)]
    fn a_call_that_starts_the_pool_returns_once_its_threads_have_run_a_job() {
        alone(60, || {
            (0..20).all(|_| {
                in_child(30, || {
                    spawn(|| ());
                    let (warmed, threads) = native::threads_warmed();
                    warmed == threads
                }) == Some(0)
            })
        });
    }

    /// A child forked right after a call that started the pool returned runs parallel work of its
    /// own. A background job (`spawn`: opening a vector file queues one, the EPSG table's) starts
    /// the pool and returns at once; the pool's threads, meanwhile, start (holding the standard
    /// library's `stack_overflow::thread_info::LOCK`) and look for work (the first one
    /// initializing crossbeam-epoch's `COLLECTOR`). A child forked in the middle of either inherits
    /// it held, and its first parallel call waits forever (both seen: the second with the
    /// grandchild's threads all in futex_do_wait at `COLLECTOR`+0x8). 50 times, in a process of
    /// its own: a child queues a background job and at once forks a grandchild, whose parallel call
    /// starts its own pool. The scenario; `a_call_that_starts_the_pool_returns_once_its_threads_have_run_a_job`
    /// is the guard.
    #[test]
    #[cfg(unix)]
    fn a_child_forked_right_after_the_pool_started_starts_threads() {
        alone(120, || {
            let doubled: Vec<usize> = (0..64).map(|i| i * 2).collect();
            (0..50).all(|_| {
                in_child(30, || {
                    spawn(|| ());
                    in_child(30, || map_range(64, |i| i * 2) == doubled) == Some(0)
                }) == Some(0)
            })
        });
    }
}
