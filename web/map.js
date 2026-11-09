// SPDX-License-Identifier: MIT
// The map of terrio.dev/web: a pan-and-zoom view of a dataset in its own grid, as `terrio view`
// serves it (crates/terrio-cli/src/map.html, whose levels, panning and zooming this follows).
// There is no server: tiles are drawn by the command line in the worker, in batches, each batch one
// run of `terrio view INPUT --tiles "x,y,w,h,tw,th;…"`.

const TILE = 256;
/** The most tiles drawn in one run: enough for a screen, few enough to show progress. */
const BATCH = 24;
const MAX_CACHED = 512;
/** A tile that failed is not asked for again for this long. */
const RETRY_AFTER = 5000;

const STYLE = `
#map-view { position: fixed; inset: 0; z-index: 20; background: var(--bg); }
#map-view canvas { position: absolute; inset: 0; width: 100%; height: 100%; cursor: grab; touch-action: none; outline: none; }
#map-view canvas.dragging { cursor: grabbing; }
#map-view .panel { position: absolute; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 6px 10px; }
#map-title { top: 12px; left: 12px; max-width: calc(100% - 96px); overflow-wrap: anywhere; font-weight: 600; }
#map-status { bottom: 12px; left: 12px; max-width: calc(100% - 48px); color: var(--muted); font-variant-numeric: tabular-nums; }
#map-status.bad { color: var(--bad); }
#map-buttons { top: 12px; right: 12px; display: flex; flex-direction: column; gap: 4px; padding: 4px !important; }
#map-buttons button { width: 44px; height: 44px; font: inherit; font-size: 18px; color: inherit; background: none; border: 1px solid var(--line); border-radius: 6px; cursor: pointer; }
#map-buttons button:hover { background: var(--tint); }
`;

let view = null;

/**
 * Opens the map of a dataset.
 * @param {{title: string, args: string[], run: (args: string[]) => Promise<{code: number, stdout: Uint8Array, stderr: string}>, abort?: () => void}} options
 *   `args` is `["view", ...options and input]`, as the worker reads them; `run` runs a command line;
 *   `abort` ends a run of the map's that is in the worker, when the map is closed.
 */
export async function openMap({ title, args, run, abort }) {
  view?.close();
  view = new MapView(title, args, run, abort);
  await view.start();
}

class MapView {
  constructor(title, args, run, abort) {
    this.args = args;
    this.run = run;
    this.abort = abort;
    this.tiles = new Map();
    this.wanted = new Map();
    this.info = null;
    this.scale = 1;
    this.centerX = 0;
    this.centerY = 0;
    this.batching = false;
    this.failure = "";
    this.pointerText = "";
    this.queued = false;
    this.closed = false;
    this.build(title);
  }

  build(title) {
    if (!document.getElementById("map-style")) {
      const style = document.createElement("style");
      style.id = "map-style";
      style.textContent = STYLE;
      document.head.append(style);
    }
    const make = (tag, props) => Object.assign(document.createElement(tag), props);
    this.root = make("div", { id: "map-view" });
    this.root.setAttribute("role", "dialog");
    this.root.setAttribute("aria-label", `Map of ${title}`);
    this.canvas = make("canvas", { tabIndex: 0 });
    this.canvas.setAttribute("aria-label", "Map of the dataset. Drag or use the arrow keys to pan; scroll or press plus and minus to zoom; press 0 to fit; Escape closes it.");
    this.context = this.canvas.getContext("2d");
    this.title = make("div", { id: "map-title", className: "panel", textContent: `${title} (loading…)` });
    this.statusBox = make("div", { id: "map-status", className: "panel" });
    this.statusBox.setAttribute("role", "status");
    const buttons = make("div", { id: "map-buttons", className: "panel" });
    const button = (text, label, action) => {
      const b = make("button", { type: "button", textContent: text });
      b.setAttribute("aria-label", label);
      // Each button gives the keyboard back to the map, which pans and zooms with the keys.
      b.addEventListener("click", () => { action(); this.canvas.focus(); });
      buttons.append(b);
    };
    button("✕", "Close the map", () => this.close());
    button("+", "Zoom in", () => this.zoomAt(1.5, innerWidth / 2, innerHeight / 2));
    button("−", "Zoom out", () => this.zoomAt(1 / 1.5, innerWidth / 2, innerHeight / 2));
    button("⤢", "Fit to window", () => this.fit());
    this.root.append(this.canvas, this.title, buttons, this.statusBox);
    document.body.append(this.root);
    this.titleText = title;
    this.listen();
    this.setStatus();
  }

