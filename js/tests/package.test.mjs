// Tests of the npm package as `node js/build.mjs` builds it (js/pkg, the `web` target): both
// modules, `terrio-js` and `terrio-js/core`, load and work, and the package names them. Run with
// `node --test js/tests/package.test.mjs` (TERRIO_JS_PKG_DIR: another package directory).

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
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
  for (const f of ["terrio_bg.wasm", "terrio.js", "terrio.d.ts", "core/terrio_bg.wasm", "core/terrio.js", "core/terrio.d.ts"]) {
    assert.ok(pkg.files.includes(f), f);
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
