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
| Raster | GeoTIFF/COG (LZW, Deflate, PackBits, ZSTD, LZMA, JPEG, WebP, LERC, JPEG XL), JPEG 2000 (JP2 and J2K), GRIB 1 and 2 (read band by band), netCDF, HDF5, Zarr and GeoZarr (zipped, or with `openZarr`), Esri ASCII grid, GeoPackage, PMTiles and MBTiles (read by window, zoom levels as overviews) | GeoTIFF, COG (lossless compressions, ZSTD included), JPEG 2000, Esri ASCII grid, GeoPackage, PMTiles, MBTiles |
| Vector | GeoJSON, TopoJSON, FlatGeobuf\*, GeoPackage, CSV, GeoParquet, KML, KMZ, GML, GPX, OSM (PBF and XML), PMTiles, MBTiles, MVT tiles, zip archives of these, and Shapefile and MapInfo (MIF and TAB) in zip archives | GeoJSON, TopoJSON, FlatGeobuf, GeoPackage, KML, KMZ, GML, GPX, PMTiles, MBTiles (`minzoom`, `maxzoom` options) |

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
MBTiles, the CRS transformations and the operations, at about half the size:

| Module | `.wasm` | gzip | Brotli |
|---|---|---|---|
| `terrio-js` | 16.9 MB | 5.9 MB | 4.3 MB |
| `terrio-js/core` | 9.2 MB | 3.2 MB | 2.4 MB |

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

## Native multidimensional arrays

See [Native multidimensional arrays](multidim.md) for scientific groups, named dimensions,
lazy coordinate arrays, time/depth slicing, transposition and netCDF/Zarr numeric snapshots.

## Indexed raster catalogs

`openCatalog(index, sources, options)` opens an index `Dataset`, vector file bytes or GeoJSON,
with a map of location strings to raster datasets or file bytes:

```javascript
const catalog = terrio.openCatalog(tileIndex, { "left.tif": left, "right.tif": right }, {
  tr: [30, 30], sortField: "priority", cacheSize: 16,
});
const pixels = catalog.readBand(1, [100, 200, 256, 256]);
```

