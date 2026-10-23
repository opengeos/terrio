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
  /** Called with the error when the threaded module could not be used. */
  onFallback?: (error: unknown) => void;
}

/** Whether WebAssembly threads can run here (Node.js, or a cross-origin isolated page). */
export function threadsSupported(): boolean;

/** Loads and initializes the threaded module where threads can run, the single-threaded one otherwise. */
export function load(options?: LoadOptions): Promise<Terrio>;
