// SPDX-License-Identifier: MIT
// The WASI preview 1 system interface the browser build of the terrio command line runs on: an
// in-memory filesystem, plus remote files under /https/ read lazily with HTTP range requests.
//
// It runs in a Web Worker, where a synchronous XMLHttpRequest is allowed: the command line's reads
// are blocking calls, so each read that needs bytes not fetched yet fetches them before returning.

const E = {
  SUCCESS: 0, ACCES: 2, BADF: 8, BUSY: 10, EXIST: 20, INVAL: 28, IO: 29, ISDIR: 31, NOENT: 44, NOSYS: 52,
  NOTDIR: 54, NOTEMPTY: 55, NOTSUP: 58, PERM: 63, ROFS: 69, SPIPE: 70,
};
const FILETYPE = { CHAR: 2, DIRECTORY: 3, REGULAR: 4 };
const OFLAGS = { CREAT: 1, DIRECTORY: 2, EXCL: 4, TRUNC: 8 };
const FDFLAGS_APPEND = 1;
const ALL_RIGHTS = 0x1fffffffn;

/** Thrown by `proc_exit` to unwind the program. */
export class Exit extends Error {
  constructor(code) {
    super(`exit ${code}`);
    this.code = code;
  }
}

let nextIno = 1n;
const now = () => BigInt(Date.now()) * 1000000n;

/** A file held in memory. */
export class MemFile {
  constructor(data = new Uint8Array(0)) {
    this.data = data;
    this.size = data.length;
    this.ino = nextIno++;
    this.mtime = now();
  }
  get readonly() { return false; }
  read(offset, length) {
    const end = Math.min(this.size, offset + length);
    return offset >= end ? new Uint8Array(0) : this.data.subarray(offset, end);
  }
  /**
   * Makes room for `end` bytes: twice the room it had, so that a file written in pieces is copied a
   * few times, not once per piece; just `end` when twice is more than the tab can allocate.
   */
  grow(end) {
    let grown;
    try {
      grown = new Uint8Array(Math.max(end, this.data.length * 2));
    } catch {
      grown = new Uint8Array(end);
    }
    grown.set(this.data.subarray(0, this.size));
    this.data = grown;
  }
  write(offset, bytes) {
    const end = offset + bytes.length;
    if (end > this.data.length) this.grow(end);
    // A write past the end leaves zeros in the gap, also where the file held bytes before it was
    // truncated.
    if (offset > this.size) this.data.fill(0, this.size, offset);
    this.data.set(bytes, offset);
    this.size = Math.max(this.size, end);
    this.mtime = now();
  }
  truncate(size) {
    if (size > this.data.length) this.grow(size);
    if (size > this.size) this.data.fill(0, this.size, size);
    this.size = size;
    this.mtime = now();
  }
  /** The contents, as a copy. */
  bytes() { return this.data.slice(0, this.size); }
}

/** A directory held in memory. */
export class MemDir {
  constructor() {
    this.entries = new Map();
    this.ino = nextIno++;
    this.mtime = now();
  }
  get readonly() { return false; }
  lookup(name) { return this.entries.get(name) ?? null; }
}

/** The HTTP requests and bytes fetched for remote files, for the page to report. */
export const httpStats = { requests: 0, bytes: 0 };

/** A synchronous GET of `url`, of a byte range `[start, end)` when given. */
function get(url, start, end) {
  const xhr = new XMLHttpRequest();
  xhr.open("GET", url, false);
  xhr.responseType = "arraybuffer";
  if (start !== undefined) xhr.setRequestHeader("Range", `bytes=${start}-${end - 1}`);
  xhr.send();
  httpStats.requests++;
  const body = new Uint8Array(xhr.response || new ArrayBuffer(0));
  httpStats.bytes += body.length;
  return { status: xhr.status, body, range: xhr.getResponseHeader("Content-Range"), length: xhr.getResponseHeader("Content-Length") };
}

/** A synchronous HEAD of `url`. */
function head(url) {
  const xhr = new XMLHttpRequest();
  xhr.open("HEAD", url, false);
  xhr.send();
  httpStats.requests++;
  return { status: xhr.status, length: xhr.getResponseHeader("Content-Length") };
}

