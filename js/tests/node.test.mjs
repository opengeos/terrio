// Tests of the WebAssembly build in Node.js. Build it with
// `wasm-pack build js --target nodejs --out-dir pkg-node --out-name terrio`, then run
// `node --test js/tests/node.test.mjs`. Tests on real files run when TERRIO_TEST_DATA names the
// directory filled by scripts/fetch_test_data.sh, and compare band statistics with those GDAL
// computed (the .aux.xml files).

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import { test } from "node:test";

const require = createRequire(import.meta.url);
const terrio = require(process.env.TERRIO_JS_PKG ?? "../pkg-node/terrio.js");
const DATA = process.env.TERRIO_TEST_DATA;

const SQUARES = {
  type: "FeatureCollection",
  features: [
    {
      type: "Feature",
      properties: { name: "west", value: 3 },
      geometry: { type: "Polygon", coordinates: [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]] },
    },
    {
      type: "Feature",
      properties: { name: "east", value: 7 },
      geometry: { type: "Polygon", coordinates: [[[20, 0], [30, 0], [30, 10], [20, 10], [20, 0]]] },
    },
  ],
};

test("version", () => {
  assert.match(terrio.version(), /^\d+\.\d+\.\d+/);
});

test("raster from array, to GeoTIFF bytes and back", () => {
  const data = new Int16Array(2 * 3 * 4).map((_, i) => i - 5);
  const ds = terrio.rasterFromArray(data, 4, 3, [500000, 30, 0, 4100000, 0, -30], "EPSG:32610", -9999);
  assert.equal(ds.kind, "raster");
  assert.equal(ds.count, 2);
  const tif = ds.toBytes("out.tif");
  assert.deepEqual([...tif.slice(0, 4)], [0x49, 0x49, 0x2a, 0x00]);
  const back = terrio.open(tif, "out.tif");
  assert.equal(back.width, 4);
  assert.equal(back.height, 3);
  assert.equal(back.dtype, "int16");
  assert.equal(back.nodata, -9999);
  assert.equal(back.crs, "EPSG:32610");
  assert.deepEqual([...back.transform], [500000, 30, 0, 4100000, 0, -30]);
  const band2 = back.readBand(2);
  assert.ok(band2 instanceof Int16Array);
  assert.deepEqual([...band2], [...data.slice(12)]);
  assert.deepEqual([...back.readBand(1, [1, 1, 2, 2])], [0, 1, 4, 5]);
  const info = back.info(true);
  assert.deepEqual(info.size, [4, 3]);
  assert.equal(info.bands[0].statistics.min, -5);
  // Compressed and tiled, and as a COG.
  for (const options of [{ compress: "DEFLATE", tileSize: 16 }, { format: "COG" }]) {
    const again = terrio.open(back.toBytes("c.tif", options), "c.tif");
    assert.deepEqual([...again.readBand(2)], [...band2]);
  }
  // Windows and bands, written as an ASCII grid.
  const asc = new TextDecoder().decode(back.toBytes("w.asc", { srcwin: [0, 0, 2, 2], bands: [1] }));
  assert.match(asc, /ncols\s+2/);
});

test("vector from GeoJSON, to other formats and back", () => {
  const ds = terrio.fromGeoJSON(SQUARES, "squares");
  assert.equal(ds.kind, "vector");
  assert.deepEqual(ds.layerNames(), ["squares"]);
  for (const name of ["squares.fgb", "squares.geojson", "squares.kml", "squares.gml"]) {
    const bytes = ds.toBytes(name);
    const back = terrio.open(bytes, name);
    const fc = back.toGeoJSON();
    assert.equal(fc.type, "FeatureCollection", name);
    assert.equal(fc.features.length, 2, name);
    // FlatGeobuf orders features along its spatial index, as GDAL writes it.
    // KML keeps the name as the placemark's Name, as GDAL's LIBKML driver does.
    const east = fc.features.find((f) => (f.properties.name ?? f.properties.Name) === "east");
    assert.deepEqual(east.geometry.coordinates[0][2], [30, 10], name);
  }
  const filtered = terrio.open(ds.toBytes("f.fgb", { where: "value > 5", select: ["name"] }), "f.fgb");
  const fc = filtered.toGeoJSON(0);
  assert.equal(fc.features.length, 1);
  assert.deepEqual(fc.features[0].properties, { name: "east" });
  const info = filtered.info();
  assert.equal(info.layers[0].feature_count, 1);
});

