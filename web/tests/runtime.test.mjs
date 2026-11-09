// SPDX-License-Identifier: MIT
// The browser build of the command line on its WASI layer (web/wasi.js), run in Node.js:
//   TERRIO_WASM=target/wasm32-wasip1/debug/terrio.wasm node --test web/tests/runtime.test.mjs
// Remote files are served by a stand-in for the browser's synchronous XMLHttpRequest.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { MemDir, MemFile, RemoteDir, RemoteFile, Wasi, cacheLimit, httpStats } from "../wasi.js";

const wasmPath = process.env.TERRIO_WASM;
const module = wasmPath ? new WebAssembly.Module(readFileSync(wasmPath)) : null;
const fixture = new URL("../../crates/terrio/tests/data/user_crs/robin.tif", import.meta.url);

/** Runs `terrio ...args` over `root`, returning the exit status and the output. */
function terrio(root, ...args) {
  const out = { stdout: "", stderr: "" };
  const decoder = new TextDecoder();
  const sink = { stdout: (b) => (out.stdout += decoder.decode(b)), stderr: (b) => (out.stderr += decoder.decode(b)) };
  const wasi = new Wasi(root, ["terrio", ...args], sink);
  const code = wasi.run(new WebAssembly.Instance(module, { wasi_snapshot_preview1: wasi.imports }));
  return { code, ...out };
}

/** Runs `terrio ...args` over `root`, returning the exit status and standard output as bytes. */
function terrioBytes(root, ...args) {
  const chunks = [];
  const wasi = new Wasi(root, ["terrio", ...args], { stdout: (b) => chunks.push(b), stderr: () => {} });
  const code = wasi.run(new WebAssembly.Instance(module, { wasi_snapshot_preview1: wasi.imports }));
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return { code, out };
}

/**
 * Serves `files` (URL -> bytes) as a server would. `mode` is "ranges", "no-ranges", "gzip",
 * "no-head" (HEAD refused with 403, as for a presigned GET URL) or "no-head-exposed" (also exposing
 * `Content-Range` to the page). `log` records each request with the bytes it returned.
 */
function serve(files, mode = "ranges") {
  const log = [];
  globalThis.XMLHttpRequest = class {
    open(method, url) { this.method = method; this.url = url; this.headers = {}; }
    setRequestHeader(name, value) { this.headers[name] = value; }
    getResponseHeader(name) {
      if (name === "Content-Length") return this.length;
      return name === "Content-Range" && (mode === "no-head-exposed" || mode === "wrong-range") ? this.contentRange ?? null : null;
    }
    send() {
      const body = files[this.url];
      log.push({ method: this.method, url: this.url, range: this.headers.Range });
      if (!body) { this.status = 404; this.response = new ArrayBuffer(0); this.length = "0"; return; }
      // A compressed response: its length and ranges count compressed bytes (here, two thirds).
      const size = mode === "gzip" ? Math.floor((body.length * 2) / 3) : body.length;
      this.length = String(size);
      if (this.method === "HEAD") {
        this.status = mode.startsWith("no-head") ? 403 : 200;
        this.response = null;
        return;
      }
      const m = /^bytes=(\d+)-(\d+)$/.exec(this.headers.Range || "");
      if (!m || mode === "no-ranges") {
        this.status = 200;
        this.response = body.slice().buffer;
        log.at(-1).bytes = body.length;
        return;
      }
      // "wrong-range": a read in the middle of the file is answered one byte later than asked.
      const shift = mode === "wrong-range" && Number(m[1]) > 0 && Number(m[2]) < size - 1 ? 1 : 0;
      const start = Number(m[1]) + shift, end = Math.min(size, start + Number(m[2]) + 1 - Number(m[1]));
      if (start >= size) { this.status = 416; this.response = new ArrayBuffer(0); return; }
      this.status = 206;
      // Decoding a piece of a compressed stream does not give back as many bytes as were asked for.
      this.response = body.slice(start, mode === "gzip" ? end - 1 : end).buffer;
      this.contentRange = `bytes ${start}-${end - 1}/${size}`;
      log.at(-1).bytes = this.response.byteLength;
    }
  };
  return log;
}

