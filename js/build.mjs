// Builds the npm package `terrio-js`: the full module (every format) at the package's root, the
// lighter `terrio-js/core` module (without GeoParquet and Arrow, netCDF/HDF5, point clouds,
// JPEG 2000, Zarr, AVIF and OSM) in `core/`, the Zarr-only `terrio-js/zarr` module (its own
// crate, js/zarr) in `zarr/`, the threaded `terrio-js/threads` module (every
// format, with `initThreadPool`) in `threads/`, the `terrio-js/memory64` module (every format,
// with 64-bit memory, for datasets larger than 4 GB) in `memory64/`, and `terrio-js/load`, which
// picks the module a page or process can run. All are optimized for size (the `wasm-release`
// profile, and wasm-opt -Oz but for memory64, whose 64-bit tables the wasm-opt wasm-pack
// downloads cannot read). The package's `exports` name them.
//
// The threaded and memory64 modules need a nightly Rust with `rust-src` and the wasm32 target
// (`NIGHTLY`, below; TERRIO_JS_NIGHTLY overrides it), as they rebuild the standard library (with
// atomics; for wasm64-unknown-unknown, a target without a prebuilt one). `--no-threads` and
// `--no-memory64` leave them out, and so does any target but `web`.
//
// Usage: node js/build.mjs [--target web|nodejs|bundler] [--out-dir pkg] [--profile wasm-release]
//        [--no-threads] [--no-memory64]
//        [--no-default-features --features netcdf-hdf5] (one selective module)
// (run from the repository's root; the output directory is relative to js/, as wasm-pack's is).

import { execFileSync } from "node:child_process";
import { copyFileSync, cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    "no-default-features": { type: "boolean", default: false },
    features: { type: "string" },
    target: { type: "string", default: "web" },
    "out-dir": { type: "string", default: "pkg" },
    profile: { type: "string", default: "wasm-release" },
    "no-threads": { type: "boolean", default: false },
    "no-memory64": { type: "boolean", default: false },
  },
});
const selective = values["no-default-features"] || values.features !== undefined;
if (!["web", "nodejs", "bundler"].includes(values.target)) throw new Error("target must be web, nodejs or bundler");
if (selective && !values["no-default-features"]) throw new Error("selective builds require --no-default-features");
if (selective && /(^|[,\s])(?:[^,\s/]+\/)?threads([,\s]|$)/.test(values.features ?? "")) {
  throw new Error("selective builds are single-threaded; use the standard threaded build instead");
}
/** The nightly Rust the threaded module is built with, pinned so that builds repeat. */
const NIGHTLY = process.env.TERRIO_JS_NIGHTLY ?? "nightly-2026-10-15";
const threads = !selective && values.target === "web" && !values["no-threads"];
const memory64 = !selective && values.target === "web" && !values["no-memory64"];
const crate = fileURLToPath(new URL(".", import.meta.url));
const out = join(crate, values["out-dir"]);
const profile = values.profile === "release" ? ["--release"] : ["--profile", values.profile];

/** Runs wasm-pack on a crate (terrio-js by default), with cargo's options after `--`. */
function wasmPack(outDir, extra, cargo = [], env = undefined, dir = crate) {
  const args = ["build", dir, ...profile, "--target", values.target, "--out-dir", outDir, "--out-name", "terrio", ...extra];
  const command = env ? ["rustup", ["run", NIGHTLY, "wasm-pack", ...args]] : ["wasm-pack", args];
  const all = cargo.length ? [...command[1], "--", ...cargo] : command[1];
  execFileSync(command[0], all, { stdio: "inherit", env: env ? { ...process.env, ...env } : process.env });
}

// Refuse an existing package for selective output so stale variants cannot be shipped.
if (selective && existsSync(out)) throw new Error("selective output directory must not exist; choose a fresh --out-dir");
const selectedCargo = selective ? ["--no-default-features", ...(values.features ? ["--features", values.features] : []), "--locked"] : [];
wasmPack(out, [], selectedCargo);
if (!selective) wasmPack(join(out, "core"), ["--no-pack"], ["--no-default-features", "--features", "core"]);
// `terrio-js/zarr`: Zarr reads alone, from its own crate (js/zarr), so that nothing else is linked.
if (!selective) wasmPack(join(out, "zarr"), ["--no-pack"], [], undefined, join(crate, "zarr"));
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
if (memory64) buildMemory64(join(out, "memory64"));
// wasm-pack ignores everything in its output directories; npm must still pack them.
for (const sub of (selective ? [] : ["core", "zarr", "threads"])) rmSync(join(out, sub, ".gitignore"), { force: true });
// The loader, which picks a module.
for (const f of (selective ? [] : ["load.js", "load.d.ts"])) copyFileSync(join(crate, f), join(out, f));

