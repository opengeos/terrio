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
| Raster | GeoTIFF/COG (LZW, Deflate, PackBits, ZSTD, LZMA, JPEG, WebP, LERC, JPEG XL), JPEG 2000 (JP2 and J2K), Esri ASCII grid, GeoPackage, PMTiles and MBTiles (read by window, zoom levels as overviews) | GeoTIFF, COG (lossless compressions, ZSTD included), JPEG 2000, Esri ASCII grid, GeoPackage, PMTiles, MBTiles |
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

## Building

```sh
wasm-pack build js --release --target web --out-dir pkg --out-name terrio
wasm-pack build js --release --target nodejs --out-dir pkg-node --out-name terrio
node --test js/tests/node.test.mjs        # TERRIO_TEST_DATA=... adds real files checked against GDAL
```
