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
| Raster | GeoTIFF/COG (LZW, Deflate, PackBits, ZSTD, LZMA, JPEG, WebP, LERC, JPEG XL), JPEG 2000 (JP2 and J2K), GRIB 1 and 2 (read band by band), netCDF, HDF5, Zarr and GeoZarr (zipped, or with `openZarr`), Esri ASCII grid, GeoPackage, PMTiles and MBTiles (read by window, zoom levels as overviews) | GeoTIFF, COG (lossless compressions, ZSTD included), JPEG 2000, Esri ASCII grid, GeoPackage, PMTiles, MBTiles |
| Vector | GeoJSON, TopoJSON, FlatGeobuf\*, GeoPackage, GeoParquet, CSV, KML, KMZ, GML, GPX, DWG and DXF (full module), OSM PBF, zip archives of these, Shapefile and MapInfo (MIF/MID) in zip archives | GeoJSON, TopoJSON, FlatGeobuf, GeoPackage, KML, KMZ, GML, GPX, DWG and DXF (full module) |
| Point cloud | LAS, LAZ, COPC, PLY, E57 | LAS, LAZ, COPC, PLY, E57 |

\* FlatGeobuf files are read by terrio's own driver, with 64-bit offsets. `open` decodes a layer
whole, within WebAssembly's 4 GB of memory; with the file's bytes, that takes about four times the
file's size for lines and five for polygons (3.2 GB and 5.4 GB in native builds): an 867 MB file of
2 million OpenStreetMap lines opens in about 3 seconds in Node.js, and a 1 GB file of 2.3 million
polygons runs out of memory, but in the memory64 module (see below). `openUrl` reads files with a
spatial index by bounding box, decoding only the features found; a file without one is still read
whole.

JPEG 2000 files decode with the codec of GDAL's JP2OpenJPEG driver (a Rust port of OpenJPEG), with
their GeoJP2 or GMLJP2 georeferencing: a Sentinel-2 band (`T10SEG_..._B04.jp2`, 10980 x 10980)
opens in about 6 seconds in Node.js. Written with a nodata value, a JPEG 2000 comes back as a zip
archive of the `.jp2` file and the `.aux.xml` sidecar that holds the value, as GDAL writes it;
`open` reads that archive back.


## A smaller download: `terrio-js/core`

