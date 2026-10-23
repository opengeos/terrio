# terrio (WebAssembly)

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark-dark.svg">
    <img src="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark.svg" alt="terrio" height="72">
  </picture>
</p>

[![npm](https://img.shields.io/npm/v/terrio-js.svg)](https://www.npmjs.com/package/terrio-js) [![CI](https://github.com/opengeos/terrio/actions/workflows/ci.yml/badge.svg)](https://github.com/opengeos/terrio/actions/workflows/ci.yml) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/opengeos/terrio/blob/main/LICENSE)

[terrio](https://github.com/opengeos/terrio), pure-Rust geospatial data I/O, compiled to
WebAssembly for browsers and Node.js. Datasets are opened from bytes, with a file name telling the
format, and written back to bytes.

```js
import init, { open, fromGeoJSON, rasterFromArray, transformPoints, describeCrs } from "terrio-js";

await init(); // in Node.js: await init({ module_or_path: fs.readFileSync(wasmPath) })

const bytes = new Uint8Array(await (await fetch("dem.tif")).arrayBuffer());
const dem = open(bytes, "dem.tif");
dem.info();                       // the summary `terrio info --json` prints
dem.width, dem.height, dem.count, dem.dtype, dem.crs, dem.transform, dem.nodata
const band = dem.readBand(1);     // Float32Array, Uint8Array... row after row
const tile = dem.readBand(1, [0, 0, 256, 256]);   // a window [col_off, row_off, width, height]
const cog = dem.toBytes("dem_cog.tif", { format: "COG", compress: "DEFLATE" });

const roads = open(geojsonBytes, "roads.geojson");
roads.toGeoJSON();                // a FeatureCollection object
const fgb = roads.toBytes("roads.fgb", { tSrs: "EPSG:3857", where: "lanes > 2" });
const ipc = roads.toArrowIPC(0, { geometryEncoding: "geoarrow" }); // Arrow IPC stream, GeoArrow geometries
const back = fromArrowIPC(ipc, "roads");

const [xs, ys] = transformPoints([-122.4], [37.8], "EPSG:4326", "EPSG:32610");
registerGrid("us_nga_egm96_15.tif", gridBytes);  // datum shift and geoid grids, from bytes
const [lon, lat, h] = transformPoints([-100], [40], "EPSG:4979", "EPSG:4326+5773", [100]);

// Point clouds: LAS, LAZ, COPC, PLY and E57.
const lidar = openPoints(lazBytes, "tile.laz");
lidar.format, lidar.pointCount, lidar.crs, lidar.info();
const ground = lidar.read({ classes: [2], bounds: [x0, y0, x1, y1] }); // COPC: also maxLevel, resolution
ground.get("z");                  // Float64Array; intensity is a Uint16Array...
ground.toBytes("ground.copc.laz"); // LAS, LAZ, COPC, PLY or E57
ground.grid(1, { stat: ["idw"] }); // a raster Dataset
ground.toVectorBytes("ground.fgb", ["z", "intensity"]);
pointsFromArrays({ x, y, z, classification }, "EPSG:32610");
```

In WebAssembly, these formats are read from and written to memory:

| | Read | Write |
|---|---|---|
| Raster | GeoTIFF/COG (LZW, Deflate, PackBits, ZSTD, LZMA, JPEG, WebP, LERC, JPEG XL), JPEG 2000 (JP2 and J2K), netCDF, HDF5, Zarr and GeoZarr (zipped, or with `openZarr`), Esri ASCII grid, GeoPackage, PMTiles and MBTiles (read by window, zoom levels as overviews) | GeoTIFF, COG (lossless compressions, ZSTD included), JPEG 2000, Esri ASCII grid, GeoPackage, PMTiles, MBTiles |
| Vector | GeoJSON, TopoJSON, FlatGeobuf\*, GeoPackage, GeoParquet, CSV, KML, KMZ, GML, GPX, OSM PBF, zip archives of these, Shapefile and MapInfo (MIF/MID) in zip archives | GeoJSON, TopoJSON, FlatGeobuf, GeoPackage, KML, KMZ, GML, GPX |
| Point cloud | LAS, LAZ, COPC, PLY, E57 | LAS, LAZ, COPC, PLY, E57 |

\* FlatGeobuf files are read by terrio's own driver, with 64-bit offsets. `open` decodes a layer
whole, within WebAssembly's 4 GB of memory; with the file's bytes, that takes about four times the
file's size for lines and five for polygons (3.2 GB and 5.4 GB in native builds): an 867 MB file of
2 million OpenStreetMap lines opens in about 3 seconds in Node.js, and a 1 GB file of 2.3 million
polygons runs out of memory. `openUrl` reads files with a spatial index by bounding box, decoding
only the features found; a file without one is still read whole.

JPEG 2000 files decode with the codec of GDAL's JP2OpenJPEG driver (a Rust port of OpenJPEG), with
their GeoJP2 or GMLJP2 georeferencing: a Sentinel-2 band (`T10SEG_..._B04.jp2`, 10980 x 10980)
opens in about 6 seconds in Node.js. Written with a nodata value, a JPEG 2000 comes back as a zip
archive of the `.jp2` file and the `.aux.xml` sidecar that holds the value, as GDAL writes it;
`open` reads that archive back.


## A smaller download: `terrio-js/core`

The package holds two WebAssembly modules with the same API. `terrio-js` has every format above;
`terrio-js/core` leaves out the heaviest ones (GeoParquet and Arrow IPC, netCDF and HDF5, point
clouds, JPEG 2000, Zarr, AVIF tiles and OpenStreetMap) and keeps GeoTIFF/COG with all its
compressions, GeoJSON and the other text formats, FlatGeobuf, GeoPackage, Shapefile, PMTiles and
MBTiles, the CRS transformations and the operations, at about half the size:

| Module | `.wasm` | gzip | Brotli |
|---|---|---|---|
| `terrio-js` | 16.4 MB | 5.6 MB | 4.1 MB |
| `terrio-js/core` | 8.9 MB | 3.0 MB | 2.3 MB |

```js
import init, { open, features } from "terrio-js/core";

await init();
features(); // { build: "terrio-js/core", features: ["flatgeobuf", "lerc", "lzma", "sqlite", "tiles"] }
```

A format the module leaves out fails with a message that says so (``built without the `jp2`
feature: terrio-js/core leaves it out, the full module (terrio-js) has it``). A page that needs those
formats only now and then can start with the core module and load the full one when it meets them,
with a dynamic `import("terrio-js")`: datasets belong to the module that opened them, so a dataset
is passed only to the functions of its own module.

Both modules are built for size: whole-program LTO, the codecs optimized for speed and the rest
for size, and wasm-opt -Oz. The full module is 21% smaller than before (20.6 MB, 6.7 MB gzipped)
and decodes and writes as fast.

## Building

```sh
node js/build.mjs                         # the npm package in js/pkg: both modules, built for size
node --test js/tests/package.test.mjs
wasm-pack build js --release --target nodejs --out-dir pkg-node --out-name terrio
node --test js/tests/node.test.mjs        # TERRIO_TEST_DATA=... adds real files checked against GDAL
# terrio-js/core, and the tests of the formats it has:
wasm-pack build js --release --target nodejs --out-dir pkg-node/core --out-name terrio --no-pack -- --no-default-features --features core
TERRIO_JS_PKG=../pkg-node/core/terrio.js node --test js/tests/node.test.mjs
node js/bench/bench.mjs ../terrio-data    # timings of decoding and writing real files
```