const big = Uint8Array.from({ length: 3 * 1024 * 1024 + 17 }, (_, i) => (i * 7) & 255);

test("a remote file is read by ranges, fetching each block once", () => {
  const log = serve({ "https://example.com/big.bin": big });
  const file = RemoteFile.probe("https://example.com/big.bin");
  assert.equal(file.size, big.length);
  assert.ok(file.whole === null, "read by ranges");
  const at = 2 * 1024 * 1024 + 5;
  assert.deepEqual(file.read(at, 1000), big.subarray(at, at + 1000));
  const requests = log.length;
  assert.deepEqual(file.read(at + 10, 10), big.subarray(at + 10, at + 20));
  assert.equal(log.length, requests, "a cached block is not fetched again");
  // Across blocks, and past the end.
  assert.deepEqual(file.read(500 * 1024, 700 * 1024), big.subarray(500 * 1024, 1200 * 1024));
  assert.deepEqual(file.read(big.length - 5, 100), big.subarray(big.length - 5));
});

test("the block cache keeps the blocks read most recently, within its limit", () => {
  const log = serve({ "https://example.com/big.bin": big });
  const saved = cacheLimit.bytes;
  cacheLimit.bytes = 2 * 512 * 1024;
  try {
    const file = RemoteFile.probe("https://example.com/big.bin");
    const at = (k) => k * 512 * 1024 + 7;
    for (const k of [1, 2, 3]) assert.deepEqual(file.read(at(k), 5), big.subarray(at(k), at(k) + 5));
    assert.ok(file.blocks.size <= 2, `${file.blocks.size} blocks kept`);
    assert.ok(file.blocks.has(3) && !file.blocks.has(1), "the least recently read go first");
    const requests = log.length;
    assert.deepEqual(file.read(at(1), 5), big.subarray(at(1), at(1) + 5));
    assert.equal(log.length, requests + 1, "a dropped block is fetched again");
  } finally {
    cacheLimit.bytes = saved;
  }
});

test("a server that ignores ranges, or compresses, gives the whole file", () => {
  for (const mode of ["no-ranges", "gzip"]) {
    const log = serve({ "https://example.com/big.bin": big }, mode);
    const file = RemoteFile.probe("https://example.com/big.bin");
    assert.equal(file.size, big.length, mode);
    assert.ok(file.whole, mode);
    assert.deepEqual(file.read(10, 20), big.subarray(10, 30), mode);
    const fetched = log.reduce((n, r) => n + (r.bytes || 0), 0);
    // Ignoring ranges, the first answer is the file: it is not fetched again.
    if (mode === "no-ranges") assert.equal(fetched, big.length, "the file is fetched once");
    else assert.ok(fetched < 2 * big.length, `${fetched} bytes fetched`);
  }
});

test("without HEAD, a file is still read by ranges when its size can be read", () => {
  // A presigned GET URL: HEAD is refused. With `Content-Range` exposed, the size comes from it.
  let log = serve({ "https://example.com/big.bin": big }, "no-head-exposed");
  let file = RemoteFile.probe("https://example.com/big.bin");
  assert.equal(file.size, big.length);
  assert.ok(file.whole === null, "read by ranges");
  assert.deepEqual(file.read(big.length - 3, 3), big.subarray(big.length - 3));
  assert.ok(log.every((r) => r.range || r.method === "HEAD"), "no request for the whole file");
  // Without it, the end is found with one-byte ranges, and the file is still read by ranges.
  log = serve({ "https://example.com/big.bin": big }, "no-head");
  file = RemoteFile.probe("https://example.com/big.bin");
  assert.equal(file.size, big.length);
  assert.ok(file.whole === null, "read by ranges");
  assert.deepEqual(file.read(big.length - 3, 3), big.subarray(big.length - 3));
  assert.ok(log.every((r) => r.range || r.method === "HEAD"), "no request for the whole file");
  // About log2 of the size in one-byte requests.
  assert.ok(log.length < 2 * Math.log2(big.length), `${log.length} requests`);
});

test("a range other than the one asked for is refused, not cached", () => {
  serve({ "https://example.com/big.bin": big }, "wrong-range");
  const file = RemoteFile.probe("https://example.com/big.bin");
  assert.ok(file.whole === null, "read by ranges");
  assert.throws(() => file.read(2 * 1024 * 1024, 10), /asked for bytes/);
});

