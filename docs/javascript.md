# JavaScript

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
const ipc = roads.toArrowIPC(0, { geometryEncoding: "geoarrow" }); // an Arrow IPC stream
const table = tableFromIPC(ipc);  // Apache Arrow JS; the geometry column is GeoArrow
const back = fromArrowIPC(tableToIPC(table), "roads");

const [xs, ys] = transformPoints([-122.4], [37.8], "EPSG:4326", "EPSG:32610");
```

In WebAssembly, these formats are read from and written to memory:

| | Read | Write |
|---|---|---|
| Raster | GeoTIFF/COG, Esri ASCII grid, GeoPackage, PMTiles and MBTiles (read by window, zoom levels as overviews) | GeoTIFF, COG (lossless compressions), Esri ASCII grid, GeoPackage, PMTiles, MBTiles |
| Vector | GeoJSON, TopoJSON, FlatGeobuf\*, GeoPackage, CSV, GeoParquet, KML, KMZ, GML, GPX, OSM (PBF and XML), PMTiles, MBTiles, MVT tiles, zip archives of these, and Shapefile and MapInfo (MIF and TAB) in zip archives | GeoJSON, TopoJSON, FlatGeobuf, GeoPackage, KML, KMZ, GML, GPX, PMTiles, MBTiles (`minzoom`, `maxzoom` options) |

\* The format library's FlatGeobuf reader overflows on large files in 32-bit WebAssembly; terrio's
own FlatGeobuf driver will replace it.

## Building

```sh
wasm-pack build js --release --target web --out-dir pkg --out-name terrio
wasm-pack build js --release --target nodejs --out-dir pkg-node --out-name terrio
node --test js/tests/node.test.mjs        # TERRIO_TEST_DATA=... adds real files checked against GDAL
```