  /**
   * The command line with `flags` right after the subcommand: after the options and positionals,
   * they could land past a `--`, where they would be taken for positionals.
   */
  withFlags(...flags) {
    return [this.args[0], ...flags, ...this.args.slice(1)];
  }

  async start() {
    this.resize();
    this.canvas.focus();
    try {
      const result = await this.run(this.withFlags("--info-json"));
      if (this.closed) return;
      if (result.code !== 0) throw new Error(result.stderr.trim().replace(/^terrio: /, "") || `exit status ${result.code}`);
      this.info = JSON.parse(new TextDecoder().decode(result.stdout));
      this.info.maxLevel = Math.max(0, Math.ceil(Math.log2(Math.max(this.info.width, this.info.height) / TILE)));
      this.title.textContent = this.titleText;
      this.fit();
    } catch (error) {
      this.title.textContent = `${this.titleText}: no map`;
      this.failure = error.message;
      this.setStatus();
    }
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    // A batch still being drawn is ended, so that it cannot hold up later commands.
    if (this.batching || !this.info) this.abort?.();
    for (const tile of this.tiles.values()) tile.image?.close();
    this.tiles.clear();
    removeEventListener("resize", this.onResize);
    removeEventListener("keydown", this.onEscape);
    this.root.remove();
    if (view === this) view = null;
  }

  setStatus() {
    this.statusBox.className = this.failure ? "panel bad" : "panel";
    this.statusBox.textContent = this.failure || this.pointerText || (this.batching ? "Drawing…" : "");
    this.statusBox.hidden = !this.statusBox.textContent;
  }

  /** The level whose pixels are the first at least as fine as the screen's. */
  levelOf() {
    return Math.max(0, Math.floor(Math.log2(1 / this.scale / (devicePixelRatio || 1)) + 1e-9));
  }

  resize() {
    const ratio = devicePixelRatio || 1;
    this.canvas.width = Math.round(innerWidth * ratio);
    this.canvas.height = Math.round(innerHeight * ratio);
    this.draw();
  }

  fit() {
    if (!this.info) return;
    this.scale = Math.min(innerWidth / this.info.width, innerHeight / this.info.height, 8);
    this.centerX = this.info.width / 2;
    this.centerY = this.info.height / 2;
    this.draw();
  }

  tileAt(level, col, row) {
    const key = `${level}/${col}/${row}`;
    const tile = this.tiles.get(key);
    if (tile) {
      this.tiles.delete(key);
      this.tiles.set(key, tile);
    }
    return tile || null;
  }

  /** Asks for a tile: it is drawn with the next batch. */
  load(level, col, row) {
    const key = `${level}/${col}/${row}`;
    const known = this.tiles.get(key);
    if (known && !(known.failedAt && performance.now() - known.failedAt > RETRY_AFTER)) return;
    if (this.wanted.has(key)) return;
    const size = TILE * 2 ** level, x = col * size, y = row * size;
    const w = Math.min(size, this.info.width - x), h = Math.min(size, this.info.height - y);
    this.wanted.set(key, { x, y, w, h, tw: Math.max(1, Math.round(w / 2 ** level)), th: Math.max(1, Math.round(h / 2 ** level)), level });
  }

