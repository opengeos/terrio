// Tests of the npm package as `node js/build.mjs` builds it (js/pkg, the `web` target): both
// modules, `terrio-js` and `terrio-js/core`, load and work, and the package names them. Run with
// `node --test js/tests/package.test.mjs` (TERRIO_JS_PKG_DIR: another package directory).

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const dir = process.env.TERRIO_JS_PKG_DIR ?? fileURLToPath(new URL("../pkg", import.meta.url));
const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));

/** Loads one of the package's modules, as `init` does in a browser, from its bytes. */
async function load(sub) {
  const mod = await import(pathToFileURL(join(dir, sub, "terrio.js")).href);
  await mod.default({ module_or_path: readFileSync(join(dir, sub, "terrio_bg.wasm")) });
  return mod;
}

test("the package names both modules and ships their files", () => {
  assert.equal(pkg.name, "terrio-js");
  assert.deepEqual(pkg.exports["."], { types: "./terrio.d.ts", default: "./terrio.js" });
  assert.deepEqual(pkg.exports["./core"], { types: "./core/terrio.d.ts", default: "./core/terrio.js" });
  assert.deepEqual(pkg.exports["./load"], { types: "./load.d.ts", default: "./load.js" });
  for (const f of ["terrio_bg.wasm", "terrio.js", "terrio.d.ts", "core/terrio_bg.wasm", "core/terrio.js", "core/terrio.d.ts", "load.js", "load.d.ts"]) {
    assert.ok(pkg.files.includes(f), f);
  }
  if (existsSync(join(dir, "threads"))) {
    assert.deepEqual(pkg.exports["./threads"], { types: "./threads/terrio.d.ts", default: "./threads/terrio.js" });
    assert.ok(pkg.files.includes("threads/"));
    assert.ok(pkg.sideEffects.includes("./threads/snippets/*"));
  }
});

for (const [sub, build] of [["", "terrio-js"], ["core", "terrio-js/core"]]) {
  test(`${build} loads and round-trips a GeoTIFF and a FlatGeobuf`, async () => {
    const terrio = await load(sub);
    assert.equal(terrio.features().build, build);
    const ds = terrio.rasterFromArray(new Int16Array([1, 2, 3, 4, 5, 6]), 3, 2, [0, 1, 0, 2, 0, -1], "EPSG:4326");
    const back = terrio.open(ds.toBytes("x.tif", { compress: "DEFLATE" }), "x.tif");
    assert.deepEqual([...back.readBand(1)], [1, 2, 3, 4, 5, 6]);
    const pt = { type: "Feature", properties: { a: 1 }, geometry: { type: "Point", coordinates: [1, 2] } };
    const fgb = terrio.fromGeoJSON({ type: "FeatureCollection", features: [pt] }, "pts").toBytes("pts.fgb");
    assert.equal(terrio.open(fgb, "pts.fgb").toGeoJSON().features[0].properties.a, 1);
  });
}

test("terrio-js/core is the smaller download", () => {
  const size = (sub) => readFileSync(join(dir, sub, "terrio_bg.wasm")).length;
  assert.ok(size("core") < size("") * 0.6, `${size("core")} and ${size("")} bytes`);
});

// The threaded module, as `terrio-js/load` starts it in Node.js: the same results as the
// single-threaded module, from work split over its threads.
const threaded = existsSync(join(dir, "threads", "terrio_bg.wasm"));

test("terrio-js/load starts the threaded module, which matches the single-threaded one", { skip: !threaded && "no threads/ module" }, async () => {
  const { load, threadsSupported } = await import(pathToFileURL(join(dir, "load.js")).href);
  assert.ok(threadsSupported());
  const mt = await load({ threads: 4 });
  assert.equal(mt.features().build, "terrio-js/threads");
  assert.equal(mt.features().threads, 4);
  await assert.rejects(mt.initThreadPool(2), /already started/);
  const st = await load({ threads: false });
  assert.equal(st.features().build, "terrio-js");
  assert.equal(st.features().threads, 1);

  // A 1500 x 1200 Float32 raster as a DEFLATE COG in 64 x 64 tiles, and a 2500 x 2100 UInt16 one
  // as a JPEG 2000 (in 1024 x 1024 tiles), written and decoded by both modules.
  const pixels = new Float32Array(1500 * 1200).map((_, i) => ((i * 7919) % 104729) / 7);
  const geo = [500000, 10, 0, 4100000, 0, -10];
  const cog = (m) => m.rasterFromArray(pixels, 1500, 1200, geo, "EPSG:32610").toBytes("x.tif", { format: "COG", compress: "DEFLATE", tileSize: 64 });
  const [a, b] = [cog(mt), cog(st)];
  assert.deepEqual(Buffer.from(a), Buffer.from(b));
  const band = (m, bytes, name) => m.open(bytes, name).readBand(1);
  assert.deepEqual(Buffer.from(band(mt, a, "x.tif").buffer), Buffer.from(pixels.buffer));
  const ints = new Uint16Array(2500 * 2100).map((_, i) => (i * 31) % 4096);
  const jp2 = (m) => m.rasterFromArray(ints, 2500, 2100, geo, "EPSG:32610").toBytes("x.jp2");
  const [j1, j2] = [jp2(mt), jp2(st)];
  assert.deepEqual(Buffer.from(j1), Buffer.from(j2));
  assert.deepEqual(Buffer.from(band(mt, j1, "x.jp2").buffer), Buffer.from(ints.buffer));
});

test("terrio-js/load falls back to the single-threaded module", async () => {
  const { load } = await import(pathToFileURL(join(dir, "load.js")).href);
  assert.equal((await load({ threads: false })).features().build, "terrio-js");
  assert.equal((await load({ core: true })).features().build, "terrio-js/core");
});

// The memory64 module, as `terrio-js/load` loads it on request: every format, single-threaded.
const wide = existsSync(join(dir, "memory64", "terrio_bg.wasm"));

test("terrio-js/load loads the memory64 module on request", { skip: !wide && "no memory64/ module" }, async () => {
  const { load, memory64Supported } = await import(pathToFileURL(join(dir, "load.js")).href);
  assert.ok(memory64Supported());
  const m = await load({ memory64: true });
  assert.equal(m.features().build, "terrio-js/memory64");
  assert.ok(m.features().features.includes("parquet"));
  const st = await load({ threads: false });
  const pixels = new Float32Array(700 * 500).map((_, i) => ((i * 7919) % 104729) / 7);
  const cog = (t) => t.rasterFromArray(pixels, 700, 500, [0, 1, 0, 500, 0, -1], "EPSG:3857").toBytes("x.tif", { format: "COG", compress: "ZSTD" });
  assert.deepEqual(Buffer.from(cog(m)), Buffer.from(cog(st)));
  const pt = { type: "Feature", properties: { a: 1 }, geometry: { type: "Point", coordinates: [1, 2] } };
  const pq = m.fromGeoJSON({ type: "FeatureCollection", features: [pt] }, "pts").toBytes("pts.parquet");
  assert.equal(m.open(pq, "pts.parquet").toGeoJSON().features[0].properties.a, 1);
});
