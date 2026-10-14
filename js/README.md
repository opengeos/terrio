# terrio (WebAssembly)

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark-dark.svg">
    <img src="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark.svg" alt="terrio" height="72">
  </picture>
</p>

[terrio](https://github.com/opengeos/terrio), pure-Rust geospatial data I/O, compiled to
WebAssembly for browsers and Node.js. Datasets are opened from bytes, with a file name telling the
format, and written back to bytes.

```js
import init, { open, fromGeoJSON, rasterFromArray, transformPoints, describeCrs } from "terrio";

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

// Point clouds: LAS, LAZ and COPC.
const lidar = openPoints(lazBytes, "tile.laz");
lidar.format, lidar.pointCount, lidar.crs, lidar.info();
const ground = lidar.read({ classes: [2], bounds: [x0, y0, x1, y1] }); // COPC: also maxLevel, resolution
ground.get("z");                  // Float64Array; intensity is a Uint16Array...
ground.toBytes("ground.copc.laz"); // LAS, LAZ or COPC
ground.grid(1, { stat: ["idw"] }); // a raster Dataset
ground.toVectorBytes("ground.fgb", ["z", "intensity"]);
pointsFromArrays({ x, y, z, classification }, "EPSG:32610");
```

In WebAssembly, these formats are read from and written to memory:

| | Read | Write |
|---|---|---|
| Raster | GeoTIFF/COG, Esri ASCII grid, GeoPackage | GeoTIFF, COG (lossless compressions), Esri ASCII grid, GeoPackage |
| Vector | GeoJSON, TopoJSON, FlatGeobuf\*, GeoPackage, GeoParquet, CSV, KML, KMZ, GML, GPX, OSM PBF, zip archives of these, Shapefile and MapInfo (MIF/MID) in zip archives | GeoJSON, TopoJSON, FlatGeobuf, GeoPackage, KML, KMZ, GML, GPX |
| Point cloud | LAS, LAZ, COPC | LAS, LAZ, COPC |

\* The format library's FlatGeobuf reader overflows on large files in 32-bit WebAssembly; terrio's
own FlatGeobuf driver will replace it.


## Building

```sh
wasm-pack build js --release --target web --out-dir pkg --out-name terrio
wasm-pack build js --release --target nodejs --out-dir pkg-node --out-name terrio
node --test js/tests/node.test.mjs        # TERRIO_TEST_DATA=... adds real files checked against GDAL
```