const BLOCK = 512 * 1024;
/** Remote files smaller than this are fetched whole. */
const WHOLE_BELOW = 2 * BLOCK;

/**
 * A remote file read lazily by blocks of `BLOCK` bytes with range requests, or held whole when the
 * server cannot serve ranges reliably: it ignores `Range`, its responses are compressed (whose
 * ranges count compressed bytes), or the file is small.
 */
export class RemoteFile {
  constructor(url, size, whole) {
    this.url = url;
    this.size = size;
    this.whole = whole;
    this.blocks = new Map();
    this.ino = nextIno++;
    this.mtime = now();
  }
  get readonly() { return true; }

  /**
   * Opens `url`, or returns `null` when there is nothing there. Throws when it cannot be read.
   *
   * The size comes from `Content-Range`, when the server lets the page read it, else from `HEAD`.
   * A `HEAD` that fails is not the end: a presigned URL is signed for GET only, and some servers
   * refuse `HEAD`. Without a size, the file is read whole.
   */
  static probe(url) {
    let h = null;
    try {
      h = head(url);
    } catch {
      // Tried again with GET below.
    }
    if (h && (h.status === 404 || h.status === 410)) return null;
    const headSize = h && h.status >= 200 && h.status < 300 && h.length !== null ? Number(h.length) : NaN;
    if (Number.isFinite(headSize) && headSize < WHOLE_BELOW) return RemoteFile.whole(url);
    let first;
    try {
      first = get(url, 0, BLOCK);
    } catch {
      throw new Error(`could not reach ${url} (a network error, or the server does not allow cross-origin requests)`);
    }
    if (first.status === 404 || first.status === 410) return null;
    // The server ignored `Range` and sent the whole file.
    if (first.status === 200) return new RemoteFile(url, first.body.length, first.body);
    // A range of an empty file.
    if (first.status === 416) return new RemoteFile(url, 0, new Uint8Array(0));
    if (first.status !== 206) throw new Error(`${url}: HTTP ${first.status}`);
    const total = /\/(\d+)$/.exec(first.range || "");
    const size = total ? Number(total[1]) : headSize;
    if (!Number.isFinite(size)) return RemoteFile.whole(url);
    // A compressed response holds a different number of bytes than asked for: its ranges count
    // compressed bytes.
    if (first.body.length !== Math.min(BLOCK, size)) return RemoteFile.whole(url);
    if (size > BLOCK) {
      let last = null;
      try {
        last = get(url, size - 1, size);
      } catch {
        // Read whole below.
      }
      if (!last || last.status !== 206 || last.body.length !== 1) return RemoteFile.whole(url);
    }
    const file = new RemoteFile(url, size, null);
    file.blocks.set(0, first.body);
    return file;
  }

  /** `url` read whole, or `null` when there is nothing there. */
  static whole(url) {
    let r;
    try {
      r = get(url);
    } catch {
      throw new Error(`could not read ${url} (a network error, or the server does not allow cross-origin requests)`);
    }
    if (r.status === 404 || r.status === 410) return null;
    if (r.status < 200 || r.status >= 300) throw new Error(`${url}: HTTP ${r.status}`);
    return new RemoteFile(url, r.body.length, r.body);
  }