test("a name without an extension is a file when there is one, else a folder", () => {
  serve({ "https://example.com/api/download": big });
  const dir = new RemoteDir("https://", []);
  const api = dir.lookup("example.com", false).lookup("api", false);
  assert.ok(api.lookup("download", true) instanceof RemoteFile);
  assert.ok(api.lookup("folder", true) instanceof RemoteDir);
});

test("every change to a file is counted, also within one clock tick", () => {
  const file = new MemFile();
  const before = file.version;
  file.write(0, Uint8Array.of(1));
  file.write(0, Uint8Array.of(2));
  file.truncate(1);
  assert.equal(file.version, before + 3);
});

test("a write past the end of a truncated file leaves zeros, not the old bytes", () => {
  const file = new MemFile();
  file.write(0, new Uint8Array(100).fill(7));
  file.truncate(0);
  file.write(50, Uint8Array.of(1, 2));
  assert.equal(file.size, 52);
  assert.deepEqual(file.read(0, 50), new Uint8Array(50));
  assert.deepEqual(file.read(50, 2), Uint8Array.of(1, 2));
  file.truncate(10);
  file.truncate(60);
  assert.deepEqual(file.read(10, 50), new Uint8Array(50));
});

test("a missing remote file is not found; names without an extension are folders", () => {
  const log = serve({});
  const errors = [];
  const dir = new RemoteDir("https://", errors);
  const host = dir.lookup("example.com", false);
  assert.ok(host instanceof RemoteDir);
  assert.equal(host.lookup("missing.tif", true), null);
  assert.ok(host.lookup("folder", true) instanceof RemoteDir);
  // A host is a folder even as the last name of a path (a command looking at its input's parents).
  assert.ok(dir.lookup("files.example.com", true) instanceof RemoteDir);
  // A name probed as a file and not found is still a folder when a path goes through it.
  assert.equal(host.lookup("v1.0", true), null);
  assert.ok(host.lookup("v1.0", false) instanceof RemoteDir);
  assert.equal(log.filter((r) => r.url === "https://files.example.com").length, 0, "a host is not fetched");
  assert.deepEqual(errors, []);
});

test("commands read and write the in-memory workspace", { skip: !module && "set TERRIO_WASM" }, () => {
  const root = new MemDir();
  root.entries.set("robin.tif", new MemFile(new Uint8Array(readFileSync(fixture))));
  const info = terrio(root, "info", "robin.tif", "--json");
  assert.equal(info.code, 0, info.stderr);
  assert.ok(JSON.parse(info.stdout).size);

  const warp = terrio(root, "warp", "robin.tif", "out/robin-4326.tif", "--t-srs", "EPSG:4326");
  assert.notEqual(warp.code, 0, "a missing folder is refused");
  root.entries.set("out", new MemDir());
  const warped = terrio(root, "warp", "robin.tif", "out/robin-4326.tif", "--t-srs", "EPSG:4326");
  assert.equal(warped.code, 0, warped.stderr);
  assert.ok(root.entries.get("out").entries.get("robin-4326.tif").size > 0);

  const geojson = JSON.stringify({ type: "FeatureCollection", features: [{ type: "Feature", properties: { name: "a" }, geometry: { type: "Point", coordinates: [1, 2] } }] });
  root.entries.set("points.geojson", new MemFile(new TextEncoder().encode(geojson)));
  const convert = terrio(root, "convert", "points.geojson", "points.gpkg");
  assert.equal(convert.code, 0, convert.stderr);
  const check = terrio(root, "check", "points.gpkg");
  assert.equal(check.code, 0, check.stderr);
  assert.match(check.stdout, /1 features/);
});

