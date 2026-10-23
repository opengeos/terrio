# JavaScript

[terrio](https://github.com/opengeos/terrio), pure-Rust geospatial data I/O, compiled to
WebAssembly for browsers and Node.js. Datasets are opened from bytes, with a file name telling the
format, and written back to bytes. Try it without installing anything on the [Playground](playground.md).

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
const ipc = roads.toArrowIPC(0, { geometryEncoding: "geoarrow" }); // an Arrow IPC stream
const table = tableFromIPC(ipc);  // Apache Arrow JS; the geometry column is GeoArrow
const back = fromArrowIPC(tableToIPC(table), "roads");

const [xs, ys] = transformPoints([-122.4], [37.8], "EPSG:4326", "EPSG:32610");
// Datum shift and geoid grids from their bytes (https://cdn.proj.org), heights and epochs.
registerGrid("us_nga_egm96_15.tif", new Uint8Array(await (await fetch(gridUrl)).arrayBuffer()));
const [lon, lat, h] = transformPoints([-100], [40], "EPSG:4979", "EPSG:4326+5773", [100]);
transformPoints([135], [-25], "EPSG:7912", "EPSG:7843", [0], { sCoordEpoch: 2010.0 });
```

In WebAssembly, these formats are read from and written to memory:

| | Read | Write |
|---|---|---|
| Raster | GeoTIFF/COG (LZW, Deflate, PackBits, ZSTD, LZMA, JPEG, WebP, LERC, JPEG XL), JPEG 2000 (JP2 and J2K), netCDF, HDF5, Zarr and GeoZarr (zipped, or with `openZarr`), Esri ASCII grid, GeoPackage, PMTiles and MBTiles (read by window, zoom levels as overviews) | GeoTIFF, COG (lossless compressions, ZSTD included), JPEG 2000, Esri ASCII grid, GeoPackage, PMTiles, MBTiles |
| Vector | GeoJSON, TopoJSON, FlatGeobuf\*, GeoPackage, CSV, GeoParquet, KML, KMZ, GML, GPX, OSM (PBF and XML), PMTiles, MBTiles, MVT tiles, zip archives of these, and Shapefile and MapInfo (MIF and TAB) in zip archives | GeoJSON, TopoJSON, FlatGeobuf, GeoPackage, KML, KMZ, GML, GPX, PMTiles, MBTiles (`minzoom`, `maxzoom` options) |

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

## Threads: `terrio-js/load` and `terrio-js/threads`

A third module, `terrio-js/threads`, has every format and a thread pool of Web Workers (or
Node.js worker threads) over shared memory: GeoTIFF tiles and strips, JPEG 2000 tiles and Zarr
chunks decode in parallel, and GeoTIFF/COG tiles, overviews and Zarr chunks are compressed in
parallel, as natively. `terrio-js/load` picks it where WebAssembly threads run (Node.js 22 or
later, and pages that are cross-origin isolated, served with `Cross-Origin-Opener-Policy:
same-origin` and `Cross-Origin-Embedder-Policy: require-corp`), starts its pool, and loads the
single-threaded module everywhere else:

```js
import { load } from "terrio-js/load";

const terrio = await load();      // or load({ threads: 4 }), load({ threads: false }), load({ core: true })
terrio.features();                // { build: "terrio-js/threads", threads: 8, ... } or { build: "terrio-js", threads: 1, ... }
const dem = terrio.open(bytes, "dem.tif");
```

The threaded module can also be imported and started by hand: `import init, { initThreadPool }
from "terrio-js/threads"`, then `await init()` and `await initThreadPool(navigator.hardwareConcurrency)`.
Its results are those of the single-threaded module, byte for byte. With 16 threads, against one
(a Xeon w7-3445, best of three):

| | Node.js 26 | Chromium | Firefox |
|---|---|---|---|
| 3612 x 3612 Float32 LZW GeoTIFF, decoded | 542 → 92 ms | 580 → 91 ms | 622 → 113 ms |
| The same, written as a DEFLATE COG with overviews | 2467 → 302 ms | 2460 → 306 ms | 2680 → 340 ms |
| 10980 x 10980 Sentinel-2 JPEG 2000 band, decoded | 6.8 → 0.9 s | 6.6 → 0.9 s | 6.7 → 0.9 s |

Work on vector features (parsing and writing GeoJSON, FlatGeobuf, GeoPackage...) stays on the
calling thread in WebAssembly: its many small allocations contend on WebAssembly's allocator, one
lock for all threads, and ran several times slower on more threads.

## Building

```sh
node js/build.mjs                         # the npm package in js/pkg: all modules, built for size
node --test js/tests/package.test.mjs     # (the threaded module needs the nightly js/build.mjs names)
TERRIO_JS_LOAD=../pkg/load.js node --test js/tests/node.test.mjs   # the suite on the threaded module
node js/tests/browser.mjs ../terrio-data  # in headless browsers (Playwright), timings included
wasm-pack build js --release --target nodejs --out-dir pkg-node --out-name terrio
node --test js/tests/node.test.mjs        # TERRIO_TEST_DATA=... adds real files checked against GDAL
# terrio-js/core, and the tests of the formats it has:
wasm-pack build js --release --target nodejs --out-dir pkg-node/core --out-name terrio --no-pack -- --no-default-features --features core
TERRIO_JS_PKG=../pkg-node/core/terrio.js node --test js/tests/node.test.mjs
node js/bench/bench.mjs ../terrio-data    # timings of decoding and writing real files
```
