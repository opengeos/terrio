// The playground page: terrio compiled to WebAssembly (the terrio-js npm package), run in the
// browser. Files are opened from memory, summarised, previewed, converted and downloaded.
(function () {
  "use strict";

  const VERSION = "0.1.1";
  const CDN = `https://cdn.jsdelivr.net/npm/terrio-js@${VERSION}/terrio.js`;

  const RASTER_FORMATS = [
    ["GeoTIFF", "GTiff", "tif"],
    ["Cloud Optimized GeoTIFF", "COG", "tif"],
    ["GeoPackage", "GPKG", "gpkg"],
    ["Esri ASCII grid", "AAIGrid", "asc"],
  ];
  const VECTOR_FORMATS = [
    ["GeoJSON", "GeoJSON", "geojson"],
    ["FlatGeobuf", "FlatGeobuf", "fgb"],
    ["GeoPackage", "GPKG", "gpkg"],
    ["KML", "KML", "kml"],
    ["KMZ", "KMZ", "kmz"],
    ["GML", "GML", "gml"],
    ["GPX", "GPX", "gpx"],
    ["TopoJSON", "TopoJSON", "topojson"],
  ];

  let modulePromise = null;
  let current = null; // { ds, name }

  /** Loads and initialises the WebAssembly module once. */
  function terrio() {
    if (!modulePromise) {
      modulePromise = import(CDN).then(async (m) => {
        await m.default();
        return m;
      });
    }
    return modulePromise;
  }

  const $ = (id) => document.getElementById(id);

  function status(text, error) {
    const el = $("tp-status");
    el.textContent = text;
    el.classList.toggle("tp-error", !!error);
  }

  function message(e) {
    return e && e.message ? e.message : String(e);
  }

  async function openBytes(bytes, name) {
    const t = await terrio();
    if (current) {
      current.ds.free();
      current = null;
    }
    status(`Opening ${name}…`);
    const ds = t.open(bytes, name);
    current = { ds, name };
    $("tp-result").hidden = false;
    $("tp-name").textContent = name;
    $("tp-convert-note").textContent = "";
    const info = ds.info();
    $("tp-info").textContent = JSON.stringify(info, null, 2);
    fillFormats(ds.kind);
    $("tp-where-row").hidden = ds.kind !== "vector";
    if (ds.kind === "raster") {
      previewRaster(ds);
    } else {
      previewVector(ds);
    }
    status(`terrio ${t.version()}: opened ${name} (${ds.kind}).`);
  }

  function fillFormats(kind) {
    const select = $("tp-format");
    select.innerHTML = "";
    for (const [label, driver, ext] of kind === "raster" ? RASTER_FORMATS : VECTOR_FORMATS) {
      const o = document.createElement("option");
      o.value = `${driver}|${ext}`;
      o.textContent = `${label} (.${ext})`;
      select.appendChild(o);
    }
  }

  // ----- Preview -----

  function canvas() {
    const c = $("tp-canvas");
    const ctx = c.getContext("2d");
    ctx.clearRect(0, 0, c.width, c.height);
    return [c, ctx];
  }

  /** Low and high percentiles of the valid values, for a contrast stretch. */
  function stretch(values, nodata) {
    const sample = [];
    const step = Math.max(1, Math.floor(values.length / 50000));
    for (let i = 0; i < values.length; i += step) {
      const v = values[i];
      if (Number.isFinite(v) && v !== nodata) sample.push(v);
    }
    if (!sample.length) return [0, 1];
    sample.sort((a, b) => a - b);
    const lo = sample[Math.floor(sample.length * 0.02)];
    const hi = sample[Math.floor(sample.length * 0.98)];
    return [lo, hi > lo ? hi : lo + 1];
  }

  function previewRaster(ds) {
    const [c, ctx] = canvas();
    // Read the smallest overview at least 512 pixels wide, or the full image if it is small.
    let width = ds.width;
    let height = ds.height;
    let overview;
    const overviews = ds.overviews || [];
    overviews.forEach((o, i) => {
      const [w, h] = Array.isArray(o) ? o : [o.width, o.height];
      if (w >= 512 && w < width) {
        width = w;
        height = h;
        overview = i;
      }
    });
    const big = width * height > 4_000_000;
    if (big && overview === undefined) {
      $("tp-preview-note").textContent = `${ds.width} × ${ds.height} pixels without overviews: too large to preview here.`;
      return;
    }
    const bands = ds.count >= 3 ? [1, 2, 3] : [1];
    const nodata = ds.nodata;
    const data = bands.map((b) => ds.readBand(b, undefined, overview));
    const ranges = data.map((d) => stretch(d, nodata));
    const img = new ImageData(width, height);
    for (let i = 0; i < width * height; i++) {
      // A pixel is empty only when every band is nodata: a single dark band (water in the near
      // infrared, say) can legitimately equal the nodata value.
      let empty = true;
      for (let k = 0; k < 3; k++) {
        const j = bands.length === 3 ? k : 0;
        const v = data[j][i];
        if (Number.isFinite(v) && v !== nodata) empty = false;
        const [lo, hi] = ranges[j];
        img.data[i * 4 + k] = Math.max(0, Math.min(255, ((v - lo) / (hi - lo)) * 255));
      }
      img.data[i * 4 + 3] = empty ? 0 : 255;
    }
    const off = new OffscreenCanvas(width, height);
    off.getContext("2d").putImageData(img, 0, 0);
    const scale = Math.min(c.width / width, c.height / height);
    const w = width * scale;
    const h = height * scale;
    ctx.imageSmoothingEnabled = scale < 1;
    ctx.drawImage(off, (c.width - w) / 2, (c.height - h) / 2, w, h);
    const what = bands.length === 3 ? "bands 1–3 as RGB" : "band 1 in grey";
    const level = overview === undefined ? "full resolution" : `overview ${overview} (${width} × ${height})`;
    $("tp-preview-note").textContent = `${what}, ${level}, stretched between the 2nd and 98th percentiles.`;
  }

  function eachCoord(geom, fn) {
    if (!geom) return;
    if (geom.type === "GeometryCollection") {
      geom.geometries.forEach((g) => eachCoord(g, fn));
      return;
    }
    const walk = (c) => (typeof c[0] === "number" ? fn(c) : c.forEach(walk));
    walk(geom.coordinates);
  }

  function previewVector(ds) {
    const [c, ctx] = canvas();
    const names = ds.layerNames();
    const fc = ds.toGeoJSON(names[0]);
    const features = fc.features || [];
    let [x0, y0, x1, y1] = [Infinity, Infinity, -Infinity, -Infinity];
    for (const f of features) {
      eachCoord(f.geometry, ([x, y]) => {
        x0 = Math.min(x0, x);
        y0 = Math.min(y0, y);
        x1 = Math.max(x1, x);
        y1 = Math.max(y1, y);
      });
    }
    if (!Number.isFinite(x0)) {
      $("tp-preview-note").textContent = "No geometries to draw.";
      return;
    }
    const pad = 10;
    const scale = Math.min((c.width - 2 * pad) / (x1 - x0 || 1), (c.height - 2 * pad) / (y1 - y0 || 1));
    const ox = (c.width - (x1 - x0) * scale) / 2;
    const oy = (c.height - (y1 - y0) * scale) / 2;
    const px = ([x, y]) => [ox + (x - x0) * scale, c.height - (oy + (y - y0) * scale)];
    const style = getComputedStyle(document.body);
    const accent = style.getPropertyValue("--md-accent-fg-color").trim() || "#ff6e42";
    const primary = style.getPropertyValue("--md-primary-fg-color").trim() || "#009485";
    ctx.lineWidth = 1;
    ctx.strokeStyle = primary;
    ctx.fillStyle = primary;
    const fill = () => {
      ctx.globalAlpha = 0.25;
      ctx.fill("evenodd");
      ctx.globalAlpha = 1;
    };
    const path = (rings, close) => {
      ctx.beginPath();
      for (const ring of rings) {
        ring.forEach((p, i) => {
          const [x, y] = px(p);
          i ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
        });
        if (close) ctx.closePath();
      }
    };
    const draw = (g) => {
      if (!g) return;
      switch (g.type) {
        case "Point":
        case "MultiPoint":
          ctx.fillStyle = accent;
          (g.type === "Point" ? [g.coordinates] : g.coordinates).forEach((p) => {
            const [x, y] = px(p);
            ctx.beginPath();
            ctx.arc(x, y, 2.5, 0, 2 * Math.PI);
            ctx.fill();
          });
          ctx.fillStyle = primary;
          break;
        case "LineString":
          path([g.coordinates], false);
          ctx.stroke();
          break;
        case "MultiLineString":
          path(g.coordinates, false);
          ctx.stroke();
          break;
        case "Polygon":
          path(g.coordinates, true);
          fill();
          ctx.stroke();
          break;
        case "MultiPolygon":
          g.coordinates.forEach((poly) => {
            path(poly, true);
            fill();
            ctx.stroke();
          });
          break;
        case "GeometryCollection":
          g.geometries.forEach(draw);
          break;
      }
    };
    features.forEach((f) => draw(f.geometry));
    const more = names.length > 1 ? ` (first of ${names.length} layers)` : "";
    $("tp-preview-note").textContent = `Layer "${names[0]}"${more}: ${features.length} features, in the layer's own coordinates.`;
  }

  // ----- Convert -----

  function download(bytes, filename) {
    const url = URL.createObjectURL(new Blob([bytes]));
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }

  function convert() {
    if (!current) return;
    const [driver, ext] = $("tp-format").value.split("|");
    const base = current.name.replace(/\.[^.]+$/, "") || "output";
    const filename = `${base}.${ext}`;
    const options = { format: driver };
    const tSrs = $("tp-tsrs").value.trim();
    if (tSrs) options.tSrs = tSrs;
    const where = $("tp-where").value.trim();
    if (where && current.ds.kind === "vector") options.where = where;
    try {
      const t0 = performance.now();
      const bytes = current.ds.toBytes(filename, options);
      const ms = Math.round(performance.now() - t0);
      download(bytes, filename);
      $("tp-convert-note").textContent = `Wrote ${filename}: ${(bytes.length / 1024).toFixed(1)} KB in ${ms} ms.`;
      $("tp-convert-note").classList.remove("tp-error");
    } catch (e) {
      $("tp-convert-note").textContent = message(e);
      $("tp-convert-note").classList.add("tp-error");
    }
  }

  // ----- Coordinates -----

  async function transform() {
    const out = $("tp-transform-out");
    try {
      const t = await terrio();
      const [xs, ys] = t.transformPoints([Number($("tp-x").value)], [Number($("tp-y").value)], $("tp-src").value, $("tp-dst").value);
      const name = t.describeCrs($("tp-dst").value).name;
      out.textContent = `${xs[0]}, ${ys[0]}  (${name})`;
      out.classList.remove("tp-error");
    } catch (e) {
      out.textContent = message(e);
      out.classList.add("tp-error");
    }
  }

  // ----- Wiring -----

  async function openFile(file) {
    try {
      await openBytes(new Uint8Array(await file.arrayBuffer()), file.name);
    } catch (e) {
      status(`Could not open ${file.name}: ${message(e)}`, true);
    }
  }

  async function openUrl(url) {
    const name = url.split("/").pop();
    try {
      status(`Downloading ${name}…`);
      const response = await fetch(url);
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      await openBytes(new Uint8Array(await response.arrayBuffer()), name);
    } catch (e) {
      status(`Could not open ${name}: ${message(e)}`, true);
    }
  }

  function init() {
    const root = $("terrio-playground");
    if (!root || root.dataset.ready) return;
    root.dataset.ready = "1";
    terrio().then(
      (t) => status(`terrio ${t.version()} is ready. Open a file to start.`),
      (e) => status(`Could not load terrio: ${message(e)}`, true),
    );
    $("tp-file").addEventListener("change", (e) => e.target.files[0] && openFile(e.target.files[0]));
    const drop = $("tp-drop");
    drop.addEventListener("dragover", (e) => {
      e.preventDefault();
      drop.classList.add("tp-over");
    });
    drop.addEventListener("dragleave", () => drop.classList.remove("tp-over"));
    drop.addEventListener("drop", (e) => {
      e.preventDefault();
      drop.classList.remove("tp-over");
      if (e.dataTransfer.files[0]) openFile(e.dataTransfer.files[0]);
    });
    root.querySelectorAll("[data-sample]").forEach((b) => b.addEventListener("click", () => openUrl(b.dataset.sample)));
    $("tp-convert").addEventListener("click", convert);
    $("tp-transform").addEventListener("click", transform);
  }

  // Runs on the first page load and after each instant navigation.
  if (window.document$) {
    window.document$.subscribe(init);
  } else {
    document.addEventListener("DOMContentLoaded", init);
  }
})();
