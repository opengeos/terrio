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
// TERRIO_JS_LOAD: the npm package's `load.js` (js/pkg, built by js/build.mjs), whose module the
// tests run on instead, the threaded one in Node.js (TERRIO_THREADS threads, 4 by default), or
// with TERRIO_MEMORY64=1 the memory64 one.
const terrio = process.env.TERRIO_JS_LOAD
  ? await (await import(new URL(process.env.TERRIO_JS_LOAD, import.meta.url).href)).load({
      threads: Number(process.env.TERRIO_THREADS ?? 4),
      memory64: process.env.TERRIO_MEMORY64 === "1",
      // The memory64 module was asked for: no other module stands in for it.
      onFallback: (e) => {
        if (process.env.TERRIO_MEMORY64 === "1") throw new Error(`memory64 was requested: ${e}`);
      },
    })
  : require(process.env.TERRIO_JS_PKG ?? "../pkg-node/terrio.js");
const DATA = process.env.TERRIO_TEST_DATA;

// The lighter build (`terrio-js/core`) leaves formats out: the tests that need them are skipped
// there (TERRIO_JS_PKG=../pkg-node/core/terrio.js runs the tests on it).
const { build: BUILD, features: BUILT } = terrio.features();
/** `false` when the module has these cargo features, otherwise why a test is skipped. */
function needs(...features) {
  const missing = features.filter((f) => !BUILT.includes(f));
  return missing.length > 0 && `${BUILD} is built without ${missing.join(", ")}`;
}

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