const path = join(out, "package.json");
const pkg = JSON.parse(readFileSync(path, "utf8"));
// A module's files, and the JavaScript its glue imports (wasm-bindgen snippets), when it has some.
const module = (dir) => [`${dir}terrio_bg.wasm`, `${dir}terrio_bg.wasm.d.ts`, `${dir}terrio.js`, `${dir}terrio.d.ts`, ...(existsSync(join(out, dir, "snippets")) ? [`${dir}snippets/`] : [])];
// The threaded module's worker script is a wasm-bindgen snippet, in `threads/snippets/`.
const files = [...module(""), ...(selective ? [] : [...module("core/"), ...module("zarr/")]), ...(threads ? ["threads/"] : []), ...(memory64 ? module("memory64/") : []), ...(selective ? [] : ["load.js", "load.d.ts"])];
// Carry project attribution alongside generated glue, declarations and WASM.
const notices = ["LICENSE", "NOTICE", "LICENSE-THIRD-PARTY.md"];
for (const name of notices) copyFileSync(join(crate, "..", name), join(out, name));
files.push(...notices);
pkg.files = [...new Set([...(pkg.files ?? []), ...files])];
// The worker script runs on its own when loaded: bundlers must keep it whole.
if (threads) pkg.sideEffects = [...new Set([...(pkg.sideEffects ?? []), "./threads/snippets/*"])];
const entry = (dir, name = "terrio") => ({ types: `./${dir}${name}.d.ts`, default: `./${dir}${name}.js` });
// The files stay importable by their paths (`terrio-js/terrio_bg.wasm`), as without `exports`.
pkg.exports = {
  ".": entry(""),
  ...(!selective ? { "./core": entry("core/"), "./zarr": entry("zarr/") } : {}),
  ...(threads ? { "./threads": entry("threads/") } : {}),
  ...(memory64 ? { "./memory64": entry("memory64/") } : {}),
  ...(!selective ? { "./load": entry("", "load") } : {}),
  "./package.json": "./package.json",
  "./*": "./*",
};
writeFileSync(path, JSON.stringify(pkg, null, 2) + "\n");

for (const f of ["terrio_bg.wasm", "core/terrio_bg.wasm", "zarr/terrio_bg.wasm", "threads/terrio_bg.wasm", "memory64/terrio_bg.wasm"].filter((f) => existsSync(join(out, f)))) {
  console.log(`${values["out-dir"]}/${f}: ${(statSync(join(out, f)).size / 1e6).toFixed(2)} MB`);
}

/**
 * The crates that do not build for wasm64-unknown-unknown as published, and the edits that make
 * them: they treat wasm32 as WebAssembly. `tar` (wbraster's) leaves out its path conversions
 * (https://github.com/alexcrichton/tar-rs, `cfg(target_arch = "wasm32")`), `arrow-buffer`
 * its alignment constant, and acadrust its mmap/thread target guards. Copies of the locked
 * versions are edited (replacing `target_arch = "wasm32"` with `target_family = "wasm"`) for this
 * build only, through cargo's `--config patch...`.
 */
