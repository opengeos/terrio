import assert from "node:assert/strict";
import * as fs from "node:fs";
import { tmpdir, homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import vm from "node:vm";
import test from "node:test";

const source = fs.readFileSync(new URL("../build.mjs", import.meta.url), "utf8");

// Exercise the real build orchestration and filesystem packaging, substituting only
// wasm-pack. No Rust build, tool download, network access or repository output.
function build(args, before = () => {}) {
  const root = fs.mkdtempSync(join(tmpdir(), "terrio-build-"));
  const crate = join(root, "js");
  fs.mkdirSync(crate);
  for (const name of ["LICENSE", "NOTICE", "LICENSE-THIRD-PARTY.md"]) fs.writeFileSync(join(root, name), name);
  for (const name of ["load.js", "load.d.ts"]) fs.writeFileSync(join(crate, name), name);
  const calls = [];
  before(crate);
  const execFileSync = (command, argv) => {
    assert.equal(command, "wasm-pack");
    calls.push(argv);
    const out = argv[argv.indexOf("--out-dir") + 1];
    fs.mkdirSync(out, { recursive: true });
    fs.mkdirSync(join(out, "snippets"));
    fs.writeFileSync(join(out, "snippets", "helper.js"), "export {};\n");
    const files = ["terrio.js", "terrio.d.ts", "terrio_bg.wasm", "terrio_bg.wasm.d.ts"];
    for (const file of files) fs.writeFileSync(join(out, file), file);
    // wasm-pack generates WASM declarations but omits them from its npm files list.
    fs.writeFileSync(join(out, "package.json"), JSON.stringify({ name: "terrio-js", files: files.filter((f) => f !== "terrio_bg.wasm.d.ts") }));
    fs.writeFileSync(join(out, ".gitignore"), "*");
  };
  try {
    const code = source.replace(/^import .*;\r?\n/gm, "").replaceAll("import.meta.url", JSON.stringify(pathToFileURL(join(crate, "build.mjs")).href));
    vm.runInNewContext(code, { ...fs, homedir, dirname, join, fileURLToPath, URL, parseArgs: (options) => parseArgs({ ...options, args }), process: { env: {} }, execFileSync, console: { log() {} } });
    const out = join(crate, "pkg");
    return { pkg: JSON.parse(fs.readFileSync(join(out, "package.json"))), calls, files: fs.readdirSync(out) };
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

for (const target of ["web", "nodejs", "bundler"]) {
  test(`selective ${target}: one locked feature build and complete package`, () => {
    const { pkg, calls, files } = build(["--target", target, "--no-default-features", "--features", "netcdf-hdf5"]);
    assert.equal(calls.length, 1);
    assert.deepEqual(Array.from(calls[0].slice(calls[0].indexOf("--") + 1)), ["--no-default-features", "--features", "netcdf-hdf5", "--locked"]);
    assert.deepEqual(Object.keys(pkg.exports).sort(), [".", "./*", "./package.json"]);
    for (const file of ["terrio.js", "terrio.d.ts", "terrio_bg.wasm", "terrio_bg.wasm.d.ts", "LICENSE", "NOTICE", "LICENSE-THIRD-PARTY.md"]) {
      assert.ok(pkg.files.includes(file), file);
      assert.ok(files.includes(file), file);
    }
    assert.ok(pkg.files.includes("snippets/"));
    assert.ok(!files.includes("load.js"));
    assert.ok(!files.includes("core"));
  });
}
test("standard full/core/zarr exports and loader remain available", () => {
  const { pkg, calls, files } = build(["--no-threads", "--no-memory64"]);
  assert.equal(calls.length, 3);
  assert.ok(pkg.exports["./core"]);
  // terrio-js/zarr: its own crate (js/zarr), built into `zarr/`.
  assert.deepEqual(pkg.exports["./zarr"], { types: "./zarr/terrio.d.ts", default: "./zarr/terrio.js" });
  assert.match(calls[2][1], /[\\/]js[\\/]zarr$/);
  for (const file of ["zarr/terrio.js", "zarr/terrio_bg.wasm", "zarr/terrio.d.ts"]) assert.ok(pkg.files.includes(file), file);
  assert.ok(pkg.exports["./load"]);
  assert.ok(files.includes("load.d.ts"));
  // Each module's glue imports its own snippets.
  assert.ok(pkg.files.includes("snippets/"));
  assert.ok(pkg.files.includes("core/snippets/"));
});
test("selective output refuses stale assets", () => {
  assert.throws(() => build(["--no-default-features"], (crate) => fs.mkdirSync(join(crate, "pkg"))), /must not exist/);
});
test("feature selection cannot silently retain full defaults", () => {
  assert.throws(() => build(["--features", "netcdf-hdf5"]), /require --no-default-features/);
});
test("selective threading and invalid targets fail before tools run", () => {
  assert.throws(() => build(["--no-default-features", "--features", "netcdf-hdf5,threads"]), /single-threaded/);
  assert.throws(() => build(["--no-default-features", "--features", "terrio-js/threads"]), /single-threaded/);
  assert.throws(() => build(["--target", "typo"]), /target must be/);
});

test("selective build without optional features remains locked", () => {
  const { calls, pkg } = build(["--no-default-features"]);
  assert.equal(calls.length, 1);
  assert.deepEqual(Array.from(calls[0].slice(calls[0].indexOf("--") + 1)), ["--no-default-features", "--locked"]);
  assert.ok(!pkg.exports["./core"]);
});
