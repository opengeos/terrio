// Tests of terrio-js/geoparquet, the GeoParquet-only module (js/geoparquet): build it with
// `wasm-pack build js/geoparquet --target nodejs --out-dir ../pkg-node/geoparquet --out-name terrio --no-pack`,
// and the full module into pkg-node (it writes the files and reads them for comparison), then run
// `node --test js/tests/geoparquet.test.mjs` (TERRIO_JS_GEOPARQUET_PKG and TERRIO_JS_PKG name other
// builds; TERRIO_TEST_DATA, a directory with ne_places_rg.parquet, adds a real file).
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const require = createRequire(import.meta.url);
const gp = require(process.env.TERRIO_JS_GEOPARQUET_PKG ?? "../pkg-node/geoparquet/terrio.js");
const terrio = require(process.env.TERRIO_JS_PKG ?? "../pkg-node/terrio.js");
const DATA = process.env.TERRIO_TEST_DATA;

/// Serves `dir` over HTTP (whole files, byte ranges and suffix ranges), counting requests.
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

/// 1500 squares of 4 degrees, with an id and a name, as GeoJSON.
function cells() {
  const fc = { type: "FeatureCollection", features: [] };
  for (let i = 0; i < 50; i++) {
    for (let j = 0; j < 30; j++) {
      const [x, y] = [-170 + i * 6, -80 + j * 5];
      const ring = [[x, y], [x + 4, y], [x + 4, y + 4], [x, y + 4], [x, y]];
      fc.features.push({ type: "Feature", properties: { id: i * 30 + j, name: `c${i}-${j}` }, geometry: { type: "Polygon", coordinates: [ring] } });
    }
  }
  return fc;
}