  /** Draws the tiles asked for, coarsest first, a batch at a time. */
  async drawBatch() {
    if (this.batching || !this.wanted.size || this.closed) return;
    this.batching = true;
    this.setStatus();
    const batch = [...this.wanted].sort(([, a], [, b]) => b.level - a.level).slice(0, BATCH);
    for (const [key] of batch) this.wanted.delete(key);
    const spec = batch.map(([, t]) => [t.x, t.y, t.w, t.h, t.tw, t.th].join(",")).join(";");
    try {
      const result = await this.run(this.withFlags("--tiles", spec));
      if (this.closed) return;
      const bytes = result.stdout;
      const data = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      // Each tile is a length and a PNG, or a zero length, filling the output exactly: anything else
      // was written by something other than the tiles, and is not drawn. A command that failed
      // before writing every tile says why on stderr.
      const unexpected = () => new Error(result.code === 0 ? "the command line wrote unexpected output"
        : (result.stderr.trim().split("\n").filter(Boolean).pop() || `exit status ${result.code}`).replace(/^terrio: /, ""));
      let at = 0;
      for (let i = 0; i < batch.length; i++) {
        const length = at + 4 <= bytes.length ? data.getUint32(at, true) : -1;
        if (length < 0 || at + 4 + length > bytes.length || (length && (length < 8 || data.getUint32(at + 4) !== 0x89504e47))) {
          throw unexpected();
        }
        at += 4 + length;
      }
      if (at !== bytes.length) throw unexpected();
      at = 0;
      const images = [];
      for (const [key, t] of batch) {
        const length = at + 4 <= bytes.length ? data.getUint32(at, true) : 0;
        at += 4;
        const png = length ? bytes.subarray(at, at + length) : null;
        at += length;
        const tile = { image: null, x: t.x, y: t.y, w: t.w, h: t.h };
        this.tiles.set(key, tile);
        if (!png) tile.failedAt = performance.now();
        else {
          images.push(createImageBitmap(new Blob([png], { type: "image/png" })).then((image) => {
            // A map closed meanwhile no longer holds the tile: the image is let go at once.
            if (this.closed) image.close();
            else tile.image = image;
          }, () => { tile.failedAt = performance.now(); }));
        }
      }
      await Promise.all(images);
      const message = result.stderr.trim().split("\n").filter(Boolean).pop();
      this.failure = result.code === 0 ? "" : `Could not draw this part: ${(message || `exit status ${result.code}`).replace(/^terrio: /, "")}`;
    } catch (error) {
      if (this.closed) return;
      for (const [key] of batch) this.tiles.set(key, { image: null, failedAt: performance.now() });
      this.failure = `Could not draw this part: ${error.message}`;
    } finally {
      this.batching = false;
      for (const [old, entry] of this.tiles) {
        if (this.tiles.size <= MAX_CACHED) break;
        if (entry.image || entry.failedAt) {
          entry.image?.close();
          this.tiles.delete(old);
        }
      }
      this.setStatus();
      this.draw();
    }
  }

  draw() {
    if (this.queued) return;
    this.queued = true;
    requestAnimationFrame(() => {
      this.queued = false;
      this.paint();
      this.drawBatch();
    });
  }

  paint() {
    const { context, canvas, info } = this;
    const ratio = devicePixelRatio || 1;
    context.setTransform(1, 0, 0, 1, 0, 0);
    context.clearRect(0, 0, canvas.width, canvas.height);
    if (!info) return;
    // Only what is on screen now is asked for.
    this.wanted.clear();
    const s = this.scale * ratio;
    context.setTransform(s, 0, 0, s, canvas.width / 2 - this.centerX * s, canvas.height / 2 - this.centerY * s);
    context.imageSmoothingEnabled = s < 1;
    const left = Math.max(0, this.centerX - innerWidth / 2 / this.scale), right = Math.min(info.width, this.centerX + innerWidth / 2 / this.scale);
    const top = Math.max(0, this.centerY - innerHeight / 2 / this.scale), bottom = Math.min(info.height, this.centerY + innerHeight / 2 / this.scale);
    if (right <= left || bottom <= top) return;
    const finest = Math.min(this.levelOf(), info.maxLevel);
    // Coarser levels first, so that they show until finer tiles arrive.
    for (let level = info.maxLevel; level >= finest; level--) {
      const size = TILE * 2 ** level;
      for (let row = Math.floor(top / size); row * size < bottom; row++) {
        for (let col = Math.floor(left / size); col * size < right; col++) {
          const tile = this.tileAt(level, col, row);
          // Cleared first, so that a coarser tile does not show through this one's transparent
          // (nodata) pixels.
          if (tile && tile.image) {
            context.clearRect(tile.x, tile.y, tile.w, tile.h);
            context.drawImage(tile.image, tile.x, tile.y, tile.w, tile.h);
          } else if (level === finest || level === info.maxLevel) this.load(level, col, row);
        }
      }
    }
  }

