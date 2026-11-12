// Tests of terrio-js/zarr, the Zarr-only module (js/zarr): build it with
// `wasm-pack build js/zarr --target nodejs --out-dir ../pkg-node/zarr --out-name terrio --no-pack`,
// then run `node --test js/tests/zarr.test.mjs` (TERRIO_JS_ZARR_PKG names another build).
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const zarr = createRequire(import.meta.url)(process.env.TERRIO_JS_ZARR_PKG ?? "../pkg-node/zarr/terrio.js");

/// A Zarr v3 store of one array `a`, 2 x 64 x 64 uint16 in chunks of 1 x 32 x 32, value
/// `b * 10000 + row * 100 + col`, with a CRS code and a transform.
function writeStore(dir) {
  const store = join(dir, "s.zarr");
  mkdirSync(join(store, "a"), { recursive: true });
  writeFileSync(join(store, "zarr.json"), JSON.stringify({ zarr_format: 3, node_type: "group", attributes: {} }));
  writeFileSync(join(store, "a", "zarr.json"), JSON.stringify({
    zarr_format: 3, node_type: "array", shape: [2, 64, 64], data_type: "uint16",
    chunk_grid: { name: "regular", configuration: { chunk_shape: [1, 32, 32] } },
    chunk_key_encoding: { name: "default", configuration: { separator: "/" } },
    fill_value: 0, dimension_names: ["band", "y", "x"],
    attributes: { "spatial:transform": [10, 0, 500000, 0, -10, 4600000], "proj:code": "EPSG:32630" },
    codecs: [{ name: "bytes", configuration: { endian: "little" } }],
  }));
  for (let b = 0; b < 2; b++) {
    for (let cy = 0; cy < 2; cy++) {
      for (let cx = 0; cx < 2; cx++) {
        const chunk = new Uint16Array(32 * 32);
        for (let r = 0; r < 32; r++) for (let c = 0; c < 32; c++) chunk[r * 32 + c] = value(b, cy * 32 + r, cx * 32 + c);
        mkdirSync(join(store, "a", "c", `${b}`, `${cy}`), { recursive: true });
        writeFileSync(join(store, "a", "c", `${b}`, `${cy}`, `${cx}`), new Uint8Array(chunk.buffer));
      }
    }
  }
}

const value = (b, r, c) => b * 10000 + r * 100 + c;

/// Serves `dir` over HTTP (whole files and byte ranges), counting requests.
async function serve(dir) {
  const stats = { requests: 0 };
  const server = createServer((req, res) => {
    stats.requests++;
    let data;
    try {
      data = readFileSync(join(dir, decodeURIComponent(req.url.split("?")[0])));
    } catch {
      res.writeHead(404);
      return res.end();
    }
    const m = /bytes=(\d*)-(\d*)/.exec(req.headers.range ?? "");
    if (!m) {
      res.writeHead(200, { "Content-Length": data.length });
      return res.end(data);
    }
    const a = m[1] === "" ? Math.max(0, data.length - +m[2]) : +m[1];
    const b = m[1] === "" || m[2] === "" ? data.length - 1 : Math.min(+m[2], data.length - 1);
    res.writeHead(206, { "Content-Range": `bytes ${a}-${b}/${data.length}`, "Content-Length": b - a + 1 });
    res.end(data.subarray(a, b + 1));
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return { url: `http://127.0.0.1:${server.address().port}/`, stats, close: () => server.close() };
}

test("terrio-js/zarr opens an array and reads windows of bands", async () => {
  const dir = mkdtempSync(join(tmpdir(), "terrio-zarr-"));
  writeStore(dir);
  const server = await serve(dir);
  try {
    const a = await zarr.openZarr(server.url + "s.zarr", { array: "a" });
    assert.deepEqual([a.width, a.height, a.count, a.dtype, a.epsg], [64, 64, 2, "uint16", 32630]);
    assert.deepEqual(Array.from(a.transform), [500000, 10, 0, 4600000, 0, -10]);
    assert.equal(a.attributes()["proj:code"], "EPSG:32630");
    // Bands reordered, a window across the four chunks.
    const [b2, b1] = await a.read({ window: [28, 10, 8, 30], bands: [2, 1] });
    assert.ok(b2 instanceof Uint16Array);
    assert.equal(b2.length, 8 * 30);
    assert.equal(b2[0], value(1, 10, 28));
    assert.equal(b1[29 * 8 + 7], value(0, 39, 35));
    // The whole array, every band.
    const all = await a.read({});
    assert.equal(all.length, 2);
    assert.equal(all[1][63 * 64 + 63], value(1, 63, 63));
    assert.equal((await a.read({ bands: [] })).length, 2);
    // Chunks read once are kept: the same window again sends no request.
    const before = server.stats.requests;
    await a.read({ window: [28, 10, 8, 30], bands: [2, 1] });
    assert.equal(server.stats.requests, before);
    assert.ok(a.stats().requests > 0);
    await assert.rejects(a.read({ window: [0, 0, 8] }), /window/);
    // Bad reads are errors, not coerced: a band the array lacks (two bands), band 0, a negative or
    // fractional coordinate, bands that are not an array, a window past the array's edge.
    await assert.rejects(a.read({ bands: [3] }), /no band 3/);
    await assert.rejects(a.read({ bands: [0] }), /no band 0/);
    await assert.rejects(a.read({ window: [-1, 0, 8, 8] }), /non-negative integers/);
    await assert.rejects(a.read({ window: [0, 0, 8.5, 8] }), /non-negative integers/);
    await assert.rejects(a.read({ bands: 1 }), /non-negative integers/);
    await assert.rejects(a.read({ windwo: [0, 0, 8, 8] }), /unknown option `windwo`/);
    await assert.rejects(a.read("x"), /must be an object/);
    await assert.rejects(a.read({ window: [60, 60, 8, 8] }));
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});

test("terrio-js/zarr refuses unknown options and options of the wrong type", async () => {
  for (const options of ["x", 5, true]) {
    await assert.rejects(zarr.openZarr("http://127.0.0.1:9/s.zarr", options), /options must be an object/);
  }
  await assert.rejects(zarr.openZarr("http://127.0.0.1:9/s.zarr", { arary: "a" }), /unknown option `arary`/);
  await assert.rejects(zarr.openZarr("http://127.0.0.1:9/s.zarr", { array: 5 }), /`array` must be a string/);
  await assert.rejects(zarr.openZarr("http://127.0.0.1:9/s.zarr", { cacheSize: -1 }), /`cacheSize` must be/);
});

test("terrio-js/zarr fails on a store that is not there", async () => {
  const dir = mkdtempSync(join(tmpdir(), "terrio-zarr-"));
  const server = await serve(dir);
  try {
    await assert.rejects(zarr.openZarr(server.url + "missing.zarr", { array: "a" }));
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});
