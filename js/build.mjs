// Builds the npm package `terrio-js`: the full module (every format) at the package's root, and
// the lighter `terrio-js/core` module (without GeoParquet and Arrow, netCDF/HDF5, point clouds,
// JPEG 2000, Zarr, AVIF and OSM) in `core/`, both optimized for size (the `wasm-release` profile
// and wasm-opt -Oz). The package's `exports` name both.
//
// Usage: node js/build.mjs [--target web|nodejs|bundler] [--out-dir pkg] [--profile wasm-release]
// (run from the repository's root; the output directory is relative to js/, as wasm-pack's is).

import { execFileSync } from "node:child_process";
import { readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    target: { type: "string", default: "web" },
    "out-dir": { type: "string", default: "pkg" },
    profile: { type: "string", default: "wasm-release" },
  },
});
const crate = fileURLToPath(new URL(".", import.meta.url));
const out = join(crate, values["out-dir"]);
const profile = values.profile === "release" ? ["--release"] : ["--profile", values.profile];

/** Runs wasm-pack on the crate, with cargo's options after `--`. */
function wasmPack(outDir, extra, cargo = []) {
  const args = ["build", crate, ...profile, "--target", values.target, "--out-dir", outDir, "--out-name", "terrio", ...extra];
  execFileSync("wasm-pack", cargo.length ? [...args, "--", ...cargo] : args, { stdio: "inherit" });
}

wasmPack(out, []);
wasmPack(join(out, "core"), ["--no-pack"], ["--no-default-features", "--features", "core"]);
// wasm-pack ignores everything in its output directories; npm must still pack `core/`.
rmSync(join(out, "core", ".gitignore"), { force: true });

const path = join(out, "package.json");
const pkg = JSON.parse(readFileSync(path, "utf8"));
const core = ["core/terrio_bg.wasm", "core/terrio.js", "core/terrio.d.ts"];
pkg.files = [...new Set([...(pkg.files ?? []), ...core])];
const entry = (dir) => ({ types: `./${dir}terrio.d.ts`, default: `./${dir}terrio.js` });
// The files stay importable by their paths (`terrio-js/terrio_bg.wasm`), as without `exports`.
pkg.exports = { ".": entry(""), "./core": entry("core/"), "./package.json": "./package.json", "./*": "./*" };
writeFileSync(path, JSON.stringify(pkg, null, 2) + "\n");

for (const f of ["terrio_bg.wasm", "core/terrio_bg.wasm"]) {
  console.log(`${values["out-dir"]}/${f}: ${(statSync(join(out, f)).size / 1e6).toFixed(2)} MB`);
}