The package holds two WebAssembly modules with the same API. `terrio-js` has every format above;
`terrio-js/core` leaves out the heaviest ones (GeoParquet and Arrow IPC, netCDF and HDF5, point
clouds, JPEG 2000, GRIB, Zarr, AVIF tiles and OpenStreetMap) and keeps GeoTIFF/COG with all its
compressions, GeoJSON and the other text formats, FlatGeobuf, GeoPackage, Shapefile, PMTiles and
MBTiles, the CRS transformations and the operations, with a smaller download. The full module also
includes DWG/DXF through the `cad` feature; the core module leaves CAD out. See the
[CAD supported subset](../docs/formats.md#cad-drawings-dwg-and-dxf).

Measured from the optimized CAD-enabled [CI package at `b371d79`](https://github.com/opengeos/terrio/actions/runs/40144408237)
(`wasm-release` profile, wasm-opt `-Oz`). MB are decimal (1,000,000 bytes); compression uses
gzip level 9 with no timestamp and Brotli quality 11. These are a recorded build snapshot,
not a guarantee for other Rust/wasm-opt versions:

| Module | `.wasm` | gzip | Brotli |
|---|---|---|---|
| `terrio-js` | 22.10 MB | 7.71 MB | 5.67 MB |
| `terrio-js/core` | 9.67 MB | 3.36 MB | 2.56 MB |

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
for size, and wasm-opt -Oz.

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

## Datasets larger than 4 GB: `terrio-js/memory64`

WebAssembly modules have 32-bit memories, at most 4 GB. The `terrio-js/memory64` module (every
format, single-threaded) is built for WebAssembly's 64-bit memories (memory64), up to 16 GB, the
limit of V8. `load({ memory64: true })` loads it where the runtime has memory64 (Node.js 24 and
later, Chrome and Edge 133, Firefox 134), and the usual 32-bit module elsewhere (Safari), which
`onFallback` reports and `features().build` tells:

```js
import { load, memory64Supported } from "terrio-js/load";

const terrio = await load({ memory64: true });
const ds = terrio.open(bytes, "polygons.fgb");   // a 1.27 GB FlatGeobuf of 289,890 polygons
terrio.features().memory;                        // 8 GB of memory: beyond a 32-bit module's reach
```

That FlatGeobuf, 290 thousand US county polygons, opens in 4 seconds in Node.js with 7.5 GB of
module memory, where the 32-bit modules run out of memory. It is 7 to 17% slower than the 32-bit
module (64-bit addresses and bounds checks), and larger: 25.4 MB, 6.4 MB gzipped (4.4 MB Brotli).
Use it for the datasets that need it. It is single-threaded, and `readBand` windows are numbers of
pixels below 2^32, as in the other modules.

It is built with the parts of the toolchain still in progress: `wasm64-unknown-unknown` is a
tier 3 Rust target without a prebuilt standard library ([platform support](https://doc.rust-lang.org/rustc/platform-support/wasm64-unknown-unknown.html)),
built on nightly with `-Z build-std`; its SIMD intrinsics, which zlib-rs uses, are unstable
([rust-lang/rust#90599](https://github.com/rust-lang/rust/issues/90599)); wasm-pack builds for
wasm32 only, so `js/build.mjs` runs cargo and wasm-bindgen (which supports wasm64) itself; and the
wasm-opt wasm-pack downloads (binaryen 117) cannot read 64-bit tables, so the module is not
passed through it. Three crates treat WebAssembly as wasm32 only, and the build patches copies of
them (`target_arch = "wasm32"` read as `target_family = "wasm"`): `tar`, which wbraster depends
on, `zstd-sys` (its libc shim, for GeoParquet's Zstandard) and `arrow-buffer`; getrandom, which
seeds ahash's hash maps, has no wasm64 backend and is given one (`crypto.getRandomValues`). The C
code (Zstandard) needs clang 20 or later: older ones index the function table with 32 bits,
where Rust's code uses a 64-bit table, and the module they link together does not validate.

## Building

```sh
node js/build.mjs                         # the npm package in js/pkg: all modules, built for size
node --test js/tests/package.test.mjs     # (threads and memory64 need the nightly js/build.mjs names; memory64, clang 20+)
TERRIO_JS_LOAD=../pkg/load.js node --test js/tests/node.test.mjs   # the suite on the threaded module
TERRIO_JS_LOAD=../pkg/load.js TERRIO_MEMORY64=1 TERRIO_LARGE_TESTS=1 node --test js/tests/node.test.mjs  # memory64, > 4 GB
node js/tests/browser.mjs ../terrio-data  # in headless browsers (Playwright), timings included
wasm-pack build js --release --target nodejs --out-dir pkg-node --out-name terrio
node --test js/tests/node.test.mjs        # TERRIO_TEST_DATA=... adds real files checked against GDAL
# terrio-js/core, and the tests of the formats it has:
wasm-pack build js --release --target nodejs --out-dir pkg-node/core --out-name terrio --no-pack -- --no-default-features --features core
TERRIO_JS_PKG=../pkg-node/core/terrio.js node --test js/tests/node.test.mjs
node js/bench/bench.mjs ../terrio-data    # timings of decoding and writing real files
```

### Warp source overviews

```js
const preview = warp(dataset, { ts: [256, 256], overview: "AUTO" });
const finer = warp(dataset, { ts: [256, 256], overview: "AUTO-1" });
const full = warp(dataset, { ts: [256, 256], overview: "NONE" });
const level = warp(dataset, { overview: 1 });
```

`AUTO` is the default. Datasets, file bytes and mixed or repeated datasets in
mosaics preserve their stored pyramids. The output grid is planned at full
resolution before source levels are selected. `toBytes` also accepts `overviews`
(decimation factors) and `overviewResampling` when writing GeoTIFF pyramids.

### Fill nodata

```js
const filled = fillNodata(dem, { maxDistance: 100, smoothingIterations: 1 });
const nearest = fillNodata(image, { band: 2, mask: validity, interpolation: "nearest" });
```

`fillNodata` is a named export. Inputs and masks accept the same datasets or file bytes
as the other operations. A mask is zero at holes and nonzero at donor pixels. Without
one, the source's mask or nodata is used. The result contains one band, retaining its
type, nodata and georeferencing; valid samples remain unchanged. `maxDistance: 0`
searches the whole image. Smoothing leaves original valid pixels and first/last rows
unchanged. As in GDAL, interpolation uses Float32 samples.

### Serialized VRTs

```js
const virtual = openVrt(xml, { "dem.tif": imageBytes });
const crop = virtual.readBand(1, [32, 48, 128, 128]);
```

`openVrt` resolves source names to `Uint8Array` bytes, retaining lazy GeoTIFF and
VRT window reads in the browser and Node.js. It supports scalar built-in derived
bands, full/separable kernel filters, mask sources and serialized warped VRTs
(affine/reprojection, GCP/TPS/homography/RPC). External pixel functions, complex
buffers and serialized cutline/alpha/geolocation warps are unsupported.

`transformPoints`, `Dataset.toBytes` and `warp` accept `allowBallpark: false` and
`onlyBest: true` to control horizontal datum-operation fallbacks. Read diagnostics
with `takeWarnings()` after an operation. `warp` also accepts `kernelVersion: "3.13"`
for GDAL 3.13 resampling; `"3.12"` remains the default. GeoTIFF/COG outputs preserve
PixelIsPoint metadata and corner georeferencing.
