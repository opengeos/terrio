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
  // The other filters: ground and others; heights above an all-ground plane are 0; its normals
  // point up.
  const cloth = pc.csf(2).get("classification");
  assert.ok(cloth.every((c) => c === 1 || c === 2) && cloth.filter((c) => c === 2).length > 0);
  const morph = pc.pmf().get("classification");
  assert.ok(morph.every((c) => c === 1 || c === 2) && morph.filter((c) => c === 2).length > 0);
  assert.ok(ground.hagNn().get("height_above_ground").every((h) => h === 0));
  assert.ok(ground.hagDelaunay().get("height_above_ground").every((h) => h === 0));
  assert.equal(pc.elm().get("classification").filter((c) => c === 7).length, 0);
  assert.equal(pc.crop("([500000, 500009.9], [4100000, 4100009.9])").length, 400);
  assert.equal(pc.crop("POLYGON((500000 4100000, 500010 4100000, 500000 4100010, 500000 4100000))").length, 231);
  assert.equal(pc.decimation(4).length, n / 4);
  assert.equal(pc.expression("Z >= 100 && Z < 101").length, 10 * 50);
  assert.ok(pc.assign("Intensity = 7 WHERE Z < 101").get("intensity").filter((v) => v === 7).length === 10 * 50);
  const normals = pc.normal().get("normal_z");
  assert.ok(normals.every((v) => Math.abs(v - 1 / Math.sqrt(1.04)) < 1e-6));
  assert.ok(pc.cluster(0.6).get("cluster_id").every((c) => c === 1n));
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
