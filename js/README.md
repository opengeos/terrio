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
| Raster | GeoTIFF/COG (LZW, Deflate, PackBits, ZSTD, LZMA, JPEG, WebP, LERC, JPEG XL), JPEG 2000 (JP2 and J2K), Esri ASCII grid, GeoPackage | GeoTIFF, COG (lossless compressions, ZSTD included), JPEG 2000, Esri ASCII grid, GeoPackage |
| Vector | GeoJSON, TopoJSON, FlatGeobuf\*, GeoPackage, GeoParquet, CSV, KML, KMZ, GML, GPX, OSM PBF, zip archives of these, Shapefile and MapInfo (MIF/MID) in zip archives | GeoJSON, TopoJSON, FlatGeobuf, GeoPackage, KML, KMZ, GML, GPX |
| Point cloud | LAS, LAZ, COPC, PLY, E57 | LAS, LAZ, COPC, PLY, E57 |

\* FlatGeobuf files are read by terrio's own driver, with 64-bit offsets. `open` decodes a layer
whole, into about five times the file's size, within WebAssembly's 4 GB of memory: an 867 MB file
of 2 million OpenStreetMap lines opens in about 3 seconds in Node.js, and a 1 GB file of
2.3 million polygons runs out of memory. `openUrl` reads larger files by bounding box.

JPEG 2000 files decode with the codec of GDAL's JP2OpenJPEG driver (a Rust port of OpenJPEG), with
their GeoJP2 or GMLJP2 georeferencing: a Sentinel-2 band (`T10SEG_..._B04.jp2`, 10980 x 10980)
opens in about 6 seconds in Node.js. Written with a nodata value, a JPEG 2000 comes back as a zip
archive of the `.jp2` file and the `.aux.xml` sidecar that holds the value, as GDAL writes it;
`open` reads that archive back.


## Building

```sh
wasm-pack build js --release --target web --out-dir pkg --out-name terrio
wasm-pack build js --release --target nodejs --out-dir pkg-node --out-name terrio
node --test js/tests/node.test.mjs        # TERRIO_TEST_DATA=... adds real files checked against GDAL
```