test("terrio-js/geoparquet reads features as terrio-js's openUrl does", async () => {
  const dir = mkdtempSync(join(tmpdir(), "terrio-geoparquet-"));
  const layer = terrio.fromGeoJSON(cells(), "cells");
  writeFileSync(join(dir, "cells.parquet"), layer.toBytes("cells.parquet"));
  // The squares near UTM zone 10N, reprojected into it.
  const near = cells();
  near.features = near.features.filter((f) => f.geometry.coordinates[0].every(([x, y]) => x >= -128 && x <= -114 && y >= 0 && y <= 70));
  writeFileSync(join(dir, "utm.parquet"), terrio.fromGeoJSON(near, "utm").toBytes("utm.parquet", { tSrs: "EPSG:32610" }));
  writeFileSync(join(dir, "not.parquet"), "not a Parquet file at all, but long enough to be one");
  const server = await serve(dir);
  try {
    const f = await gp.openGeoParquet(server.url + "cells.parquet");
    const full = await terrio.openUrl(server.url + "cells.parquet");
    // No `crs` in the file: OGC:CRS84, EPSG:4326 here.
    assert.equal(f.epsg, 4326);
    assert.deepEqual(f.rowGroups, full.info().rowGroups);
    assert.equal(f.rowGroups.reduce((s, g) => s + g.rows, 0), 1500);
    // The features, requests and bytes of terrio-js's reads with the same options.
    for (const options of [{}, { bbox: [0, 0, 10, 10] }, { bbox: [-11, -11, -9, 30], columns: ["ID"] }, { rowGroups: [0], columns: [] }]) {
      const ours = await f.read(options);
      const theirs = (await full.read(options)).toGeoJSON();
      assert.deepEqual(ours, theirs, JSON.stringify(options));
      assert.deepEqual(f.stats(), full.stats(), JSON.stringify(options));
    }
    // An inverted bbox is taken by its corners' extent.
    assert.deepEqual(await f.read({ bbox: [10, 10, 0, 0] }), await f.read({ bbox: [0, 0, 10, 10] }));
    // A bbox keeps the features that intersect it, not those of its row groups.
    const inBox = await f.read({ bbox: [0, 0, 10, 10] });
    assert.ok(inBox.features.length > 0 && inBox.features.length < 20);
    assert.deepEqual(Object.keys((await f.read({ columns: ["name"] })).features[0].properties), ["name"]);
    // Reprojected when written, kept as it is when read: the EPSG code of its PROJJSON.
    const utm = await gp.openGeoParquet(server.url + "utm.parquet");
    assert.equal(utm.epsg, 32610);
    const utmFull = await terrio.openUrl(server.url + "utm.parquet");
    assert.deepEqual(await utm.read({}), (await utmFull.read({})).toGeoJSON());
    // A server that refuses suffix ranges: opened from the first block, the footer fetched after,
    // as terrio-js does; small blocks take several rounds.
    const noSuffix = (url, init) => (/^bytes=-/.test(init?.headers?.Range ?? "") ? Promise.resolve(new Response(null, { status: 416 })) : fetch(url, init));
    for (const options of [{ fetch: noSuffix }, { blockSize: 4096 }, { fetch: noSuffix, blockSize: 4096 }]) {
      const a = await gp.openGeoParquet(server.url + "cells.parquet", options);
      const b = await terrio.openUrl(server.url + "cells.parquet", options);
      assert.deepEqual(a.rowGroups, b.info().rowGroups);
      assert.deepEqual(await a.read({ bbox: [0, 0, 10, 10] }), (await b.read({ bbox: [0, 0, 10, 10] })).toGeoJSON());
      assert.deepEqual(a.stats(), b.stats(), JSON.stringify(Object.keys(options)));
    }
    await assert.rejects(gp.openGeoParquet(server.url + "not.parquet", { fetch: noSuffix }), /not a Parquet file/);
    // Bad reads are errors, not coerced.
    await assert.rejects(f.read({ rowGroups: [1] }), /no row group 1/);
    await assert.rejects(f.read({ rowGroups: [-1] }), /non-negative integers/);
    await assert.rejects(f.read({ rowGroups: 0 }), /`rowGroups` must be an array/);
    await assert.rejects(f.read({ bbox: [0, 0, 10] }), /`bbox` must be/);
    await assert.rejects(f.read({ bbox: [0, 0, 10, "x"] }), /`bbox` must be/);
    await assert.rejects(f.read({ bbox: [0, 0, 10, NaN] }), /`bbox` must be/);
    // Not infinite either: openUrl refuses them too (its options go through JSON).
    await assert.rejects(f.read({ bbox: [-Infinity, -90, Infinity, 90] }), /`bbox` must be/);
    await assert.rejects(full.read({ bbox: [-Infinity, -90, Infinity, 90] }));
    await assert.rejects(f.read({ columns: [1] }), /array of strings/);
    await assert.rejects(f.read({ bbx: [0, 0, 1, 1] }), /unknown option `bbx`/);
    await assert.rejects(f.read("x"), /must be an object/);
    await assert.rejects(gp.openGeoParquet(server.url + "cells.parquet", { blockSize: 1.5 }), /whole number of bytes/);
    await assert.rejects(gp.openGeoParquet(server.url + "cells.parquet", { array: "x" }), /unknown option `array`/);
    await assert.rejects(gp.openGeoParquet(server.url + "not.parquet"), /not a Parquet file/);
    await assert.rejects(gp.openGeoParquet(server.url + "missing.parquet"));
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});

test("terrio-js/geoparquet reads a real file's row groups and bbox as terrio-js does", { skip: !DATA && "TERRIO_TEST_DATA is not set" }, async (t) => {
  if (!existsSync(join(DATA, "ne_places_rg.parquet"))) return t.skip("ne_places_rg.parquet is missing");
  const server = await serve(DATA);
  try {
    const f = await gp.openGeoParquet(server.url + "ne_places_rg.parquet");
    assert.deepEqual(f.rowGroups.map((g) => g.rows), [1000, 1000, 1000, 1000, 1000, 1000, 1000, 342]);
    // The count node.test.mjs asserts for terrio-js (its "as GDAL reads them" test), and its features.
    const full = await terrio.openUrl(server.url + "ne_places_rg.parquet");
    const fc = await f.read({ bbox: [0, 40, 20, 55] });
    assert.equal(fc.features.length, 306);
    assert.deepEqual(fc, (await full.read({ bbox: [0, 40, 20, 55] })).toGeoJSON());
    assert.equal((await f.read({ rowGroups: [7] })).features.length, 342);
    await full.read({ rowGroups: [7] });
    assert.deepEqual(f.stats(), full.stats());
  } finally {
    server.close();
  }
});
