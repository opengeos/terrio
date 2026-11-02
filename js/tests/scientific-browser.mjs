// Coordinator: node js/tests/scientific-browser.mjs [web-module-path] [wasm-path]
// Defaults match wasm-pack --target web --out-dir pkg --out-name terrio.
// PLAYWRIGHT_MODULE and BROWSERS follow browser.mjs conventions.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { resolve, extname, relative } from "node:path";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../", import.meta.url));
const modulePath = resolve(process.argv[2] ?? resolve(root, "pkg/terrio.js"));
const wasmPath = resolve(process.argv[3] ?? modulePath.replace(/\.js$/, "_bg.wasm"));
// Fail immediately for missing build artifacts; no implicit build or skip.
readFileSync(modulePath); readFileSync(wasmPath);
const coldSamples = Number(process.env.SCIENTIFIC_COLD_SAMPLES ?? 5);
const warmSamples = Number(process.env.SCIENTIFIC_WARM_SAMPLES ?? 20);
for (const n of [coldSamples, warmSamples]) {
  if (!Number.isSafeInteger(n) || n < 2 || n > 1000) throw new Error("sample counts must be integers in [2, 1000]");
}
const require = createRequire(resolve(process.cwd(), "package.json"));
const playwright = require(process.env.PLAYWRIGHT_MODULE ?? "playwright");
const server = createServer((req, res) => {
  const url = new URL(req.url, "http://localhost");
  if (url.pathname === "/") { res.setHeader("Content-Type", "text/html"); res.end("<!doctype html><title>Scientific conformance</title>"); return; }
  let path = resolve(root, "." + decodeURIComponent(url.pathname));
  if (url.pathname === "/module.js") path = modulePath;
  else if (url.pathname === "/module_bg.wasm") path = wasmPath;
  else if (relative(root, path).startsWith("..")) { res.writeHead(403); res.end(); return; }
  try {
    const bytes = readFileSync(path);
    res.setHeader("Content-Type", { ".mjs": "text/javascript", ".js": "text/javascript", ".wasm": "application/wasm" }[extname(path)] ?? "application/octet-stream");
    res.end(bytes);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
try {
  for (const name of (process.env.BROWSERS ?? "chromium").split(",")) {
    const browser = await playwright[name].launch();
    try {
      const page = await browser.newPage();
      await page.goto(`http://127.0.0.1:${server.address().port}/`);
      const result = await page.evaluate(async ({ coldSamples, warmSamples }) => {
        const { conformance } = await import("/tests/scientific-conformance.mjs");
        const initStart = performance.now();
        const terrio = await import("/module.js");
        const wasm = await terrio.default({ module_or_path: "/module_bg.wasm" });
        const initializationMs = performance.now() - initStart;
        const bytes = new Uint8Array(await (await fetch("/tests/scientific-data/packed.nc")).arrayBuffer());
        const direct = conformance(terrio, bytes, wasm.memory);
        const expected = [...direct.copied];
        const transfer = structuredClone(direct.copied, { transfer: [direct.copied.buffer] });
        if (direct.copied.byteLength !== 0 || transfer.length !== 24) throw new Error("main transfer ownership");
        const workerRun = async (url, example = false) => {
          const worker = new Worker(url, { type: "module" });
          try {
            for (let id = 0; id < 3; id++) {
              const input = bytes.slice();
              const messages = await new Promise((resolve, reject) => {
                const received = [];
                const timer = setTimeout(() => reject(new Error("worker timeout")), 30000);
                worker.onerror = (e) => { clearTimeout(timer); reject(new Error(e.message)); };
                worker.onmessage = ({ data }) => {
                  if (data.id !== id || data.error) { clearTimeout(timer); reject(new Error(data.error ?? "wrong response id")); return; }
                  received.push(data);
                  if (example || data.detached) { clearTimeout(timer); resolve(received); }
                };
                worker.postMessage({ id, moduleUrl: new URL("/module.js", location.href).href, wasmUrl: new URL("/module_bg.wasm", location.href).href, bytes: input }, [input.buffer]);
                if (input.byteLength !== 0) { clearTimeout(timer); reject(new Error("input not detached")); }
              });
              const values = example ? messages[0].values : messages[0].result.copied;
              const want = example ? expected.slice(12) : expected;
              if (!(values instanceof Int16Array) || values.length !== want.length || values.some((v, i) => v !== want[i])) throw new Error("worker native values");
              // Copies remain valid after worker-side handle disposal, next reads and termination.
              worker.retained = values;
            }
            return worker.retained;
          } finally { worker.terminate(); }
        };
        const copied = await workerRun("/tests/scientific-worker.mjs");
        await workerRun("/examples/scientific-worker.mjs", true);
        if (copied.some((v, i) => v !== expected[i])) throw new Error("copy after termination");
        const lifecycle = [];
        for (let i = 0; i < coldSamples; i++) {
          const worker = new Worker("/tests/scientific-lifecycle.mjs", { type: "module" });
          try {
            lifecycle.push(await new Promise((resolve, reject) => {
              const timer = setTimeout(() => reject(new Error("lifecycle timeout")), 60000);
              worker.onerror = (e) => { clearTimeout(timer); reject(new Error(e.message)); };
              worker.onmessage = ({ data }) => {
                clearTimeout(timer);
                if (data.error) reject(new Error(data.error)); else resolve(data);
              };
              worker.postMessage({ moduleUrl: new URL("/module.js", location.href).href,
                wasmUrl: new URL("/module_bg.wasm", location.href).href, bytes, runs: warmSamples + 1 });
            }));
          } finally { worker.terminate(); }
        }
        const summary = (values) => {
          const sorted = [...values].sort((a, b) => a - b);
          const n = sorted.length;
          return { count: n, medianMs: (sorted[Math.floor((n - 1) / 2)] + sorted[Math.floor(n / 2)]) / 2,
            p95Ms: sorted[Math.ceil(n * 0.95) - 1], maxMs: sorted[n - 1] };
        };
        const benchmark = { initialization: summary(lifecycle.map((r) => r.initializationMs)),
          warmInitialization: summary(lifecycle.flatMap((r) => r.warmInitializationMs)),
          firstDecode: summary(lifecycle.map((r) => r.samples[0])),
          warmDecode: summary(lifecycle.flatMap((r) => r.samples.slice(1))), raw: lifecycle };
        return { isolated: crossOriginIsolated, initializationMs, fixtureBytes: bytes.length, elapsedMs: direct.elapsedMs, growth: direct.growth, workerCycles: 6, benchmark };
      }, { coldSamples, warmSamples });
      assert.equal(result.isolated, false);
      assert.equal(result.growth, true);
      console.log(name, JSON.stringify(result));
    } finally { await browser.close(); }
  }
} finally { await new Promise((r) => server.close(r)); }
