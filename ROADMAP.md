# Roadmap

terrio reads, writes, converts and reprojects the geospatial formats most workflows use, in pure
Rust, so that it builds anywhere Rust does, WebAssembly included. It does not try to replace all
of GDAL: the long tail of formats stays reachable through an optional GDAL backend.

## Principles

- Pure Rust by default. Every default feature builds for `wasm32-unknown-unknown`; anything that
  needs C sits behind an opt-in feature.
- Reuse the ecosystem. The format codecs and projections come from whitebox_next_gen's pure-Rust
  crates (`wbraster`, `wbvector`, `wbprojection`), plus `tiff` for GeoTIFF decoding. Where a
  codec is wrong or missing, terrio supplies its own driver and reports the problem upstream.
- One data model across formats, so a driver is a small amount of code and conversions are free.
- Read cloud data in place: range requests, lazy tiles and chunks, no full download.
- Correctness is measured against GDAL. Every driver is tested on real-world files and its output
  compared with GDAL's.

## Non-goals

- Matching GDAL's C API or its option names one to one.
- Proprietary formats that need vendor SDKs (MrSID, ECW, FileGDB raster).
- Database drivers (PostGIS, Oracle, SQL Server) in the core crate.

## 0.1: Foundations

- Data model: `Dataset`, raster `Band` with block access, vector `Layer` with `Feature`, field
  schemas and geometry (on `geo-types`), and a driver registry with format detection.
- Virtual file system: local files, HTTP(S) with range requests, S3 (with SigV4 signing), Google
  Cloud Storage and Azure; zip archives.
- Coordinate reference systems: EPSG codes, WKT and PROJ strings in and out; transformations for
  the common projections through `proj4rs`.
- GeoJSON and GeoTIFF read and write, so the model is proven on one vector and one raster format.
- `terrio info` for any supported dataset.

## 0.2: Core formats

- Raster: Cloud Optimized GeoTIFF (overviews, internal tiling, all common compressions), ASCII
  grids.
- Vector: Shapefile (with `.prj`, `.cpg` and zipped), FlatGeobuf (with its spatial index),
  GeoParquet 1.1 (with bounding-box filtering), CSV with point or WKT columns.
- Attribute and spatial filters pushed down to the drivers that support them.
- `terrio convert` between any two supported formats.

## 0.3: Processing

- Warp: reprojection and resampling (nearest, bilinear, cubic, average, mode), with output
  extent and resolution control.
- Translate: subset by window or bounds, change data type, scale, add overviews, write COGs.
- Rasterize vectors and polygonize rasters.
- Mosaics: a virtual dataset (an equivalent of GDAL's VRT) over many rasters.
- `terrio warp`, `terrio translate`, `terrio rasterize`, `terrio polygonize`.

## 0.4: More formats

- GeoPackage, vector and raster tiles. This needs a SQLite reader; deciding between a pure-Rust
  reader and an opt-in `rusqlite` feature is the main open question for this release.
- Zarr 2 and 3, and GeoZarr conventions.
- KML/KMZ, GPX, GML, TopoJSON.
- PMTiles and MBTiles archives.
- STAC items and collections as datasets.

## 0.5: Bindings and the GDAL backend

- Python bindings (PyO3), with NumPy arrays for rasters and Arrow for vector layers. Done:
  `open`, lazy window and overview reads (remote COGs included), `Raster.from_array`, vector
  layers as GeoJSON, WKB and GeoDataFrames, and every operation, with arrays checked against
  GDAL's Python bindings. Still to do: Arrow export of vector layers, type stubs.
- An npm package built from the WebAssembly target. Done: datasets opened from bytes (GeoTIFF,
  ASCII grid and the text and FlatGeobuf vector formats), bands as typed arrays, GeoJSON out,
  conversion to bytes, CRS transforms. Still to do: Shapefile, GeoParquet and GeoPackage from
  memory, which need byte-based readers in those drivers.
- Datasets opened from and written to bytes (`terrio::memory`).
- An optional `gdal` feature that opens anything GDAL can when it is installed, through the same
  data model.
- Arrow and GeoArrow import and export for vector layers.

## 1.0: Stable

- A stable public API, documented with examples.
- A conformance suite of real-world files per format, with results compared against GDAL.
- Benchmarks against GDAL for reading, writing, warping and converting.
- A minimum supported Rust version policy.

## Later

- CRS: datum shift grids (NTv2, GeoTIFF grids), vertical datums, a bundled EPSG database.
- NetCDF and HDF5 classic formats in pure Rust, starting with reading.
- JPEG 2000 and other compressed raster formats as pure-Rust decoders mature.
- Point clouds (LAS, LAZ, COPC).
- Raster algebra and terrain tools (hillshade, slope, aspect).

## Decisions

- The core API is synchronous and reads datasets into memory; remote files are downloaded first.
  GeoTIFFs, local or remote, can instead be opened lazily (`LazyRaster`) and read by window and
  overview level, decoding only the tiles needed and fetching them with HTTP range requests.
- HTTP goes through `ureq` (with rustls) and S3 requests are signed by terrio itself, rather than
  through `object_store`, to keep the API synchronous and the dependencies small.
- GeoPackage uses `wbvector`'s pure-Rust SQLite engine, so no C dependency is needed.
- The EPSG registry comes from `wbprojection`.