test("a Parquet file in the workspace is read", { skip: !module && "set TERRIO_WASM" }, () => {
  // The `parquet` crate reads a file through `File::try_clone`, which WASI does not have.
  const parquet = new URL("../../crates/terrio/tests/data/parquet_zstd/snappy.parquet", import.meta.url);
  const root = new MemDir();
  root.entries.set("snappy.parquet", new MemFile(new Uint8Array(readFileSync(parquet))));
  const info = terrio(root, "info", "snappy.parquet");
  assert.equal(info.code, 0, info.stderr);
  assert.match(info.stdout, /Features: 500/);
  const convert = terrio(root, "convert", "snappy.parquet", "snappy.geojson");
  assert.equal(convert.code, 0, convert.stderr);
  const file = root.entries.get("snappy.geojson");
  const out = JSON.parse(new TextDecoder().decode(file.data.subarray(0, file.size)));
  assert.equal(out.features.length, 500);
});

test("commands read remote files lazily under /https/", { skip: !module && "set TERRIO_WASM" }, () => {
  const tif = new Uint8Array(readFileSync(fixture));
  const log = serve({ "https://example.com/data/robin.tif": tif });
  const root = new MemDir();
  root.entries.set("https", new RemoteDir("https://", []));
  const before = httpStats.requests;
  const info = terrio(root, "info", "/https/example.com/data/robin.tif");
  assert.equal(info.code, 0, info.stderr);
  assert.match(info.stdout, /Driver: GTiff/);
  assert.ok(httpStats.requests > before);
  // Sidecars are looked up on the server, once each.
  const sidecars = log.filter((r) => r.url.endsWith(".aux.xml"));
  assert.ok(sidecars.length <= 2, JSON.stringify(sidecars));
});

test("binary output on standard output is nothing but the PNGs", { skip: !module && "set TERRIO_WASM" }, () => {
  // `quiet` cannot silence standard output on WASI: nothing else may be written there.
  const root = new MemDir();
  root.entries.set("robin.tif", new MemFile(new Uint8Array(readFileSync(fixture))));
  const png = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  const iend = (bytes) => new TextDecoder("latin1").decode(bytes.subarray(bytes.length - 8, bytes.length - 4)) === "IEND";

  const preview = terrioBytes(root, "preview", "robin.tif", "-o", "-");
  assert.equal(preview.code, 0);
  assert.deepEqual([...preview.out.subarray(0, 8)], png);
  assert.ok(iend(preview.out), "one PNG, ending the output");

  const tiles = terrioBytes(root, "view", "--tiles", "0,0,2,2,8,8;0,0,1,1,4,4", "robin.tif");
  assert.equal(tiles.code, 0);
  const view = new DataView(tiles.out.buffer);
  let at = 0, count = 0;
  while (at < tiles.out.length) {
    const length = view.getUint32(at, true);
    const tile = tiles.out.subarray(at + 4, at + 4 + length);
    assert.deepEqual([...tile.subarray(0, 8)], png);
    assert.ok(iend(tile));
    at += 4 + length;
    count++;
  }
  assert.equal(at, tiles.out.length, "the tiles fill the output exactly");
  assert.equal(count, 2);
});

test("local vsi operations work in the workspace", { skip: !module && "set TERRIO_WASM" }, () => {
  // WASI has files: copies into folders, syncs, moves and recursive deletes all reach them.
  const root = new MemDir();
  root.entries.set("a.txt", new MemFile(new TextEncoder().encode("a")));
  const dir = new MemDir();
  dir.entries.set("x.txt", new MemFile(new TextEncoder().encode("x")));
  root.entries.set("dir", dir);
  root.entries.set("dest", new MemDir());
  const names = (d) => [...d.entries.keys()].sort();
  for (const args of [["vsi", "copy", "a.txt", "dest"], ["vsi", "copy", "-r", "dir", "dest"], ["vsi", "sync", "dir", "synced"], ["vsi", "move", "dir", "moved"]]) {
    const r = terrio(root, ...args);
    assert.equal(r.code, 0, `${args.join(" ")}: ${r.stderr}`);
  }
  assert.deepEqual(names(root.entries.get("dest")), ["a.txt", "dir"]);
  assert.deepEqual(names(root.entries.get("synced").entries.get("dir")), ["x.txt"]);
  assert.deepEqual(names(root.entries.get("moved")), ["x.txt"]);
  assert.ok(!root.entries.has("dir"), "moved away");
  const deleted = terrio(root, "vsi", "delete", "-r", "dest");
  assert.equal(deleted.code, 0, deleted.stderr);
  assert.ok(!root.entries.has("dest"));
});

