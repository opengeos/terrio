# Roadmap

terrio reads, writes, converts and reprojects the geospatial formats most workflows use (rasters,
vector data and point clouds), in pure Rust, so that it builds anywhere Rust does, WebAssembly included. It does not try to replace all
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
- Coordinate reference systems: EPSG codes, WKT and PROJ strings in and out; transformations that
  match GDAL's (units, axis orders, prime meridians and datum transformations) for every EPSG
  geographic and projected CRS. Done.
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
- Terrain analysis and raster algebra, matching GDAL pixel for pixel: `terrio dem` (`gdaldem`:
  hillshade, slope, aspect, color relief, TRI, TPI, roughness), `terrio calc` (`gdal_calc.py`,
  with NumPy's typing rules), `terrio proximity` (`gdal_proximity.py`) and `terrio sieve`
  (`gdal_sieve.py`).

## 0.4: More formats

- GeoPackage, vector and raster tiles. Done: terrio reads and writes the SQLite files itself
  (feature and attribute tables, R-tree spatial indexes, tile pyramids with PNG or JPEG tiles,
  gridded coverages for Int16, UInt16 and Float32 rasters, files holding both), from files or
  bytes. Still to do: spatial filters through the R-tree, updating existing files.
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
  conversion to bytes, CRS transforms, zipped Shapefile and MapInfo and GeoPackage from memory.
- Datasets opened from and written to bytes (`terrio::memory`).
- An optional `gdal` feature that opens anything GDAL can when it is installed, through the same
  data model.
- Arrow and GeoArrow import and export for vector layers.

## 1.0: Stable

- A stable public API, documented with examples.
- A conformance suite of real-world files per format, with results compared against GDAL.
- Benchmarks against GDAL for reading, writing, warping and converting.
- A minimum supported Rust version policy.

## Point clouds

Done: LAS 1.0 to 1.4 (point formats 0 to 10, extra bytes, VLRs and EVLRs, WKT and GeoKey CRSs),
LAZ (LASzip, including its first unchunked compressor) and COPC, read and written; COPC files,
local or remote, read in place by bounds, octree level, point spacing or point budget, with HTTP
range requests; filters by bounds, classes and returns; reprojection; gridding to rasters as PDAL's
`writers.gdal` does (min, max, mean, IDW, count, standard deviation, window filling); conversion to
and from vector point layers; `terrio info`, `terrio convert` and `terrio grid`; Python (NumPy) and
JavaScript (typed arrays) bindings. Every value is compared with laspy, and headers, COPC queries,
grids and reprojections with PDAL (`scripts/compare_pointclouds.py`).

Still to do: EPT and other point cloud formats (PLY, E57), streaming reads and writes of files
larger than memory, waveform data packets, more PDAL filters (ground classification, outliers,
thinning by voxel), and gridding with TIN or nearest-neighbour interpolation.

## Later

- CRS: datum shift grids (NTv2, GeoTIFF grids), vertical datums, time-dependent transformations
  between dynamic datums (coordinate epochs).
- NetCDF and HDF5 in pure Rust: classic and netCDF-4 reading and classic (CDF-1/2/5) writing are
  done; netCDF-4 writing and groups below the root remain.
- JPEG 2000 and other compressed raster formats as pure-Rust decoders mature.
- Band histograms as `gdalinfo -hist` computes them; color relief written as a VRT.

## Decisions

- The core API is synchronous and reads datasets into memory; remote files are downloaded first.
  GeoTIFFs, local or remote, can instead be opened lazily (`LazyRaster`) and read by window and
  overview level, decoding only the tiles needed and fetching them with HTTP range requests.
- HTTP goes through `ureq` (with rustls) and S3 requests are signed by terrio itself, rather than
  through `object_store`, to keep the API synchronous and the dependencies small.
- GeoPackage is read and written by terrio's own SQLite file reader and writer (pure Rust): no
  C dependency, and files written in one go.
- terrio carries its own EPSG table (`crates/terrio/data/epsg.tsv.zst`, about 220 KB, generated
  from GDAL and PROJ by `scripts/gen_epsg_table.py`): the PROJ strings of the CRSs, their WKT and
  the datum transformations GDAL lists for them, so that terrio picks the same transformation as
  GDAL for each point. The projection formulas are PROJ's, ported to Rust; `wbprojection` remains
  the fallback for projections EPSG CRSs do not use and for WKT 2 parsing.
- Point clouds use the `laz` crate for LASzip, and terrio's own LAS and COPC code rather than the
  `las` crate (whose header reader keeps only the first EVLR) or `wblidar` (which reads COPC points
  in hierarchy order and cannot read LASzip's unchunked compressor). Point clouds are not
  `Dataset`s: they have their own reader (`pointcloud::open_points`) and columnar `PointCloud`.
