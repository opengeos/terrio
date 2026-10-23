# Why terrio?

Rust already has ways to read geospatial data: bindings to GDAL, the GeoRust crates, GeoArrow,
whitebox_next_gen's format crates, and other pure-Rust GDAL alternatives. This page explains what
terrio adds, and when one of the others is the better choice.

## The short answer

terrio is for reading, writing, converting and reprojecting the common raster and vector formats
**in pure Rust, with GDAL's results**:

- **No C library to install.** It builds anywhere Rust does, `wasm32-unknown-unknown` included,
  and ships as a single binary, a Python wheel and an npm package.
- **One `open()` for many formats.** Rasters and vectors are detected from the file and read into
  one data model, so converting between any two formats is a single call.
- **GDAL is the reference.** Every driver and operation is compared with GDAL 3.12 on real files
  in continuous integration, and differences are treated as bugs: the
  [conformance report](conformance.md) lists over 20,000 comparisons and every one that differs.
  Coordinate transformations use the same operations GDAL picks for each EPSG CRS.
- **GDAL's command line, reimplemented.** `info`, `convert`, `translate`, `warp`, `mosaic`,
  `rasterize` and `polygonize` take GDAL's options, so existing scripts translate directly.

It does not try to replace all of GDAL. If you need a format terrio doesn't have, an optional
[GDAL backend](gdal.md) reads it through an installed GDAL; if you need the fastest processing
for every operation, GDAL is still the answer.

## Compared with the alternatives

| | What it is | Formats | Pure Rust / WebAssembly | Reprojection |
|---|---|---|---|---|
| **GDAL** | The C/C++ reference library and utilities | Over 200 drivers | No; browser builds via Emscripten | PROJ, complete |
| **`gdal` crate** | Rust bindings to an installed GDAL | Whatever that GDAL has | No: needs libgdal and headers | Through GDAL |
| **GeoRust crates** | Geometry types, algorithms and one crate per format | One per crate (GeoJSON, WKT, GPX, KML, GeoTIFF…) | Mostly yes | `proj` binds C PROJ; `proj4rs` is pure Rust but PROJ-string only |
| **GeoArrow / `geoparquet`** | Columnar geometry in Arrow memory | GeoParquet, Arrow IPC, and others through GDAL or adapters | Yes | Not its job |
| **whitebox_next_gen** (`wbraster`, `wbvector`, `wbprojection`) | Whitebox's pure-Rust format and projection crates | Many rasters and vectors | Yes | `wbprojection` |
| **OxiGeo** (formerly OxiGDAL) | Another pure-Rust GDAL alternative | About 16 drivers by its own count, including HDF5 and GRIB | Yes | Its own |
| **terrio** | Pure-Rust I/O with GDAL as the reference | The common raster, vector and tile formats, point clouds and STAC ([Formats](formats.md)); the rest through optional GDAL | Yes | GDAL's operations for 6,354 EPSG CRSs |

### GDAL and the `gdal` crate

GDAL reads more formats than anything else, is fast and mature, and is what most of the
geospatial world is built on. The [`gdal`](https://crates.io/crates/gdal) crate gives Rust safe
access to it, and is the right choice when GDAL can be installed where your code runs.

The cost is the C and C++ dependency tree: GDAL, PROJ, GEOS, SQLite, libtiff, curl and more, matched
to the version the bindings were generated for. That makes cross-compiling, static binaries,
small containers and serverless functions harder, and rules out the plain `wasm32-unknown-unknown`
target. terrio trades GDAL's breadth for a build that is `cargo build` everywhere. Where GDAL is
available, the optional `gdal` feature uses it for the formats terrio doesn't read, behind the
same `open()`.

### The GeoRust crates

[GeoRust](https://georust.org) provides the shared geometry types (`geo-types`), the algorithms
(`geo`), format crates such as `geojson`, `wkt`, `wkb`, `gpx`, `kml`, `geotiff` and `geozero`,
and `proj` and `proj4rs` for coordinates; the wider ecosystem adds crates such as `shapefile`
and `flatgeobuf`. They are excellent building blocks.

What they don't provide is the layer above: one function that opens any of them, detects the
format, reads attributes into a common schema, handles rasters, reads from `s3://` or a zip
archive, and writes to any other format with GDAL's layer and field conventions. Each crate has
its own API and data model, and raster support is limited to TIFF decoding. Reprojection is
either C PROJ (`proj`) or PROJ strings without the EPSG transformation choices (`proj4rs`).
terrio is that layer. For geometry algorithms, use `geo` on top of it.

### GeoArrow and GeoParquet

[GeoArrow](https://geoarrow.org) and the [`geoarrow-rs`](https://github.com/geoarrow/geoarrow-rs)
crates hold geometries in columnar Arrow memory, which is the right model for analytics, DuckDB,
Polars and fast GeoParquet. terrio reads and writes GeoParquet too, with the same `parquet` and
Arrow crates, but holds features row by row in one model shared with every other format. Layers convert
to and from Arrow record batches and GeoArrow geometry columns, so the two hand data to each
other without copying through files.

### whitebox_next_gen

terrio builds on [whitebox_next_gen](https://github.com/jblindsay/whitebox_next_gen)'s
`wbraster`, `wbvector` and `wbprojection` crates for its data model, many codecs and its projection
engine. Testing them against GDAL found codecs that decoded wrong pixels or lost georeferencing,
so terrio has its own drivers for GeoTIFF, JPEG 2000, Zarr, netCDF, GeoParquet, FlatGeobuf, KML,
OSM and others, its own EPSG table and PROJ's projection formulas, and reports the problems
upstream (see [Architecture](architecture.md)). On top it adds format detection, remote and
archived inputs, lazy COG reads, GDAL-style operations and the command line.

### Other pure-Rust GDAL alternatives

[OxiGeo](https://github.com/cool-japan/oxigeo) (formerly OxiGDAL) shares the goal of a pure-Rust
GDAL alternative and lists formats terrio doesn't read. We haven't compared the two. terrio's emphasis is narrower: a smaller set of formats, each checked against
GDAL file by file in CI, and a command line that mirrors GDAL's. If a format you need is in
another library and not in terrio, use it.

## When to use something else

- **You need many formats terrio doesn't have** (File Geodatabase, MrSID, HDF4, databases):
  use GDAL, directly, through the `gdal` crate, or through terrio's `gdal` feature.
- **You need the fastest conversion or warping today:** GDAL. terrio is faster for some tasks and
  slower for others; see [Benchmarks](benchmarks.md).
- **You need datum grids or vertical datums:** PROJ, through GDAL or the `proj` crate. terrio does
  not apply NTv2 or other shift grids yet.
- **You need geometry algorithms** (buffers, intersections, simplification): `geo`, with terrio
  reading and writing the data.
- **You work in columnar data:** GeoArrow, GeoParquet and DuckDB.

## When terrio fits

- A Rust program or service that must build without system libraries, cross-compile, or ship as
  one static binary.
- Geospatial I/O in the browser or another WebAssembly runtime, through the
  [npm package](javascript.md) or the Rust library.
- Reading cloud-optimized GeoTIFFs, PMTiles and COPC point clouds piece by piece from HTTP,
  `s3://`, `gs://` or `az://`, and STAC items and searches, without GDAL.
- Replacing `gdal_translate`, `gdalwarp` or `ogr2ogr` calls in scripts and CI where installing
  GDAL is the hard part (see [Coming from GDAL](gdal.md)).
- A Python environment that needs common formats as NumPy arrays without GDAL's wheels
  ([Python](python.md)).
