// SPDX-License-Identifier: MIT
// Runs the terrio command line (terrio.wasm, built for wasm32-wasip1) in a Web Worker, over an
// in-memory workspace. Messages from the page:
//   { type: "init" }                         -> { type: "ready", catalog }
//   { type: "put", path, buffer }            -> { type: "put", path }
//   { type: "remove", path }                 -> { type: "removed", path }
//   { type: "run", args }                    -> { type: "out", stream, text }*, then
//                                               { type: "done", code, changed: [{path, buffer}], removed: [path], http }
// Errors come back as { type: "error", message }.

import { Wasi, MemDir, MemFile, RemoteDir, httpStats } from "./wasi.js";

let module = null;
const root = new MemDir();
const remoteErrors = [];

/** The remote directories, made again before each run so that every run sees current files. */
function mountRemote() {
  remoteErrors.length = 0;
  root.entries.set("https", new RemoteDir("https://", remoteErrors));
}

/** The workspace's files, by path, with their nodes. */
function walk(dir = root, prefix = "", out = new Map()) {
  for (const [name, node] of dir.entries) {
    if (dir === root && name === "https") continue;
    const path = prefix + name;
    if (node instanceof MemDir) walk(node, `${path}/`, out);
    else if (node instanceof MemFile) out.set(path, node);
  }
  return out;
}

function parentOf(path, create) {
  const parts = path.split("/").filter(Boolean);
  const name = parts.pop();
  let dir = root;
  for (const part of parts) {
    let next = dir.entries.get(part);
    if (!next) {
      if (!create) return null;
      next = new MemDir();
      dir.entries.set(part, next);
    }
    if (!(next instanceof MemDir)) throw new Error(`${part} is not a folder`);
    dir = next;
  }
  return { dir, name };
}

/** Runs the command line `args` (without the program name), streaming its output. */
function run(args, onOutput) {
  mountRemote();
  const decoders = { stdout: new TextDecoder(), stderr: new TextDecoder() };
  const out = {};
  for (const stream of ["stdout", "stderr"]) {
    out[stream] = (bytes) => onOutput(stream, decoders[stream].decode(bytes, { stream: true }));
  }
  const wasi = new Wasi(root, ["terrio", ...args], out, { HOME: "/", PWD: "/", TERM: "dumb", NO_COLOR: "1" });
  const instance = new WebAssembly.Instance(module, { wasi_snapshot_preview1: wasi.imports });
  let code;
  try {
    code = wasi.run(instance);
  } catch (error) {
    // A trap: a panic (Rust aborts on WebAssembly), running out of memory or of stack.
    onOutput("stderr", `\nterrio web: the command stopped: ${error.message}\n`);
    code = 101;
  }
  for (const stream of ["stdout", "stderr"]) onOutput(stream, decoders[stream].decode());
  for (const message of new Set(remoteErrors)) onOutput("stderr", `terrio web: ${message}\n`);
  return code;
}

self.onmessage = async ({ data }) => {
  try {
    if (data.type === "init") {
      module = await WebAssembly.compileStreaming(fetch(new URL("terrio.wasm", import.meta.url)));
      let json = "";
      const code = run(["--json-usage"], (stream, text) => { if (stream === "stdout") json += text; });
      if (code !== 0) throw new Error("terrio --json-usage failed");
      self.postMessage({ type: "ready", catalog: JSON.parse(json) });
    } else if (data.type === "put") {
      const { dir, name } = parentOf(data.path, true);
      dir.entries.set(name, new MemFile(new Uint8Array(data.buffer)));
      self.postMessage({ type: "put", path: data.path });
    } else if (data.type === "remove") {
      const at = parentOf(data.path, false);
      if (at) at.dir.entries.delete(at.name);
      self.postMessage({ type: "removed", path: data.path });
    } else if (data.type === "run") {
      const before = new Map([...walk()].map(([path, node]) => [path, { node, mtime: node.mtime, size: node.size }]));
      const http = { requests: httpStats.requests, bytes: httpStats.bytes };
      const code = run(data.args, (stream, text) => text && self.postMessage({ type: "out", stream, text }));
      const after = walk();
      const changed = [], transfer = [];
      for (const [path, node] of after) {
        const old = before.get(path);
        if (!old || old.node !== node || old.mtime !== node.mtime || old.size !== node.size) {
          const buffer = node.bytes().buffer;
          changed.push({ path, buffer });
          transfer.push(buffer);
        }
      }
      const removed = [...before.keys()].filter((path) => !after.has(path));
      self.postMessage({
        type: "done", code, changed, removed,
        http: { requests: httpStats.requests - http.requests, bytes: httpStats.bytes - http.bytes },
      }, transfer);
    }
  } catch (error) {
    self.postMessage({ type: "error", message: error.message });
  }
};
