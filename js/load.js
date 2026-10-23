// terrio-js/load: loads and initializes the module a page or process can run. The threaded
// module (`terrio-js/threads`), with its thread pool started, where WebAssembly threads work:
// Node.js, and cross-origin isolated pages (`crossOriginIsolated`, served with the
// `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp`
// headers), where `SharedArrayBuffer` exists. The single-threaded module (`terrio-js`)
// otherwise, or the lighter one (`terrio-js/core`) when asked for.
//
//   import { load } from "terrio-js/load";
//   const terrio = await load();            // { threads: true } by default
//   terrio.features();                      // { build: "terrio-js/threads", threads: 8, ... }
//   const ds = terrio.open(bytes, "dem.tif");

const isNode = typeof process !== "undefined" && process.versions != null && process.versions.node != null;

/** The most threads started by default. */
const MAX_DEFAULT_THREADS = 16;

/** What `init` takes for a module's `.wasm`: its bytes in Node.js (no `fetch` of files), its URL elsewhere. */
function source(url) {
  if (!isNode) return url;
  const { readFileSync } = process.getBuiltinModule("node:fs");
  const { fileURLToPath } = process.getBuiltinModule("node:url");
  return readFileSync(fileURLToPath(url));
}

/** The number of threads to start when not given: the processors, up to 16. */
function defaultThreads() {
  const n = isNode ? process.getBuiltinModule("node:os").availableParallelism() : globalThis.navigator?.hardwareConcurrency;
  return Math.max(1, Math.min(n || 1, MAX_DEFAULT_THREADS));
}

/** Whether WebAssembly threads can run here. */
export function threadsSupported() {
  if (typeof SharedArrayBuffer !== "function" || typeof Atomics !== "object") return false;
  if (isNode) return typeof process.getBuiltinModule === "function";
  return globalThis.crossOriginIsolated === true && typeof Worker === "function";
}

/**
 * Loads and initializes a module. Options: `threads` (`true` by default: as many threads as
 * processors, up to 16; a number of threads; or `false` for the single-threaded module), `core`
 * (the lighter module, single-threaded), and `onFallback`, called with the error when the
 * threaded module was tried and the single-threaded one is used instead.
 */
export async function load(options = {}) {
  const { threads = true, core = false, onFallback } = options;
  if (!core && threads !== false && threadsSupported()) {
    let mod;
    try {
      mod = await import("./threads/terrio.js");
      await mod.default({ module_or_path: source(new URL("./threads/terrio_bg.wasm", import.meta.url)) });
    } catch (e) {
      mod = undefined;
      onFallback?.(e);
    }
    if (mod) {
      const n = typeof threads === "number" ? threads : defaultThreads();
      // Without its pool (workers refused by the page's policy, for one), the threaded module
      // still works, on the calling thread.
      if (n > 1) await mod.initThreadPool(n).catch((e) => onFallback?.(e));
      return mod;
    }
  }
  const mod = core ? await import("./core/terrio.js") : await import("./terrio.js");
  const wasm = core ? new URL("./core/terrio_bg.wasm", import.meta.url) : new URL("./terrio_bg.wasm", import.meta.url);
  await mod.default({ module_or_path: source(wasm) });
  return mod;
}