  zoomAt(factor, screenX, screenY) {
    if (!this.info) return;
    const fitted = Math.min(innerWidth / this.info.width, innerHeight / this.info.height, 8);
    const next = Math.min(Math.max(this.scale * factor, fitted / 2), 32);
    const dx = screenX - innerWidth / 2, dy = screenY - innerHeight / 2;
    this.centerX += dx / this.scale - dx / next;
    this.centerY += dy / this.scale - dy / next;
    this.scale = next;
    this.draw();
  }

  describe(event) {
    const { info } = this;
    if (!info) return;
    const col = this.centerX + (event.clientX - innerWidth / 2) / this.scale, row = this.centerY + (event.clientY - innerHeight / 2) / this.scale;
    if (col < 0 || row < 0 || col >= info.width || row >= info.height) {
      this.pointerText = "";
    } else {
      const g = info.geoTransform;
      const coordinates = g ? `x ${(g[0] + col * g[1] + row * g[2]).toFixed(5)}, y ${(g[3] + col * g[4] + row * g[5]).toFixed(5)}` : "";
      this.pointerText = info.kind === "raster" ? `column ${Math.floor(col)}, row ${Math.floor(row)}${coordinates && `; ${coordinates}`}` : coordinates;
    }
    this.setStatus();
  }

  listen() {
    const canvas = this.canvas;
    let drag = null;
    canvas.addEventListener("pointerdown", (event) => {
      drag = { x: event.clientX, y: event.clientY };
      canvas.setPointerCapture(event.pointerId);
      canvas.classList.add("dragging");
    });
    canvas.addEventListener("pointermove", (event) => {
      this.describe(event);
      if (!drag) return;
      this.centerX -= (event.clientX - drag.x) / this.scale;
      this.centerY -= (event.clientY - drag.y) / this.scale;
      drag = { x: event.clientX, y: event.clientY };
      this.draw();
    });
    for (const name of ["pointerup", "pointercancel"]) {
      canvas.addEventListener(name, () => {
        drag = null;
        canvas.classList.remove("dragging");
      });
    }
    canvas.addEventListener("wheel", (event) => {
      event.preventDefault();
      // Deltas in lines or pages are made pixels first.
      const pixels = event.deltaY * (event.deltaMode === 1 ? 33 : event.deltaMode === 2 ? innerHeight : 1);
      this.zoomAt(Math.exp(-pixels * 0.0015), event.clientX, event.clientY);
    }, { passive: false });
    canvas.addEventListener("keydown", (event) => {
      const step = 80 / this.scale;
      const moves = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] };
      if (moves[event.key]) {
        this.centerX += moves[event.key][0];
        this.centerY += moves[event.key][1];
        this.draw();
      } else if (event.key === "+" || event.key === "=") this.zoomAt(1.5, innerWidth / 2, innerHeight / 2);
      else if (event.key === "-") this.zoomAt(1 / 1.5, innerWidth / 2, innerHeight / 2);
      else if (event.key === "0") this.fit();
      else return;
      event.preventDefault();
    });
    this.onResize = () => this.resize();
    this.onEscape = (event) => { if (event.key === "Escape") this.close(); };
    addEventListener("resize", this.onResize);
    addEventListener("keydown", this.onEscape);
  }
}
