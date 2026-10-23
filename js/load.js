// terrio-js/load: loads and initializes the module a page or process can run. The threaded
// module (`terrio-js/threads`), with its thread pool started, where WebAssembly threads work:
// Node.js, and cross-origin isolated pages (`crossOriginIsolated`, served with the
// `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp`
// headers), where `SharedArrayBuffer` exists. The single-threaded module (`terrio-js`)
// otherwise, or the lighter one (`terrio-js/core`) when asked for. For datasets larger than
// 4 GB, `{ memory64: true }` loads the module with 64-bit memory (`terrio-js/memory64`, up to
// 16 GB, single-threaded) where the runtime supports memory64.
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

/** Whether WebAssembly's 64-bit memories (memory64) work here: a module declaring one validates. */
export function memory64Supported() {
  // The header, then a memory section holding one memory of 64-bit limits (flag 4), minimum 0.
  const module = new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, 5, 3, 1, 4, 0]);
  try {
    return WebAssembly.validate(module);
  } catch {
    return false;
  }
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
 * (the lighter module, single-threaded), `memory64` (the module with 64-bit memory, for datasets
 * larger than 4 GB, where the runtime supports it; single-threaded), and `onFallback`, called
 * with the error when the threaded or memory64 module was tried and another is used instead.
 */
export async function load(options = {}) {
  const { threads = true, core = false, memory64 = false, onFallback } = options;
  if (memory64 && !core) {
    if (memory64Supported()) {
      try {
        const mod = await import("./memory64/terrio.js");
        await mod.default({ module_or_path: source(new URL("./memory64/terrio_bg.wasm", import.meta.url)) });
        return mod;
      } catch (e) {
        onFallback?.(e);
      }
    } else {
      onFallback?.(new Error("this runtime has no WebAssembly memory64"));
    }
  }
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