  read(offset, length) {
    const end = Math.min(this.size, offset + length);
    if (offset >= end) return new Uint8Array(0);
    if (this.whole) return this.whole.subarray(offset, end);
    const first = Math.floor(offset / BLOCK), last = Math.floor((end - 1) / BLOCK);
    // Fetch the missing blocks, contiguous runs of them in one request each.
    for (let b = first; b <= last;) {
      if (this.blocks.has(b)) { b++; continue; }
      let e = b;
      while (e + 1 <= last && !this.blocks.has(e + 1)) e++;
      const start = b * BLOCK, stop = Math.min(this.size, (e + 1) * BLOCK);
      const r = get(this.url, start, stop);
      if (r.status !== 206 || r.body.length !== stop - start) throw new Error(`${this.url}: a range request returned HTTP ${r.status}, ${r.body.length} bytes`);
      for (let k = b; k <= e; k++) this.blocks.set(k, r.body.subarray((k - b) * BLOCK, Math.min(r.body.length, (k - b + 1) * BLOCK)));
      b = e + 1;
    }
    if (first === last) {
      const s = offset - first * BLOCK;
      return this.blocks.get(first).subarray(s, s + (end - offset));
    }
    const out = new Uint8Array(end - offset);
    for (let b = first; b <= last; b++) {
      const block = this.blocks.get(b), base = b * BLOCK;
      const s = Math.max(offset, base) - base, t = Math.min(end, base + block.length) - base;
      out.set(block.subarray(s, t), base + s - offset);
    }
    return out;
  }
}

/**
 * A directory of remote files: `/https/host/path` is `https://host/path`. Names are looked up on
 * the server when opened, and remembered, found or not. It lists only what has been looked up.
 */
export class RemoteDir {
  constructor(prefix, errors) {
    this.prefix = prefix;
    this.errors = errors;
    this.entries = new Map();
    this.ino = nextIno++;
    this.mtime = now();
  }
  get readonly() { return true; }
  lookup(name, last) {
    const known = this.entries.get(name);
    // A name with more path after it is a folder, whatever an earlier lookup of it found.
    if (known !== undefined && (last || known instanceof RemoteDir)) return known;
    let node;
    // Hosts, and names without an extension, are taken for folders: probing them would fetch
    // whatever page the server returns for a folder.
    if (!last || this.prefix === "https://" || !name.includes(".")) {
      node = new RemoteDir(`${this.prefix}${name}/`, this.errors);
    } else {
      try {
        node = RemoteFile.probe(this.prefix + name);
      } catch (error) {
        this.errors.push(error.message);
        node = null;
      }
    }
    this.entries.set(name, node);
    return node;
  }
}

const isDir = (node) => node instanceof MemDir || node instanceof RemoteDir;

/** The filesystem and the WASI imports of one run of a program. */
export class Wasi {
  /**
   * @param {MemDir} root the filesystem, preopened as `/`.
   * @param {string[]} args the command line, the program name first.
   * @param {{stdout: (bytes: Uint8Array) => void, stderr: (bytes: Uint8Array) => void}} out
   */
  constructor(root, args, out, env = {}) {
    this.root = root;
    this.args = args;
    this.env = Object.entries(env).map(([k, v]) => `${k}=${v}`);
    this.out = out;
    this.memory = null;
    this.fds = new Map([
      [0, { kind: "stdin" }],
      [1, { kind: "stdout" }],
      [2, { kind: "stderr" }],
      [3, { kind: "dir", node: root, preopen: "/" }],
    ]);
    this.nextFd = 4;
    this.imports = this.makeImports();
  }

  view() { return new DataView(this.memory.buffer); }
  bytes() { return new Uint8Array(this.memory.buffer); }
  string(ptr, len) { return new TextDecoder().decode(this.bytes().subarray(ptr, ptr + len)); }

  /** Resolves `path` from the directory of `fd`: `{ parent, name, node }`, or an errno. */
  resolve(fd, path, { create = false } = {}) {
    const base = this.fds.get(fd);
    if (!base || base.kind !== "dir") return E.BADF;
    const parts = [];
    for (const part of path.split("/")) {
      if (part === "" || part === ".") continue;
      if (part === "..") {
        if (!parts.length) return E.PERM;
        parts.pop();
      } else parts.push(part);
    }
    // Directories are resolved from the root, so that `..` stays within it.
    const start = base.path ? base.path.split("/").filter(Boolean) : [];
    const full = base.node === this.root ? parts : [...start, ...parts];
    let parent = null, node = this.root, name = "";
    for (let i = 0; i < full.length; i++) {
      if (!isDir(node)) return E.NOTDIR;
      parent = node;
      name = full[i];
      node = node.lookup(name, i === full.length - 1);
      if (node === null && i < full.length - 1) return E.NOENT;
    }
    if (node === null && !create) return E.NOENT;
    return { parent, name, node, path: full.join("/") };
  }

