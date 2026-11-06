// SPDX-License-Identifier: MIT
// Runs the terrio command line (terrio.wasm, built for wasm32-wasip1) in a Web Worker, over an
// in-memory workspace. Messages from the page:
//   { type: "init" }                         -> { type: "ready", catalog }
//   { type: "put", path, buffer }            -> { type: "put", path }
//   { type: "remove", path }                 -> { type: "removed", path }
//   { type: "run", args, binaryStdout }      -> { type: "out", stream, text }*, then
//                                               { type: "done", code, changed: [{path, buffer}], removed: [path], http, stdout }
// With `binaryStdout`, standard output is not streamed as text but returned whole as `stdout`
// (an ArrayBuffer): `terrio preview -o -` writes PNG bytes there.
// Errors come back as { type: "error", message }.

import { Wasi, MemDir, MemFile, RemoteDir, httpStats } from "./wasi.js";

let module = null;
const root = new MemDir();
const remoteErrors = [];

/** How long a remote name found missing (a sidecar file, mostly) is not looked up again. */
const MISS_TTL = 60 * 1000;

/**
 * The remote directories. They are kept from one run to the next, with the blocks fetched, so that
 * the map's batches of tiles, each a run, do not fetch a file again; a name not found, or that could
 * not be read, is looked up again after `MISS_TTL`.
 */
function mountRemote() {
  remoteErrors.length = 0;
  if (!root.entries.has("https")) root.entries.set("https", new RemoteDir("https://", remoteErrors));
  forgetMissing(root.entries.get("https"), Date.now());
}

function forgetMissing(dir, now) {
  for (const [name, node] of dir.entries) {
    if (node === null && now - (dir.missedAt.get(name) ?? 0) > MISS_TTL) {
      dir.entries.delete(name);
      dir.missedAt.delete(name);
      dir.missedWhy.delete(name);
    } else if (node instanceof RemoteDir) forgetMissing(node, now);
  }
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
function run(args, onOutput, stdoutBytes = null) {
  mountRemote();
  const decoders = { stdout: new TextDecoder(), stderr: new TextDecoder() };
  const out = {};
  for (const stream of ["stdout", "stderr"]) {
    out[stream] = (bytes) => onOutput(stream, decoders[stream].decode(bytes, { stream: true }));
  }
  if (stdoutBytes) out.stdout = (bytes) => stdoutBytes.push(bytes);
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
      // `/https` is where remote files are read.
      if (data.path.split("/").find(Boolean) === "https") throw new Error(`a workspace file or folder cannot be named "https": rename ${data.path}`);
      const { dir, name } = parentOf(data.path, true);
      if (dir.entries.get(name) instanceof MemDir) throw new Error(`${data.path} is a folder in the workspace: remove it first, or rename the file`);
      dir.entries.set(name, new MemFile(new Uint8Array(data.buffer)));
      self.postMessage({ type: "put", path: data.path });
    } else if (data.type === "remove") {
      const at = parentOf(data.path, false);
      if (at) at.dir.entries.delete(at.name);
      self.postMessage({ type: "removed", path: data.path });
    } else if (data.type === "run") {
      const before = new Map([...walk()].map(([path, node]) => [path, { node, version: node.version }]));
      const http = { requests: httpStats.requests, bytes: httpStats.bytes };
      const chunks = data.binaryStdout ? [] : null;
      const code = run(data.args, (stream, text) => text && self.postMessage({ type: "out", stream, text }), chunks);
      const after = walk();
      const changed = [], transfer = [];
      for (const [path, node] of after) {
        const old = before.get(path);
        if (!old || old.node !== node || old.version !== node.version) {
          const buffer = node.bytes().buffer;
          changed.push({ path, buffer });
          transfer.push(buffer);
        }
      }
      const removed = [...before.keys()].filter((path) => !after.has(path));
      const stdout = chunks ? await new Blob(chunks).arrayBuffer() : null;
      if (stdout) transfer.push(stdout);
      self.postMessage({
        type: "done", code, changed, removed, stdout,
        http: { requests: httpStats.requests - http.requests, bytes: httpStats.bytes - http.bytes },
      }, transfer);
    }
  } catch (error) {
    self.postMessage({ type: "error", message: error.message });
  }
};
