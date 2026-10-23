//! The thread pool of the threaded module (`terrio-js/threads`, built with WebAssembly's
//! `atomics`): `initThreadPool(n)` starts `n` workers (Web Workers, or Node.js worker threads)
//! over the module's shared memory, each running a rayon thread, and makes them the pool terrio's
//! parallel work runs on: decoding GeoTIFF chunks and JPEG 2000 tiles, compressing tiles, and the
//! other loops natively run on rayon.

use std::cell::Cell;
use std::sync::{Mutex, mpsc};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// The channel the pool hands its threads to the workers on.
type Threads = Mutex<mpsc::Receiver<rayon::ThreadBuilder>>;

#[wasm_bindgen(module = "/src/threads.js")]
extern "C" {
    #[wasm_bindgen(js_name = startWorkers)]
    fn start_workers(module: JsValue, memory: JsValue, main_js: JsValue, count: usize, receiver: usize) -> js_sys::Promise;
}

#[wasm_bindgen]
extern "C" {
    /// The URL of this module's JavaScript, which the workers import.
    #[wasm_bindgen(thread_local_v2, js_namespace = ["import", "meta"], js_name = url)]
    static MAIN_JS: js_sys::JsString;
}

thread_local! {
    /// The pool's threads, once started.
    static POOL_SIZE: Cell<usize> = const { Cell::new(0) };
    /// Whether `initThreadPool` was called (and has not failed).
    static STARTED: Cell<bool> = const { Cell::new(false) };
}

/// The number of threads of the pool (0 before `initThreadPool`).
pub(crate) fn pool_size() -> usize {
    POOL_SIZE.with(Cell::get)
}

/// Starts the thread pool: `threads` workers (`navigator.hardwareConcurrency` in browsers,
/// `os.availableParallelism()` in Node.js, for example). Until it has run, and in the
/// single-threaded modules, which have no `initThreadPool`, the work runs on the calling thread.
/// It runs once; browsers allow it on cross-origin isolated pages only (`crossOriginIsolated`),
/// where `SharedArrayBuffer` exists.
#[wasm_bindgen(js_name = initThreadPool)]
pub async fn init_thread_pool(threads: usize) -> Result<(), JsError> {
    if threads == 0 {
        return Err(JsError::new("initThreadPool: at least 1 thread"));
    }
    if STARTED.with(|s| s.replace(true)) {
        return Err(JsError::new("initThreadPool: the thread pool is already started"));
    }
    let started = start(threads).await;
    if started.is_err() {
        STARTED.with(|s| s.set(false));
    }
    started
}

/// Starts the workers, then the pool over them.
async fn start(threads: usize) -> Result<(), JsError> {
    let (tx, rx) = mpsc::channel();
    // The workers take their threads from the channel for as long as the module lives.
    let rx: &'static Threads = Box::leak(Box::new(Mutex::new(rx)));
    let main_js = MAIN_JS.with(|url| JsValue::from(url.clone()));
    let started = start_workers(wasm_bindgen::module(), wasm_bindgen::memory(), main_js, threads, rx as *const Threads as usize);
    JsFuture::from(started).await.map_err(|e| JsError::new(&format!("initThreadPool: {}", js_message(&e))))?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .spawn_handler(move |thread| tx.send(thread).map_err(|_| std::io::Error::other("the workers are gone")))
        .build()
        .map_err(|e| JsError::new(&format!("initThreadPool: {e}")))?;
    terrio::set_thread_pool(pool).map_err(|_| JsError::new("initThreadPool: the thread pool is already started"))?;
    POOL_SIZE.with(|n| n.set(threads));
    Ok(())
}

/// A JavaScript error's message.
fn js_message(e: &JsValue) -> String {
    e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())).or_else(|| e.as_string()).unwrap_or_else(|| format!("{e:?}"))
}

/// Runs one of the pool's threads, in a worker: called by the worker's script (`threads.js`).
#[wasm_bindgen(js_name = __terrio_start_worker, skip_typescript)]
pub fn start_worker(receiver: usize) {
    // SAFETY: the address of the channel `init_thread_pool` leaked, which lives as long as the
    // module.
    let threads = unsafe { &*(receiver as *const Threads) };
    let thread = threads.lock().unwrap_or_else(|e| e.into_inner()).recv();
    if let Ok(thread) = thread {
        thread.run();
    }
}
