// The workers of the threaded module's thread pool (`initThreadPool`): Web Workers in browsers,
// worker threads in Node.js. Each instantiates the same WebAssembly module over the same shared
// memory, then runs a rayon worker thread until the page or process ends.
//
// This file is both the main thread's helper (`startWorkers`) and the workers' script: a worker
// loads it and waits for the message that starts it.

const INIT = "terrio_worker_init";
const READY = "terrio_worker_ready";
const isNode = typeof process !== "undefined" && process.versions != null && process.versions.node != null;
// Node.js's worker threads (Node.js 20.16 or later), loaded without a top-level await.
const threads = isNode ? process.getBuiltinModule("node:worker_threads") : null;

/** Starts one worker: instantiates the module, says it is ready, then runs its rayon thread. */
async function run(data, post) {
  const pkg = await import(data.mainJS);
  await pkg.default({ module_or_path: data.module, memory: data.memory });
  post({ type: READY });
  pkg.__terrio_start_worker(data.receiver);
}

if (isNode) {
  if (!threads.isMainThread && threads.workerData?.type === INIT) {
    run(threads.workerData, (m) => threads.parentPort.postMessage(m));
  }
} else if (typeof WorkerGlobalScope !== "undefined" && self instanceof WorkerGlobalScope) {
  // Other messages may reach a worker that loads terrio for its own use: only ours start one.
  self.addEventListener("message", function onMessage({ data }) {
    if (data?.type !== INIT) return;
    self.removeEventListener("message", onMessage);
    run(data, (m) => self.postMessage(m));
  });
}

// The workers, kept referenced: Firefox collects Web Workers that share memory with the page but
// are not (https://bugzilla.mozilla.org/show_bug.cgi?id=1702191).
let workers = [];

/** Waits for a worker's message of a type. */
function waitFor(worker, type) {
  return new Promise((resolve, reject) => {
    const onMessage = (event) => {
      const data = isNode ? event : event.data;
      if (data?.type === type) resolve(data);
    };
    if (isNode) {
      worker.on("message", onMessage);
      worker.once("error", reject);
    } else {
      worker.addEventListener("message", onMessage);
      worker.addEventListener("error", (e) => reject(e.error ?? new Error(e.message)), { once: true });
    }
  });
}

/**
 * Starts `count` workers over the module and its shared memory, and resolves once each is ready
 * to take a rayon thread from `receiver` (the address of the channel the pool sends them on).
 */
export async function startWorkers(module, memory, mainJS, count, receiver) {
  const data = { type: INIT, module, memory, mainJS, receiver };
  const url = new URL(import.meta.url);
  let started;
  if (isNode) {
    started = Array.from({ length: count }, () => {
      return new threads.Worker(url, { workerData: data });
    });
  } else {
    started = Array.from({ length: count }, () => {
      const worker = new Worker(url, { type: "module" });
      worker.postMessage(data);
      return worker;
    });
  }
  await Promise.all(started.map((w) => waitFor(w, READY)));
  // The pool's threads never end: once started, they must not keep the process alive.
  if (isNode) started.forEach((w) => w.unref());
  workers = workers.concat(started);
}