  open(node, extra) {
    const fd = this.nextFd++;
    this.fds.set(fd, { node, pos: 0, ...extra });
    return fd;
  }

  filestat(ptr, node, kind) {
    const v = this.view();
    for (let i = 0; i < 64; i += 8) v.setBigUint64(ptr + i, 0n, true);
    v.setBigUint64(ptr + 8, node ? node.ino : 0n, true);
    v.setUint8(ptr + 16, kind === "char" ? FILETYPE.CHAR : isDir(node) ? FILETYPE.DIRECTORY : FILETYPE.REGULAR);
    v.setBigUint64(ptr + 24, 1n, true);
    v.setBigUint64(ptr + 32, BigInt(node && !isDir(node) ? node.size : 0), true);
    const t = node ? node.mtime : now();
    v.setBigUint64(ptr + 40, t, true);
    v.setBigUint64(ptr + 48, t, true);
    v.setBigUint64(ptr + 56, t, true);
  }

  iovecs(iovs, count) {
    const v = this.view(), out = [];
    for (let i = 0; i < count; i++) out.push([v.getUint32(iovs + i * 8, true), v.getUint32(iovs + i * 8 + 4, true)]);
    return out;
  }

  readInto(entry, iovs, count, offset) {
    let total = 0, pos = offset;
    for (const [ptr, len] of this.iovecs(iovs, count)) {
      const chunk = entry.node.read(pos, len);
      this.bytes().set(chunk, ptr);
      total += chunk.length;
      pos += chunk.length;
      if (chunk.length < len) break;
    }
    return total;
  }

  writeFrom(entry, iovs, count, offset) {
    let total = 0, pos = offset;
    for (const [ptr, len] of this.iovecs(iovs, count)) {
      entry.node.write(pos, this.bytes().slice(ptr, ptr + len));
      total += len;
      pos += len;
    }
    return total;
  }