test("appending to a vector tile archive keeps its other layers", { skip: !module && "set TERRIO_WASM" }, () => {
  const root = new MemDir();
  const points = (name, x) => new TextEncoder().encode(JSON.stringify({ type: "FeatureCollection", features: [{ type: "Feature", properties: { name }, geometry: { type: "Point", coordinates: [x, 10] } }] }));
  root.entries.set("roads.geojson", new MemFile(points("r", 1)));
  root.entries.set("parks.geojson", new MemFile(points("p", 2)));
  assert.equal(terrio(root, "convert", "roads.geojson", "t.pmtiles").code, 0);
  const appended = terrio(root, "convert", "parks.geojson", "t.pmtiles", "--append");
  assert.equal(appended.code, 0, appended.stderr);
  const info = terrio(root, "info", "t.pmtiles");
  assert.match(info.stdout, /Layer: roads/);
  assert.match(info.stdout, /Layer: parks/);
});

test("sidecars of a URL with a query are not asked for", () => {
  const url = "https://example.com/a.tif?X-Amz-Signature=abc";
  const log = serve({ [url]: big });
  const errors = [];
  const host = new RemoteDir("https://", errors).lookup("example.com", false);
  assert.ok(host.lookup("a.tif?X-Amz-Signature=abc", true) instanceof RemoteFile);
  const requests = log.length;
  assert.equal(host.lookup("a.tif?X-Amz-Signature=abc.aux.xml", true), null);
  assert.equal(log.length, requests, "no request for the sidecar");
  assert.deepEqual(errors, []);
});

test("a different URL with a query is still looked up", () => {
  const log = serve({ "https://example.com/a.tif": big, "https://example.com/a.tif?version=2": big });
  const host = new RemoteDir("https://", []).lookup("example.com", false);
  assert.ok(host.lookup("a.tif", true) instanceof RemoteFile);
  assert.ok(host.lookup("a.tif?version=2", true) instanceof RemoteFile);
  assert.ok(log.some((r) => r.url.endsWith("?version=2")));
});

test("the remote mount cannot be renamed away or replaced", { skip: !module && "set TERRIO_WASM" }, () => {
  const root = new MemDir();
  root.entries.set("https", new RemoteDir("https://", []));
  root.entries.set("dir", new MemDir());
  assert.notEqual(terrio(root, "vsi", "move", "dir", "https").code, 0);
  assert.notEqual(terrio(root, "vsi", "move", "https", "elsewhere").code, 0);
  assert.ok(root.entries.get("https") instanceof RemoteDir);
});

test("a path through a known remote file keeps the file", () => {
  serve({ "https://example.com/a.tif": big });
  const host = new RemoteDir("https://", []).lookup("example.com", false);
  const file = host.lookup("a.tif", true);
  assert.ok(file instanceof RemoteFile);
  assert.ok(host.lookup("a.tif", false) === file, "not replaced by a folder");
  assert.ok(host.lookup("a.tif", true) === file);
});

test("a folder is not renamed over a file or into itself", () => {
  const root = new MemDir();
  const dir = new MemDir();
  dir.entries.set("b", new MemDir());
  root.entries.set("A", dir);
  root.entries.set("f.txt", new MemFile(new Uint8Array(1)));
  const wasi = new Wasi(root, ["terrio"], { stdout: () => {}, stderr: () => {} });
  wasi.memory = new WebAssembly.Memory({ initial: 1 });
  const rename = (from, to) => {
    const enc = new TextEncoder();
    const a = enc.encode(from), b = enc.encode(to);
    wasi.bytes().set(a, 0);
    wasi.bytes().set(b, 512);
    return wasi.imports.path_rename(3, 0, a.length, 3, 512, b.length);
  };
  assert.notEqual(rename("A", "f.txt"), 0, "a folder over a file");
  assert.notEqual(rename("A", "A/b/c"), 0, "a folder into itself");
  assert.ok(root.entries.get("A") === dir);
  assert.ok(root.entries.get("f.txt") instanceof MemFile);
  assert.equal(rename("A", "B"), 0);
  assert.ok(root.entries.get("B") === dir);
});
