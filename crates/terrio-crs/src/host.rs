//! What an embedding library supplies to the engine (see [`set_host`]): where configuration
//! options are read, how background work runs, and what is done before a process-wide lock is
//! taken. Without it, the engine reads the process environment, runs background work on a thread
//! of its own, and does nothing before a lock.
//!
//! A library whose process may `fork()` while other threads use the engine (Python's
//! `multiprocessing` forks on Linux) holds the engine's process-wide locks across the fork, with
//! [`hold_for_fork`] in its `pthread_atfork` handlers, so that a child never starts with one held
//! by a thread it does not have; its `before_lock` hook registers those handlers before any of
//! the locks is first taken.

use std::sync::OnceLock;

/// The hooks of an embedding library.
#[derive(Clone, Copy)]
pub struct Host {
    /// The value of a configuration option (`TERRIO_NETWORK`, `PROJ_NETWORK`,
    /// `TERRIO_CDN_ENDPOINT`, `PROJ_NETWORK_ENDPOINT`), as the library resolves it: for instance
    /// options scoped to the calling thread, then the environment.
    pub config: fn(&str) -> Option<String>,
    /// Runs work in the background: the EPSG table's prefetch.
    pub spawn: fn(Box<dyn FnOnce() + Send>),
    /// Called before the engine takes one of its process-wide locks.
    pub before_lock: fn(),
}

static HOST: OnceLock<Host> = OnceLock::new();

/// Installs the hooks of an embedding library, once, before it first uses the engine. `false`
/// when hooks were installed already (they are kept).
pub fn set_host(host: Host) -> bool {
    HOST.set(host).is_ok()
}

/// A configuration option's value: the host's, else the environment's. Read by the network
/// code, which WebAssembly builds leave out.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn config(name: &str) -> Option<String> {
    match HOST.get() {
        Some(h) => (h.config)(name),
        None => std::env::var(name).ok(),
    }
}

/// The value of the first of `names` that is set (see [`config`]).
#[cfg(not(target_family = "wasm"))]
pub(crate) fn config_any(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| config(n))
}

/// Runs `f` in the background: on the host's pool, else on a thread of its own.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn spawn(f: impl FnOnce() + Send + 'static) {
    match HOST.get() {
        Some(h) => (h.spawn)(Box::new(f)),
        None => drop(std::thread::spawn(f)),
    }
}

/// Before one of the engine's process-wide locks is taken (see the module's documentation).
#[inline]
pub(crate) fn before_lock() {
    if let Some(h) = HOST.get() {
        (h.before_lock)();
    }
}

/// Takes the engine's process-wide locks, for a fork: the grids' (settings, grids registered in
/// memory, files read, failed fetches, availability, grid sets built) and the grid transport's.
/// The forking thread holds the guards pushed to `held` while the process is copied, and drops
/// them after, in the parent and in the child. Each lock is held for map operations only, never
/// while another is taken, so any order is free of deadlocks.
#[cfg(all(unix, not(target_family = "wasm")))]
pub fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    crate::grid::hold_for_fork(held);
    #[cfg(remote)]
    crate::network::hold_for_fork(held);
}

/// While another thread holds one lock, [`hold_for_fork`] waits for it: the lock is one it takes.
/// `hold` takes the lock and keeps it until the closure it is given returns.
#[cfg(all(test, unix, not(target_family = "wasm")))]
pub(crate) fn assert_held_for_fork(name: &str, hold: impl FnOnce(&dyn Fn()) + Send + 'static) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    // One check at a time: another's held lock would make this one's `hold_for_fork` wait too.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let holding = std::sync::Arc::new(AtomicBool::new(false));
    let h = holding.clone();
    let holder = std::thread::spawn(move || {
        hold(&|| {
            h.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(400));
        })
    });
    while !holding.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = Instant::now();
    let mut held = Vec::new();
    hold_for_fork(&mut held);
    let waited = start.elapsed();
    drop(held);
    holder.join().unwrap();
    assert!(waited >= Duration::from_millis(250), "hold_for_fork did not wait for the {name} lock ({waited:?})");
}