test("CRS transforms", () => {
  const [xs, ys] = terrio.transformPoints([0, 180], [0, 0], "EPSG:4326", "EPSG:3857");
  assert.ok(xs instanceof Float64Array);
  assert.ok(Math.abs(xs[1] - 20037508.342789244) < 1e-6);
  assert.ok(Math.abs(ys[0]) < 1e-6);
  const crs = terrio.describeCrs("EPSG:32633");
  assert.equal(crs.epsg, 32633);
  assert.match(crs.name, /UTM zone 33N/);
  assert.equal(crs.geographic, false);
});

test("errors", () => {
  assert.throws(() => terrio.open(new Uint8Array([1, 2, 3]), "x.unknown"), /not recognized/);
  assert.throws(() => terrio.describeCrs("nonsense"), /cannot parse/);
  const ds = terrio.fromGeoJSON(SQUARES);
  assert.throws(() => ds.readBand(1), /not a raster/);
  assert.throws(() => ds.toBytes("x.mif"), /cannot be written to memory/);
});

/** GDAL's statistics of a band, from the .aux.xml file it wrote next to the raster. */
function gdalStatistics(path, band) {
  const xml = readFileSync(`${path}.aux.xml`, "utf8");
  const block = xml.split("<PAMRasterBand").find((b) => b.includes(`band="${band}"`));
  const get = (key) => Number(block.match(new RegExp(`key="STATISTICS_${key}">([^<]+)<`))[1]);
  return { min: get("MINIMUM"), max: get("MAXIMUM"), mean: get("MEAN") };
}

const RASTERS = ["srtm_sub.tif", "dem.tif", "cog.tif", "landsat.tif", "nlcd_knoxville.tif"];

for (const name of RASTERS) {
  test(`${name} matches GDAL's statistics`, { skip: !DATA && "TERRIO_TEST_DATA is not set" }, (t) => {
    const path = join(DATA, name);
    if (!existsSync(path) || !existsSync(`${path}.aux.xml`)) return t.skip(`${name} or its statistics are missing`);
    const ds = terrio.open(readFileSync(path), name);
    for (let b = 1; b <= ds.count; b++) {
      const values = ds.readBand(b);
      assert.equal(values.length, ds.width * ds.height);
      let [min, max, sum, n] = [Infinity, -Infinity, 0, 0];
      for (const v of values) {
        if (Number.isNaN(v) || v === ds.nodata) continue;
        min = Math.min(min, v);
        max = Math.max(max, v);
        sum += v;
        n++;
      }
      const expected = gdalStatistics(path, b);
      // GDAL prints the statistics with 14 significant digits.
      const close = (a, e) => Math.abs(a - e) <= 1e-12 * Math.max(1, Math.abs(e));
      assert.ok(close(min, expected.min), `${name} band ${b} min ${min} != ${expected.min}`);
      assert.ok(close(max, expected.max), `${name} band ${b} max ${max} != ${expected.max}`);
      assert.ok(Math.abs(sum / n - expected.mean) <= 1e-6 * Math.max(1, Math.abs(expected.mean)), `${name} band ${b} mean`);
    }
  });
}

const VECTORS = [
  ["countries.geojson", 176],
  ["countries.kml", 176],
  ["countries.gml", 176],
  ["las_vegas_roads.geojson", null],
  ["hike.gpx", null],
];

for (const [name, count] of VECTORS) {
  test(`${name} reads from bytes`, { skip: !DATA && "TERRIO_TEST_DATA is not set" }, (t) => {
    const path = join(DATA, name);
    if (!existsSync(path)) return t.skip(`${name} is missing`);
    const ds = terrio.open(readFileSync(path), name);
    const fc = ds.toGeoJSON();
    assert.equal(fc.type, "FeatureCollection");
    if (count !== null) assert.equal(fc.features.length, count);
    else assert.ok(fc.features.length > 0);
    // Through GeoJSON and back.
    const back = terrio.open(ds.toBytes("x.geojson"), "x.geojson").toGeoJSON();
    assert.equal(back.features.length, fc.features.length);
  });
}

test("zipped shapefiles are read from memory", { skip: !DATA && "TERRIO_TEST_DATA is not set" }, (t) => {
  const path = join(DATA, "ne_countries.zip");
  if (!existsSync(path)) return t.skip("ne_countries.zip is missing");
  // GDAL: 177 features, EPSG:4326.
  const fc = terrio.open(readFileSync(path), "ne_countries.zip").toGeoJSON();
  assert.equal(fc.features.length, 177);
  assert.ok(fc.features.some((f) => f.properties.NAME === "Côte d'Ivoire"));
});
