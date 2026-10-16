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
  for (const options of [{ compress: "DEFLATE", tileSize: 16 }, { compress: "ZSTD" }, { format: "COG" }]) {
    const again = terrio.open(back.toBytes("c.tif", options), "c.tif");
    assert.deepEqual([...again.readBand(2)], [...band2]);
  }
  // Windows and bands, written as an ASCII grid.
  const asc = new TextDecoder().decode(back.toBytes("w.asc", { srcwin: [0, 0, 2, 2], bands: [1] }));
  assert.match(asc, /ncols\s+2/);
});

test("JPEG 2000 to bytes and back", () => {
  // Int16 with a nodata value: the JP2 file and its .aux.xml sidecar, zipped, as GDAL keeps it.
  const dem = new Int16Array(40 * 30).map((_, i) => ((i * 37) % 3000) - 1500);
  const ds = terrio.rasterFromArray(dem, 40, 30, [500000, 10, 0, 4100000, 0, -10], "EPSG:32611", -1500);
  const zipped = ds.toBytes("dem.jp2");
  assert.deepEqual([...zipped.slice(0, 2)], [0x50, 0x4b]);
  const back = terrio.open(zipped, "dem.zip");
  assert.equal(back.dtype, "int16");
  assert.equal(back.nodata, -1500);
  assert.equal(back.crs, "EPSG:32611");
  assert.deepEqual([...back.transform], [500000, 10, 0, 4100000, 0, -10]);
  assert.deepEqual([...back.readBand(1)], [...dem]);
  // RGB bytes, without nodata: the JP2 file alone, recognized by its signature.
  const rgb = new Uint8Array(3 * 64 * 48).map((_, i) => (i * 7) % 256);
  const jp2 = terrio.rasterFromArray(rgb, 64, 48, [10, 0.001, 0, 50, 0, -0.001], "EPSG:4326").toBytes("rgb.jp2");
  assert.deepEqual([...jp2.slice(4, 8)], [0x6a, 0x50, 0x20, 0x20]);
  const img = terrio.open(jp2, "rgb");
  assert.equal(img.count, 3);
  assert.equal(img.crs, "EPSG:4326");
  assert.deepEqual([...img.readBand(3)], [...rgb.slice(2 * 64 * 48)]);
  assert.throws(() => terrio.rasterFromArray(new Float32Array(4), 2, 2, [0, 1, 0, 2, 0, -1]).toBytes("f.jp2"), /8- and 16-bit integers/);
});

