// Tests of terrio-js/cog, the GeoTIFF-only module (js/cog): build it with
// `wasm-pack build js/cog --target nodejs --out-dir ../pkg-node/cog --out-name terrio --no-pack`,
// and the full module into pkg-node (it writes the COG and reads it for comparison), then run
// `node --test js/tests/cog.test.mjs` (TERRIO_JS_COG_PKG and TERRIO_JS_PKG name other builds).
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const require = createRequire(import.meta.url);
const cog = require(process.env.TERRIO_JS_COG_PKG ?? "../pkg-node/cog/terrio.js");
const terrio = require(process.env.TERRIO_JS_PKG ?? "../pkg-node/terrio.js");

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

test("terrio-js/cog reads windows, bands and overviews as terrio-js does", async () => {
  const dir = mkdtempSync(join(tmpdir(), "terrio-cog-"));
  // Two Uint16 bands of 1200 x 1000, a DEFLATE COG with overviews, written by terrio-js.
  const [w, h] = [1200, 1000];
  const data = new Uint16Array(2 * w * h).map((_, i) => i % 7919);
  const src = terrio.rasterFromArray(data, w, h, [500000, 10, 0, 4100000, 0, -10], "EPSG:32610", 0);
  writeFileSync(join(dir, "dem.tif"), src.toBytes("dem.tif", { format: "COG", compress: "DEFLATE" }));
  writeFileSync(join(dir, "not.tif"), "not a TIFF at all, but long enough to fill the first block");
  const server = await serve(dir);
  try {
    const r = await cog.openCog(server.url + "dem.tif");
    const full = await terrio.openUrl(server.url + "dem.tif");
    assert.deepEqual([r.width, r.height, r.count, r.dtype, r.epsg, r.nodata], [w, h, 2, "uint16", 32610, 0]);
    assert.deepEqual(Array.from(r.transform), [500000, 10, 0, 4100000, 0, -10]);
    assert.ok(r.overviews.length > 0);
    assert.deepEqual(r.overviews, full.overviews);
    // Bands reordered, a window across tiles: the pixels terrio-js reads.
    const win = [500, 400, 100, 300];
    const [b2, b1] = await r.read({ window: win, bands: [2, 1] });
    assert.ok(b2 instanceof Uint16Array);
    assert.deepEqual([...b2], [...src.readBand(2, win)]);
    assert.deepEqual([...b1], [...src.readBand(1, win)]);
    // The requests terrio-js sends for the same read: the headers and the tiles under the window.
    await full.readBand(2, win);
    await full.readBand(1, win);
    assert.deepEqual(r.stats(), full.stats());
    assert.ok(r.stats().bytes < statSync(join(dir, "dem.tif")).size);
    // Tiles read once are kept: the same window again sends no request.
    const before = server.stats.requests;
    await r.read({ window: win, bands: [2, 1] });
    assert.equal(server.stats.requests, before);
    // An overview, whole, and a window of it.
    const [ow, oh] = r.overviews[0];
    const [o1] = await r.read({ overview: 0, bands: [1] });
    assert.deepEqual([...o1], [...(await full.readBand(1, undefined, 0))]);
    assert.equal(o1.length, ow * oh);
    const [o2] = await r.read({ overview: 0, window: [10, 20, 30, 40], bands: [2] });
    assert.deepEqual([...o2], [...(await full.readBand(2, [10, 20, 30, 40], 0))]);
    // Every band by default, and for an empty list.
    assert.equal((await r.read({ window: [0, 0, 8, 8] })).length, 2);
    assert.equal((await r.read({ window: [0, 0, 8, 8], bands: [] })).length, 2);
    // Bad reads are errors, not coerced.
    await assert.rejects(r.read({ bands: [3] }), /no band 3/);
    await assert.rejects(r.read({ bands: [0] }), /no band 0/);
    await assert.rejects(r.read({ overview: 9 }), /no overview 9/);
    await assert.rejects(r.read({ overview: -1 }), /`overview` must be/);
    await assert.rejects(r.read({ window: [-1, 0, 8, 8] }), /non-negative integers/);
    await assert.rejects(r.read({ window: [0, 0, 8] }), /window/);
    await assert.rejects(r.read({ window: [w - 4, 0, 8, 8] }));
    await assert.rejects(r.read({ windwo: [0, 0, 8, 8] }), /unknown option `windwo`/);
    await assert.rejects(r.read("x"), /must be an object/);
    await assert.rejects(cog.openCog(server.url + "not.tif"), /not a TIFF/);
    await assert.rejects(cog.openCog(server.url + "missing.tif"));
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});

test("terrio-js/cog runs reads awaited together at once, within twelve requests, and fetches a block once", async () => {
  const dir = mkdtempSync(join(tmpdir(), "terrio-cog-"));
  // 64 tiles of 64 KB: a read of one tile is a request of its own.
  const [w, h] = [2048, 2048];
  const src = terrio.rasterFromArray(new Uint8Array(w * h).map((_, i) => i % 251), w, h, [0, 1, 0, h, 0, -1], "EPSG:4326");
  writeFileSync(join(dir, "tiles.tif"), src.toBytes("tiles.tif", { format: "COG", compress: "NONE", tileSize: 256 }));
  const server = await serve(dir);
  let active = 0;
  let peak = 0;
  // Each response comes 50 ms late: reads queued one after another never overlap theirs.
  const fetch = async (url, init) => {
    peak = Math.max(peak, ++active);
    await new Promise((r) => setTimeout(r, 50));
    try {
      return await globalThis.fetch(url, init);
    } finally {
      active--;
    }
  };
  try {
    const r = await cog.openCog(server.url + "tiles.tif", { fetch });
    peak = 0;
    // A window in each of sixteen tiles, read together.
    const windows = Array.from({ length: 16 }, (_, i) => [1034 + (i % 4) * 256, 1034 + Math.floor(i / 4) * 256, 64, 64]);
    const out = await Promise.all(windows.map((window) => r.read({ window })));
    windows.forEach((window, i) => assert.deepEqual([...out[i][0]], [...src.readBand(1, window)]));
    // Together, and within the twelve requests a remote keeps in flight.
    assert.ok(peak >= 2 && peak <= 12, `reads awaited together send their requests together, saw ${peak} at once`);
    // Two reads of the same tile together: its bytes are requested once, as for one read.
    const one = await cog.openCog(server.url + "tiles.tif", { fetch });
    const two = await cog.openCog(server.url + "tiles.tif", { fetch });
    const opened = two.stats().requests;
    await one.read({ window: windows[0] });
    const [a, b] = await Promise.all([two.read({ window: windows[0] }), two.read({ window: windows[0] })]);
    assert.deepEqual([...a[0]], [...b[0]]);
    assert.equal(two.stats().requests - opened, one.stats().requests - opened);
    assert.equal(two.stats().bytes, one.stats().bytes);
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});

test("terrio-js/cog refuses unknown options and options of the wrong type", async () => {
  for (const options of ["x", 5, true]) {
    await assert.rejects(cog.openCog("http://127.0.0.1:9/a.tif", options), /options must be an object/);
  }
  await assert.rejects(cog.openCog("http://127.0.0.1:9/a.tif", { blocksize: 512 }), /unknown option `blocksize`/);
  await assert.rejects(cog.openCog("http://127.0.0.1:9/a.tif", { cacheSize: -1 }), /`cacheSize` must be/);
  await assert.rejects(cog.openCog("http://127.0.0.1:9/a.tif", { blockSize: "big" }), /`blockSize` must be/);
  // Whole numbers, as openUrl takes them, rather than truncated.
  await assert.rejects(cog.openCog("http://127.0.0.1:9/a.tif", { blockSize: 1000.5 }), /`blockSize` must be a whole number/);
  await assert.rejects(cog.openCog("http://127.0.0.1:9/a.tif", { cacheSize: 1e30 }), /`cacheSize` must be a whole number/);
});

test("terrio-js/cog leaves JPEG XL out and says so", async () => {
  const dir = mkdtempSync(join(tmpdir(), "terrio-cog-"));
  const src = terrio.rasterFromArray(new Uint8Array(64 * 64).map((_, i) => i % 251), 64, 64, [0, 1, 0, 64, 0, -1], "EPSG:4326");
  writeFileSync(join(dir, "jxl.tif"), src.toBytes("jxl.tif", { format: "GTiff", compress: "JXL" }));
  const server = await serve(dir);
  try {
    const r = await cog.openCog(server.url + "jxl.tif");
    assert.equal(r.width, 64);
    await assert.rejects(r.read(), /built without the `jxl` feature/);
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});

// The open resolves while the rest of the first block is held; the read of every pixel waits for it.
for (const [name, open, readAll] of [
  ["terrio-js/cog", (url) => cog.openCog(url), async (r) => (await r.read({ bands: [1] }))[0]],
  ["terrio-js openUrl", (url) => terrio.openUrl(url), (r) => r.readBand(1)],
]) {
  test(`${name} opens from the first bytes of a block still arriving`, async () => {
    const dir = mkdtempSync(join(tmpdir(), "terrio-cog-"));
    const [w, h] = [600, 500];
    const data = new Uint16Array(w * h).map((_, i) => i % 7919);
    const src = terrio.rasterFromArray(data, w, h, [500000, 10, 0, 4100000, 0, -10], "EPSG:32610", 0);
    const bytes = src.toBytes("a.tif", { format: "COG", compress: "DEFLATE", tileSize: 128 });
    // Bytes 0- arrive in two parts: the first 4 KiB (the header), the rest once released.
    let release, timer;
    const held = new Promise((r) => (release = r));
    const firstBlock = [];
    const server = createServer(async (req, res) => {
      const [, a, b] = /bytes=(\d+)-(\d+)/.exec(req.headers.range).map(Number);
      const end = Math.min(b, bytes.length - 1);
      if (a === 0) firstBlock.push(req.headers.range);
      res.writeHead(206, { "Content-Range": `bytes ${a}-${end}/${bytes.length}`, "Content-Length": end - a + 1 });
      if (a === 0) {
        res.write(bytes.subarray(0, 4096));
        await held;
        res.end(bytes.subarray(4096, end + 1));
      } else {
        res.end(bytes.subarray(a, end + 1));
      }
    });
    await new Promise((r) => server.listen(0, "127.0.0.1", r));
    try {
      const url = `http://127.0.0.1:${server.address().port}/a.tif`;
      const opened = await Promise.race([open(url), new Promise((r) => (timer = setTimeout(r, 5000, "timeout")))]);
      assert.notEqual(opened, "timeout", "the open waited for the whole first block");
      assert.deepEqual([opened.width, opened.height], [w, h]);
      // A read of every pixel needs the rest of the block: it waits for it, not a second request.
      const read = readAll(opened);
      setTimeout(release, 50);
      const band = await read;
      assert.deepEqual([...band], [...src.readBand(1)]);
      assert.deepEqual(firstBlock, ["bytes=0-65535"]);
      assert.ok(opened.stats().bytes >= Math.min(65536, bytes.length));
    } finally {
      clearTimeout(timer);
      release();
      server.closeAllConnections();
      server.close();
      rmSync(dir, { recursive: true });
    }
  });
}

test("terrio-js/cog refuses a first block whose Content-Range is not where it asked", async () => {
  const [w, h] = [600, 500];
  const src = terrio.rasterFromArray(new Uint16Array(w * h), w, h, [500000, 10, 0, 4100000, 0, -10], "EPSG:32610", 0);
  const bytes = src.toBytes("a.tif", { format: "COG", compress: "DEFLATE", tileSize: 128 });
  // The bytes asked for, said to be from another position.
  const server = createServer((req, res) => {
    const m = /bytes=(\d+)-(\d+)/.exec(req.headers.range ?? "");
    const [a, b] = m ? [+m[1], Math.min(+m[2], bytes.length - 1)] : [0, bytes.length - 1];
    res.writeHead(206, { "Content-Range": `bytes ${a + 1}-${b + 1}/${bytes.length + 1}`, "Content-Length": b - a + 1 });
    res.end(bytes.subarray(a, b + 1));
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  try {
    await assert.rejects(cog.openCog(`http://127.0.0.1:${server.address().port}/a.tif`), /Content-Range/);
  } finally {
    server.close();
  }
});
