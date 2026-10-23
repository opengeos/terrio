// Timings of the WebAssembly module on real files, in Node.js: decoding a tiled GeoTIFF and a
// JPEG 2000 band, writing a compressed COG, and converting a FlatGeobuf to GeoJSON. Run it with
// `node js/bench/bench.mjs <data dir> [package dir]`, the data directory being the one
// scripts/fetch_test_data.sh fills, and the package one built for Node.js (js/pkg-node by default).
// It prints the best of a few runs of each, in milliseconds, as JSON.

import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const [data, pkg = fileURLToPath(new URL("../pkg-node", import.meta.url))] = process.argv.slice(2);
if (!data) {
  console.error("usage: node js/bench/bench.mjs <data dir> [package dir]");
  process.exit(2);
}
const terrio = createRequire(import.meta.url)(join(resolve(pkg), "terrio.js"));
if (process.env.TERRIO_THREADS && terrio.initThreadPool) {
  await terrio.initThreadPool(Number(process.env.TERRIO_THREADS));
}

/** The best time of `runs` calls of `f`, in milliseconds. */
function best(f, runs = 3) {
  let min = Infinity;
  for (let i = 0; i < runs; i++) {
    const t = performance.now();
    f();
    min = Math.min(min, performance.now() - t);
  }
  return Math.round(min);
}

const has = (feature) => !terrio.features || terrio.features().features.includes(feature);
const file = (name) => readFileSync(join(data, name));
const results = {};

// A 3612 x 3612 Float32 DEM, LZW-compressed in 512 x 512 tiles: decoded whole.
const demBytes = file("USGS_1_n40w107.tif");
const dem = terrio.open(demBytes, "dem.tif");
results.decodeGeoTIFF = best(() => dem.readBand(1));
// The same DEM written as a DEFLATE COG, with its overviews.
results.writeCOG = best(() => dem.toBytes("dem_cog.tif", { format: "COG", compress: "DEFLATE" }));
// A Sentinel-2 band, 10980 x 10980 in JPEG 2000: opened (which decodes it) and read whole.
if (has("jp2") && existsSync(join(data, "s2_l1c_B04.jp2"))) {
  const jp2 = file("s2_l1c_B04.jp2");
  results.decodeJPEG2000 = best(() => terrio.open(jp2, "B04.jp2").readBand(1), 1);
}
// 3,233 US counties: FlatGeobuf read and written as GeoJSON.
if (has("flatgeobuf")) {
  const fgb = file("UScounties.fgb");
  results.flatGeobufToGeoJSON = best(() => terrio.open(fgb, "counties.fgb").toBytes("counties.geojson"));
}
console.log(JSON.stringify(results));