test("vector layers as Arrow IPC streams, and back", () => {
  const ds = terrio.fromGeoJSON(SQUARES, "squares");
  for (const options of [undefined, { geometryEncoding: "geoarrow" }, { geometryEncoding: "geoarrow", coordinates: "interleaved" }]) {
    const bytes = ds.toArrowIPC(0, options);
    // An IPC stream starts with a continuation marker.
    assert.deepEqual([...bytes.slice(0, 4)], [255, 255, 255, 255]);
    const back = terrio.fromArrowIPC(bytes, "squares");
    assert.equal(back.crs, "EPSG:4326");
    const fc = back.toGeoJSON();
    assert.equal(fc.features.length, 2);
    assert.deepEqual(fc.features[1].properties, { name: "east", value: 7 });
    assert.deepEqual(fc.features[1].geometry.coordinates[0][2], [30, 10]);
  }
  assert.throws(() => ds.toArrowIPC(0, { geometryEncoding: "nope" }), /unknown geometry encoding/);
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

test("STAC items read as GDAL reads STAC GeoJSON", () => {
  const item = {
    type: "Feature",
    stac_version: "1.0.0",
    stac_extensions: ["https://stac-extensions.github.io/projection/v1.1.0/schema.json"],
    id: "S2B_13TDE_20230705_0_L2A",
    collection: "sentinel-2-l2a",
    properties: { datetime: "2023-07-05T18:03:00Z", "eo:cloud_cover": 0.5, "proj:epsg": 32613 },
    geometry: { type: "Polygon", coordinates: [[[-106, 39.6], [-104.9, 39.6], [-104.9, 40.6], [-106, 39.6]]] },
    links: [],
    assets: { red: { href: "B04.tif", type: "image/tiff; application=geotiff", roles: ["data"] } },
  };
  const ds = terrio.open(new TextEncoder().encode(JSON.stringify(item)), "item.json");
  const props = ds.toGeoJSON().features[0].properties;
  assert.equal(props.id, "S2B_13TDE_20230705_0_L2A");
  assert.equal(props["eo:cloud_cover"], 0.5);
  assert.equal(props.stac_version, "1.0.0");
  assert.equal(props.collection, "sentinel-2-l2a");
  assert.equal(props["assets.red.href"], "B04.tif");
});

test("PMTiles and MBTiles: rasters by window, vector tiles as layers", () => {
  // A 512 x 256 RGB raster on the tiles of zoom level 11 (EPSG:3857).
  const MAX_GM = 20037508.342789244;
  const res = (2 * MAX_GM) / 256 / 2048;
  const transform = [-MAX_GM + 1000 * 256 * res, res, 0, MAX_GM - 700 * 256 * res, 0, -res];
  const data = new Uint8Array(3 * 256 * 512).map((_, i) => i % 251);
  const ds = terrio.rasterFromArray(data, 512, 256, transform, "EPSG:3857");
  for (const name of ["rgb.pmtiles", "rgb.mbtiles"]) {
    const back = terrio.open(ds.toBytes(name), name);
    assert.equal(back.kind, "raster", name);
    assert.equal(back.width, 512);
    assert.equal(back.height, 256);
    assert.equal(back.crs, "EPSG:3857");
    assert.deepEqual(back.overviews, [[256, 128]]);
    const win = back.readBand(2, [10, 20, 4, 1]);
    assert.ok(win instanceof Uint8Array);
    assert.deepEqual([...win], [...data.slice(256 * 512 + 20 * 512 + 10, 256 * 512 + 20 * 512 + 14)], name);
  }
  const places = terrio.fromGeoJSON(SQUARES, "squares");
  for (const name of ["sq.pmtiles", "sq.mbtiles"]) {
    const back = terrio.open(places.toBytes(name, { maxzoom: 3 }), name);
    assert.equal(back.kind, "vector", name);
    assert.deepEqual(back.layerNames(), ["squares"]);
    const fc = back.toGeoJSON();
    assert.equal(fc.features.length, 2, name);
    assert.deepEqual(fc.features.map((f) => f.properties.name).sort(), ["east", "west"]);
  }
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

test("CRS transforms with heights, epochs and grids", () => {
  // Heights are kept between 2D CRSs.
  const [, , zs] = terrio.transformPoints([0], [0], "EPSG:4326", "EPSG:3857", [100]);
  assert.equal(zs[0], 100);
  // ITRF2014 at epoch 2010.0 to GDA2020 (GDAL's result with the source coordinate epoch).
  const [x, y, z] = terrio.transformPoints([135], [-25], "EPSG:9000", "EPSG:7844", [0], { sCoordEpoch: 2010 });
  assert.ok(Math.abs(x[0] - 135.0000030617912) < 1e-11);
  assert.ok(Math.abs(y[0] - -24.999994698869124) < 1e-11);
  assert.ok(Math.abs(z[0] - -0.0015074610710144043) < 1e-6);
  // A grid registered from its bytes: NAD27 to NAD83 as GDAL computes it with NADCON5.
  const grid = DATA && join(DATA, "grids", "us_noaa_nadcon5_nad27_nad83_1986_conus.tif");
  if (grid && existsSync(grid)) {
    terrio.registerGrid("us_noaa_nadcon5_nad27_nad83_1986_conus.tif", readFileSync(grid));
    const [lon, lat] = terrio.transformPoints([-100], [40], "EPSG:4267", "EPSG:4269");
    assert.ok(Math.abs(lon[0] - -100.0004055982497) < 1e-10, `${lon[0]}`);
    assert.ok(Math.abs(lat[0] - 40.00000580229538) < 1e-10, `${lat[0]}`);
  }
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

/** A band's sum of pixels weighted by their position, `value * (index % 251)`. */
const weightedSum = (band) => band.reduce((s, v, i) => s + v * (i % 251), 0);

// JPEG 2000 files as GDAL 3.12.2 (JP2OpenJPEG) reads them: size, type, CRS, geotransform and each
// band's weighted sum (computed with NumPy).
const JPEG2000 = [
  // Sentinel-2 L1C and L2A bands, georeferenced by their GMLJP2 boxes.
  ["s2_l1c_B01.jp2", [1830, 1830], "uint16", "EPSG:32610", [499980, 60, 0, 4200000, 0, -60], [230149703898]],
  ["s2_l2a_B01_60m.jp2", [1830, 1830], "uint16", "EPSG:32610", [499980, 60, 0, 4200000, 0, -60], [166014464965]],
  ["s2_l2a_SCL_20m.jp2", [5490, 5490], "uint8", "EPSG:32610", [499980, 20, 0, 4200000, 0, -20], [4811992167]],
  ["s2_l2a_TCI_60m.jp2", [1830, 1830], "uint8", "EPSG:32610", [499980, 60, 0, 4200000, 0, -60], [8077143450, 8388213892, 8191826120]],
  // GMLJP2 in EPSG:4326, latitude first.
  [
    "gmljp2_dtedsm_epsg_4326_axes.jp2",
    [121, 121],
    "int16",
    "EPSG:4326",
    [42.99958333333337, 0.00827134986225895, 0, 34.00041666666663, 0, -0.00827134986225895],
    [109686149],
  ],
  // A 1-bit alpha channel, promoted to 0 and 255.
  ["stefan_full_rgba_alpha_1bit.jp2", [162, 150], "uint8", undefined, null, [65041074, 60546965, 9711194, 362353215]],
  // JPEG 2000 copies of test rasters, written by GDAL.
  ["lv_dem.jp2", [970, 498], "int16", "EPSG:3857", [-12824908, 1, 0, 4317505, 0, -1], [40594721304]],
  [
    "srtm_sub.jp2",
    [500, 400],
    "int16",
    "EPSG:4326",
    [-119.94744359355316, 0.0008084837557075694, 0, 37.81843967840713, 0, -0.0008084837557075694],
    [40199496687],
  ],
];

for (const [name, size, dtype, crs, transform, sums] of JPEG2000) {
  test(`${name} reads as GDAL reads it`, { skip: !DATA && "TERRIO_TEST_DATA is not set" }, (t) => {
    const path = join(DATA, name);
    if (!existsSync(path)) return t.skip(`${name} is missing`);
    const ds = terrio.open(readFileSync(path), name);
    assert.deepEqual([ds.width, ds.height], size);
    assert.equal(ds.dtype, dtype);
    assert.equal(ds.crs, crs);
    if (transform) {
      ds.transform.forEach((v, i) => assert.ok(Math.abs(v - transform[i]) <= 1e-9 * Math.max(1, Math.abs(transform[i])), `${name} transform`));
    }
    const got = Array.from({ length: ds.count }, (_, b) => weightedSum(ds.readBand(b + 1)));
    assert.deepEqual(got, sums);
    // Written back as JPEG 2000, losslessly.
    const again = terrio.open(ds.toBytes("again.jp2"), "again.jp2");
    assert.deepEqual([...again.readBand(1)], [...ds.readBand(1)]);
    assert.equal(again.crs, crs);
  });
}

// GeoTIFFs in the compressions GDAL writes beyond the common ones, as GDAL reads them: each band's
// weighted sum (integer files) or sum and NaN count (Float32 files).
const GTIFF_CODECS = [
  ["rgb_jpeg_ycbcr.tif", [1679542889, 943807543, 1835727455]],
  ["rgb_jpeg.tif", [1674413809, 935764082, 1858995246]],
  ["rgb_webp.tif", [1694423642, 943937085, 1860784108]],
  ["rgb_jxl.tif", [1670997934, 927535433, 1854570638]],
  ["rgb_lerc.tif", [1670997934, 927535433, 1854570638]],
  ["rgb_lzma.tif", [1670997934, 927535433, 1854570638]],
  ["i16_lerc_deflate.tif", [40199496687]],
  ["i16_lerc_zstd.tif", [40199496687]],
  ["u16_jxl.tif", [42699017287]],
  ["f32_lerc.tif", [27721949.789093018], 247638],
  ["f32_lerc_lossy_cog.tif", [27721952.621803284], 247638],
  ["f32_jxl.tif", [27721949.789093018], 247638],
  ["f32_lzma.tif", [27721949.789093018], 247638],
];

for (const [name, expected, nans] of GTIFF_CODECS) {
  test(`gtiff_codecs/${name} reads as GDAL reads it`, { skip: !DATA && "TERRIO_TEST_DATA is not set" }, (t) => {
    const path = join(DATA, "gtiff_codecs", name);
    if (!existsSync(path)) return t.skip(`${name} is missing`);
    const ds = terrio.open(readFileSync(path), name);
    assert.equal(ds.count, expected.length);
    for (let b = 1; b <= ds.count; b++) {
      const band = ds.readBand(b);
      if (nans === undefined) {
        assert.equal(weightedSum(band), expected[b - 1], `${name} band ${b}`);
      } else {
        const valid = band.filter((v) => !Number.isNaN(v));
        assert.equal(band.length - valid.length, nans);
        const sum = valid.reduce((s, v) => s + v, 0);
        assert.ok(Math.abs(sum - expected[b - 1]) <= 1e-9 * expected[b - 1], `${name} band ${b}: ${sum}`);
      }
    }
  });
}

const VECTORS = [
  ["countries.geojson", 176],
  ["countries.kml", 176],
  ["countries.gml", 176],
  ["countries.gpkg", 176],
  // Vector tiles written by GDAL: features clipped to the tiles of zoom level 3, as GDAL reads them.
  ["countries_mvt.pmtiles", 279],
  ["countries_mvt.mbtiles", 279],
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

test("points from arrays, to LAS, LAZ and COPC bytes and back", () => {
  const n = 5000;
  const x = Float64Array.from({ length: n }, (_, i) => 500000 + (i % 100) * 1.25);
  const y = Float64Array.from({ length: n }, (_, i) => 4100000 + Math.floor(i / 100) * 1.5);
  const z = Float64Array.from({ length: n }, (_, i) => 100 + (i % 37) * 0.25);
  const classification = Uint8Array.from({ length: n }, (_, i) => (i % 3 === 0 ? 2 : 1));
  const intensity = Uint16Array.from({ length: n }, (_, i) => i);
  const height = Float32Array.from({ length: n }, (_, i) => i / 8);
  const pc = terrio.pointsFromArrays({ x, y, z, classification, intensity, height }, "EPSG:32610");
  assert.equal(pc.length, n);
  for (const name of ["a.las", "a.laz", "a.copc.laz"]) {
    const file = terrio.openPoints(pc.toBytes(name), name);
    assert.equal(file.format, { "a.las": "LAS", "a.laz": "LAZ", "a.copc.laz": "COPC" }[name]);
    assert.equal(file.crs, "EPSG:32610");
    assert.equal(file.pointCount, n);
    const back = file.read();
    assert.ok(back.dimensions.includes("height"));
    const sum = (a) => a.reduce((s, v) => s + Number(v), 0);
    assert.equal(sum(back.get("intensity")), sum(intensity));
    assert.ok(Math.abs(sum(back.get("z")) - sum(z)) < 1e-6);
    assert.ok(back.get("height") instanceof Float32Array);
  }
  // Filters while reading.
  const ground = terrio.readPoints(pc.toBytes("a.laz"), "a.laz", { classes: [2], bounds: [500000, 4100000, 500050, 4100030] });
  assert.ok(ground.length > 0 && ground.length < n / 3);
  assert.ok(ground.get("classification").every((c) => c === 2));
  // A DEM.
  const dem = pc.grid(2, { stat: ["min", "count"] });
  assert.equal(dem.kind, "raster");
  assert.equal(dem.count, 2);
  const counts = dem.readBand(2);
  assert.ok(counts.reduce((s, v) => s + v, 0) >= n);
  // A FlatGeobuf of points.
  const fgb = terrio.open(pc.toVectorBytes("points.fgb", ["z", "classification"]), "points.fgb");
  assert.equal(fgb.toGeoJSON().features.length, n);
});

test("PLY and E57 bytes, PDAL's filters and TIN gridding", () => {
  const n = 4000;
  const x = Float64Array.from({ length: n }, (_, i) => 500000 + (i % 80) * 0.5);
  const y = Float64Array.from({ length: n }, (_, i) => 4100000 + Math.floor(i / 80) * 0.5);
  const z = Float64Array.from({ length: n }, (_, i) => 100 + (i % 80) * 0.1);
  const returnNumber = new Uint8Array(n).fill(1);
  const numberOfReturns = new Uint8Array(n).fill(1);
  const pc = terrio.pointsFromArrays({ x, y, z, return_number: returnNumber, number_of_returns: numberOfReturns }, "EPSG:32610");
  for (const name of ["a.ply", "a.e57"]) {
    const back = terrio.readPoints(pc.toBytes(name), name);
    assert.equal(back.length, n);
    assert.deepEqual(Array.from(back.get("z").slice(0, 3)), Array.from(z.slice(0, 3)));
  }
  // A tilted plane: all ground.
  const ground = pc.smrf({ cell: 1 });
  assert.ok(ground.get("classification").every((c) => c === 2));
  assert.equal(pc.range("Z[100:101)").length, 10 * 50);
  assert.equal(pc.voxelDownsize(1, "first").length, 1066);
  assert.equal(pc.outlier({ method: "radius", radius: 0.6, minK: 1 }).get("classification").filter((c) => c === 7).length, 0);
  // Linear interpolation of a plane is exact.
  const tin = pc.grid(1, { method: "linear", radius: 0 });
  const band = tin.readBand(1);
  assert.ok(band.some((v) => v !== -9999));
});

test("COPC files read by octree level", { skip: !DATA && "TERRIO_TEST_DATA is not set" }, (t) => {
  const path = join(DATA, "pointcloud", "simple.copc.laz");
  if (!existsSync(path)) return t.skip("simple.copc.laz is missing");
  const file = terrio.openPoints(readFileSync(path), "simple.copc.laz");
  assert.equal(file.format, "COPC");
  // PDAL's readers.copc reads the same counts.
  assert.deepEqual([0, 1, 2, 3].map((l) => file.read({ maxLevel: l }).length), [24, 90, 287, 1065]);
  const info = file.info();
  assert.equal(info.copc.depth, 4);
  const laz = terrio.readPoints(readFileSync(join(DATA, "pointcloud", "autzen_trim.laz")), "autzen_trim.laz");
  assert.equal(laz.length, 110000);
  assert.equal(laz.get("x")[0], 637177.98);
});

// ─── Operations ─────────────────────────────────────────────────────────────────────────────────

const SKIP_DATA = !DATA && "TERRIO_TEST_DATA is not set";

/// Compares a raster with a GDAL-written reference: size, geotransform, pixel type, nodata and
/// pixels (identical, NaN equal to NaN, or within `tol`).
function sameRaster(got, refPath, { tol = 0, nodata = true } = {}) {
  const ref = terrio.open(readFileSync(refPath), refPath.split("/").pop());
  assert.deepEqual([got.width, got.height, got.count], [ref.width, ref.height, ref.count], refPath);
  assert.equal(got.dtype, ref.dtype, refPath);
  got.transform.forEach((v, i) => assert.ok(Math.abs(v - ref.transform[i]) <= 1e-9 * Math.max(1, Math.abs(v)), `${refPath}: transform ${got.transform} != ${ref.transform}`));
  // Float32 nodata values as GDAL writes them (3.402823466e+38 for the largest).
  const same = (a, b) => a === b || (got.dtype === "float32" && a !== undefined && b !== undefined && Math.fround(a) === Math.fround(b));
  if (nodata) assert.ok(same(got.nodata, ref.nodata), `${refPath}: nodata ${got.nodata} != ${ref.nodata}`);
  for (let b = 1; b <= ref.count; b++) {
    const a = got.readBand(b);
    const r = ref.readBand(b);
    let diff = 0;
    for (let i = 0; i < r.length; i++) {
      const x = a[i];
      const y = r[i];
      if (x === y || (x !== x && y !== y)) continue;
      if (tol > 0 && Math.abs(Number(x) - Number(y)) <= tol) continue;
      diff++;
    }
    assert.equal(diff, 0, `${refPath}: band ${b}, ${diff} of ${r.length} pixels differ`);
  }
}

test("operations on in-memory rasters and layers", () => {
  const data = new Float32Array(20 * 10).map((_, i) => (i % 20) * 10 + Math.floor(i / 20));
  const dem = terrio.rasterFromArray(data, 20, 10, [500000, 30, 0, 4100000, 0, -30], "EPSG:32610", -9999);
  const warped = terrio.warp(dem, { tSrs: "EPSG:4326", resampling: "bilinear" });
  assert.equal(warped.crs, "EPSG:4326");
  const t = terrio.translate(dem, { srcwin: [2, 3, 5, 4], ot: "Int16" });
  assert.deepEqual([t.width, t.height, t.dtype], [5, 4, "int16"]);
  assert.equal(t.readBand(1)[0], 23);
  const half = terrio.translate(dem, { outsize: ["50%", "50%"] });
  assert.deepEqual([half.width, half.height], [10, 5]);
  const hs = terrio.dem(dem, "hillshade", { z: 2 });
  assert.equal(hs.dtype, "uint8");
  const relief = terrio.dem(dem, "color-relief", { colorTable: "0 0 0 255\n300 255 0 0\n", alpha: true });
  assert.equal(relief.count, 4);
  const sum = terrio.calc("A + B * 2", { A: dem, B: dem }, { type: "Float32" });
  assert.equal(sum.readBand(1)[21], data[21] * 3);
  assert.throws(() => terrio.calc("A", { A: dem }, { bands: { Z: 1 } }), /not an input/);
  const parts = [terrio.translate(dem, { srcwin: [0, 0, 10, 10] }), terrio.translate(dem, { srcwin: [10, 0, 10, 10] })];
  const merged = terrio.mosaic(parts, {});
  assert.deepEqual([...merged.readBand(1)], [...dem.readBand(1)]);
  // Bytes as inputs, and the input left usable.
  const fromBytes = terrio.warp([dem.toBytes("a.tif"), dem], { tSrs: "EPSG:32610" });
  assert.equal(fromBytes.width, 20);
  assert.equal(dem.width, 20);

  const mask = terrio.calc("A > 150", { A: dem }, { type: "Byte", nodata: "none" });
  const prox = terrio.proximity(mask, { distunits: "GEO" });
  assert.equal(prox.dtype, "float32");
  const sieved = terrio.sieve(mask, { threshold: 3, connectedness: 8 });
  assert.equal(sieved.width, 20);
  const polys = terrio.polygonize(mask, { field: "v" });
  assert.equal(polys.kind, "vector");
  const fc = polys.toGeoJSON();
  assert.ok(fc.features.length >= 2);
  assert.ok("v" in fc.features[0].properties);
  // Rasterize GeoJSON, a dataset or bytes; cut a raster with GeoJSON polygons.
  const squares = terrio.fromGeoJSON(SQUARES);
  for (const src of [SQUARES, squares, squares.toBytes("s.fgb")]) {
    const r = terrio.rasterize(src, { attribute: "value", tr: [1, 1], te: [0, 0, 30, 10], ot: "Byte" });
    assert.deepEqual([r.width, r.height], [30, 10]);
    assert.equal(r.readBand(1)[0], 3);
    assert.equal(r.readBand(1)[29], 7);
  }
  const like = terrio.rasterize(SQUARES, { burn: [5], like: terrio.rasterize(SQUARES, { burn: 1, ts: [60, 20] }) });
  assert.deepEqual([like.width, like.height], [60, 20]);
  const cut = terrio.warp(terrio.rasterize(SQUARES, { burn: 1, tr: [1, 1], te: [0, 0, 30, 10], ot: "Byte", init: 9 }), {
    cutline: { type: "Polygon", coordinates: [[[0, 0], [5, 0], [5, 5], [0, 5], [0, 0]]] },
    cropToCutline: true,
  });
  assert.deepEqual([cut.width, cut.height], [5, 5]);
  assert.throws(() => terrio.warp(dem, { unknown: 1 }), /unknown field/);
  assert.throws(() => terrio.dem(dem, "color-relief"), /colorTable/);
  assert.throws(() => terrio.translate(squares, {}), /vector dataset/);
});

const GDAL_OPS = [
  ["translate srcwin", "translate_srcwin.tif", (r) => terrio.translate(r("usgs_dem.tif"), { srcwin: [100, 200, 500, 400] })],
  ["translate outsize", "translate_outsize.tif", (r) => terrio.translate(r("las_vegas_dem.tif"), { outsize: ["25%", "25%"] })],
  ["translate bands and type", "translate_bands.tif", (r) => terrio.translate(r("landsat.tif"), { bands: [3, 1], ot: "Int16" })],
  ["translate scale", "translate_scale.tif", (r) => terrio.translate(r("usgs_dem.tif"), { ot: "Byte", scale: [1000, 4000, 0, 255] })],
  ["warp to EPSG:4326 with nodata", "warp_4326.tif", (r) => terrio.warp(r("las_vegas_dem.tif"), { tSrs: "EPSG:4326", srcNodata: 0, dstNodata: -1 })],
  ["warp to EPSG:3857", "warp_3857.tif", (r) => terrio.warp(r("usgs_dem.tif"), { tSrs: "EPSG:3857" })],
  [
    "warp with a cutline",
    "warp_cutline.tif",
    (r) => terrio.warp(r("las_vegas_dem.tif"), { cutline: JSON.parse(readFileSync(join(DATA, "wasm_refs", "cutline.geojson"), "utf8")), cropToCutline: true, dstNodata: -9999 }),
  ],
  ["mosaic", "mosaic.tif", (r) => terrio.mosaic([r("wasm_refs/part2.tif"), r("wasm_refs/part1.tif")])],
  ...["hillshade", "slope", "aspect", "TRI", "TPI", "roughness"].map((m) => [`dem ${m}`, `dem_${m}.tif`, (r) => terrio.dem(r("srtm_sub.tif"), m)]),
  ["dem multidirectional hillshade", "dem_hillshade_multi.tif", (r) => terrio.dem(r("srtm_sub.tif"), "hillshade", { multidirectional: true, computeEdges: true })],
  ["dem slope ZevenbergenThorne", "dem_slope_zt.tif", (r) => terrio.dem(r("srtm_sub.tif"), "slope", { percent: true, alg: "ZevenbergenThorne" })],
  ["dem color relief", "dem_color.tif", (r) => terrio.dem(r("srtm_sub.tif"), "color-relief", { colorTable: readFileSync(join(DATA, "wasm_refs", "colors.txt"), "utf8"), alpha: true })],
  [
    "calc NDVI",
    "calc_ndvi.tif",
    (r) => {
      const l = r("landsat.tif");
      return terrio.calc("(B.astype(float32) - A) / (B.astype(float32) + A)", { A: l, B: l }, { bands: { A: 3, B: 4 }, type: "Float32" });
    },
  ],
  ["calc Int16 wrap-around", "calc_wrap.tif", (r) => terrio.calc("A * 20", { A: r("srtm_sub.tif") })],
  ["proximity", "proximity.tif", (r) => terrio.proximity(r("nlcd_knoxville.tif"), { values: [11] })],
  ["sieve", "sieve.tif", (r) => terrio.sieve(r("nlcd_knoxville.tif"), { threshold: 10 })],
  ["rasterize burn", "rasterize_burn.tif", (r) => terrio.rasterize(readFileSync(join(DATA, "countries.geojson")), { burn: 1, tr: [0.1, 0.1], ot: "Byte" })],
  ["rasterize attribute", "rasterize_attribute.tif", (r) => terrio.rasterize({ bytes: readFileSync(join(DATA, "TN_Counties.geojson")), filename: "TN_Counties.geojson" }, { attribute: "POPULATION", tr: [0.002, 0.002], ot: "Int32" })],
];

for (const [name, ref, run, options] of GDAL_OPS) {
  test(`${name}: GDAL's pixels`, { skip: SKIP_DATA }, (t) => {
    const refPath = join(DATA, "wasm_refs", ref);
    if (!existsSync(refPath)) return t.skip("the GDAL references are missing: run scripts/fetch_test_data.sh with GDAL");
    const cache = new Map();
    const read = (f) => {
      if (!cache.has(f)) cache.set(f, terrio.open(readFileSync(join(DATA, f)), f.split("/").pop()));
      return cache.get(f);
    };
    sameRaster(run(read), refPath, options);
  });
}

test("polygonize: GDAL's polygons", { skip: SKIP_DATA }, (t) => {
  const refPath = join(DATA, "wasm_refs", "polygonize.fgb");
  if (!existsSync(refPath)) return t.skip("the GDAL references are missing");
  const nlcd = terrio.open(readFileSync(join(DATA, "nlcd_knoxville.tif")), "nlcd_knoxville.tif");
  const mine = terrio.polygonize(nlcd).toGeoJSON().features;
  const gdal = terrio.open(readFileSync(refPath), "polygonize.fgb").toGeoJSON().features;
  assert.equal(mine.length, gdal.length);
  const perValue = (fs) => {
    const m = new Map();
    for (const f of fs) m.set(f.properties.DN, (m.get(f.properties.DN) ?? 0) + 1);
    return [...m.entries()].sort((a, b) => a[0] - b[0]);
  };
  assert.deepEqual(perValue(mine), perValue(gdal));
});

test("a FlatGeobuf larger than 100 MB", { skip: SKIP_DATA }, (t) => {
  const path = join(DATA, "big_counties.fgb");
  if (!existsSync(path)) return t.skip("big_counties.fgb is missing");
  const bytes = readFileSync(path);
  assert.ok(bytes.length > 100e6);
  const ds = terrio.open(bytes, "big_counties.fgb");
  const info = ds.info();
  assert.equal(info.layers[0].feature_count, 9 * 3221);
  const sub = terrio.open(ds.toBytes("sub.fgb", { limit: 5 }), "sub.fgb");
  assert.equal(sub.toGeoJSON().features.length, 5);
});

// ─── Remote files ───────────────────────────────────────────────────────────────────────────────

/// Serves a directory over HTTP with range requests, counting the bytes sent.
async function serve(dir) {
  const { createServer } = await import("node:http");
  const { statSync, openSync, readSync, closeSync } = await import("node:fs");
  const stats = { requests: 0, bytes: 0 };
  const server = createServer((req, res) => {
    const path = join(dir, decodeURIComponent(req.url.split("?")[0]));
    let size;
    try {
      size = statSync(path).size;
    } catch {
      res.writeHead(404);
      return res.end();
    }
    stats.requests++;
    const m = /bytes=(\d+)-(\d+)?/.exec(req.headers.range ?? "");
    if (req.method === "HEAD" || !m) {
      res.writeHead(200, { "Content-Length": size });
      if (req.method === "HEAD") return res.end();
      stats.bytes += size;
      return res.end(readFileSync(path));
    }
    const a = +m[1];
    const b = Math.min(m[2] ? +m[2] : size - 1, size - 1);
    const buf = Buffer.alloc(b - a + 1);
    const fd = openSync(path, "r");
    readSync(fd, buf, 0, buf.length, a);
    closeSync(fd);
    stats.bytes += buf.length;
    res.writeHead(206, { "Content-Range": `bytes ${a}-${b}/${size}`, "Content-Length": buf.length });
    res.end(buf);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return { url: `http://127.0.0.1:${server.address().port}/`, stats, close: () => server.close() };
}

test("remote reads send at most 12 range requests at once", async () => {
  const { mkdtempSync, writeFileSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const dir = mkdtempSync(join(tmpdir(), "terrio-remote-"));
  // 30,000 small squares: a strip across them is scattered over the file (Hilbert order).
  const grid = { type: "FeatureCollection", features: [] };
  for (let i = 0; i < 200; i++) {
    for (let j = 0; j < 150; j++) {
      const [x, y] = [-180 + i * 1.8, -90 + j * 1.2];
      grid.features.push({ type: "Feature", properties: { id: i * 150 + j }, geometry: { type: "Polygon", coordinates: [[[x, y], [x + 1, y], [x + 1, y + 1], [x, y + 1], [x, y]]] } });
    }
  }
  writeFileSync(join(dir, "grid.fgb"), terrio.fromGeoJSON(grid, "grid").toBytes("grid.fgb"));
  const server = await serve(dir);
  try {
    let [inFlight, peak] = [0, 0];
    const counting = (...args) => {
      peak = Math.max(peak, ++inFlight);
      return fetch(...args).finally(() => inFlight--);
    };
    const ds = await terrio.openUrl(server.url + "grid.fgb", { blockSize: 512, fetch: counting });
    const ids = (await ds.read({ bbox: [-180, 0, 180, 0.5] })).toGeoJSON().features.map((f) => f.properties.id);
    const expected = grid.features
      .filter((f) => f.geometry.coordinates[0][0][1] <= 0.5 && f.geometry.coordinates[0][2][1] >= 0)
      .map((f) => f.properties.id);
    assert.equal(expected.length, 200);
    assert.deepEqual(ids.sort((a, b) => a - b), expected.sort((a, b) => a - b));
    assert.ok(ds.stats().requests > 12, `${ds.stats().requests} requests`);
    assert.ok(peak <= 12, `${peak} requests at once`);
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});

test("remote files read with range requests", async () => {
  const { mkdtempSync, writeFileSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const dir = mkdtempSync(join(tmpdir(), "terrio-remote-"));
  // A COG, a FlatGeobuf, a GeoParquet file, PMTiles and COPC, written by terrio.
  const data = new Float32Array(1500 * 1200).map((_, i) => i % 7919);
  const dem = terrio.rasterFromArray(data, 1500, 1200, [500000, 10, 0, 4100000, 0, -10], "EPSG:32610", -1);
  writeFileSync(join(dir, "dem.tif"), dem.toBytes("dem.tif", { format: "COG", compress: "DEFLATE" }));
  const cells = { type: "FeatureCollection", features: [] };
  for (let i = 0; i < 50; i++) {
    for (let j = 0; j < 30; j++) {
      const [x, y] = [-170 + i * 6, -80 + j * 5];
      cells.features.push({ type: "Feature", properties: { id: i * 30 + j }, geometry: { type: "Polygon", coordinates: [[[x, y], [x + 4, y], [x + 4, y + 4], [x, y + 4], [x, y]]] } });
    }
  }
  const layer = terrio.fromGeoJSON(cells, "cells");
  writeFileSync(join(dir, "cells.fgb"), layer.toBytes("cells.fgb"));
  writeFileSync(join(dir, "cells.parquet"), layer.toBytes("cells.parquet"));
  writeFileSync(join(dir, "cells.pmtiles"), layer.toBytes("cells.pmtiles", { maxzoom: 4 }));
  const n = 20000;
  const pts = { x: new Float64Array(n), y: new Float64Array(n), z: new Float64Array(n) };
  for (let i = 0; i < n; i++) [pts.x[i], pts.y[i], pts.z[i]] = [(i * 7919) % 1000, (i * 104729) % 1000, i % 50];
  writeFileSync(join(dir, "pts.copc.laz"), terrio.pointsFromArrays(pts, "EPSG:32610").toBytes("pts.copc.laz"));
  const server = await serve(dir);
  try {
    const cog = await terrio.openUrl(server.url + "dem.tif", { blockSize: 16384 });
    assert.deepEqual([cog.kind, cog.format, cog.width, cog.height, cog.crs], ["raster", "GTiff", 1500, 1200, "EPSG:32610"]);
    assert.ok(cog.overviews.length > 0);
    const w = await cog.readBand(1, [600, 500, 100, 80]);
    assert.ok(w instanceof Float32Array);
    assert.deepEqual([...w], [...dem.readBand(1, [600, 500, 100, 80])]);
    // The headers and the two tiles under the window.
    assert.ok(cog.stats().bytes < cog.size / 3, `${cog.stats().bytes} of ${cog.size} bytes`);
    const ovr = await cog.readBand(1, undefined, 0);
    assert.equal(ovr.length, cog.overviews[0][0] * cog.overviews[0][1]);
    const sub = await cog.read({ window: [0, 0, 64, 32] });
    assert.deepEqual([sub.kind, sub.width, sub.height], ["raster", 64, 32]);
    // Reads awaited together, with a cache too small to keep their blocks: they run in turn, so
    // that one read's trimming does not evict the blocks another fetched and still needs.
    const busy = await terrio.openUrl(server.url + "dem.tif", { blockSize: 4096, cacheSize: 4096 });
    const windows = [[0, 0, 300, 200], [1200, 1000, 300, 200], [600, 500, 100, 80], [50, 900, 200, 250]];
    const reads = await Promise.all(windows.map((w) => busy.readBand(1, w)));
    windows.forEach((w, k) => assert.deepEqual([...reads[k]], [...dem.readBand(1, w)], `window ${w}`));

    const fgb = await terrio.openUrl(server.url + "cells.fgb");
    assert.equal(fgb.info().feature_count, 1500);
    const before = fgb.stats().bytes;
    // The squares that intersect the box, edges included (as ogr2ogr -spat keeps them).
    const expected = cells.features
      .filter((f) => {
        const [[x0, y0], , [x1, y1]] = f.geometry.coordinates[0];
        return x0 <= 10 && x1 >= 0 && y0 <= 10 && y1 >= 0;
      })
      .map((f) => f.properties.id);
    assert.equal(expected.length, 9);
    const hits = (await fgb.read({ bbox: [0, 0, 10, 10] })).toGeoJSON().features;
    assert.deepEqual(hits.map((f) => f.properties.id).sort((a, b) => a - b), expected);
    assert.ok(fgb.stats().bytes - before < fgb.size / 4);

    const pq = await terrio.openUrl(server.url + "cells.parquet");
    const groups = pq.info().rowGroups;
    assert.equal(groups.reduce((s, g) => s + g.rows, 0), 1500);
    const last = await pq.read({ rowGroups: [groups.length - 1] });
    assert.equal(last.toGeoJSON().features.length, groups.at(-1).rows);
    const inBox = (await pq.read({ bbox: [0, 0, 10, 10] })).toGeoJSON().features;
    assert.deepEqual(inBox.map((f) => f.properties.id).sort((a, b) => a - b), expected);

    const tiles = await terrio.openUrl(server.url + "cells.pmtiles");
    assert.equal(tiles.kind, "tiles");
    const tile = await tiles.getTile(0, 0, 0);
    assert.ok(tile instanceof Uint8Array && tile.length > 0);
    assert.equal(await tiles.getTile(4, 15, 0), undefined);
    const vt = await tiles.read({ zoom: 4, bbox: [0, 0, 1e6, 1e6] });
    assert.ok(vt.toGeoJSON().features.length > 0);

    const copc = await terrio.openUrl(server.url + "pts.copc.laz");
    assert.deepEqual([copc.kind, copc.format, copc.crs], ["pointcloud", "COPC", "EPSG:32610"]);
    const box = await copc.read({ bounds: [100, 100, 300, 300] });
    const all = terrio.readPoints(readFileSync(join(dir, "pts.copc.laz")), "pts.copc.laz", { bounds: [100, 100, 300, 300] });
    assert.equal(box.length, all.length);
    const coarse = await copc.read({ maxLevel: 0 });
    assert.equal(coarse.length, terrio.readPoints(readFileSync(join(dir, "pts.copc.laz")), "pts.copc.laz", { maxLevel: 0 }).length);

    await assert.rejects(terrio.openUrl(server.url + "missing.tif"), /404/);
    // The requests the server saw are those terrio counted.
    const counted = [cog, busy, fgb, pq, tiles, copc].reduce((s, d) => s + d.stats().bytes, 0);
    assert.equal(counted, server.stats.bytes);
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});

test("remote real files read with range requests, as GDAL reads them", { skip: SKIP_DATA }, async (t) => {
  if (!existsSync(join(DATA, "ne_places_rg.parquet"))) return t.skip("ne_places_rg.parquet is missing");
  const server = await serve(DATA);
  try {
    const cog = await terrio.openUrl(server.url + "usgs_dem.tif");
    const local = terrio.open(readFileSync(join(DATA, "usgs_dem.tif")), "usgs_dem.tif");
    const w = await cog.readBand(1, [1500, 1500, 512, 512]);
    assert.deepEqual([...w], [...local.readBand(1, [1500, 1500, 512, 512])]);
    const o = await cog.readBand(1, [0, 0, 400, 400], 1);
    assert.deepEqual([...o], [...local.readBand(1, [0, 0, 400, 400], 1)]);
    assert.ok(cog.stats().bytes < cog.size / 8, `${cog.stats().bytes} of ${cog.size}`);
    // GDAL's -spat results.
    const fgb = await terrio.openUrl(server.url + "UScounties.fgb");
    const ids = (await fgb.read({ bbox: [-100, 35, -95, 40] })).toGeoJSON().features.length;
    assert.equal(ids, 138);
    assert.ok(fgb.stats().bytes < fgb.size / 4);
    const pq = await terrio.openUrl(server.url + "ne_places_rg.parquet");
    assert.deepEqual(pq.info().rowGroups.map((g) => g.rows), [1000, 1000, 1000, 1000, 1000, 1000, 1000, 342]);
    assert.equal((await pq.read({ bbox: [0, 40, 20, 55] })).toGeoJSON().features.length, 306);
    const copc = await terrio.openUrl(server.url + "pointcloud/madison.copc.laz");
    const h = copc.info();
    const [cx, cy] = [(h.min[0] + h.max[0]) / 2, (h.min[1] + h.max[1]) / 2];
    const opts = { bounds: [cx - 100, cy - 100, cx + 100, cy + 100], maxLevel: 3 };
    const remote = await copc.read(opts);
    const whole = terrio.readPoints(readFileSync(join(DATA, "pointcloud", "madison.copc.laz")), "madison.copc.laz", opts);
    assert.equal(remote.length, whole.length);
    assert.deepEqual([...remote.get("z").slice(0, 100)], [...whole.get("z").slice(0, 100)]);
    assert.ok(copc.stats().bytes < copc.size / 3);
    const pm = await terrio.openUrl(server.url + "cb_2018_us_zcta510_500k.pmtiles");
    const vt = await pm.read({ zoom: 7, bbox: [-9000000, 4000000, -8900000, 4100000] });
    assert.ok(vt.toGeoJSON().features.length > 0);
    assert.ok(pm.stats().bytes < pm.size / 10);
  } finally {
    server.close();
  }
});

// Public files (opt in with TERRIO_NETWORK_TESTS=1): the bytes fetched show that only the ranges
// needed are read.
test("remote public files", { skip: !process.env.TERRIO_NETWORK_TESTS && "TERRIO_NETWORK_TESTS is not set" }, async () => {
  const cog = await terrio.openUrl("https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif");
  assert.equal((await cog.readBand(1, [1500, 1500, 512, 512])).length, 512 * 512);
  assert.equal((await cog.readBand(1, undefined, 2)).length, cog.overviews[2][0] * cog.overviews[2][1]);
  const pm = await terrio.openUrl("https://r2-public.protomaps.com/protomaps-sample-datasets/cb_2018_us_zcta510_500k.pmtiles");
  assert.ok((await pm.getTile(7, 37, 48)).length > 0);
  const copc = await terrio.openUrl("https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz");
  assert.ok((await copc.read({ maxLevel: 1 })).length > 0);
  const fgb = await terrio.openUrl("https://flatgeobuf.org/test/data/UScounties.fgb");
  assert.equal((await fgb.read({ bbox: [-100, 35, -95, 40] })).toGeoJSON().features.length, 138);
  for (const d of [cog, pm, copc, fgb]) {
    const { requests, bytes } = d.stats();
    console.log(`${d.url}: ${requests} requests, ${bytes} of ${d.size} bytes (${((100 * bytes) / d.size).toFixed(2)}%)`);
    assert.ok(bytes < d.size / 4);
  }
});

// ─── More formats in memory ─────────────────────────────────────────────────────────────────────

test("netCDF, HDF5 and Zarr in memory; LERC, JPEG XL and JPEG GeoTIFFs", () => {
  const data = new Int16Array(2 * 30 * 20).map((_, i) => (i * 37) % 2000);
  const ds = terrio.rasterFromArray(data, 30, 20, [500000, 30, 0, 4100000, 0, -30], "EPSG:32610", -9999);
  for (const [name, options] of [
    ["x.nc", {}],
    ["x.nc", { creationOptions: { FORMAT: "NC4", COMPRESS: "DEFLATE" } }],
    ["x.zarr", {}],
    ["x.zarr", { creationOptions: { FORMAT: "ZARR_V3" }, compress: "ZSTD" }],
  ]) {
    const bytes = ds.toBytes(name, options);
    // Zarr stores come back zipped.
    const back = terrio.open(bytes, name.endsWith(".zarr") ? "x.zip" : name);
    assert.deepEqual([back.width, back.height, back.count, back.crs], [30, 20, 2, "EPSG:32610"], name);
    assert.deepEqual([...back.transform], [500000, 30, 0, 4100000, 0, -30], name);
    assert.deepEqual([...back.readBand(2)], [...data.slice(600)], name);
  }
  // A netCDF-4 file is HDF5: read through the HDF5 driver by its name.
  const nc4 = ds.toBytes("x.nc", { creationOptions: { FORMAT: "NC4" } });
  assert.equal(terrio.open(nc4, "x.h5").width, 30);
  // Lossless GeoTIFF compressions, and JPEG.
  const bytes8 = new Uint8Array(3 * 64 * 48).map((_, i) => (i % 64) * 2 + Math.floor(i / 64) % 48);
  const rgb = terrio.rasterFromArray(bytes8, 64, 48, [0, 1, 0, 48, 0, -1], "EPSG:3857");
  for (const options of [{ compress: "LERC" }, { compress: "LERC_ZSTD", tileSize: 16 }, { compress: "JXL" }, { format: "COG", compress: "LERC_DEFLATE" }]) {
    const back = terrio.open(rgb.toBytes("c.tif", options), "c.tif");
    for (let b = 1; b <= 3; b++) assert.deepEqual([...back.readBand(b)], [...rgb.readBand(b)], JSON.stringify(options));
  }
  const floats = terrio.rasterFromArray(new Float32Array(40 * 30).map((_, i) => Math.sin(i) * 100), 40, 30, [0, 1, 0, 30, 0, -1], "EPSG:4326");
  const lossy = terrio.open(floats.toBytes("f.tif", { compress: "LERC", creationOptions: { MAX_Z_ERROR: 0.01 } }), "f.tif").readBand(1);
  const exact = floats.readBand(1);
  assert.ok(lossy.every((v, i) => Math.abs(v - exact[i]) <= 0.01 + 1e-6));
  const jpeg = terrio.open(rgb.toBytes("j.tif", { compress: "JPEG", creationOptions: { JPEG_QUALITY: 90 } }), "j.tif");
  const [a, b] = [jpeg.readBand(1), rgb.readBand(1)];
  const meanError = a.reduce((s, v, i) => s + Math.abs(v - b[i]), 0) / a.length;
  assert.ok(meanError < 3, `JPEG mean error ${meanError}`);
  assert.throws(() => floats.toBytes("j.tif", { compress: "JPEG" }), /JPEG is written for/);
});

test("Zarr stores and netCDF/HDF5 products as GDAL reads them", { skip: SKIP_DATA }, async (t) => {
  const { readdirSync, statSync } = await import("node:fs");
  const srtm = terrio.open(readFileSync(join(DATA, "srtm_sub.tif")), "srtm_sub.tif");
  const same = (ds, label) => {
    assert.deepEqual([ds.width, ds.height, ds.crs], [srtm.width, srtm.height, srtm.crs], label);
    ds.transform.forEach((v, i) => assert.ok(Math.abs(v - srtm.transform[i]) < 1e-9, `${label} transform`));
    assert.deepEqual([...ds.readBand(1)], [...srtm.readBand(1)], label);
  };
  same(terrio.open(readFileSync(join(DATA, "srtm_sub.nc")), "srtm_sub.nc"), "srtm_sub.nc");
  // Zarr stores written by GDAL, as key-value stores (v2 Blosc, v3 Zstandard).
  const store = (dir) => {
    const m = new Map();
    const walk = (d, prefix) => {
      for (const f of readdirSync(d)) {
        const p = join(d, f);
        if (statSync(p).isDirectory()) walk(p, `${prefix}${f}/`);
        else m.set(`${prefix}${f}`, readFileSync(p));
      }
    };
    walk(dir, "");
    return m;
  };
  for (const z of ["srtm_sub_v3.zarr", "srtm_sub_blosc.zarr"]) {
    if (existsSync(join(DATA, z))) same(terrio.openZarr(store(join(DATA, z))), z);
  }
  // Subdatasets of an HDF5 (JPSS ATMS, with a user block) and a netCDF-4 product (Sentinel-5P).
  const refs = [
    ["h5_bt.tif", "SATMS_j01_d20230101_t0000026_e0000343_b26525_c20230101002941202701_oeac_ops.h5", "HDF5:\"x.h5\"://All_Data/ATMS-SDR_All/BrightnessTemperature"],
    ["nc4_methane.tif", "S5P_OFFL_L2__CH4____20230101T000319_20230101T014449_27036_03_020400_20230103T110255.nc", "NETCDF:\"x.nc\":/PRODUCT/methane_mixing_ratio"],
  ];
  for (const [ref, file, name] of refs) {
    if (!existsSync(join(DATA, "wasm_refs", ref)) || !existsSync(join(DATA, file))) {
      t.diagnostic(`${ref} or ${file} is missing`);
      continue;
    }
    const ds = terrio.open(readFileSync(join(DATA, file)), name);
    const gdal = terrio.open(readFileSync(join(DATA, "wasm_refs", ref)), ref);
    assert.deepEqual([ds.width, ds.height, ds.count, ds.dtype], [gdal.width, gdal.height, gdal.count, gdal.dtype], ref);
    for (let b = 1; b <= gdal.count; b++) {
      const [x, y] = [ds.readBand(b), gdal.readBand(b)];
      assert.ok(x.every((v, i) => v === y[i] || (v !== v && y[i] !== y[i])), `${ref} band ${b}`);
    }
  }
});
