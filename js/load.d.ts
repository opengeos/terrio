/** The module `load` returns: terrio-js's API, and `initThreadPool` in the threaded module. */
export type Terrio = typeof import("./terrio.js") & {
  /** Starts the thread pool (the threaded module only; `load` already did). */
  initThreadPool?: (threads: number) => Promise<void>;
};

export interface LoadOptions {
  /** `true` (the default): as many threads as processors, up to 16; a number; `false`: none. */
  threads?: boolean | number;
  /** The lighter module, `terrio-js/core` (single-threaded). */
  core?: boolean;
  /**
   * The module with 64-bit memory, `terrio-js/memory64`, for datasets larger than 4 GB, where the
   * runtime supports it; elsewhere a 32-bit module loads, as without it (see `onFallback`). It is
   * single-threaded (`threads` is not used), and cannot be combined with `core`.
   */
  memory64?: boolean;
  /** Called with the error when the threaded or memory64 module could not be used. */
  onFallback?: (error: unknown) => void;
}

/** Whether WebAssembly's 64-bit memories work here (Node.js 24+, Chrome 133+, Firefox 134+). */
export function memory64Supported(): boolean;

/** Whether WebAssembly threads can run here (Node.js, or a cross-origin isolated page). */
export function threadsSupported(): boolean;

/** Loads and initializes the threaded module where threads can run, the single-threaded one otherwise. */
export function load(options?: LoadOptions): Promise<Terrio>;
