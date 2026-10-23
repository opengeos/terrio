// Builds the npm package `terrio-js`: the full module (every format) at the package's root, the
// lighter `terrio-js/core` module (without GeoParquet and Arrow, netCDF/HDF5, point clouds,
// JPEG 2000, Zarr, AVIF and OSM) in `core/`, the threaded `terrio-js/threads` module (every
// format, with `initThreadPool`) in `threads/`, and `terrio-js/load`, which picks the module a
// page or process can run. All are optimized for size (the `wasm-release` profile and wasm-opt
// -Oz). The package's `exports` name them.
//
// The threaded module needs a nightly Rust with `rust-src` and the wasm32 target (`NIGHTLY`,
// below; TERRIO_JS_NIGHTLY overrides it), as WebAssembly threads rebuild the standard library
// with atomics; `--no-threads` leaves it out, and so does any target but `web`.
//
// Usage: node js/build.mjs [--target web|nodejs|bundler] [--out-dir pkg] [--profile wasm-release]
//        [--no-threads]
// (run from the repository's root; the output directory is relative to js/, as wasm-pack's is).

import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    target: { type: "string", default: "web" },
    "out-dir": { type: "string", default: "pkg" },
    profile: { type: "string", default: "wasm-release" },
    "no-threads": { type: "boolean", default: false },
  },
});
/** The nightly Rust the threaded module is built with, pinned so that builds repeat. */
const NIGHTLY = process.env.TERRIO_JS_NIGHTLY ?? "nightly-2026-10-15";
const threads = values.target === "web" && !values["no-threads"];
const crate = fileURLToPath(new URL(".", import.meta.url));
const out = join(crate, values["out-dir"]);
const profile = values.profile === "release" ? ["--release"] : ["--profile", values.profile];

/** Runs wasm-pack on the crate, with cargo's options after `--`. */
function wasmPack(outDir, extra, cargo = [], env = undefined) {
  const args = ["build", crate, ...profile, "--target", values.target, "--out-dir", outDir, "--out-name", "terrio", ...extra];
  const command = env ? ["rustup", ["run", NIGHTLY, "wasm-pack", ...args]] : ["wasm-pack", args];
  const all = cargo.length ? [...command[1], "--", ...cargo] : command[1];
  execFileSync(command[0], all, { stdio: "inherit", env: env ? { ...process.env, ...env } : process.env });
}

wasmPack(out, []);
wasmPack(join(out, "core"), ["--no-pack"], ["--no-default-features", "--features", "core"]);
if (threads) {
  // Shared memory (up to 4 GB, as without threads), and the thread-local storage symbols
  // wasm-bindgen sets up each thread with; the target's own flags (.cargo/config.toml) are
  // replaced by RUSTFLAGS, so they are repeated.
  const link = ["--allow-undefined", "--shared-memory", "--max-memory=4294967296", "--import-memory"];
  link.push(...["__wasm_init_tls", "__tls_size", "__tls_align", "__tls_base"].map((s) => `--export=${s}`));
  const rustflags = ["-C", "target-feature=+atomics,+bulk-memory", ...link.flatMap((a) => ["-C", `link-arg=${a}`])];
  wasmPack(join(out, "threads"), ["--no-pack"], ["--features", "threads", "-Z", "build-std=panic_abort,std"], {
    RUSTFLAGS: rustflags.join(" "),
    // Its own target directory: the standard library rebuilt with atomics.
    CARGO_TARGET_DIR: join(crate, "..", "target", "wasm-threads"),
  });
}
// wasm-pack ignores everything in its output directories; npm must still pack them.
for (const sub of ["core", "threads"]) rmSync(join(out, sub, ".gitignore"), { force: true });
// The loader, which picks a module.
for (const f of ["load.js", "load.d.ts"]) copyFileSync(join(crate, f), join(out, f));

const path = join(out, "package.json");
const pkg = JSON.parse(readFileSync(path, "utf8"));
const module = (dir) => [`${dir}terrio_bg.wasm`, `${dir}terrio.js`, `${dir}terrio.d.ts`];
// The threaded module's worker script is a wasm-bindgen snippet, in `threads/snippets/`.
const files = [...module("core/"), ...(threads ? ["threads/"] : []), "load.js", "load.d.ts"];
pkg.files = [...new Set([...(pkg.files ?? []), ...files])];
// The worker script runs on its own when loaded: bundlers must keep it whole.
if (threads) pkg.sideEffects = [...new Set([...(pkg.sideEffects ?? []), "./threads/snippets/*"])];
const entry = (dir, name = "terrio") => ({ types: `./${dir}${name}.d.ts`, default: `./${dir}${name}.js` });
// The files stay importable by their paths (`terrio-js/terrio_bg.wasm`), as without `exports`.
pkg.exports = {
  ".": entry(""),
  "./core": entry("core/"),
  ...(threads ? { "./threads": entry("threads/") } : {}),
  "./load": entry("", "load"),
  "./package.json": "./package.json",
  "./*": "./*",
};
writeFileSync(path, JSON.stringify(pkg, null, 2) + "\n");

for (const f of ["terrio_bg.wasm", "core/terrio_bg.wasm", "threads/terrio_bg.wasm"].filter((f) => existsSync(join(out, f)))) {
  console.log(`${values["out-dir"]}/${f}: ${(statSync(join(out, f)).size / 1e6).toFixed(2)} MB`);
}