  makeImports() {
    const self = this;
    const strings = (list, ptrs, buf) => {
      const v = self.view(), enc = new TextEncoder();
      for (const s of list) {
        v.setUint32(ptrs, buf, true);
        ptrs += 4;
        const b = enc.encode(`${s}\0`);
        self.bytes().set(b, buf);
        buf += b.length;
      }
      return E.SUCCESS;
    };
    const sizes = (list, countPtr, sizePtr) => {
      const v = self.view(), enc = new TextEncoder();
      v.setUint32(countPtr, list.length, true);
      v.setUint32(sizePtr, list.reduce((n, s) => n + enc.encode(s).length + 1, 0), true);
      return E.SUCCESS;
    };
    const file = (fd) => {
      const entry = self.fds.get(fd);
      return entry && entry.node && !isDir(entry.node) ? entry : null;
    };
    const guard = (f) => (...a) => {
      try {
        return f(...a);
      } catch (error) {
        if (error instanceof Exit) throw error;
        self.out.stderr(new TextEncoder().encode(`terrio web: ${error.message}\n`));
        return E.IO;
      }
    };
    const wasi = {
      args_get: (ptrs, buf) => strings(self.args, ptrs, buf),
      args_sizes_get: (c, s) => sizes(self.args, c, s),
      environ_get: (ptrs, buf) => strings(self.env, ptrs, buf),
      environ_sizes_get: (c, s) => sizes(self.env, c, s),
      clock_res_get: (id, ptr) => { self.view().setBigUint64(ptr, 1000n, true); return E.SUCCESS; },
      clock_time_get: (id, precision, ptr) => {
        const t = id === 0 ? now() : BigInt(Math.round(performance.now() * 1e6));
        self.view().setBigUint64(ptr, t, true);
        return E.SUCCESS;
      },
      random_get: (ptr, len) => {
        for (let i = 0; i < len; i += 65536) crypto.getRandomValues(self.bytes().subarray(ptr + i, ptr + Math.min(len, i + 65536)));
        return E.SUCCESS;
      },
      proc_exit: (code) => { throw new Exit(code); },
      proc_raise: () => E.NOSYS,
      sched_yield: () => E.SUCCESS,
      poll_oneoff: (inPtr, outPtr, count, neventsPtr) => {
        // Every subscription fires at once: there is nothing to wait for in a single thread.
        const v = self.view();
        for (let i = 0; i < count; i++) {
          const s = inPtr + i * 48, o = outPtr + i * 32;
          v.setBigUint64(o, v.getBigUint64(s, true), true);
          v.setUint16(o + 8, 0, true);
          v.setUint8(o + 10, v.getUint8(s + 8));
        }
        v.setUint32(neventsPtr, count, true);
        return E.SUCCESS;
      },

      fd_write: guard((fd, iovs, count, nwritten) => {
        const entry = self.fds.get(fd);
        if (!entry) return E.BADF;
        let n;
        if (entry.kind === "stdout" || entry.kind === "stderr") {
          n = 0;
          for (const [ptr, len] of self.iovecs(iovs, count)) {
            self.out[entry.kind](self.bytes().slice(ptr, ptr + len));
            n += len;
          }
        } else {
          if (!file(fd)) return E.BADF;
          if (entry.node.readonly) return E.ROFS;
          if (entry.append) entry.pos = entry.node.size;
          n = self.writeFrom(entry, iovs, count, entry.pos);
          entry.pos += n;
        }
        self.view().setUint32(nwritten, n, true);
        return E.SUCCESS;
      }),
      fd_pwrite: guard((fd, iovs, count, offset, nwritten) => {
        const entry = file(fd);
        if (!entry) return E.BADF;
        if (entry.node.readonly) return E.ROFS;
        self.view().setUint32(nwritten, self.writeFrom(entry, iovs, count, Number(offset)), true);
        return E.SUCCESS;
      }),
      fd_read: guard((fd, iovs, count, nread) => {
        const entry = self.fds.get(fd);
        if (entry && entry.kind === "stdin") { self.view().setUint32(nread, 0, true); return E.SUCCESS; }
        if (!file(fd)) return entry ? E.ISDIR : E.BADF;
        const n = self.readInto(entry, iovs, count, entry.pos);
        entry.pos += n;
        self.view().setUint32(nread, n, true);
        return E.SUCCESS;
      }),
      fd_pread: guard((fd, iovs, count, offset, nread) => {
        const entry = file(fd);
        if (!entry) return E.BADF;
        self.view().setUint32(nread, self.readInto(entry, iovs, count, Number(offset)), true);
        return E.SUCCESS;
      }),
      fd_seek: (fd, offset, whence, newPtr) => {
        const entry = file(fd);
        if (!entry) return self.fds.has(fd) ? E.SPIPE : E.BADF;
        const base = whence === 0 ? 0 : whence === 1 ? entry.pos : entry.node.size;
        const pos = base + Number(offset);
        if (pos < 0) return E.INVAL;
        entry.pos = pos;
        self.view().setBigUint64(newPtr, BigInt(pos), true);
        return E.SUCCESS;
      },
      fd_tell: (fd, ptr) => {
        const entry = file(fd);
        if (!entry) return E.BADF;
        self.view().setBigUint64(ptr, BigInt(entry.pos), true);
        return E.SUCCESS;
      },
      fd_close: (fd) => (self.fds.delete(fd) ? E.SUCCESS : E.BADF),
      fd_sync: () => E.SUCCESS,
      fd_datasync: () => E.SUCCESS,
      fd_advise: () => E.SUCCESS,
      fd_allocate: (fd, offset, len) => {
        const entry = file(fd);
        if (!entry) return E.BADF;
        const end = Number(offset) + Number(len);
        if (end > entry.node.size) entry.node.truncate(end);
        return E.SUCCESS;
      },
      fd_fdstat_get: (fd, ptr) => {
        const entry = self.fds.get(fd);
        if (!entry) return E.BADF;
        const v = self.view();
        const type = entry.kind === "stdin" || entry.kind === "stdout" || entry.kind === "stderr" ? FILETYPE.CHAR : isDir(entry.node) ? FILETYPE.DIRECTORY : FILETYPE.REGULAR;
        v.setUint8(ptr, type);
        v.setUint16(ptr + 2, entry.append ? FDFLAGS_APPEND : 0, true);
        v.setBigUint64(ptr + 8, ALL_RIGHTS, true);
        v.setBigUint64(ptr + 16, ALL_RIGHTS, true);
        return E.SUCCESS;
      },
      fd_fdstat_set_flags: (fd, flags) => {
        const entry = self.fds.get(fd);
        if (!entry) return E.BADF;
        entry.append = Boolean(flags & FDFLAGS_APPEND);
        return E.SUCCESS;
      },
      fd_fdstat_set_rights: () => E.SUCCESS,
      fd_filestat_get: (fd, ptr) => {
        const entry = self.fds.get(fd);
        if (!entry) return E.BADF;
        self.filestat(ptr, entry.node, entry.node ? "" : "char");
        return E.SUCCESS;
      },
      fd_filestat_set_size: (fd, size) => {
        const entry = file(fd);
        if (!entry) return E.BADF;
        if (entry.node.readonly) return E.ROFS;
        entry.node.truncate(Number(size));
        return E.SUCCESS;
      },
      fd_filestat_set_times: () => E.SUCCESS,
      fd_prestat_get: (fd, ptr) => {
        const entry = self.fds.get(fd);
        if (!entry || !entry.preopen) return E.BADF;
        self.view().setUint8(ptr, 0);
        self.view().setUint32(ptr + 4, new TextEncoder().encode(entry.preopen).length, true);
        return E.SUCCESS;
      },
      fd_prestat_dir_name: (fd, ptr, len) => {
        const entry = self.fds.get(fd);
        if (!entry || !entry.preopen) return E.BADF;
        self.bytes().set(new TextEncoder().encode(entry.preopen).subarray(0, len), ptr);
        return E.SUCCESS;
      },
      fd_readdir: (fd, buf, len, cookie, usedPtr) => {
        const entry = self.fds.get(fd);
        if (!entry || !isDir(entry.node)) return E.BADF;
        const names = [".", "..", ...[...entry.node.entries].filter(([, n]) => n !== null).map(([k]) => k)];
        const v = self.view(), enc = new TextEncoder();
        let used = 0;
        for (let i = Number(cookie); i < names.length && used < len; i++) {
          const name = enc.encode(names[i]);
          const node = i < 2 ? entry.node : entry.node.entries.get(names[i]);
          const header = new DataView(new ArrayBuffer(24));
          header.setBigUint64(0, BigInt(i + 1), true);
          header.setBigUint64(8, node.ino, true);
          header.setUint32(16, name.length, true);
          header.setUint8(20, isDir(node) ? FILETYPE.DIRECTORY : FILETYPE.REGULAR);
          const record = new Uint8Array(24 + name.length);
          record.set(new Uint8Array(header.buffer));
          record.set(name, 24);
          const n = Math.min(record.length, len - used);
          self.bytes().set(record.subarray(0, n), buf + used);
          used += n;
        }
        v.setUint32(usedPtr, used, true);
        return E.SUCCESS;
      },
      fd_renumber: (from, to) => {
        const entry = self.fds.get(from);
        if (!entry || !self.fds.has(to)) return E.BADF;
        self.fds.set(to, entry);
        self.fds.delete(from);
        return E.SUCCESS;
      },

      path_open: guard((dirfd, dirflags, pathPtr, pathLen, oflags, rightsBase, rightsInheriting, fdflags, fdPtr) => {
        const path = self.string(pathPtr, pathLen);
        const r = self.resolve(dirfd, path, { create: Boolean(oflags & OFLAGS.CREAT) });
        if (typeof r === "number") return r;
        let { node } = r;
        if (node && oflags & OFLAGS.CREAT && oflags & OFLAGS.EXCL) return E.EXIST;
        const writing = (rightsBase & 0x40n) !== 0n || (oflags & (OFLAGS.CREAT | OFLAGS.TRUNC)) !== 0;
        if (!node) {
          if (!r.parent || r.parent.readonly) return r.parent ? E.ROFS : E.NOENT;
          node = new MemFile();
          r.parent.entries.set(r.name, node);
          r.parent.mtime = now();
        }
        if (oflags & OFLAGS.DIRECTORY && !isDir(node)) return E.NOTDIR;
        if (isDir(node)) {
          if (writing && !(oflags & OFLAGS.DIRECTORY) && (oflags & (OFLAGS.CREAT | OFLAGS.TRUNC))) return E.ISDIR;
          self.view().setUint32(fdPtr, self.open(node, { kind: "dir", path: r.path }), true);
          return E.SUCCESS;
        }
        if (writing && node.readonly) return E.ROFS;
        if (oflags & OFLAGS.TRUNC) node.truncate(0);
        self.view().setUint32(fdPtr, self.open(node, { kind: "file", append: Boolean(fdflags & FDFLAGS_APPEND) }), true);
        return E.SUCCESS;
      }),
      path_filestat_get: guard((fd, flags, pathPtr, pathLen, ptr) => {
        const r = self.resolve(fd, self.string(pathPtr, pathLen));
        if (typeof r === "number") return r;
        self.filestat(ptr, r.node);
        return E.SUCCESS;
      }),
      path_filestat_set_times: () => E.SUCCESS,
      path_create_directory: guard((fd, pathPtr, pathLen) => {
        const r = self.resolve(fd, self.string(pathPtr, pathLen), { create: true });
        if (typeof r === "number") return r;
        if (r.node) return E.EXIST;
        if (!r.parent || r.parent.readonly) return E.ROFS;
        r.parent.entries.set(r.name, new MemDir());
        return E.SUCCESS;
      }),
      // The root has no parent: it cannot be removed, unlinked or renamed.
      path_remove_directory: guard((fd, pathPtr, pathLen) => {
        const r = self.resolve(fd, self.string(pathPtr, pathLen));
        if (typeof r === "number") return r;
        if (!r.parent) return E.BUSY;
        if (!(r.node instanceof MemDir)) return isDir(r.node) ? E.ROFS : E.NOTDIR;
        if (r.node.entries.size) return E.NOTEMPTY;
        r.parent.entries.delete(r.name);
        return E.SUCCESS;
      }),
      path_unlink_file: guard((fd, pathPtr, pathLen) => {
        const r = self.resolve(fd, self.string(pathPtr, pathLen));
        if (typeof r === "number") return r;
        if (isDir(r.node)) return E.ISDIR;
        if (r.parent.readonly) return E.ROFS;
        r.parent.entries.delete(r.name);
        return E.SUCCESS;
      }),
      path_rename: guard((fd, oldPtr, oldLen, newFd, newPtr, newLen) => {
        const from = self.resolve(fd, self.string(oldPtr, oldLen));
        if (typeof from === "number") return from;
        const to = self.resolve(newFd, self.string(newPtr, newLen), { create: true });
        if (typeof to === "number") return to;
        if (!from.parent || !to.parent) return E.BUSY;
        if (from.parent.readonly || !to.parent || to.parent.readonly) return E.ROFS;
        if (to.node && isDir(to.node) && !isDir(from.node)) return E.ISDIR;
        if (to.node instanceof MemDir && to.node.entries.size) return E.NOTEMPTY;
        from.parent.entries.delete(from.name);
        to.parent.entries.set(to.name, from.node);
        return E.SUCCESS;
      }),
      path_readlink: guard((fd, pathPtr, pathLen) => {
        const r = self.resolve(fd, self.string(pathPtr, pathLen));
        return typeof r === "number" ? r : E.INVAL;
      }),
      path_symlink: () => E.NOTSUP,
      path_link: () => E.NOTSUP,
    };
    return new Proxy(wasi, {
      get: (target, name) => target[name] ?? (() => E.NOSYS),
    });
  }

  /** Runs `module`'s `_start`, returning its exit status. */
  run(instance) {
    this.memory = instance.exports.memory;
    try {
      instance.exports._start();
      return 0;
    } catch (error) {
      if (error instanceof Exit) return error.code;
      throw error;
    }
  }
}
