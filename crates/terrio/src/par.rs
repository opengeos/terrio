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
//! - `with_waiting_workers`, for scoped native I/O workers, cancelled and joined before a fork;
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

//! Data-parallel closures must use resolved, captured options and return diagnostics explicitly.
//! Thread-local configuration and warnings stay on their originating thread. Contention can
//! select serial execution on that thread instead of a worker, so closures that consult thread
//! locals can observe different state; placement does not promise to propagate those locals.
//!
//! In native builds, a worker of another rayon pool (a pool of the caller's own) that waits on this
//! one does so from a thread of its own: waiting through rayon, it would run its own pool's other
//! jobs on its stack meanwhile, nested in its call (see `without_stealing`). A fork waits for that
//! thread, as for `map_waiting`'s, until the call it waits for has ended.

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

    /// The generation of guards belonging to this process, advanced in the child after a fork.
    #[cfg(threads)]
    pub(super) fn fork_generation() -> u64 {
        FORKS.load(Ordering::Acquire)
    }
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
    /// Active I/O batches whose reads must stop before a fork can copy thread state. Entries
    /// are added after Running::enter and removed before its guard drops. The registry itself
    /// is held across fork too, because another forking thread may inspect it without a job.
    #[cfg(all(unix, remote))]
    static CANCELLATIONS: std::sync::Mutex<Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>> = std::sync::Mutex::new(Vec::new());

    #[cfg(all(unix, remote))]
    fn cancellations() -> std::sync::MutexGuard<'static, Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>> {
        CANCELLATIONS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(all(unix, remote))]
    pub(super) struct CancelOnFork(std::sync::Arc<std::sync::atomic::AtomicBool>);

    #[cfg(all(unix, remote))]
    pub(super) fn cancel_on_fork(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> CancelOnFork {
        cancellations().push(flag.clone());
        // A fork may have inspected the registry after this job was counted but before its
        // flag was published. In that case this registering thread publishes cancellation.
        if GATE.load(Ordering::SeqCst) >= FORK {
            flag.store(true, Ordering::SeqCst);
        }
        CancelOnFork(flag)
    }

    #[cfg(all(unix, remote))]
    impl Drop for CancelOnFork {
        fn drop(&mut self) {
            let mut active = cancellations();
            if let Some(index) = active.iter().position(|flag| std::sync::Arc::ptr_eq(flag, &self.0)) {
                active.swap_remove(index);
            }
        }
    }

    #[cfg(all(unix, remote))]
    pub(super) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
        held.push(Box::new(cancellations()));
    }

    /// The background jobs held back by the forks in progress, newest first (see [`submit`]).
    static HELD_BACK: AtomicPtr<HeldBack> = AtomicPtr::new(std::ptr::null_mut());

    /// A background job in [`HELD_BACK`], with the one held back before it.
    struct HeldBack {
        job: Box<dyn FnOnce() + Send>,
        next: *mut HeldBack,
    }

    /// The pool of this process, if started, without starting it.
    pub(super) fn started() -> Option<&'static rayon::ThreadPool> {
        // SAFETY: a published pool is never freed.
        unsafe { POOL.load(Ordering::Acquire).as_ref() }.filter(|s| s.forks == FORKS.load(Ordering::Acquire)).map(|s| &s.pool)
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
        // A job spawned on the pool that panics stops there, the panic hook having reported it:
        // without a handler, rayon aborts the process (rayon-core 1.13). This covers `spawn` and a
        // `rayon::spawn` by a dependency inside `install`; `join`, `scope` and parallel iterators
        // still give the panic to their caller. A job's result reaches its caller through a
        // channel or a lazily initialized value, so the caller sees a closed channel (an error) or
        // an uninitialized value (computed again on its own thread). Under memory pressure, a
        // decoder that panics on a failed allocation (`ruzstd`'s ring buffer did, in the EPSG table
        // `crs::prefetch` loads) thus no longer aborts a process that would answer.
        let pool = rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("terrio-par-{i}"))
            .panic_handler(|_| {})
            .build()
            .expect("terrio's thread pool starts");
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
        super::without_stealing(|| {
            pool.broadcast(move |_| {
                w.fetch_add(1, Ordering::AcqRel);
            })
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

    /// A background job running, counted in [`GATE`] while it lives (also the threads of a
    /// [`super::map_waiting`], and the thread a worker of another pool waits from in
    /// [`super::without_stealing`]).
    pub(super) struct Running;

    impl Running {
        /// Counts a job as running, unless a fork is in progress.
        pub(super) fn enter() -> Option<Self> {
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
        #[cfg(remote)]
        for flag in cancellations().iter() {
            flag.store(true, Ordering::SeqCst);
        }
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
        // Other threads' guards are gone; surviving caller guards belong to the old generation.
        super::OUTSIDE_ON_POOL.store(false, Ordering::SeqCst);
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

/// Takes the process-wide locks of terrio's modules, for a fork (`native::prepare`): the decoded
/// chunks' cache, the spare TIFF decoders, the remote files' cache, the request statistics, path
/// options and retry warnings, the process's HTTP agent, the STAC tokens, the allow-list, the files
/// in memory, the grids' caches and the warper's nodata warnings. Each is held for map operations
/// (the agent while one is built) only, never while another is taken, so any order is free of
/// deadlocks.
#[cfg(all(unix, not(target_family = "wasm")))]
fn hold_locks(held: &mut Vec<Box<dyn std::any::Any>>) {
    crate::block_cache::hold_for_fork(held);
    crate::geotiff::hold_for_fork(held);
    crate::formats::hold_for_fork(held);
    crate::memfs::hold_for_fork(held);
    crate::crs::hold_for_fork(held);
    crate::ops::warp::hold_for_fork(held);
    #[cfg(remote)]
    {
        native::hold_for_fork(held);
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

/// Runs a native I/O batch with its worker threads counted until they have joined. A fork
/// cancels the batch before waiting for its threads. When a fork already holds background
/// work back, `work(false)` runs without worker threads instead, as map_waiting's fallback does.
#[cfg(remote)]
pub(crate) fn with_waiting_workers<R>(cancel: std::sync::Arc<std::sync::atomic::AtomicBool>, work: impl FnOnce(bool) -> R) -> R {
    fork_safe();
    if let Some(_running) = native::Running::enter() {
        #[cfg(unix)]
        let _cancel = native::cancel_on_fork(cancel);
        #[cfg(not(unix))]
        let _ = cancel;
        work(true)
    } else {
        work(false)
    }
}

/// A thread pool parallel work runs on: [`native::pool`], or the one the host started in
/// WebAssembly.
#[cfg(threads)]
struct Pool(&'static rayon::ThreadPool);

/// Whether a call from a thread outside the pool has its parallel work on the pool. While one
/// has, another outside call runs serially instead of queueing on the pool (see
/// [`Pool::enter`]). A fork clears it; surviving guards must not change the child's.
#[cfg(all(threads, not(target_family = "wasm")))]
static OUTSIDE_ON_POOL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How a [`Pool`] call runs: on the pool, or serially on the calling thread (see [`Pool::enter`]).
/// In WebAssembly `enter` only returns `Pool`, so the serial variant is never built there.
#[cfg(threads)]
#[cfg_attr(target_family = "wasm", allow(dead_code))]
enum Enter {
    /// Runs on the caller's thread without holding the pool, so a nested `par` call inside it
    /// can take the pool when no other outside call holds it.
    Serial,
    Pool(Option<OutsideGuard>),
}

/// Holds the pool for one outside thread's call while it lives; does nothing in WebAssembly.
#[cfg(threads)]
#[cfg_attr(target_family = "wasm", allow(dead_code))]
struct OutsideGuard {
    #[cfg(not(target_family = "wasm"))]
    forks: u64,
}

#[cfg(all(threads, not(target_family = "wasm")))]
impl Drop for OutsideGuard {
    fn drop(&mut self) {
        // The forking thread can retain a guard while new child work holds the child's pool.
        if self.forks == native::fork_generation() {
            OUTSIDE_ON_POOL.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(threads)]
impl Pool {
    fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        without_stealing(|| self.0.install(f))
    }

    /// Applies placement; a call on the pool keeps its outside guard until it returns or unwinds.
    fn run<R: Send>(&self, f: impl FnOnce(bool) -> R + Send) -> R {
        match self.enter() {
            Enter::Serial => f(false),
            Enter::Pool(_outside) => self.install(|| f(true)),
        }
    }

    /// Uses the pool for its workers and for one outside call at a time, which holds a guard for
    /// the whole call; an outside call overlapping it runs serially. Serial calls do not hold the
    /// pool: two outside threads at work keep it busy with one of their calls while the other
    /// runs on its own thread, rather than both running serially. Serial work must not use
    /// rayon's iterators, which would enter its global pool. A nested `par` call made from serial
    /// work can take the pool when it is free, so it must not hold a lock that a pool job takes.
    ///
    /// Thread-local configuration and warnings do not follow work to pool threads.
    /// Serial fallback observes the caller's state, so that observation can depend on
    /// contention. Capture resolved options and return warnings explicitly from worker work.
    fn enter(&self) -> Enter {
        #[cfg(not(target_family = "wasm"))]
        {
            if self.0.current_thread_index().is_none() {
                use std::sync::atomic::Ordering::Relaxed;
                return match OUTSIDE_ON_POOL.compare_exchange(false, true, Relaxed, Relaxed) {
                    Ok(_) => Enter::Pool(Some(OutsideGuard { forks: native::fork_generation() })),
                    Err(_) => Enter::Serial,
                };
            }
            Enter::Pool(None)
        }
        #[cfg(target_family = "wasm")]
        {
            Enter::Pool(None)
        }
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
        without_stealing(|| native::pool().install(f))
    }
    #[cfg(target_family = "wasm")]
    {
        f()
    }
}

/// Runs `wait`, which waits on terrio's pool, so that the calling thread runs nothing else in the
/// meantime. A worker of another rayon pool (a pool of the caller's own) waiting on this one through
/// rayon runs jobs of its own pool while it waits, nested on its stack: as many as that pool has
/// queued, such as reads of the VRT the waiting read is reading. Such a worker waits on a thread of
/// its own instead, which runs nothing else, counted as background work for the fork protocol from
/// before it starts until it has ended (see [`map_waiting`]). When a fork is in progress, or the OS
/// refuses the thread, the worker waits itself.
#[cfg(not(target_family = "wasm"))]
fn without_stealing<R: Send>(wait: impl FnOnce() -> R + Send) -> R {
    if rayon::current_thread_index().is_none() || native::started().is_some_and(|p| p.current_thread_index().is_some()) {
        return wait();
    }
    #[cfg(unix)]
    native::register();
    let Some(running) = native::Running::enter() else {
        return wait();
    };
    let wait = std::sync::Mutex::new(Some(wait));
    let take = || wait.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().expect("the wait runs once");
    let waited = std::thread::scope(|s| {
        std::thread::Builder::new().name("terrio-wait".into()).spawn_scoped(s, || take()()).ok().map(|waiter| waiter.join())
    });
    drop(running);
    match waited {
        Some(waited) => waited.unwrap_or_else(|p| std::panic::resume_unwind(p)),
        // Without its waiting thread, this worker waits itself and may run its pool's other jobs:
        // this wait no longer counts for a fork.
        None => take()(),
    }
}

/// Runs `wait`: WebAssembly has no thread of its own to wait from.
#[cfg(all(threads, target_family = "wasm"))]
fn without_stealing<R: Send>(wait: impl FnOnce() -> R + Send) -> R {
    wait()
}

/// Runs `f` in the background on terrio's thread pool (native builds). A fork waits for the
/// background work running to finish (see the module's documentation), so `f` must be short, and
/// must not wait for other background work. A panic in `f` stops `f` alone, never the process:
/// the panic hook reports it, and a channel `f` would answer through is closed.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn spawn(f: impl FnOnce() + Send + 'static) {
    native::spawn(f);
}

/// Maps `f` over `0..n`, in parallel, keeping the order.
fn map_range_on<const COARSE: bool, R: Send>(n: usize, f: impl Fn(usize) -> R + Sync + Send) -> Vec<R> {
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE, n, 2) {
        return p.run(|parallel| if parallel { (0..n).into_par_iter().map(f).collect() } else { (0..n).map(f).collect() });
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
        return p.run(|parallel| {
            if parallel {
                items.par_iter().with_min_len(2).map_init(init, |s, t| f(s, t)).collect()
            } else {
                let mut s = init();
                items.iter().map(|t| f(&mut s, t)).collect()
            }
        });
    }
    let mut s = init();
    items.iter().map(|t| f(&mut s, t)).collect()
}

/// Calls `f` on each item of an owned list with its index, in parallel.
fn for_each_owned_on<const COARSE: bool, T: Send>(items: Vec<T>, f: impl Fn(usize, T) + Sync + Send) {
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE, items.len(), 2) {
        return p.run(|parallel| {
            if parallel {
                items.into_par_iter().enumerate().for_each(|(i, t)| f(i, t));
            } else {
                items.into_iter().enumerate().for_each(|(i, t)| f(i, t));
            }
        });
    }
    items.into_iter().enumerate().for_each(|(i, t)| f(i, t));
}

/// Calls `f` on consecutive chunks of `size` items (the last may be shorter) with the chunk's
/// index, in parallel.
fn chunks_mut_on<const COARSE: bool, T: Send>(data: &mut [T], size: usize, f: impl Fn(usize, &mut [T]) + Sync + Send) {
    let size = size.max(1);
    #[cfg(threads)]
    if let Some(p) = pool_for(COARSE, data.len().div_ceil(size), 2) {
        return p.run(|parallel| {
            if parallel {
                data.par_chunks_mut(size).enumerate().for_each(|(i, c)| f(i, c));
            } else {
                data.chunks_mut(size).enumerate().for_each(|(i, c)| f(i, c));
            }
        });
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

/// Maps `f` over `0..n`, keeping the order, with up to `at_once` calls at a time on the calling
/// thread and threads of their own (native builds), for work that mostly waits on a server: on
/// the pool, as many requests would be in flight as the machine has cores. The first observed
/// error is returned, and once a call has failed, no worker starts another. Native HTTP reads
/// already in flight and their retry waits are cancelled; nested waiting work inherits this
/// cancellation. An HTTP cancellation error never replaces the error that stopped the batch.
/// When independent calls fail concurrently, completion timing determines which error is
/// observed first; error selection is therefore not deterministic across runs.
///
/// The threads see the caller's scoped options and STAC asset read policy
/// ([`crate::stac::AssetReadContext`]), which a new thread would not have. They count as
/// background work for the fork protocol (see the module's documentation) from before the first
/// starts until the last has ended (joined): a fork waits for them, so that a child never copies a
/// thread starting, ending or half way through an initialization. When a fork is already in
/// progress, the calls run one after another on the calling thread instead of waiting for it,
/// so that a call nested in work the fork waits for cannot deadlock. One call at a time, and in
/// WebAssembly, on the calling thread.
pub(crate) fn map_waiting<R: Send>(n: usize, at_once: usize, f: impl Fn(usize) -> crate::Result<R> + Sync) -> crate::Result<Vec<R>> {
    // The fork handlers, registered before the threads are counted: the pool may not be started.
    #[cfg(all(unix, not(target_family = "wasm")))]
    if at_once.min(n) > 1 {
        native::register();
    }
    #[cfg(not(target_family = "wasm"))]
    if at_once.min(n) > 1
        && let Some(_running) = native::Running::enter()
    {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let context = crate::stac::AssetReadContext::capture(crate::stac::AssetReadPolicy::current());
        let (next, failed) = (AtomicUsize::new(0), std::sync::Arc::new(AtomicBool::new(false)));
        #[cfg(remote)]
        let cancellation = crate::http::ReadCancellation::capture_with(failed.clone());
        // A worker that cannot enter the context (consent revoked since the read began) claims no
        // item. Publish any initiating error before cancelling the other workers.
        let failure = std::sync::Mutex::new(None);
        let fail = |error| {
            failure.lock().unwrap().get_or_insert(error);
            failed.store(true, Ordering::SeqCst);
        };
        // A worker's loop: items claimed until none is left or one has failed. The calling thread
        // runs one too, already in its own context.
        let work = |enter: bool| {
            #[cfg(remote)]
            let _cancellation = cancellation.enter();
            let mut mine = Vec::new();
            let _scope = match enter.then(|| context.enter()).transpose() {
                Ok(scope) => scope,
                Err(e) => {
                    fail(e);
                    return mine;
                }
            };
            while !failed.load(Ordering::Relaxed) {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                match f(i) {
                    Ok(value) => mine.push((i, value)),
                    Err(error) => {
                        fail(error);
                        break;
                    }
                }
            }
            mine
        };
        // Joined when the scope ends, before `_running` lets a fork go on. A thread the OS
        // refuses leaves its share to the others and the calling thread.
        let mut done: Vec<(usize, R)> = std::thread::scope(|s| {
            let workers: Vec<_> = (1..at_once.min(n))
                .map_while(|_| std::thread::Builder::new().name("terrio-wait".into()).spawn_scoped(s, || work(true)).ok())
                .collect();
            let mut done = work(false);
            for w in workers {
                done.extend(w.join().unwrap_or_else(|p| std::panic::resume_unwind(p)));
            }
            done
        });
        done.sort_by_key(|(i, _)| *i);
        // Record an error before publishing cancellation, so that a lower-index cancelled
        // request cannot hide the actual failure.
        return match failure.into_inner().unwrap() {
            Some(e) => Err(e),
            None => Ok(done.into_iter().map(|(_, value)| value).collect()),
        };
    }
    #[cfg(target_family = "wasm")]
    let _ = at_once;
    (0..n).map(f).collect()
}

/// Maps a coarse range in order with scratch owned by each Rayon work piece.
/// The serial fallback reuses one scratch value; no state outlives this call.
pub(crate) fn map_range_init_coarse<S, R: Send>(
    n: usize,
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, usize) -> R + Sync + Send,
) -> Vec<R> {
    #[cfg(threads)]
    if let Some(p) = pool_for(true, n, 2) {
        return p.run(|parallel| {
            if parallel {
                (0..n).into_par_iter().map_init(init, f).collect()
            } else {
                let mut scratch = init();
                (0..n).map(|i| f(&mut scratch, i)).collect()
            }
        });
    }
    let mut scratch = init();
    (0..n).map(|i| f(&mut scratch, i)).collect()
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

/// Runs a fork test's `scenario`, or one that needs terrio's pool not started yet, in a process of
/// its own, where it must return true: this test binary started again for this one test
/// (`--exact`), killed when still running after `secs` seconds. A fork copies every thread's state, and the test binary runs other tests on other
/// threads, which hold locks of their own when the fork comes (the standard library's, for one,
/// while a thread starts or ends): a child forked from the test process may wait for them
/// forever. In a process of its own, a scenario's forks copy only the scenario's threads.
#[cfg(all(test, not(target_family = "wasm")))]
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
    #[test]
    fn initialized_range_keeps_order_and_serial_scratch() {
        let init_count = std::sync::atomic::AtomicUsize::new(0);
        let init = || {
            init_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::rc::Rc::new(std::cell::Cell::new(0usize))
        };
        let empty: Vec<usize> = map_range_init_coarse(0, init, |_, i| i);
        assert!(empty.is_empty());
        let one = map_range_init_coarse(1, init, |s, i| {
            s.set(s.get() + 1);
            (i, s.get())
        });
        assert_eq!(one, [(0, 1)]);
        assert_eq!(init_count.load(std::sync::atomic::Ordering::Relaxed), 2, "one initialization per serial call");
        let results = map_range_init_coarse(1000, init, |s, i| {
            s.set(s.get() + 1);
            if i % 7 == 0 { Err(i) } else { Ok(i) }
        });
        assert_eq!(results, (0..1000).map(|i| if i % 7 == 0 { Err(i) } else { Ok(i) }).collect::<Vec<_>>());
    }

    use super::*;

    // Linux only: on macOS the forked child of a multi-threaded test process dies of SIGTRAP.
    #[test]
    #[cfg(target_os = "linux")]
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

    /// A background job that panics stops alone: the process goes on, and the pool runs later
    /// jobs.
    #[test]
    fn a_background_job_that_panics_does_not_abort_the_process() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        spawn(move || {
            let _keep = tx;
            panic!("a background job panics");
        });
        // The job's sender is dropped as it unwinds: its caller sees the channel closed.
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(30)), Err(std::sync::mpsc::RecvTimeoutError::Disconnected));
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(move || tx.send(7).unwrap());
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(30)), Ok(7));
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
        // Bigger work runs on the pool when no other outside call holds it — or serially on the
        // caller when another test's outside call does (the flag is process-wide, so a strict
        // pool-use assertion is not deterministic in a shared test process) — but never on
        // rayon's global pool.
        let ran = map_range(64, |_| (std::thread::current().id(), std::thread::current().name().map(str::to_string)));
        assert!(ran.iter().all(|(id, name)| *id == caller || name.as_deref().is_some_and(|n| n.starts_with("terrio-par-"))), "{ran:?}");
    }

    /// Pool placement is checked alone, so that no unrelated module's call holds the pool.
    #[test]
    #[cfg(all(threads, not(target_family = "wasm")))]
    fn lone_outside_work_uses_terrios_pool() {
        alone(30, || {
            let caller = std::thread::current().id();
            let p = Pool(native::pool());
            let entry = p.enter();
            assert!(matches!(entry, Enter::Pool(_)));
            drop(entry);
            map_range(64, |_| (std::thread::current().id(), std::thread::current().name().map(str::to_string)))
                .iter()
                .all(|(id, name)| *id != caller && name.as_deref().is_some_and(|n| n.starts_with("terrio-par-")))
        });
    }

    /// The thread that forks can retain a guard after the child clears its outside pool call.
    #[test]
    #[cfg(all(threads, unix))]
    fn a_guard_surviving_fork_does_not_disable_later_pool_work() {
        alone(30, || {
            let first = Pool(native::pool()).enter();
            assert!(matches!(first, Enter::Pool(_)));
            let serial = Pool(native::pool()).enter();
            assert!(matches!(serial, Enter::Serial));
            in_child(10, || {
                let current = Pool(native::pool()).enter();
                assert!(matches!(current, Enter::Pool(_)));
                drop(serial);
                drop(first);
                assert!(OUTSIDE_ON_POOL.load(std::sync::atomic::Ordering::Relaxed));
                let overlap = Pool(native::pool()).enter();
                assert!(matches!(overlap, Enter::Serial));
                drop(overlap);
                drop(current);
                !OUTSIDE_ON_POOL.load(std::sync::atomic::Ordering::Relaxed) && matches!(Pool(native::pool()).enter(), Enter::Pool(_))
            }) == Some(0)
        });
    }

    /// While another thread outside the pool has a call on the pool, an outside call enters
    /// serially, so overlapping outside callers do not queue on the pool together. Alone, so that
    /// no other test's call holds the pool when the first call enters.
    #[test]
    #[cfg(all(threads, not(target_family = "wasm")))]
    fn an_outside_call_enters_serially_while_another_outside_call_holds_the_pool() {
        alone(30, || {
            use std::sync::atomic::{AtomicBool, Ordering};
            let busy = std::sync::Arc::new(AtomicBool::new(false));
            let release = std::sync::Arc::new(AtomicBool::new(false));
            let (b, done) = (busy.clone(), release.clone());
            let h = std::thread::spawn(move || {
                Pool(native::pool()).run(|parallel| {
                    b.store(true, Ordering::SeqCst);
                    assert!(wait_for(&done, 10), "the observer did not release the first call");
                    parallel
                })
            });
            assert!(wait_for(&busy, 10), "the first outside call did not start");
            let entered = Pool(native::pool()).enter();
            let serial = matches!(entered, Enter::Serial);
            drop(entered);
            release.store(true, Ordering::SeqCst);
            h.join().unwrap() && serial
        });
    }

    /// An outside call that enters while only a serial outside call is still in flight (the
    /// pool-served one it raced with has ended) uses the pool: serial calls do not hold it, so two
    /// outside threads at work keep the pool busy with one call while the other runs serially.
    /// Alone, so that no other test's work holds the pool.
    #[test]
    #[cfg(all(threads, not(target_family = "wasm")))]
    fn an_outside_call_uses_the_pool_while_only_a_serial_outside_call_is_in_flight() {
        alone(30, || {
            use std::sync::atomic::{AtomicBool, Ordering};
            let (first_started, release_first, serial_started, release_serial) = (
                std::sync::Arc::new(AtomicBool::new(false)),
                std::sync::Arc::new(AtomicBool::new(false)),
                std::sync::Arc::new(AtomicBool::new(false)),
                std::sync::Arc::new(AtomicBool::new(false)),
            );
            let p = Pool(native::pool());
            let (a, b) = (first_started.clone(), release_first.clone());
            let h = std::thread::spawn(move || {
                p.run(|parallel| {
                    a.store(true, Ordering::SeqCst);
                    assert!(wait_for(&b, 10), "the observer did not release the first call");
                    parallel
                })
            });
            assert!(wait_for(&first_started, 10), "the first outside call did not start");
            let p = Pool(native::pool());
            let (b, c) = (serial_started.clone(), release_serial.clone());
            let serial = std::thread::spawn(move || {
                p.run(|parallel| {
                    b.store(true, Ordering::SeqCst);
                    assert!(wait_for(&c, 10), "the observer did not release the serial call");
                    parallel
                })
            });
            assert!(wait_for(&serial_started, 10), "the second outside call did not start");
            // The first call ends while the serial one is still in flight.
            release_first.store(true, Ordering::SeqCst);
            let first_parallel = h.join().unwrap();
            // This call sees only the serial one in flight, and takes the pool.
            let entered = Pool(native::pool()).enter();
            let pool = matches!(entered, Enter::Pool(Some(_)));
            drop(entered);
            release_serial.store(true, Ordering::SeqCst);
            let second_parallel = serial.join().unwrap();
            first_parallel && !second_parallel && pool
        });
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

    /// The calls of a `map_waiting` run at once on threads of their own, results in order; once a
    /// call fails, no worker starts another and an initiating error is returned. The
    /// calls on the calling thread (one at a time) stop at the first error as well.
    #[test]
    fn map_waiting_runs_at_once_and_stops_after_a_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let arrived = AtomicUsize::new(0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        // Each call waits for the others: one at a time, each would wait out the deadline.
        let together = map_waiting(4, 4, |i| {
            arrived.fetch_add(1, Ordering::SeqCst);
            while arrived.load(Ordering::SeqCst) < 4 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok((i, arrived.load(Ordering::SeqCst)))
        })
        .unwrap();
        assert_eq!(together, (0..4).map(|i| (i, 4)).collect::<Vec<_>>());
        assert_eq!(map_waiting(100, 8, |i| Ok(i * 2)).unwrap(), (0..100).map(|i| i * 2).collect::<Vec<_>>());
        for at_once in [1, 4] {
            let calls = AtomicUsize::new(0);
            let observed = std::sync::Mutex::new(Vec::new());
            let r: crate::Result<Vec<usize>> = map_waiting(1000, at_once, |i| {
                calls.fetch_add(1, Ordering::SeqCst);
                if i < 3 {
                    Ok(i)
                } else {
                    observed.lock().unwrap().push(i);
                    Err(crate::Error::InvalidArgument(format!("call {i}")))
                }
            });
            let observed = observed.into_inner().unwrap();
            assert!(
                matches!(&r, Err(crate::Error::InvalidArgument(m)) if m.strip_prefix("call ").and_then(|s| s.parse::<usize>().ok()).is_some_and(|i| observed.contains(&i))),
                "{:?}",
                r.err()
            );
            // The 3 that succeed, and at most one failing call a worker.
            let calls = calls.load(Ordering::SeqCst);
            assert!(calls <= 3 + at_once, "{calls} calls, {at_once} at once");
        }
    }

    /// The threads of a `map_waiting` see the caller's scoped options, which a new thread would
    /// not: a read's chunk policy (`TERRIO_ZARR_ALLOW_BIG_TILE_SIZE`) and its HTTP options.
    #[test]
    fn map_waiting_threads_see_the_callers_options() {
        let key = "TERRIO_TEST_MAP_WAITING_OPTION";
        let seen = crate::config::with_options(&[(key, "scoped")], || map_waiting(8, 8, |_| Ok(crate::config::get(key)))).unwrap();
        assert!(seen.iter().all(|v| v.as_deref() == Some("scoped")), "{seen:?}");
        assert!(map_waiting(8, 8, |_| Ok(crate::config::get(key))).unwrap().iter().all(Option::is_none));
    }

    /// The threads of a `map_waiting` count as background work for a fork: a fork asked for while
    /// they wait (on a server) waits for them to end, so that the child copies none of them
    /// starting, ending or half way through; the child then runs a `map_waiting` of its own. In a
    /// process of its own (`alone`), so that a fork that never ends stops only that process.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_fork_waits_for_the_threads_of_map_waiting() {
        alone(60, || {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::time::{Duration, Instant};
            let (live, done, release) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
            let reader = {
                let (live, done, release) = (live.clone(), done.clone(), release.clone());
                std::thread::spawn(move || {
                    map_waiting(4, 4, |i| {
                        live.fetch_add(1, Ordering::SeqCst);
                        while !release.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        done.fetch_add(1, Ordering::SeqCst);
                        Ok(i)
                    })
                })
            };
            let deadline = Instant::now() + Duration::from_secs(20);
            while live.load(Ordering::SeqCst) < 4 {
                assert!(Instant::now() < deadline, "the waiting threads did not start");
                std::thread::sleep(Duration::from_millis(1));
            }
            // The fork, from another thread, while the 4 threads wait.
            let (forked, fork_result) = std::sync::mpsc::channel();
            let in_fork = done.clone();
            std::thread::spawn(move || {
                let _ = forked.send(in_child(30, move || {
                    // Copied once the threads had all ended; the child's own threads start.
                    in_fork.load(Ordering::SeqCst) == 4 && map_waiting(8, 4, Ok).is_ok_and(|v| v == (0..8).collect::<Vec<_>>())
                }));
            });
            while !native::fork_in_progress() {
                assert!(Instant::now() < deadline, "the fork did not start");
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(100));
            let waited = fork_result.try_recv().is_err() && done.load(Ordering::SeqCst) == 0;
            release.store(true, Ordering::SeqCst);
            let exit = fork_result.recv_timeout(Duration::from_secs(60));
            let read = reader.join().unwrap();
            waited && exit == Ok(Some(0)) && read.is_ok_and(|v| v == [0, 1, 2, 3])
        });
    }

    #[test]
    #[cfg(all(unix, remote))]
    fn waiting_workers_are_counted_cancelled_and_fall_back_during_a_fork() {
        alone(30, || {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::time::{Duration, Instant};
            let cancel = Arc::new(AtomicBool::new(false));
            let (sent, result) = std::sync::mpsc::channel();
            let mut forking = None;
            with_waiting_workers(cancel.clone(), |parallel| {
                assert!(parallel);
                // A nested scope may share the cancellation flag. Dropping it removes one
                // registry entry, while the enclosing batch remains cancellable and counted.
                assert!(with_waiting_workers(cancel.clone(), |nested| nested));
                forking = Some(std::thread::spawn(move || {
                    native::begin_fork();
                    sent.send(()).unwrap();
                    native::end_fork();
                }));
                let deadline = Instant::now() + Duration::from_secs(10);
                while !native::fork_in_progress() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(wait_for(&cancel, 10), "the fork must cancel the active batch");
                assert!(result.try_recv().is_err(), "the fork must wait for this scope to end");
                let serial = with_waiting_workers(Arc::new(AtomicBool::new(false)), |parallel| !parallel);
                assert!(serial, "nested work during a fork must fall back without waiting");
            });
            result.recv_timeout(Duration::from_secs(10)).unwrap();
            forking.unwrap().join().unwrap();
            // Model a fork inspecting the registry between counting a batch and publishing its
            // flag. The publishing side must notice the fork and set its own cancellation.
            native::begin_fork();
            let late = Arc::new(AtomicBool::new(false));
            let registration = native::cancel_on_fork(late.clone());
            assert!(late.load(Ordering::SeqCst));
            drop(registration);
            native::end_fork();
            with_waiting_workers(Arc::new(AtomicBool::new(false)), |parallel| parallel)
        });
    }

    /// Waits for `flag`, for at most `secs` seconds: whether it was set.
    #[cfg(not(target_family = "wasm"))]
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
        alone(30, || {
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
            true
        });
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