test("the module's build and its formats", () => {
  assert.ok(["flatgeobuf", "sqlite", "tiles", "lzma", "lerc"].every((f) => BUILT.includes(f)), BUILT.join());
  if (BUILD === "terrio-js/threads") assert.ok(terrio.features().threads > 1, "the thread pool is started");
  assert.ok(terrio.features().memory > 0);
  if (BUILD !== "terrio-js/core") {
    assert.ok(["jp2", "netcdf-hdf5", "parquet", "pointcloud", "zarr"].every((f) => BUILT.includes(f)), BUILT.join());
  } else {
    assert.equal(BUILD, "terrio-js/core");
    // The formats left out say which module has them.
    const xyz = { x: new Float64Array(1), y: new Float64Array(1), z: new Float64Array(1) };
    assert.throws(() => terrio.pointsFromArrays(xyz), /terrio-js\/core leaves it out, the full module \(terrio-js\) has it/);
    assert.throws(() => terrio.fromGeoJSON(SQUARES).toArrowIPC(), /without the `parquet` feature/);
    const tif = terrio.rasterFromArray(new Uint8Array(4), 2, 2, [0, 1, 0, 2, 0, -1]);
    assert.throws(() => tif.toBytes("x.jp2"), /without the `jp2` feature/);
  }
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

test("Zstandard GeoTIFFs with predictors, decoded without the C library", () => {
  // Tiled and in strips, interleaved bands, the horizontal and floating-point predictors.
  const ints = new Int16Array(3 * 70 * 50).map((_, i) => ((i * 37) % 2001) - 1000);
  const floats = new Float32Array(70 * 50).map((_, i) => Math.sin(i / 7) * 1000 + i / 3);
  const cases = [
    [terrio.rasterFromArray(ints, 70, 50, [0, 1, 0, 50, 0, -1], "EPSG:4326"), "2", { tileSize: 16 }],
    [terrio.rasterFromArray(ints, 70, 50, [0, 1, 0, 50, 0, -1], "EPSG:4326"), "2", { creationOptions: { INTERLEAVE: "PIXEL" } }],
    [terrio.rasterFromArray(floats, 70, 50, [0, 1, 0, 50, 0, -1], "EPSG:4326"), "3", { tileSize: 32 }],
  ];
  for (const [ds, predictor, extra] of cases) {
    const options = { ...extra, compress: "ZSTD", creationOptions: { ...extra.creationOptions, PREDICTOR: predictor } };
    const back = terrio.open(ds.toBytes("z.tif", options), "z.tif");
    for (let b = 1; b <= ds.count; b++) {
      assert.deepEqual([...back.readBand(b)], [...ds.readBand(b)], `predictor ${predictor}, band ${b}`);
    }
  }
});

test("JPEG 2000 to bytes and back", { skip: needs("jp2") }, () => {
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

test("vector layers as Arrow IPC streams, and back", { skip: needs("parquet") }, () => {
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
  // PROJ 9.8.1 resolves the authority version separately from the EPSG code.
  for (const reference of [
    "urn:ogc:def:crs:EPSG:6.6:4326",
    "urn:ogc:def:crs:EPSG:9.9:4326",
    "http://www.opengis.net/def/crs/EPSG/0/4326",
    "https://opengis.net/def/crs/EPSG/6.6/4326",
  ]) {
    assert.equal(terrio.describeCrs(reference).epsg, 4326);
    const [x, y] = terrio.transformPoints([10], [40], reference, "EPSG:3857");
    assert.ok(Math.abs(x[0] - 1113194.9079327357) < 1e-7);
    assert.ok(Math.abs(y[0] - 4865942.279503175) < 1e-7);
  }
});

test("CRS benchmark projection regressions", () => {
  const reference = JSON.parse(readFileSync(new URL("../../crates/terrio-crs/tests/data/crs_benchmark.json", import.meta.url), "utf8"));
  for (const p of reference.projections) {
    const definition = p.proj + (p.id === "wintri" ? ` +lat_1=${Math.acos(2 / Math.PI) * 180 / Math.PI}` : "");
    const forward = p.points.flatMap((ll, i) => p.fwd[i] === null ? [] : [{ ll, xy: p.fwd[i], inv: p.inv[i] }]);
    const [xs, ys] = terrio.transformPoints(forward.map((p) => p.ll[0]), forward.map((p) => p.ll[1]), p.geographic, definition);
    forward.forEach((q, i) => assert.ok(Math.hypot(xs[i] - q.xy[0], ys[i] - q.xy[1]) < 1e-3, `${p.id} forward ${q.ll}`));
    p.points.forEach((ll, i) => {
      if (p.fwd[i] === null) {
        assert.throws(() => terrio.transformPoints([ll[0]], [ll[1]], p.geographic, definition), undefined, `${p.id} hidden ${ll}`);
      } else if (p.inv[i] === null) {
        let recovered;
        try {
          const [lo, la] = terrio.transformPoints([p.fwd[i][0]], [p.fwd[i][1]], definition, p.geographic);
          recovered = [lo[0], la[0]];
        } catch {
          return; // Refusing the valid boundary is also allowed by the reference contract.
        }
        const dl = ((recovered[0] - ll[0] + 540) % 360 - 180) * Math.cos(ll[1] * Math.PI / 180);
        const tolerance = Math.abs(ll[1]) === 90 ? 1e-6 : 1e-7;
        assert.ok(Math.hypot(dl, recovered[1] - ll[1]) < tolerance, `${p.id} recovered boundary ${ll}`);
      }
    });
    const inverse = forward.filter((p) => p.inv !== null);
    const [lons, lats] = terrio.transformPoints(inverse.map((p) => p.xy[0]), inverse.map((p) => p.xy[1]), definition, p.geographic);
    inverse.forEach((q, i) => {
      const dlon = ((lons[i] - q.inv[0] + 540) % 360 - 180) * Math.cos(q.inv[1] * Math.PI / 180);
      // One last bit before asin shifts the reference latitude at poles by 8.5e-7°.
      const tolerance = Math.abs(q.ll[1]) === 90 ? 1e-6 : 1e-7;
      assert.ok(Math.hypot(dlon, lats[i] - q.inv[1]) < tolerance, `${p.id} inverse ${q.ll}`);
    });
  }
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

test("GRIB read band by band", { skip: needs("grib") }, () => {
  // Two fields of one message, the second reusing the first's bitmap (GDAL's test file).
  const name = "subgrids_reuse_bitmap.grib2";
  const ds = terrio.open(readFileSync(new URL(`../../crates/terrio/tests/data/grib/${name}`, import.meta.url)), name);
  assert.deepEqual([ds.kind, ds.width, ds.height, ds.count, ds.dtype, ds.nodata], ["raster", 20, 20, 2, "float64", 9999]);
  // The sums of GDAL 3.12.2's values (crates/terrio/tests/grib.rs).
  const sum = (band) => band.reduce((s, v) => s + v, 0);
  assert.deepEqual([sum(ds.readBand(1)), sum(ds.readBand(2))], [50706, 51294]);
  const metadata = Object.fromEntries(ds.info().metadata);
  assert.equal(metadata["BAND_1#GRIB_ELEMENT"], "UGRD");
  assert.equal(metadata["BAND_2#GRIB_ELEMENT"], "VGRD");
});

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
  test(`${name} reads as GDAL reads it`, { skip: needs("jp2") || (!DATA && "TERRIO_TEST_DATA is not set") }, (t) => {
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

test("points from arrays, to LAS, LAZ and COPC bytes and back", { skip: needs("pointcloud") }, () => {
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

test("PLY and E57 bytes, PDAL's filters and TIN gridding", { skip: needs("pointcloud") }, () => {
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
  // PDAL's other neighbourhood filters: one dense cluster; features of a plane.
  assert.ok(pc.dbscan(0.6, 3).get("cluster_id").every((c) => c === 0n));
  const feats = pc.covarianceFeatures({ featureSet: ["Linearity", "Planarity", "Scattering"], mode: "raw" });
  const [lin, plan, scat] = ["linearity", "planarity", "scattering"].map((d) => feats.get(d));
  assert.ok(lin.every((v, i) => Math.abs(v + plan[i] + scat[i] - 1) < 1e-9));
  const ev = pc.eigenvalues(8, true);
  assert.ok(ev.get("eigenvalue0").every((v, i) => Math.abs(v + ev.get("eigenvalue1")[i] + ev.get("eigenvalue2")[i] - 1) < 1e-12));
  assert.ok(pc.approximateCoplanar().get("coplanar").some((v) => v === 1));
  assert.ok(pc.estimateRank().get("rank").every((v) => v === 2));
  assert.ok(pc.lof().get("local_outlier_factor").every((v) => v > 0));
  assert.ok(pc.nndistance(4).get("nndistance").every((v) => v >= 0.5 - 1e-12));
  assert.ok(pc.radialDensity(1).get("radial_density").every((v) => v > 0));
  assert.ok(pc.reciprocity().get("reciprocity").every((v) => v >= 0 && v <= 100));
  assert.ok(pc.miniball().get("miniball").every((v) => v >= 0 && v <= 1));
  assert.ok(pc.zsmooth("Smoothed", 1).get("smoothed").every((v) => v >= 100 && v < 108));
  const ground2 = pc.skewnessBalancing().get("z");
  assert.ok(ground2.every((v, i) => i === 0 || v >= ground2[i - 1]));
  assert.equal(pc.head(5).length, 5);
  assert.equal(pc.tail(5, true).length, n - 5);
  assert.equal(pc.sort(["Z"], "DESC").get("z")[0], 107.9);
  assert.equal(pc.locate("Z", "min").get("z")[0], 100);
  assert.equal(pc.iqr("Z").length, n);
  assert.ok(pc.mad("Z", 1).length < n);
  // A raster of the plane (a row below the points: the bottom edge is outside) and squares of zones.
  const surface = Float64Array.from({ length: 40 * 26 }, (_, i) => 100 + ((i % 40) + 0.5) * 0.2);
  const plane = terrio.rasterFromArray(surface, 40, 26, [500000, 1, 0, 4100025, 0, -1], "EPSG:32610");
  assert.equal(pc.dem(plane, "Z[0.2:0.2]").length, n);
  assert.ok(pc.colorization(plane, "Red:1:10").get("red").every((v) => v >= 1000));
  const square = (x0, zone) => ({
    type: "Feature",
    properties: { name: "z", zone },
    geometry: { type: "Polygon", coordinates: [[[x0, 4100000], [x0 + 20, 4100000], [x0 + 20, 4100030], [x0, 4100030], [x0, 4100000]]] },
  });
  const zones = terrio.fromGeoJSON({ type: "FeatureCollection", features: [square(500000, 3), square(500020, 4)] }, "zones", "EPSG:32610");
  const zoned = ground.overlay(zones, "Classification").get("classification");
  assert.ok(zoned.every((c, i) => c === (x[i] <= 500020 ? 3 : 4)));
  // Linear interpolation of a plane is exact.
  const tin = pc.grid(1, { method: "linear", radius: 0 });
  const band = tin.readBand(1);
  assert.ok(band.some((v) => v !== -9999));
});

test("COPC files read by octree level", { skip: needs("pointcloud") || (!DATA && "TERRIO_TEST_DATA is not set") }, (t) => {
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

test("neighbors, collar cleaning and LUT enhancement", () => {
  const values = new Float32Array([1, 2, 3, 4, -1, 6, 7, 8, 9]);
  const dem = terrio.rasterFromArray(values, 3, 3, [0, 1, 0, 3, 0, -1], "EPSG:32610", -1);
  const mean = terrio.neighbors(dem, { kernel: "equal" });
  assert.equal(mean.dtype, "float64");
  assert.equal(mean.readBand(1)[4], -1);
  assert.deepEqual([...terrio.neighbors(dem, { kernel: "equal", nodata: " -9999 " }).readBand(1)],
    [...terrio.neighbors(dem, { kernel: "equal", nodata: -9999 }).readBand(1)]);
  assert.throws(() => terrio.neighbors(dem, { kernel: "equal", nodata: "invalid" }), /invalid nodata/);
  assert.equal(mean.readBand(1)[0], (1 + 1 + 2 + 1 + 1 + 2 + 4 + 4) / 8);
  const two = terrio.neighbors(dem, { kernel: ["equal", "u"], method: ["max", "sum"], ot: "Float32", nodata: "none" });
  assert.equal(two.count, 2);
  assert.equal(two.readBand(1)[0], 4);
  assert.throws(() => terrio.neighbors(dem, { kernel: "sharpen", size: 5 }), /size 5/);

  // A dark, noisy collar around a bright middle.
  const img = new Uint8Array(8 * 6).map((_, i) => {
    const [x, y] = [i % 8, Math.floor(i / 8)];
    return x > 0 && x < 7 && y > 0 && y < 5 ? 150 + x : (x + y) % 5;
  });
  const scene = terrio.rasterFromArray(img, 8, 6, [0, 1, 0, 6, 0, -1], "EPSG:32610");
  const clean = terrio.cleanCollar(scene, { addAlpha: true, pixelDistance: 0 });
  assert.equal(clean.count, 2);
  assert.equal(clean.readBand(1)[0], 0);
  assert.equal(clean.readBand(2)[0], 0);
  assert.equal(clean.readBand(2)[9], 255);
  assert.equal(terrio.nearblack(scene, { colors: ["0"], colorThreshold: 4, pixelDistance: 0 }).readBand(1)[9], 151);
  assert.equal(terrio.nearblack(scene, { colors: "0", colorThreshold: 4, pixelDistance: 0 }).readBand(1)[9], 151);
  assert.throws(() => terrio.nearblack(scene, { addMask: true }), /addMask/);

  const lut = terrio.enhanceLuts(scene, {});
  assert.throws(() => terrio.enhanceLuts(scene, { config: lut }), /only computes equalization tables/);
  assert.ok(lut.startsWith("1:Band -0.5:ScaleMin 255.5:ScaleMax "));
  const eq = terrio.enhance(scene, { equalize: true });
  assert.deepEqual([...terrio.enhance(scene, { config: lut }).readBand(1)], [...eq.readBand(1)]);
  assert.deepEqual([...terrio.enhance(scene, { srcScale: [0, 256] }).readBand(1)], [...img]);
  assert.throws(() => terrio.enhance(scene, {}), /equalize/);
  assert.throws(() => terrio.cleanCollar(scene, { white: true, colors: "0" }), /exclusive/);
  for (const [nodata, expected] of [[-9999, 241], [300, 44], [Infinity, 0]]) {
    const source = terrio.rasterFromArray(new Float64Array([nodata, 128]), 2, 1,
      [0, 1, 0, 1, 0, -1], "EPSG:32610", nodata);
    assert.deepEqual([...terrio.enhance(source, { srcScale: [0, 256] }).readBand(1)], [expected, 128]);
  }
  const nodata = 1.0000000001;
  const source = terrio.rasterFromArray(new Float64Array([nodata, 1]), 2, 1,
    [0, 1, 0, 1, 0, -1], "EPSG:32610", nodata);
  const precise = terrio.enhance(source, { srcScale: [0, 256], ot: "Float64" });
  assert.equal(precise.nodata, nodata);
  assert.deepEqual([...precise.readBand(1)], [nodata, 1]);
});

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

// A working set larger than 4 GB, which only the memory64 module holds: a 2 GB raster, copied in,
// written as a GeoTIFF of as many bytes (the raster, a copy of it and the tiles in memory at
// once), and read back by window. Opt in with TERRIO_LARGE_TESTS=1 (about 16 GB of memory).
test("a raster of 2 GB, written and read back (memory64)", {
  skip: (BUILD !== "terrio-js/memory64" && "not the memory64 module") || (!process.env.TERRIO_LARGE_TESTS && "TERRIO_LARGE_TESTS is not set"),
}, () => {
  const [w, h] = [45000, 45000];
  const pixels = new Uint8Array(w * h);
  for (let r = 0; r < h; r++) pixels.fill(r % 251, r * w, (r + 1) * w);
  const ds = terrio.rasterFromArray(pixels, w, h, [0, 1, 0, h, 0, -1], "EPSG:3857");
  const tif = ds.toBytes("big.tif", { tileSize: 1024 });
  assert.ok(tif.length > w * h);
  assert.ok(terrio.features().memory > 2 ** 32, `${terrio.features().memory} bytes of memory`);
  // Its memory goes back to the module (which keeps it, as WebAssembly memories do not shrink).
  ds.free();
  const back = terrio.open(tif, "big.tif");
  assert.deepEqual([back.width, back.height], [w, h]);
  assert.deepEqual([...back.readBand(1, [w - 2, h - 3, 2, 3])], [(h - 3) % 251, (h - 3) % 251, (h - 2) % 251, (h - 2) % 251, (h - 1) % 251, (h - 1) % 251]);
  back.free();
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
  const [parquet, points] = [!needs("parquet"), !needs("pointcloud")];
  if (parquet) writeFileSync(join(dir, "cells.parquet"), layer.toBytes("cells.parquet"));
  writeFileSync(join(dir, "cells.pmtiles"), layer.toBytes("cells.pmtiles", { maxzoom: 4 }));
  const n = 20000;
  const pts = { x: new Float64Array(n), y: new Float64Array(n), z: new Float64Array(n) };
  for (let i = 0; i < n; i++) [pts.x[i], pts.y[i], pts.z[i]] = [(i * 7919) % 1000, (i * 104729) % 1000, i % 50];
  if (points) writeFileSync(join(dir, "pts.copc.laz"), terrio.pointsFromArrays(pts, "EPSG:32610").toBytes("pts.copc.laz"));
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

    const pq = parquet && (await terrio.openUrl(server.url + "cells.parquet"));
    if (pq) {
      const groups = pq.info().rowGroups;
      assert.equal(groups.reduce((s, g) => s + g.rows, 0), 1500);
      const last = await pq.read({ rowGroups: [groups.length - 1] });
      assert.equal(last.toGeoJSON().features.length, groups.at(-1).rows);
      const inBox = (await pq.read({ bbox: [0, 0, 10, 10] })).toGeoJSON().features;
      assert.deepEqual(inBox.map((f) => f.properties.id).sort((a, b) => a - b), expected);
    }

    const tiles = await terrio.openUrl(server.url + "cells.pmtiles");
    assert.equal(tiles.kind, "tiles");
    const tile = await tiles.getTile(0, 0, 0);
    assert.ok(tile instanceof Uint8Array && tile.length > 0);
    assert.equal(await tiles.getTile(4, 15, 0), undefined);
    const vt = await tiles.read({ zoom: 4, bbox: [0, 0, 1e6, 1e6] });
    assert.ok(vt.toGeoJSON().features.length > 0);

    const copc = points && (await terrio.openUrl(server.url + "pts.copc.laz"));
    if (copc) {
      assert.deepEqual([copc.kind, copc.format, copc.crs], ["pointcloud", "COPC", "EPSG:32610"]);
      const box = await copc.read({ bounds: [100, 100, 300, 300] });
      const all = terrio.readPoints(readFileSync(join(dir, "pts.copc.laz")), "pts.copc.laz", { bounds: [100, 100, 300, 300] });
      assert.equal(box.length, all.length);
      const coarse = await copc.read({ maxLevel: 0 });
      assert.equal(coarse.length, terrio.readPoints(readFileSync(join(dir, "pts.copc.laz")), "pts.copc.laz", { maxLevel: 0 }).length);
    }

    await assert.rejects(terrio.openUrl(server.url + "missing.tif"), /404/);
    // The requests the server saw are those terrio counted.
    const counted = [cog, busy, fgb, pq, tiles, copc].filter(Boolean).reduce((s, d) => s + d.stats().bytes, 0);
    assert.equal(counted, server.stats.bytes);
  } finally {
    server.close();
    rmSync(dir, { recursive: true });
  }
});

test("remote real files read with range requests, as GDAL reads them", { skip: needs("parquet", "pointcloud") || SKIP_DATA }, async (t) => {
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

test("netCDF, HDF5 and Zarr in memory; LERC, JPEG XL and JPEG GeoTIFFs", { skip: needs("netcdf-hdf5", "zarr") }, () => {
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

test("rasters written into netCDF-4 groups", { skip: needs("netcdf-hdf5") }, () => {
  const a = terrio.rasterFromArray(new Float32Array(30 * 20).map((_, i) => i / 7), 30, 20, [500000, 30, 0, 4100000, 0, -30], "EPSG:32610");
  const b = terrio.rasterFromArray(new Int16Array(8 * 6).map((_, i) => i - 9), 8, 6, [-105, 0.5, 0, 40, 0, -0.5], "EPSG:4326", -9999);
  const nc = new terrio.NetcdfGroups();
  nc.add("/terrain/a", a);
  nc.add("/b", b);
  assert.equal(nc.length, 2);
  const bytes = nc.toBytes({ COMPRESS: "DEFLATE" });
  for (const [path, src] of [["/terrain/a/Band1", a], ["/b/Band1", b]]) {
    const back = terrio.open(bytes, `NETCDF:"x.nc":${path}`);
    assert.deepEqual([back.width, back.height, back.crs], [src.width, src.height, src.crs], path);
    assert.deepEqual([...back.readBand(1)], [...src.readBand(1)], path);
  }
  const twice = new terrio.NetcdfGroups();
  twice.add("/g", a);
  twice.add("g", b);
  assert.throws(() => twice.toBytes(), /two rasters in the netCDF group/);
  assert.throws(() => new terrio.NetcdfGroups().toBytes(), /no rasters/);
});

test("Zarr stores and netCDF/HDF5 products as GDAL reads them", { skip: needs("netcdf-hdf5", "zarr") || SKIP_DATA }, async (t) => {
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

test("warp selects stored overviews for datasets, bytes and repeated shared inputs", () => {
  const rows = Array.from({ length: 32 }, (_, y) => Array.from({ length: 32 }, (_, x) => y * y + x + 0.25).join(" ")).join("\n");
  const plain = terrio.open(new TextEncoder().encode(`ncols 32\nnrows 32\nxllcorner 0\nyllcorner 0\ncellsize 1\n${rows}\n`), "source.asc");
  const bytes = plain.toBytes("pyramid.tif", { overviews: [2, 4, 8], overviewResampling: "average" });
  const lazy = terrio.open(bytes, "pyramid.tif");
  const expected = [...lazy.readBand(1, undefined, 1)];
  for (const src of [lazy, bytes, { bytes, filename: "pyramid.tif" }, [lazy, lazy], [bytes, lazy]]) {
    for (const overview of [undefined, "AUTO", 1, "1"]) {
      const out = terrio.warp(src, { ts: [8, 8], overview });
      assert.deepEqual([...out.readBand(1)], expected);
      assert.deepEqual([...out.transform], [0, 4, 0, 32, 0, -4]);
      out.free();
    }
  }
  const full = terrio.warp(lazy, { ts: [8, 8], overview: "NONE" });
  assert.notDeepEqual([...full.readBand(1)], expected);
  const loaded = terrio.warp([plain, plain], { ts: [8, 8], overview: "NONE" });
  assert.deepEqual([...loaded.readBand(1)], [...full.readBand(1)]);
  loaded.free();
  const fine = terrio.warp(lazy, { ts: [8, 8], overview: "AUTO-1" });
  const explicit = terrio.warp(lazy, { ts: [8, 8], overview: 0 });
  assert.deepEqual([...fine.readBand(1)], [...explicit.readBand(1)]);
  assert.throws(() => terrio.warp(lazy, { overview: "invalid" }), /overview selection/);
  plain.free(); lazy.free(); full.free(); fine.free(); explicit.free();
});

test("SQL computed columns, DISTINCT and summary aggregates", () => {
  const src = { type: "FeatureCollection", features: [2, 2, null].map((v) => ({
    type: "Feature", geometry: { type: "Point", coordinates: [0, 0] }, properties: { v },
  })) };
  const computed = terrio.sql(src, "SELECT v*2 AS doubled FROM layer").toGeoJSON();
  assert.equal(computed.features[0].properties.doubled, 4);
  const summary = terrio.sql(src, "SELECT COUNT(*) AS n, SUM(v) AS total FROM layer").toGeoJSON();
  assert.deepEqual(summary.features[0].properties, { n: 3, total: 4 });
  assert.equal(summary.features[0].geometry, null);
  assert.equal(terrio.sql(src, "SELECT DISTINCT v FROM layer").toGeoJSON().features.length, 2);
});

test("vector dataset commands: concat, partition, setFieldType, makePoint, schemas", () => {
  const places = (name) => ({ type: "FeatureCollection", name, features: [["TN", 1, "-86.5"], ["NY", 2, "-74"], ["TN", 3, "x"]].map(
    ([state, n, lon], i) => ({ type: "Feature", geometry: { type: "Point", coordinates: [i, i] }, properties: { state, n, lon, lat: 36 } }),
  ) });
  const a = terrio.fromGeoJSON(places("a"), "a");
  const merged = terrio.concat([a, { bytes: a.toBytes("b.gpkg"), filename: "b.gpkg" }], { mode: "single", sourceLayerFieldName: "src" });
  assert.deepEqual(merged.layerNames(), ["merged"]);
  const features = merged.toGeoJSON().features;
  assert.equal(features.length, 6);
  assert.equal(features[5].properties.src, "b_a"); // {AUTO_NAME}: the file and layer names
  assert.throws(() => terrio.concat([a], { outputLayer: "x" }), /merge-per-layer-name/);
  assert.throws(() => terrio.concat([a], { mode: "single", sourceLayerFieldName: "state" }), /source-layer field.*already exists/);

  const parts = terrio.partition(a, { fields: ["state"], format: "GeoJSON" });
  assert.deepEqual(parts.map((p) => p.path), ["a/state=TN/part_0000000001.json", "a/state=NY/part_0000000001.json"]);
  assert.equal(terrio.open(parts[0].bytes, "p.json").toGeoJSON().features.length, 2);

  const typed = terrio.setFieldType(a, "String", { fieldName: "n" }).toGeoJSON();
  assert.equal(typed.features[2].properties.n, "3");
  assert.throws(() => terrio.setFieldType(a, "Real", { fieldName: "nope" }), /cannot find field/);

  const two = terrio.fromGeoJSON({ type: "FeatureCollection", features: places("a").features.slice(0, 2) });
  const pts = terrio.makePoint(two, "lon", "lat", { crs: "EPSG:4326" }).toGeoJSON();
  assert.deepEqual(pts.features[1].geometry.coordinates, [-74, 36]);
  assert.throws(() => terrio.makePoint(a, "lon", "lat"), /invalid value/);

  const bytes = terrio.createVector("new.gpkg", { outputLayer: "sites", geometryType: "POINT", crs: "EPSG:32633", fields: ["name:STRING(20)", "total:INTEGER64"] });
  const schema = terrio.exportSchema({ bytes, filename: "new.gpkg" });
  assert.equal(schema.layers[0].name, "sites");
  assert.equal(schema.layers[0].geometryFields[0].type, "Point");
  assert.deepEqual(schema.layers[0].geometryFields[0].coordinateSystem, { authid: "EPSG:32633" });
  assert.deepEqual(schema.layers[0].fields.map((f) => f.type), ["String", "Integer64"]);
  const copy = terrio.createVector("copy.gpkg", { schema });
  assert.deepEqual(terrio.exportSchema({ bytes: copy, filename: "copy.gpkg" }), schema);
  for (const base of [{ schema }, { like: a }]) {
    for (const explicit of [{ fields: [] }, { geometryType: "POINT" }, { crs: "EPSG:4326" }, { fid: "rowid" }]) {
      assert.throws(() => terrio.createVector("invalid.gpkg", { ...base, ...explicit }), /give one of schema, like, or explicit fields/);
    }
  }
  a.free(); merged.free();
});

test("unscale, nodataToAlpha, reclassify and asFeatures", () => {
  const gt = [100, 10, 0, 50, 0, -10];
  const src = terrio.rasterFromArray(new Uint8Array([1, 2, 255, 40]), 2, 2, gt, "EPSG:32611", 255);
  const alpha = terrio.nodataToAlpha(src);
  assert.equal(alpha.count, 2);
  assert.deepEqual([...alpha.readBand(2)], [255, 255, 0, 255]);
  assert.deepEqual([...terrio.nodataToAlpha(src, null).readBand(2)], [255, 255, 0, 255]);
  assert.deepEqual([...terrio.nodataToAlpha(src, { nodata: 1 }).readBand(2)], [0, 255, 255, 255]);
  assert.throws(() => terrio.nodataToAlpha(src, { nodata: NaN }), /strings/);
  assert.throws(() => terrio.nodataToAlpha(src, { nodata: Infinity }), /strings/);
  assert.throws(() => terrio.nodataToAlpha(src, { nodata: null }), /null/);
  assert.throws(() => terrio.nodataToAlpha(src, { nodata: [NaN] }), /strings/);
  const special = terrio.rasterFromArray(new Float64Array([NaN, Infinity, -Infinity, 3]), 2, 2, gt);
  for (const [nodata, expected] of [["nan", [0, 255, 255, 255]], ["inf", [255, 0, 255, 255]], ["-inf", [255, 255, 0, 255]]]) {
    assert.deepEqual([...terrio.nodataToAlpha(special, { nodata }).readBand(2)], expected);
  }
  const fractional = terrio.rasterFromArray(new Float32Array([0.1, 0.2]), 2, 1, gt, "EPSG:32611", 0.1);
  assert.equal(terrio.asFeatures(fractional, { skipNodata: true }).toGeoJSON().features.length, 1);
  assert.deepEqual(Object.keys(terrio.asFeatures(src, { bands: [1, 1] }).toGeoJSON().features[0].properties), ["BAND_1"]);
  const rc = terrio.reclassify(src, "[0,3)=1;[3,50]=2;NO_DATA=NO_DATA");
  assert.deepEqual([...rc.readBand(1)], [1, 1, 255, 2]);
  const fromFile = terrio.reclassify(src, "", { mappingFile: "# c\n[0,3)=7\nDEFAULT=PASS_THROUGH\n", ot: "Int16" });
  assert.equal(fromFile.dtype, "int16");
  assert.deepEqual([...fromFile.readBand(1)], [7, 7, 255, 40]);
  assert.throws(() => terrio.reclassify(src, "1=2"), /no specified mapping/);
  assert.throws(() => terrio.reclassify(src, "@map.txt"), /mappingFile/);
  const nanSource = terrio.rasterFromArray(new Float32Array([NaN, 1]), 2, 1,
    [0, 1, 0, 1, 0, -1], "EPSG:32610", NaN);
  const intervals = Array.from({length: 81}, (_, i) => (i * 37) % 81)
    .map(i => i === 80 ? "NO_DATA=0" : `${i}=${i}`).join(";");
  assert.throws(() => terrio.reclassify(nanSource, intervals), /overlaps/);
  assert.throws(() => terrio.reclassify(src, "1=2", {mappingFile: "1=3"}), /not both/);
  const scaled = terrio.unscale(src.toBytes("s.tif"), { ot: "Float64" });
  assert.equal(scaled.dtype, "float64");
  const fc = terrio.asFeatures(src, { geometryType: "polygon", skipNodata: true, includeRowCol: true }).toGeoJSON();
  assert.equal(fc.features.length, 3);
  assert.deepEqual(fc.features[2].properties, { ROW: 1, COL: 1, BAND_1: 40 });
  assert.deepEqual(fc.features[0].geometry.coordinates[0][1], [100, 40]);
  assert.throws(() => terrio.asFeatures(src, { geometryType: "line" }), /geometry type/);
});

test("viewshed visibility, height and byte input", () => {
  const dem = terrio.rasterFromArray(new Float64Array([0, 0, 10, 0, 0]), 5, 1, [0, 10, 0, 10, 0, -10], "EPSG:32611");
  assert.deepEqual([...terrio.viewshed(dem, 5, 5, { cc: 0 }).readBand(1)], [255, 255, 255, 0, 0]);
  const ground = terrio.viewshed(dem.toBytes("dem.tif"), 5, 5, { cc: 0, om: "GROUND" });
  assert.equal(ground.dtype, "float64");
  assert.deepEqual([...ground.readBand(1)], [0, 0, 0, 14, 18]);
  assert.equal(terrio.viewshed(dem, 5, 5, { md: 20 }).width, 3);
  assert.throws(() => terrio.viewshed(dem, 5, 5, { om: "ACCUM" }), /mode/);
});

test("warp from subsampled pixel-center geolocation arrays", () => {
  const values = Float64Array.from({ length: 12 }, (_, i) => i + 1);
  const source = terrio.rasterFromArray(values, 4, 3, [0, 1, 0, 3, 0, -1]);
  const x = terrio.rasterFromArray(new Float64Array([0, 0, 0, 0, 11, 13, 11, 13]), 2, 2, [0, 1, 0, 2, 0, -1]);
  const y = terrio.rasterFromArray(new Float64Array([0, 0, 0, 0, 49, 49, 47, 47]), 2, 2, [0, 1, 0, 2, 0, -1]);
  const result = terrio.warp(source, {
    geolocation: { x, y, xBand: 2, yBand: 2, pixelStep: 2, lineStep: 2, pixelCenter: true },
    te: [10, 47, 14, 50], ts: [4, 3], tSrs: "EPSG:4326", et: 0,
  });
  assert.deepEqual([...result.readBand(1)], [...values]);
  assert.throws(() => terrio.warp(source, { geoloc: true }), /no geolocation arrays/);
});

test("fillNodata: holes, nearest ties, explicit masks and invalid options", () => {
  const src = terrio.open(new TextEncoder().encode("ncols 3\nnrows 3\nxllcorner 0\nyllcorner 0\ncellsize 1\nNODATA_value -9999\n0 1 2\n3 -9999 5\n6 7 8\n"), "holes.asc");
  const out = terrio.fillNodata(src, { maxDistance: 1 });
  // ASCII grids read as Float64, with Float32 arithmetic for filled samples.
  assert.equal(out.readBand(1)[4], Math.fround(13 / 3));
  assert.equal(terrio.fillNodata(src, { interpolation: "nearest" }).readBand(1)[4], 1);
  const mask = terrio.open(new TextEncoder().encode("ncols 3\nnrows 3\nxllcorner 0\nyllcorner 0\ncellsize 1\n0 0 0\n0 0 0\n0 0 0\n"), "mask.asc");
  assert.deepEqual([...terrio.fillNodata(src, { mask }).readBand(1)], [...src.readBand(1)]);
  const bytes = src.toBytes("holes.tif");
  for (const input of [bytes, { bytes, filename: "holes.tif" }]) {
    const filled = terrio.fillNodata(input, { maxDistance: 1 });
    assert.equal(filled.readBand(1)[4], Math.fround(13 / 3));
    filled.free();
  }
  assert.equal(src.readBand(1)[4], -9999);
  assert.throws(() => terrio.fillNodata(src, { maxDistance: -1 }), /nonnegative/);
  src.free(); out.free(); mask.free();
});

test("native multidimensional views retain integer precision and store lifetimes", { skip: needs("netcdf-hdf5", "zarr") }, () => {
  const encode = (value) => new TextEncoder().encode(JSON.stringify(value));
  const files = { ".zgroup": encode({ zarr_format: 2 }), ".zattrs": encode({}) };
  const base = 9007199254741001n;
  files["cube/.zarray"] = encode({ zarr_format: 2, shape: [2, 2, 3], chunks: [1, 2, 3], dtype: "<i8", compressor: null, fill_value: null, order: "C", filters: null });
  files["cube/.zattrs"] = encode({ _ARRAY_DIMENSIONS: ["time", "y", "x"] });
  for (let time = 0; time < 2; time++) {
    const raw = new Uint8Array(6 * 8);
    const view = new DataView(raw.buffer);
    for (let i = 0; i < 6; i++) view.setBigInt64(i * 8, base + BigInt(time * 6 + i), true);
    files[`cube/${time}.0.0`] = raw;
  }
  const ds = terrio.openMultidim(files);
  assert.ok(ds.arrayPaths.includes("/cube"));
  const array = ds.array("cube");
  // Vec<usize> shapes use BigUint64Array in memory64; these fixture sizes are exact Numbers.
  assert.deepEqual(Array.from(array.shape, Number), [2, 2, 3]);
  const positional = array.iselAxis(0, 1).sliceAxis(1, 1, 1);
  assert.deepEqual([...positional.read()], [7n, 10n].map((n) => base + n));
  positional.free();
  const view = array.isel("time", 1).transpose(["x", "y"]);
  assert.deepEqual(Array.from(view.shape, Number), [3, 2]);
  assert.ok(view.read() instanceof BigInt64Array);
  assert.deepEqual([...view.read()], [6n, 9n, 7n, 10n, 8n, 11n].map((n) => base + n));
  const nc = terrio.openMultidim(ds.toBytes());
  assert.deepEqual([...nc.array("cube").read()], [...array.read()]);
  const back = terrio.openMultidim(nc.toZarrStore());
  assert.deepEqual([...back.array("cube").read()], [...array.read()]);
  ds.free(); array.free();
  assert.equal(view.read()[0], base + 6n);
  view.free(); nc.free(); back.free();
});

test("openCatalog: lazy windows and z-order with named in-memory sources", () => {
  const index = { type: "FeatureCollection", features: [["left", 0, 2], ["right", 2, 1], ["missing", 100, 3]].map(([location, x, z]) => ({
    type: "Feature", properties: { location, z }, geometry: { type: "Polygon", coordinates: [[[x, 0], [x + 4, 0], [x + 4, 4], [x, 4], [x, 0]]] },
  })) };
  const left = terrio.rasterFromArray(new Float32Array(16).fill(10), 4, 4, [0, 1, 0, 4, 0, -1], "EPSG:4326");
  const right = terrio.rasterFromArray(new Float32Array(16).fill(20), 4, 4, [2, 1, 0, 4, 0, -1], "EPSG:4326");
  const catalog = terrio.openCatalog(index, { left, right }, { tr: [1, 1], te: [0, 0, 6, 4], sortField: "z", cacheSize: 1 });
  assert.deepEqual([catalog.width, catalog.height, catalog.count], [6, 4, 1]);
  assert.deepEqual([...catalog.readBand(1)].slice(0, 6), [10, 10, 10, 10, 20, 20]);
  assert.deepEqual([...catalog.readBand(1, [4, 0, 2, 2])], [20, 20, 20, 20]);
  assert.throws(() => terrio.openCatalog(index, { left }, { cacheSize: 0 }), /positive/);
  catalog.free(); left.free(); right.free();
});


test("openCatalog: selected data band retains the required source alpha", () => {
  const index = { type: "FeatureCollection", features: [{ type: "Feature", properties: { location: "source" },
    geometry: { type: "Polygon", coordinates: [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]]] } }] };
  const values = new Float64Array(96);
  for (let band = 0; band < 5; band++) values.fill((band + 1) * 10, band * 16, (band + 1) * 16);
  values.fill(255, 80); values[80] = 0;
  const source = terrio.rasterFromArray(values, 4, 4, [0, 1, 0, 4, 0, -1], "EPSG:4326");
  const catalog = terrio.openCatalog(index, { source }, { tr: [1, 1], te: [0, 0, 4, 4], bands: 6, alphaBand: 6 });
  assert.equal(catalog.dtype, "uint8");
  assert.deepEqual([...catalog.readBand(3)], [0, ...Array(15).fill(30)]);
  const untagged = terrio.openCatalog(index, { source }, { tr: [1, 1], te: [0, 0, 4, 4], bands: 5 });
  source.free();
  assert.deepEqual([...untagged.readBand(3)], Array(16).fill(30));
  assert.deepEqual([...catalog.readBand(3)], [0, ...Array(15).fill(30)]);
  catalog.free(); untagged.free();
});

test("openVrt resolves uploaded sources and reads derived windows lazily", () => {
  const src = terrio.open(new TextEncoder().encode("ncols 3\nnrows 3\nxllcorner 0\nyllcorner 0\ncellsize 1\n0 1 2\n3 4 5\n6 7 8\n"), "source.asc");
  const bytes = src.toBytes("upload.tif");
  const xml = '<VRTDataset rasterXSize="3" rasterYSize="3"><GeoTransform>0,1,0,3,0,-1</GeoTransform><VRTRasterBand dataType="Float64" subClass="VRTDerivedRasterBand"><PixelFunctionType>sum</PixelFunctionType><PixelFunctionArguments k="2"/><SimpleSource><SourceFilename>upload.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>';
  const out = terrio.openVrt(xml, { "upload.tif": bytes });
  assert.deepEqual([...out.readBand(1, [1, 1, 2, 2])], [6, 7, 9, 10]);
  assert.throws(() => terrio.openVrt(xml.replace("sum", "external_custom_function"), { "upload.tif": bytes }), /external_custom_function/);
  const fractional = terrio.rasterFromArray(new Float64Array([1.250000000001, 2.75]), 2, 1, [0, 1, 0, 1, 0, -1]);
  const bands = ["Byte", "Float32", "Float64"].map(type => `<VRTRasterBand dataType="${type}"><SimpleSource><SourceFilename>source.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand>`).join("");
  const mixed = terrio.openVrt(`<VRTDataset rasterXSize="2" rasterYSize="1">${bands}</VRTDataset>`, { "source.tif": fractional.toBytes("source.tif") });
  assert.equal(mixed.dtype, "float64");
  assert.deepEqual([...mixed.readBand(1)], [1, 3]);
  assert.deepEqual([...mixed.readBand(2)], [Math.fround(1.250000000001), Math.fround(2.75)]);
  assert.deepEqual([...mixed.readBand(3)], [1.250000000001, 2.75]);
  mixed.free(); fractional.free();
  src.free(); out.free();
});

test("pansharpen weights, selected bands and unequal resolutions", () => {
  const pan = terrio.rasterFromArray(new Uint16Array(16).fill(80), 4, 4, [0, 1, 0, 4, 0, -1], "EPSG:32611");
  const spectral = terrio.rasterFromArray(new Uint16Array([20, 20, 20, 20, 40, 40, 40, 40]), 2, 2, [0, 2, 0, 4, 0, -2], "EPSG:32611");
  const out = terrio.pansharpen(pan, spectral, { ot: "Float64" });
  assert.equal(out.dtype, "float64");
  assert.deepEqual([...out.readBand(1)], new Array(16).fill(53));
  assert.deepEqual([...out.readBand(2)], new Array(16).fill(107));
  const selected = terrio.pansharpen(pan.toBytes("pan.tif"), [spectral], { weights: [0.75, 0.25], bands: [2] });
  assert.deepEqual([...selected.readBand(1)], new Array(16).fill(128));
  assert.throws(() => terrio.pansharpen(pan, spectral, { weights: [1] }), /weight/);
  pan.free(); spectral.free(); out.free(); selected.free();
});



test("pansharpenVrt creates a lazy uploaded-source virtual raster", () => {
  const pan = terrio.rasterFromArray(new Uint16Array(64).fill(80), 8, 8, [0, 1, 0, 8, 0, -1], "EPSG:32611");
  const spectral = terrio.rasterFromArray(new Uint16Array([...new Array(16).fill(20), ...new Array(16).fill(40)]), 4, 4, [0, 2, 0, 8, 0, -2], "EPSG:32611");
  const sources = { "pan.tif": pan.toBytes("pan.tif"), "spectral.tif": spectral.toBytes("spectral.tif") };
  const xml = terrio.pansharpenVrt("pan.tif", ["spectral.tif"], sources, { bands: [2, 1, 2] });
  assert.match(xml, /VRTPansharpenedDataset/);
  const sharp = terrio.openVrt(xml, sources);
  assert.deepEqual([...sharp.readBand(1, [2, 3, 3, 2])], new Array(6).fill(107));
  assert.equal(sharp.count, 3);
  pan.free(); spectral.free(); sharp.free();
});


test("pansharpenVrt keeps direct source masks and ignores them in temporary upsampling", () => {
  const pan = terrio.rasterFromArray(new Uint16Array(16).fill(80), 4, 4, [0, 1, 0, 4, 0, -1], "EPSG:32611");
  // GDAL-generated 2x2 UInt16 bands filled with 20/40 and an all-zero internal TIFF mask.
  const masked = new Uint8Array(Buffer.from("SUkqAAgAAAAQAAABAwABAAAAAgAAAAEBAwABAAAAAgAAAAIBAwACAAAAEAAQAAMBAwABAAAAAQAAAAYBAwABAAAAAQAAABEBBAABAAAAFgIAABUBAwABAAAAAgAAABYBAwABAAAAAgAAABcBBAABAAAAEAAAABwBAwABAAAAAQAAAFIBAwABAAAAAAAAAFMBAwACAAAAAQABAA6DDAADAAAAzgAAAIKEDAAGAAAA5gAAAK+HAwAgAAAAFgEAALGHAgAeAAAAVgEAAHQBAAAAAAAAAAAAQAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBAAAAAAAAAAAABAAEAAAAHAAAEAAABAAEAAQQAAAEAAQACBLGHFgAAAAEIsYcHABYABggAAAEAjiMADAAAAQBjfwQMAAABACkjV0dTIDg0IC8gVVRNIHpvbmUgMTFOfFdHUyA4NHwADQD+AAQAAQAAAAQAAAAAAQMAAQAAAAIAAAABAQMAAQAAAAIAAAACAQMAAQAAAAEAAAADAQMAAQAAAAgAAAAGAQMAAQAAAAQAAAARAQQAAQAAACYCAAAVAQMAAQAAAAEAAAAWAQMAAQAAAAIAAAAXAQQAAQAAAA0AAAAcAQMAAQAAAAEAAAA9AQMAAQAAAAEAAABTAQMAAQAAAAEAAAAAAAAAFAAoABQAKAAUACgAFAAoAHicAQIA/f8AAAACAAE=", "base64"));
  const sources = { "pan.tif": pan.toBytes("pan.tif"), "spectral.tif": masked };
  const xml = terrio.pansharpenVrt("pan.tif", ["spectral.tif"], sources, {});
  const out = terrio.openVrt(xml, sources);
  assert.deepEqual([...out.readBand(1)], new Array(16).fill(53));
  assert.deepEqual([...out.readBand(1, [1, 1, 2, 1])], [0, 0]);
  pan.free(); out.free();
});

test("openCatalog: defers unused encoded sources and preserves supplied lazy datasets", () => {
  const index = { type: "FeatureCollection", features: [["left", 0], ["right", 2]].map(([location, x]) => ({
    type: "Feature", properties: { location }, geometry: { type: "Polygon", coordinates: [[[x, 0], [x + 4, 0], [x + 4, 4], [x, 4], [x, 0]]] },
  })) };
  const source = terrio.rasterFromArray(new Uint8Array(16).fill(10), 4, 4, [0, 1, 0, 4, 0, -1], "EPSG:4326");
  const bytes = source.toBytes("source.tif");
  const lazy = terrio.open(bytes, "source.tif");
  for (const left of [lazy, { bytes, filename: "source.tif" }]) {
    const catalog = terrio.openCatalog(index, { left, right: { bytes: new Uint8Array(12), filename: "invalid.tif" } }, { tr: [1, 1], te: [0, 0, 6, 4], bands: 1 });
    assert.deepEqual([...catalog.readBand(1, [0, 0, 1, 1])], [10]);
    catalog.free();
  }
  assert.deepEqual([...lazy.readBand(1, [0, 0, 1, 1])], [10]);
  lazy.free(); source.free();
});

test("CAD DWG/DXF byte roundtrips preserve drawing coordinates, labels and holes", { skip: needs("cad") }, () => {
  const source = terrio.open(new TextEncoder().encode(JSON.stringify({
    type: "FeatureCollection",
    features: [
      { type: "Feature", properties: { Layer: "survey", Text: "日本語" }, geometry: { type: "Point", coordinates: [12, 34, 56] } },
      { type: "Feature", properties: { Layer: "survey" }, geometry: { type: "Polygon", coordinates: [
        [[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]],
        [[2, 2], [4, 2], [4, 4], [2, 4], [2, 2]],
      ] } },
      { type: "Feature", properties: { Text: "" }, geometry: { type: "Point", coordinates: [1, 2, 3] } },
      { type: "Feature", properties: { Text: "annotation", EntityType: "POINT" }, geometry: { type: "Point", coordinates: [1, 2, 3] } },
      { type: "Feature", properties: { Text: "multi\\Pline", EntityType: "MTEXT" }, geometry: { type: "Point", coordinates: [1, 2, 3] } },
    ],
  })), "drawing.geojson");
  for (const ext of ["dxf", "dwg"]) {
    const bytes = source.toBytes(`drawing.${ext}`);
    const back = terrio.open(bytes, `drawing.${ext}`);
    assert.deepEqual(back.layerNames(), ["entities"]);
    const features = back.toGeoJSON().features;
    assert.equal(features.length, 5);
    assert.deepEqual(features[0].geometry.coordinates, [12, 34, 56]);
    assert.equal(features[0].properties.Layer, "survey");
    assert.equal(features[0].properties.Text, "日本語");
    assert.equal(features[1].geometry.type, "Polygon");
    assert.equal(features[1].geometry.coordinates.length, 2);
    assert.deepEqual(features.slice(2).map((f) => f.properties.EntityType), ["POINT", "POINT", "MTEXT"]);
    assert.equal(features[4].properties.Text, "multi\\Pline");
    back.free();
  }
  source.free();
});

test("coordinate operation safeguards and warnings", () => {
  terrio.takeWarnings();
  const [x, y] = terrio.transformPoints([110], [35], "EPSG:4610", "EPSG:4326");
  assert.equal(x[0], 110);
  assert.equal(y[0], 35);
  assert.ok(Array.from(terrio.takeWarnings()).some((w) => w.includes("ballpark")));
  assert.throws(() => terrio.transformPoints([110], [35], "EPSG:4610", "EPSG:4326", undefined, { allowBallpark: false }), /ALLOW_BALLPARK/);
  assert.throws(() => terrio.transformPoints([600000], [200000], "EPSG:21781", "EPSG:2056", undefined, { onlyBest: true }), /ONLY_BEST/);
  assert.ok(Array.from(terrio.takeWarnings()).some((w) => w.includes("missing grid")));
});

test("GDAL 3.13 terrain and native WebP creation options", () => {
  const values = new Float32Array([942.315, 946.702, 951.43, 945.132, 943.95, 959.37, 948.15, 954.318, 960.01]);
  const dem = terrio.rasterFromArray(values, 3, 3, [500000, 3.5, 0, 4100000, 0, -3.5], "EPSG:32610");
  assert.ok(Math.abs(terrio.dem(dem, "slope", { kernelVersion: "3.13" }).readBand(1)[4] - 64.0974) < 1e-5);
  const rgb = new Uint8Array(3 * 20 * 12).map((_, i) => i * 37 % 251);
  const image = terrio.rasterFromArray(rgb, 20, 12, [0, 1, 0, 12, 0, -1]);
  assert.throws(() => image.toBytes("bad.tif", { compress: "WEBP", creationOptions: { WEBP_LOSSLESS: "maybe" } }), /WEBP_LOSSLESS/);
  assert.throws(() => image.toBytes("bad.tif", { compress: "WEBP", creationOptions: { QUALITY: "abc" } }), /invalid QUALITY/);
  const bytes = image.toBytes("webp.tif", { compress: "WEBP", creationOptions: { WEBP_LOSSLESS: "YES" } });
  const decoded = terrio.open(bytes, "webp.tif");
  for (let band = 1; band <= 3; band++) assert.deepEqual([...decoded.readBand(band)], [...rgb.slice((band-1)*240, band*240)]);
  const low = image.toBytes("low.tif", { compress: "WEBP", creationOptions: { WEBP_LEVEL: "10" } });
  const high = image.toBytes("high.tif", { compress: "WEBP", creationOptions: { WEBP_LEVEL: "95" } });
  assert.notDeepEqual(low, high);
  assert.notDeepEqual([...terrio.open(low, "low.tif").readBand(1)], [...terrio.open(high, "high.tif").readBand(1)]);
  const grid = terrio.rasterFromArray(new Float32Array(65 * 35).map((_, i) => i % 71), 65, 35, [0,1,0,35,0,-1]);
  const cog = terrio.open(grid.toBytes("gdal.tif", { format: "COG", tileSize: 16, creationOptions: { COG_OVERVIEW_POLICY: "GDAL_3_13" } }), "gdal.tif");
  assert.deepEqual(cog.info().overviews, [[32,17],[16,8]]);
});

test("geocentric XYZ coordinates and vector reprojection retain all three ordinates", () => {
  const reference = JSON.parse(readFileSync(new URL("../../crates/terrio-crs/tests/data/geocentric_reference.json", import.meta.url)));
  for (const c of reference.cases) {
    const actual = terrio.transformPoints(c.points.map(p => p[0]), c.points.map(p => p[1]), c.src, c.dst, c.points.map(p => p[2]), { sCoordEpoch: c.source_epoch ?? undefined });
    for (let i = 0; i < c.points.length; i++) {
      for (let axis = 0; axis < 3; axis++) assert.ok(Math.abs(actual[axis][i] - c.expected[i][axis]) <= c.tolerance[axis], `${c.name}, point ${i}, axis ${axis}`);
    }
  }
  assert.equal(terrio.describeCrs("EPSG:4978").geocentric, true);
  assert.equal(terrio.describeCrs("EPSG:4978").geographic, false);
  const layer = terrio.fromGeoJSON({ type: "FeatureCollection", features: [{ type: "Feature", geometry: { type: "Point", coordinates: [147, -42, 100] }, properties: { id: 7 } }] });
  const out = terrio.open(layer.toBytes("xyz.geojson", { tSrs: "EPSG:4978" }), "xyz.geojson");
  const point = out.toGeoJSON().features[0].geometry.coordinates;
  for (const [i, expected] of [-3981230.6461390946, 2585441.4118733606, -4245670.74916175].entries()) assert.ok(Math.abs(point[i] - expected) < 0.001);
});

test("Shapefile ZIP writes point, line and polygon with scalar attributes and nulls", () => {
  for (const geometry of [
    { type: "Point", coordinates: [8.5, 47.4] },
    { type: "Point", coordinates: [8.5, 47.4, 123.5] },
    { type: "LineString", coordinates: [[0, 0], [2, 3], [5, 1]] },
    { type: "Polygon", coordinates: [[[0, 0], [0, 5], [5, 5], [5, 0], [0, 0]], [[1, 1], [2, 1], [2, 2], [1, 2], [1, 1]]] },
  ]) {
    const source = { type: "FeatureCollection", features: [
      { type: "Feature", geometry, properties: { name: "Zürich", id: 12345678901, real: 2.5, flag: true } },
      { type: "Feature", geometry: null, properties: { name: null, id: null, real: null, flag: null } },
    ] };
    const ds = terrio.fromGeoJSON(source, "source", "EPSG:4326");
    assert.equal(ds.toGeoJSON().features[0].properties.name, "Zürich");
    const bytes = ds.toBytes("places.shp.zip");
    assert.deepEqual(Array.from(bytes.slice(0, 4)), [80, 75, 3, 4]);
    const back = terrio.open(bytes, "places.shp.zip");
    assert.equal(back.info().layers[0].crs.epsg, 4326);
    assert.deepEqual(back.layerNames(), ["places"]);
    const actual = back.toGeoJSON();
    assert.deepEqual(actual.features.map(f => f.geometry), source.features.map(f => f.geometry));
    assert.deepEqual(actual.features.map(f => f.properties), source.features.map(f => f.properties));
    assert.throws(() => ds.toBytes("places.shp"), /shp.zip/);
    assert.ok(ds.toBytes("places.zip", { format: "ESRI Shapefile" }).length > 0);
  }
  const point = (text) => terrio.fromGeoJSON({ type: "FeatureCollection", features: [
    { type: "Feature", geometry: { type: "Point", coordinates: [1, 2] }, properties: { url: text } },
  ] });
  assert.equal(terrio.open(point("é".repeat(127)).toBytes("p.shp.zip"), "p.zip").toGeoJSON().features[0].properties.url, "é".repeat(127));
  assert.equal(terrio.open(point("*").toBytes("p.shp.zip"), "p.zip").toGeoJSON().features[0].properties.url, "*");
  assert.throws(() => point("https://example.org/" + "a".repeat(254)).toBytes("p.shp.zip"), /254 UTF-8 bytes/);
  assert.throws(() => point("é".repeat(128)).toBytes("p.shp.zip"), /254 UTF-8 bytes/);
});

test("PROJJSON CRS definitions match GDAL in traditional GIS order", () => {
  const reference = JSON.parse(readFileSync(new URL("../../crates/terrio-crs/tests/data/projjson_reference.json", import.meta.url)));
  for (const c of reference.cases) {
    assert.equal(terrio.describeCrs(JSON.stringify(c.src)).epsg, null);
    assert.equal(terrio.describeCrs(JSON.stringify(c.dst)).epsg, null);
    const actual = terrio.transformPoints(c.points.map(p => p[0]), c.points.map(p => p[1]), JSON.stringify(c.src), JSON.stringify(c.dst));
    for (let i = 0; i < c.points.length; i++) {
      for (let axis = 0; axis < 2; axis++) assert.ok(Math.abs(actual[axis][i] - c.expected[i][axis]) <= c.tolerance[axis], `${c.name}, point ${i}, axis ${axis}`);
    }
  }
  assert.throws(() => terrio.describeCrs('{"type":"VerticalCRS","name":"unsupported"}'), /PROJJSON/);
});

test("IAU_2015 planetary coordinates match the full GDAL registry oracle", () => {
  const reference = JSON.parse(readFileSync(new URL("../../crates/terrio-crs/tests/data/iau_reference.json", import.meta.url)));
  for (const c of reference.cases) {
    const source = `IAU_2015:${c.base}`, target = `IAU_2015:${c.code}`;
    const result = terrio.transformPoints(c.points.map(p => p[0]), c.points.map(p => p[1]), source, target);
    for (let i = 0; i < c.points.length; i++) {
      for (let axis = 0; axis < 2; axis++) assert.ok(Math.abs(result[axis][i] - c.forward[i][axis]) <= 0.001, `${target} forward ${i}/${axis}`);
      if (c.inverse[i] === null) continue; // Native tests cover domain refusals/recovery.
      const inverse = terrio.transformPoints([c.forward[i][0]], [c.forward[i][1]], target, source);
      for (let axis = 0; axis < 2; axis++) {
        let delta = inverse[axis][0] - c.inverse[i][axis];
        if (axis === c.longitude_axis) delta = ((delta + 180) % 360 + 360) % 360 - 180;
        assert.ok(Math.abs(delta) <= 1e-8, `${target} inverse ${i}/${axis}: ${delta}`);
      }
    }
  }
  for (const c of reference.extras) {
    const result = terrio.transformPoints(c.points.map(p => p[0]), c.points.map(p => p[1]), c.source, c.target);
    const inverse = terrio.transformPoints(c.forward.map(p => p[0]), c.forward.map(p => p[1]), c.target, c.source);
    for (let i = 0; i < c.points.length; i++) for (let axis = 0; axis < 2; axis++) {
      assert.ok(Math.abs(result[axis][i] - c.forward[i][axis]) * c.metres_per_unit <= 0.001, c.name);
      assert.ok(Math.abs(inverse[axis][i] - c.inverse[i][axis]) <= 1e-8, c.name);
    }
  }
  assert.deepEqual(terrio.describeCrs("IAU:2015:49900").authority, ["IAU_2015", 49900]);
  assert.equal(terrio.describeCrs("IAU_2015:49900").epsg, null);
  assert.throws(() => terrio.describeCrs("IAU:2020:49900"), /IAU/);
  assert.throws(() => terrio.transformPoints([0], [0], "IAU_2015:49900", "EPSG:4326"), /celestial body/);
});


test("IAU_2015 strict datum selection", () => {
  assert.throws(() => terrio.transformPoints([45], [-10], "IAU_2015:49900", "IAU_2015:49901", undefined, { allowBallpark: false }), /ALLOW_BALLPARK/);
  const result = terrio.transformPoints([45], [-10], "IAU_2015:49901", "IAU_2015:49902", undefined, { allowBallpark: false });
  assert.ok(Math.abs(result[0][0] - 10) < 1e-8);
  assert.ok(Math.abs(result[1][0] - 44.6617680466192) < 1e-8);
});


test("edited IAU_2015 WKT parameters override its root identifier", () => {
  const source = "IAU_2015:49910";
  const wkt = terrio.describeCrs(source).wkt;
  const edited = wkt.replace('PARAMETER["False easting",0,', 'PARAMETER["False easting",1234,');
  assert.notEqual(wkt, edited);
  assert.equal(terrio.describeCrs(edited).authority, null);
  const [x, y] = terrio.transformPoints([0], [0], source, edited);
  assert.ok(Math.abs(x[0] - 1234) < 1e-8);
  assert.ok(Math.abs(y[0]) < 1e-8);
});


test("near-sided and tilted perspective projections match PROJ", () => {
  const reference = JSON.parse(readFileSync(new URL("../../crates/terrio-crs/tests/data/perspective_reference.json", import.meta.url), "utf8"));
  for (const c of reference.cases) {
    const visible = c.samples.filter(s => s[2] !== null);
    const result = terrio.transformPoints(visible.map(s => s[0]), visible.map(s => s[1]), c.source, c.target);
    for (let i = 0; i < visible.length; i++) for (let j = 0; j < 2; j++) {
      assert.ok(Math.abs(result[j][i] - visible[i][2][j]) * c.metres_per_unit <= 0.001, `${c.name} forward ${i}/${j}`);
    }
    const reversible = visible.filter(s => s[3] !== null);
    const inverse = terrio.transformPoints(reversible.map(s => s[2][0]), reversible.map(s => s[2][1]), c.target, c.source);
    for (let i = 0; i < reversible.length; i++) {
      const [lon, lat] = reversible[i][3];
      const dlon = ((inverse[0][i] - lon + 540) % 360 - 180) * Math.cos(lat * Math.PI / 180);
      assert.ok(Math.abs(dlon) <= 1e-8 && Math.abs(inverse[1][i] - lat) <= 1e-8, `${c.name} inverse ${i}`);
    }
    const invisible = c.samples.find(s => s[2] === null);
    assert.throws(() => terrio.transformPoints([invisible[0]], [invisible[1]], c.source, c.target));
    for (const [point, expected] of c.inverse_samples) if (expected === null) {
      assert.throws(() => terrio.transformPoints([point[0]], [point[1]], c.target, c.source));
    }
  }
});

test("vector geometry operations: simplify, segmentize, hulls, swap-xy, set-geom-type, combine, sort", () => {
  const line = {
    type: "FeatureCollection",
    features: [
      { type: "Feature", properties: { k: "a" }, geometry: { type: "LineString", coordinates: [[0, 0], [5, 0.1], [10, 0]] } },
      { type: "Feature", properties: { k: "a" }, geometry: { type: "Point", coordinates: [1, 2] } },
      { type: "Feature", properties: { k: "b" }, geometry: null },
    ],
  };
  const simple = terrio.simplify(line, 1).toGeoJSON();
  assert.deepEqual(simple.features[0].geometry.coordinates, [[0, 0], [10, 0]]);
  assert.equal(simple.features[2].geometry, null);
  const dense = terrio.segmentize(line, 3).toGeoJSON();
  assert.equal(dense.features[0].geometry.coordinates.length, 5);
  assert.equal(terrio.convexHull(line).toGeoJSON().features[1].geometry.type, "Point");
  const pts = {
    type: "Feature",
    properties: {},
    geometry: { type: "MultiPoint", coordinates: [[0, 0], [10, 1], [11, 10], [1, 12], [5, 5], [5, 9], [2, 3], [8, 3]] },
  };
  assert.equal(terrio.concaveHull(pts, 0, { allowHoles: false }).toGeoJSON().features[0].geometry.type, "Polygon");
  assert.deepEqual(terrio.swapXy(line).toGeoJSON().features[1].geometry.coordinates, [2, 1]);
  const multi = terrio.setGeomType(line, { multi: true }).toGeoJSON();
  assert.equal(multi.features[0].geometry.type, "MultiLineString");
  assert.equal(terrio.setGeomType(line, { geometryType: "POINT", skip: true }).toGeoJSON().features.length, 2);
  const groups = terrio.combine(line, { groupBy: ["k"] }).toGeoJSON();
  assert.deepEqual(groups.features.map((f) => f.properties.k), ["a", "b"]);
  assert.equal(groups.features[0].geometry.type, "GeometryCollection");
  const sorted = terrio.sort(line, { method: "strtree" }).toGeoJSON();
  assert.equal(sorted.features[2].geometry, null);
  assert.throws(() => terrio.sort(line, { method: "nope" }), /sort method/);
  assert.throws(() => terrio.simplify(line, -1), /tolerance/);
});

test("compare, pixelInfo and xyz", () => {
  const values = Float64Array.from({ length: 20 }, (_, i) => i);
  const a = terrio.rasterFromArray(values, 5, 4, [100, 10, 0, 240, 0, -10], "EPSG:32611");
  const edited = Float64Array.from(values);
  edited[7] += 2.5;
  const b = terrio.rasterFromArray(edited, 5, 4, [100, 10, 0, 240, 0, -10], "EPSG:32611");
  assert.equal(terrio.compare(a, a).difference_count, 0);
  const c = terrio.compare(a, b);
  assert.deepEqual(c.differences.map((d) => d.message), ["1: pixels differing: 1", "1: maximum pixel value difference: 2.500000"]);
  assert.equal(c.bands[0].pixels_differing, 1);
  const bytes = a.toBytes("a.tif");
  assert.equal(terrio.compare(bytes, bytes).difference_count, 0);
  const otherBytes = b.toBytes("b.tif");
  assert.equal(terrio.compare(bytes, otherBytes).differences[0].kind, "binary");
  const reads = [0, 0];
  const named = (payload, index) => ({ get bytes() { reads[index]++; return payload; }, filename: `${index}.tif` });
  assert.equal(terrio.compare(named(bytes, 0), named(otherBytes, 1)).differences[0].kind, "binary");
  assert.deepEqual(reads, [1, 1]);
  assert.equal(terrio.compare(bytes, b, {skipBinary: true, skipMetadata: true}).difference_count, c.difference_count);
  assert.equal(terrio.compare(a, otherBytes, {skipBinary: true, skipMetadata: true}).difference_count, c.difference_count);

  const twoBands = terrio.rasterFromArray(new Float64Array([0, 10]), 1, 1, [0, 1, 0, 1, 0, -1]);
  assert.equal(terrio.pixelInfo(twoBands, [[0, 0]], {format: "value", fieldSep: "|"}), "0|10\n");
  const g = terrio.pixelInfo(a, [[125, 225, "site"]], { positionCrs: "dataset", resampling: "bilinear" });
  assert.equal(g.crs.properties.name, "urn:ogc:def:crs:EPSG::32611");
  assert.equal(g.features[0].properties.extra_content, "site");
  const marked = terrio.pixelInfo(a, [[0.5, 0.5, "\0NaN"]]);
  assert.equal(marked.features[0].properties.extra_content, "\0NaN");
  assert.equal(g.features[0].properties.bands[0].raw_value, 7);
  assert.equal(terrio.pixelInfo(a, [[0.5, 0.5]], { format: "text" }), "Report:\n  Location: (0P,0L)\n  Band 1:\n    Value: 0\n");
  const points = { type: "FeatureCollection", features: [{ type: "Feature", properties: { id: 3 }, geometry: { type: "Point", coordinates: [125, 225] } }] };
  const fromLayer = terrio.pixelInfo(a, points, { positionCrs: "dataset", format: "json" });
  assert.equal(fromLayer[0].bands[0].value, 7);

  assert.equal(terrio.xyz(a, { skip: [2], srcwin: [0, 0, 5, 1] }), "105.000 235.000 0\n125.000 235.000 2\n145.000 235.000 4\n");
});

test("promoteTo3d transforms ellipsoidal heights of a 2D CRS", () => {
  // GDAL 3.12.2 / PROJ 9.8.1, RD New to WGS 84 3D, without grids.
  const args = [[85000], [447000], "EPSG:28992", "EPSG:4979", [0]];
  const normal = terrio.transformPoints(...args);
  const promoted = terrio.transformPoints(...args, { promoteTo3d: true });
  assert.equal(normal[2][0], 0);
  assert.ok(Math.abs(promoted[2][0] - 43.57639261614531) < 0.001);
  assert.ok(Math.abs(promoted[0][0] - 4.367683424792349) < 1e-8);
  assert.ok(Math.abs(promoted[1][0] - 52.006949098822965) < 1e-8);
});

test("promoteTo3d applies to vector toBytes and preserves 2D geometry", () => {
  const source = terrio.fromGeoJSON({ type: "FeatureCollection", features: [
    { type: "Feature", properties: {}, geometry: { type: "Point", coordinates: [85000, 447000, 0] } },
    { type: "Feature", properties: {}, geometry: { type: "Point", coordinates: [85000, 447000, 100] } },
    { type: "Feature", properties: {}, geometry: { type: "Point", coordinates: [85000, 447000] } },
  ] }, "points", "EPSG:28992");
  const convert = promoteTo3d => terrio.open(source.toBytes("points.geojson", {
    tSrs: "EPSG:4979", promoteTo3d,
  }), "points.geojson").toGeoJSON().features.map(feature => feature.geometry.coordinates);
  assert.deepEqual(convert(false).map(point => point[2]), [0, 100, undefined]);
  const promoted = convert(true);
  for (const [index, height] of [[0, 43.57639261614531], [1, 143.576799856499]]) {
    assert.ok(Math.abs(promoted[index][2] - height) < 0.001, `height ${index}: ${promoted[index][2]}`);
  }
  assert.equal(promoted[2].length, 2);
});

test("unterminated quoted PROJ values raise an error", () => {
  assert.throws(() => terrio.transformPoints([10], [40], "EPSG:4326",
    '+proj=tmerc +datum=WGS84 +title="abc +k=0.9996'), /unterminated quoted PROJ value/);
});


test("vector dataset commands: rename, edit and key update preserve source datasets", () => {
  const geo = (rows) => ({ type: "FeatureCollection", features: rows.map(([name, n], i) => ({ type: "Feature", properties: {name, n}, geometry: {type: "Point", coordinates: [i, i]} })) });
  const original = terrio.concat([geo([["x", 1], ["y", 2]])], {mode: "single", outputLayer: "Café"});
  const renamed = terrio.renameLayer(original, {ascii: true, lowerCase: true});
  assert.equal(terrio.exportSchema(renamed).layers[0].name, "cafe");
  assert.equal(terrio.exportSchema(original).layers[0].name, "Café");
  const edited = terrio.editVector(renamed, {crs: "EPSG:3857"});
  assert.equal(terrio.exportSchema(edited).layers[0].geometryFields[0].coordinateSystem.authid, "EPSG:3857");
  const updated = terrio.updateVector(geo([["x", 9], ["z", 3]]), renamed, {key: ["name"]});
  assert.equal(updated.info().layers[0].feature_count, 3);
  assert.equal(renamed.info().layers[0].feature_count, 2);
  assert.throws(() => terrio.editVector(original, {metadata: ["A=B"]}), /unknown field/);
});