The index uses an in-memory R-tree in WASM; only selected sources enter the bounded cache.
Supplied lazy datasets retain window/overview reads, and encoded files are opened only when
selected. Existing dataset buffers/readers remain shared with the caller; freeing the original
Dataset does not close the catalog's handle.
Locations are keys in the supplied source map; WASM does not open filesystem paths or URLs.
Options are `locationField`, `sortField`, `sortDescending`, `tr`, `te`, `srs`, `bands`, `ot`,
`nodata`, `alphaBand`, `cacheSize` and `resampling`. Source masks/nodata and zero alpha are
transparent; tiles in other CRSs are reprojected into the index CRS. See
[CLI catalogs](cli.md#catalog) for the supported GTI subset.

## Pansharpening

```javascript
const sharp = terrio.pansharpen(pan, rgbnir, { bands: [1, 2, 3], bitdepth: 12, nodata: 0 });
const rgb = terrio.pansharpen(pan, [red, green, blue], { weights: [0.6, 0.3, 0.1] });
const sources = { "pan.tif": panBytes, "rgb.tif": spectralBytes };
const xml = terrio.pansharpenVrt("pan.tif", ["rgb.tif"], sources, { bands: [1, 2, 3] });
const virtual = terrio.openVrt(xml, sources);
const window = virtual.readBand(1, [100, 200, 64, 64]);
```

Inputs can be datasets or raster bytes. All spectral inputs contribute to intensity;
`bands` selects/reorders output bands. See [pansharpen](cli.md#pansharpen) for working
types, interpolation, nodata, bit depth and extent handling.

## Focal statistics, collars and enhancement

```javascript
const max5 = terrio.neighbors(dem, { kernel: "equal", size: 5, method: "max" });
const edges = terrio.neighbors(image, { kernel: ["gaussian", "edge2"], band: 2, ot: "Float32" });
const clean = terrio.cleanCollar(scene, { addAlpha: true });          // flood fill
const nb = terrio.nearblack(scene, { colors: ["0,0,0", "255,255,255"], pixelDistance: 0 });
const lut = terrio.enhanceLuts(image, {});                            // gdalenhance's LUT file text
const eq = terrio.enhance(image, { equalize: true });
const again = terrio.enhance(image, { config: lut });
```

`nearblack` and `cleanCollar` accept `colors` as a single string or an array of strings.

These are `terrio neighbors`, `clean-collar`, `nearblack` and `enhance` (see
[neighbors](cli.md#neighbors), [clean-collar](cli.md#clean-collar-and-nearblack) and
[enhance](cli.md#enhance)), on rasters in memory. Datasets hold no mask: `addMask` is refused,
and an input's mask is not kept (use `addAlpha`). There is no in-place update; LUT files are
passed and returned as text.

## SQL queries

`sql(src, query)` returns a `Dataset` using the shared [OGR SQL subset](sql.md).
Inputs may be a `Dataset`, file bytes, `{ bytes, filename }`, or GeoJSON.

```js
const result = sql(countries, "SELECT ADMIN, POP_EST/1000000.0 AS millions FROM countries");
const summary = sql(countries, "SELECT COUNT(*) AS n, SUM(POP_EST) AS population FROM countries");
```

Raw GeoJSON inputs use the layer name `layer`; use `Layer.from_geojson(..., name=...)`
(Python) or `fromGeoJSON(src, name)` (JavaScript) to assign another name.

## Vector geometry operations

`simplify(src, tolerance)`, `segmentize(src, maxLength)`, `convexHull(src)`,
`concaveHull(src, ratio, { allowHoles, tight })`, `swapXy(src)`, `setGeomType(src, {
geometryType, multi, single, linear, dim, layerOnly, featureOnly, skip })`, `combine(src, {
groupBy, keepNested, addExtraFields })` and `sort(src, { method })` run the
[vector geometry commands](cli.md#vector-geometry-commands) on every layer of a `Dataset`,
file bytes or GeoJSON, and return a `Dataset`.

```js
const hulls = concaveHull(combine(places, { groupBy: ["adm0_a3"] }), 0.3, { allowHoles: true });
const simple = simplify(counties, 0.01);
const sorted = sort(simple, { method: "hilbert" });
```

## Vector datasets

```js
const merged = terrio.concat([countries, { bytes: usBytes, filename: "us.gpkg" }], { mode: "single", sourceLayerFieldName: "source" });
const parts = terrio.partition(counties, { fields: ["STATE"], format: "GPKG" }); // [{ path, bytes }]
const typed = terrio.setFieldType(countries, "Integer64", { fieldName: "POP_EST" });
const points = terrio.makePoint(trace, "longitude", "latitude", { crs: "EPSG:4326" });
const schema = terrio.exportSchema(countries); // the OGR_SCHEMA object
const empty = terrio.createVector("empty.gpkg", { schema }); // the new file's bytes
```

These are `terrio concat`, `partition`, `set-field-type`, `make-point`, `export-schema` and
`create` (GDAL's `gdal vector` commands of the same names), in memory: `partition` returns the
parts' relative paths and bytes, and `createVector` the bytes of the dataset, rather than
writing files. `exportSchema` describes the layers read (GeoPackage column declarations are
read only from files, by the command line, Python and Rust). See
[Vector datasets](cli.md#vector-datasets) for the options and GDAL's rules.

### Renaming and updating vector datasets

`renameLayer(src, options)`, `editVector(src, options)` and
`updateVector(src, dst, options)` return a new `Dataset`, preserving their inputs.
Serialize the result with `toBytes` to persist it.

```javascript
const renamed = terrio.renameLayer(countries, {ascii: true, lowerCase: true});
const assigned = terrio.editVector(renamed, {crs: "EPSG:4326"});
const updated = terrio.updateVector(changes, renamed, {key: ["name"], mode: "merge"});
```

Rename accepts `inputLayer`, `outputLayer`, `ascii`, `lowerCase`,
`filenameCompatible`, `reservedCharacters`, `replacementCharacter`, `maxLength`.
Edit accepts `activeLayer`, `crs` (`"none"` removes it), `geometryType`, `unsetFid`.
CRS assignment does not transform coordinates. Dataset metadata edits require the
native path API because the memory model does not retain dataset metadata.
Update accepts `inputLayer`, `outputLayer`, `key`, `mode` (`merge`, `update-only`,
`append-only`); it matches feature IDs when `key` is omitted.

## Viewshed

```javascript
const visible = terrio.viewshed(dem, 500000, 4400000, { oz: 2, tz: 0, md: 5000 });
const height = terrio.viewshed(dem, 500000, 4400000, { om: "GROUND", cc: 0 });
```

The input can be a dataset or raster bytes. `om` selects NORMAL visibility, DEM minimum
visible elevation, or GROUND minimum height above the DEM. `cc` is 1 minus refraction.
See [viewshed](cli.md#viewshed) for units, curvature, radius cropping and nodata behavior.

## Compare, pixel info and XYZ

```javascript
const report = terrio.compare(reference, output, { skipBinary: true });
console.log(report.difference_count, report.bands[0].pixels_differing, report.bands[0].max_abs_diff);
const values = terrio.pixelInfo(dem, [[-105.5, 39.5, "site A"]], { positionCrs: "EPSG:4326", resampling: "bilinear" });
const fromPoints = terrio.pixelInfo(dem, sitesGeoJSON, { format: "json" });
const text = terrio.xyz(dem, { srcwin: [0, 0, 10, 10], skipNodata: true });
```

Rasters are `Dataset`s (read by window) or file bytes. `compare` makes the binary comparison
when both inputs are bytes; GeoTIFF `TIFFTAG_*` items and RPC tags, `.aux.xml` metadata and subdatasets need files and are not compared
in WebAssembly. `pixelInfo` takes `[x, y]`/`[x, y, text]` positions, or a vector dataset, file
bytes or GeoJSON of points, and returns GeoJSON (an object), `json` (an array), or `csv`,
`text` and `value` strings. `xyz` returns the text. See [compare](cli.md#compare),
[pixel-info](cli.md#pixel-info) and [xyz](cli.md#xyz).

`pixelInfo(src, positions, {format: "value", fieldSep: "|"})` selects the
separator between band values. For vector file bytes, pass the GeoJSON result to
`fromGeoJSON` and serialize that Dataset with `toBytes`.

`xyz` returns the complete text as one JavaScript string. Use `srcwin`/`skip` for large
XYZ exports. Native Rust XYZ writers stream the output. Byte-based `compare` copies each
input once and reuses that buffer for binary comparison and lazy reads.


## Pixel-wise operations

```javascript
const physical = terrio.unscale(packed, { ot: "Float64" });
const rgba = terrio.nodataToAlpha(landsat, { nodata: 0 });
const groups = terrio.reclassify(nlcd, "11=1;[21,25)=2;DEFAULT=0", { ot: "Byte" });
const fromFile = terrio.reclassify(nlcd, "", { mappingFile: await (await fetch("map.txt")).text() });
const pixels = terrio.asFeatures(dem, { geometryType: "polygon", includeXy: true, skipNodata: true });
```

Inputs are datasets or raster bytes; `asFeatures` returns a vector dataset (`toGeoJSON`,
`toBytes`). WebAssembly cannot read local files, so a mapping file's text is passed as
`mappingFile` instead of `@file`. See [unscale](cli.md#unscale),
[nodata-to-alpha](cli.md#nodata-to-alpha), [reclassify](cli.md#reclassify) and
[as-features](cli.md#as-features).

For `nodataToAlpha`, non-finite overrides use the strings `"nan"`, `"inf"` or `"-inf"`
(as a scalar or in the nodata array). Raw JavaScript non-finite numbers and explicit null
are rejected before JSON serialization, so they cannot silently become a default override.

## Geolocation-array warping

Pass longitude and latitude rasters explicitly when array datasets cannot be reopened from
paths in WebAssembly. `x` and `y` accept a `Dataset`, file bytes, or `{ bytes, filename }`:

```javascript
const output = terrio.warp(swath, {
  tSrs: "EPSG:4326",
  geolocation: { x: longitude, y: latitude, pixelStep: 2, lineStep: 2, pixelCenter: true },
});
```

`xBand` and `yBand` select the 1-based coordinate bands (default 1).
Optional `srs` defaults to EPSG:4326, `pixelOffset`/`lineOffset` to 0, and steps to 1.
`pixelCenter` shifts by half an array step. `geoloc: true` requires geolocation metadata;
the explicit `geolocation` option supplies arrays for the first source. The operation remains
pure Rust in full and core WASM builds. See [CLI warping](cli.md#warp) for invalid coordinates,
antimeridian handling and boundary differences from GDAL.
