// The npm package in headless browsers (Playwright): `terrio-js/load` picks the threaded module
// on a cross-origin isolated page and starts its thread pool, and the single-threaded module on a
// page that is not; both decode and write the same bytes. With a data directory (the one
// scripts/fetch_test_data.sh fills), it also times real files on both.
//
// Usage: node js/tests/browser.mjs [data dir]   (js/pkg built by `node js/build.mjs`; Playwright
// from PLAYWRIGHT_MODULE, or `playwright` resolved from the working directory; BROWSERS: a
// comma-separated list of chromium, firefox, webkit, chromium by default)

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const pkg = fileURLToPath(new URL("../pkg", import.meta.url));
const data = process.argv[2];
const require = createRequire(join(process.cwd(), "/"));
const playwright = require(process.env.PLAYWRIGHT_MODULE ?? "playwright");

const TYPES = { ".js": "text/javascript", ".wasm": "application/wasm", ".html": "text/html", ".json": "application/json" };

// The page: loads a module with `load`, then times a few reads and writes.
const PAGE = `<!doctype html><meta charset="utf-8"><script type="module">
const params = new URLSearchParams(location.search);
const { load } = await import("/pkg/load.js");
const threads = params.get("threads") === "0" ? false : true;
const fallbacks = [];
const terrio = await load({ threads, onFallback: (e) => fallbacks.push(String(e)) });
const time = (f, runs = 3) => { let best = Infinity, out; for (let i = 0; i < runs; i++) { const t = performance.now(); out = f(); best = Math.min(best, performance.now() - t); } return [Math.round(best), out]; };
const digest = async (bytes) => Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))).map((b) => b.toString(16).padStart(2, "0")).join("");
const result = { isolated: crossOriginIsolated, features: terrio.features(), fallbacks, times: {} };
// A synthetic raster, written as a tiled DEFLATE COG and decoded.
const pixels = new Float32Array(2000 * 1500).map((_, i) => ((i * 7919) % 104729) / 7);
const ds = terrio.rasterFromArray(pixels, 2000, 1500, [500000, 10, 0, 4100000, 0, -10], "EPSG:32610");
const [, cog] = time(() => ds.toBytes("x.tif", { format: "COG", compress: "DEFLATE", tileSize: 128 }), 1);
result.cogSha256 = await digest(cog);
const back = terrio.open(cog, "x.tif").readBand(1);
result.roundTrip = back.length === pixels.length && back.every((v, i) => v === pixels[i]);
if (params.get("data")) {
  const get = async (name) => new Uint8Array(await (await fetch("/data/" + name)).arrayBuffer());
  const dem = terrio.open(await get("USGS_1_n40w107.tif"), "dem.tif");
  result.times.decodeGeoTIFF = time(() => dem.readBand(1))[0];
  result.times.writeCOG = time(() => dem.toBytes("c.tif", { format: "COG", compress: "DEFLATE" }))[0];
  const jp2 = await get("s2_l1c_B04.jp2");
  result.times.decodeJPEG2000 = time(() => terrio.open(jp2, "B04.jp2").readBand(1), 1)[0];
}
window.__result = result;
</script>`;

/** Serves the package at /pkg/ and the data at /data/, with or without cross-origin isolation. */
function serve(isolated) {
  const server = createServer((req, res) => {
    const url = new URL(req.url, "http://localhost");
    const headers = isolated ? { "Cross-Origin-Opener-Policy": "same-origin", "Cross-Origin-Embedder-Policy": "require-corp" } : {};
    if (url.pathname === "/") {
      res.writeHead(200, { "Content-Type": "text/html", ...headers });
      return res.end(PAGE);
    }
    const [root, rest] = url.pathname.startsWith("/pkg/") ? [pkg, url.pathname.slice(5)] : data && url.pathname.startsWith("/data/") ? [data, url.pathname.slice(6)] : [null, null];
    const path = root && join(root, normalize(decodeURIComponent(rest)).replace(/^(\.\.[/\\])+/, ""));
    if (!path || !existsSync(path)) {
      res.writeHead(404);
      return res.end();
    }
    res.writeHead(200, { "Content-Type": TYPES[extname(path)] ?? "application/octet-stream", ...headers });
    res.end(readFileSync(path));
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

const [isolated, plain] = await Promise.all([serve(true), serve(false)]);
const results = {};
try {
  for (const name of (process.env.BROWSERS ?? "chromium").split(",")) {
    const browser = await playwright[name].launch();
    try {
      const run = async (server, query) => {
        const page = await browser.newPage();
        page.on("pageerror", (e) => console.error(`${name}: ${e}`));
        await page.goto(`http://127.0.0.1:${server.address().port}/?${query}`);
        await page.waitForFunction(() => window.__result, null, { timeout: 600_000 });
        const r = await page.evaluate(() => window.__result);
        await page.close();
        return r;
      };
      const q = data ? "data=1" : "";
      const mt = await run(isolated, q);
      const st = await run(isolated, `${q}&threads=0`);
      const fallback = await run(plain, "");
      results[name] = { threaded: mt, singleThreaded: st, notIsolated: fallback };
      assert.equal(mt.isolated, true);
      assert.equal(mt.features.build, "terrio-js/threads", JSON.stringify(mt));
      assert.ok(mt.features.threads > 1, JSON.stringify(mt.features));
      assert.equal(st.features.build, "terrio-js");
      assert.equal(fallback.isolated, false);
      assert.equal(fallback.features.build, "terrio-js");
      for (const r of [mt, st, fallback]) assert.ok(r.roundTrip);
      assert.equal(mt.cogSha256, st.cogSha256);
      assert.equal(fallback.cogSha256, st.cogSha256);
    } finally {
      await browser.close();
    }
  }
} finally {
  isolated.close();
  plain.close();
}
for (const [name, r] of Object.entries(results)) {
  console.log(name, JSON.stringify({ threads: r.threaded.features.threads, threaded: r.threaded.times, singleThreaded: r.singleThreaded.times }));
}
console.log("ok");