function wasm64Patches() {
  return {
    tar: (dir) => readdirSync(join(dir, "src")).filter((f) => f.endsWith(".rs")).map((f) => [join("src", f), /target_arch = "wasm32"/g, 'target_family = "wasm"']),
    // acadrust uses wasm32-specific guards for mmap and thread spawning.
    acadrust: (dir) => ["Cargo.toml", ...readdirSync(join(dir, "src"), { recursive: true }).filter((f) => f.endsWith(".rs")).map((f) => join("src", f))]
      .filter((f) => /target_arch = "wasm32"/.test(readFileSync(join(dir, f), "utf8")))
      .map((f) => [f, /target_arch = "wasm32"/g, 'target_family = "wasm"']),
    "arrow-buffer": () => [["src/alloc/alignment.rs", /#\[cfg\(target_arch = "wasm32"\)\]/g, '#[cfg(target_family = "wasm")]']],
  };
}

/** The copies of a tool wasm-pack downloaded into its cache, as `wasm-bindgen`. */
function wasmPackTool(prefix, file) {
  const cache = process.env.WASM_PACK_CACHE ?? (process.platform === "darwin" ? join(homedir(), "Library", "Caches", ".wasm-pack") : join(homedir(), ".cache", ".wasm-pack"));
  const found = existsSync(cache) ? readdirSync(cache).filter((d) => d.startsWith(prefix)).map((d) => join(cache, d, file)) : [];
  return found.filter((f) => existsSync(f));
}

/**
 * Builds the memory64 module (wasm64-unknown-unknown): cargo, then wasm-bindgen, which wasm-pack
 * does not run for that target. Up to 16 GB of memory, V8's limit for 64-bit memories.
 */
function buildMemory64(outDir) {
  const target = "wasm64-unknown-unknown";
  const targetDir = join(crate, "..", "target", "wasm-memory64");
  // The patches make cargo rewrite Cargo.lock: it is put back as it is now, before anything runs.
  const lockPath = join(crate, "..", "Cargo.lock");
  const lock = readFileSync(lockPath);
  const meta = JSON.parse(
    execFileSync("cargo", ["metadata", "--format-version", "1", "--filter-platform", target, "--manifest-path", join(crate, "Cargo.toml")], {
      maxBuffer: 1 << 28,
    }),
  );
  const patches = [];
  for (const [name, edits] of Object.entries(wasm64Patches())) {
    // One version of each, from crates.io: a patch replaces every version of a crate.
    const found = meta.packages.filter((p) => p.name === name && p.source?.startsWith("registry+"));
    if (found.length !== 1) throw new Error(`memory64 patch of ${name}: ${found.length} versions from crates.io in the build, not 1`);
    const dir = join(targetDir, "patched", name);
    rmSync(dir, { recursive: true, force: true });
    cpSync(dirname(found[0].manifest_path), dir, { recursive: true });
    let changed = 0;
    for (const [file, from, to] of edits(dir)) {
      const path = join(dir, file);
      const source = readFileSync(path, "utf8");
      const patched = source.replace(from, to);
      // `tar`'s edit runs over all its files, some without a wasm32 cfg; the others' must match.
      if (patched === source && name !== "tar") throw new Error(`memory64 patch of ${name}: ${file} has no match for ${from}`);
      changed += patched !== source;
      writeFileSync(path, patched);
    }
    if (changed === 0) throw new Error(`memory64 patch of ${name}: nothing to edit (has it gained wasm64 support?)`);
    patches.push("--config", `patch.crates-io.${name}.path=${JSON.stringify(dir)}`);
  }
  const link = ["--allow-undefined", "--max-memory=17179869184"].flatMap((a) => ["-C", `link-arg=${a}`]);
  // zlib-rs's SIMD for wasm64 (core::arch::wasm64) is unstable; ahash seeds with getrandom, whose
  // wasm64 backend js/src/lib.rs defines.
  const rustflags = ["-Zcrate-attr=feature(simd_wasm64)", "--cfg", 'getrandom_backend="custom"', ...link];
  try {
    execFileSync("rustup", ["run", NIGHTLY, "cargo", "build", "-p", "terrio-js", "--target", target, ...profile, "-Z", "build-std=panic_abort,std", ...patches], {
      cwd: crate,
      stdio: "inherit",
      env: { ...process.env, RUSTFLAGS: rustflags.join(" "), CARGO_TARGET_DIR: targetDir },
    });
  } finally {
    writeFileSync(lockPath, lock);
  }
  const profileDir = values.profile === "release" ? "release" : values.profile;
  const wasm = join(targetDir, target, profileDir, "terrio_js.wasm");
  // The wasm-bindgen of the crate's version, as wasm-pack installed it for the other modules.
  const version = /name = "wasm-bindgen"\r?\nversion = "([^"]+)"/.exec(lock.toString())?.[1];
  if (!version) throw new Error("wasm-bindgen is not in Cargo.lock");
  const bindgen = ["wasm-bindgen", ...wasmPackTool("wasm-bindgen-", "wasm-bindgen")].find((b) => {
    try {
      return execFileSync(b, ["--version"]).toString().trim() === `wasm-bindgen ${version}`;
    } catch {
      return false;
    }
  });
  if (!bindgen) throw new Error(`wasm-bindgen ${version} not found (on PATH or in wasm-pack's cache)`);
  mkdirSync(outDir, { recursive: true });
  execFileSync(bindgen, ["--target", values.target, "--out-dir", outDir, "--out-name", "terrio", wasm], { stdio: "inherit" });
}
