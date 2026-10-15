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
  bytes; spatial filters through the R-tree; existing files updated (features appended, layers
  replaced or added, other tables kept), checked against `ogr2ogr` and `validate_gpkg.py`.
  Still to do: updates in place (files are rewritten whole).
- Zarr 2 and 3, and GeoZarr conventions. Done: GeoZarr's `spatial:`, `proj:` and `multiscales`
  conventions read (multiscales levels as overviews, read by window) and written
  (`GEOREFERENCING_CONVENTION=SPATIAL_PROJ`, overviews laid out as GDAL lays them out), and CF grid
  mappings written as rioxarray writes them. Still to do: remote Zarr stores, read in place.
- KML/KMZ, GPX, GML, TopoJSON.
- PMTiles and MBTiles archives. Done: raster tiles (PNG, JPEG, WebP) read as rasters with their
  zoom levels as overviews, and vector tiles (MVT) as layers, as GDAL's MBTiles, PMTiles and MVT
  drivers read them; PMTiles read in place over HTTP; rasters written as tile pyramids and vector
  layers as MVT tiles (archives or directories), as GDAL writes them. Checked against GDAL
  (`scripts/compare_tiles_with_gdal.py`). Still to do: updating existing archives, AVIF tiles,
  repairing invalid polygons as GEOS does.
- STAC items and collections as datasets. Done: items, item collections, collections and STAC
  API searches (local or remote, following `next` links) as footprint layers read as GDAL reads
  STAC GeoJSON; assets opened as `STAC:"item":asset` (COGs in place); mosaics of an asset over
  items as GDAL's STACIT driver builds them (`STACIT:"search":asset=red`), read by window. Still
  to do: POST pagination, STACTA (tiled assets), signed URLs (Planetary Computer).

## 0.5: Bindings and the GDAL backend

- Python bindings (PyO3), with NumPy arrays for rasters and Arrow for vector layers. Done:
  `open`, lazy window and overview reads (remote COGs included), `Raster.from_array`, vector
  layers as GeoJSON, WKB and GeoDataFrames, Arrow through the PyCapsule interface (pyarrow,
  GeoPandas, Polars, GDAL), and every operation, with arrays checked against GDAL's Python
  bindings; type stubs checked with `mypy --strict` and `stubtest`.
- An npm package built from the WebAssembly target. Done: datasets opened from bytes (GeoTIFF,
  ASCII grid and the text and FlatGeobuf vector formats), bands as typed arrays, GeoJSON out,
  conversion to bytes, CRS transforms, zipped Shapefile and MapInfo and GeoPackage from memory,
  Arrow IPC streams.
- Datasets opened from and written to bytes (`terrio::memory`).
- An optional `gdal` feature that opens anything GDAL can when it is installed, through the same
  data model. Done: fallback in `open`, `open_raster`, `open_vector` and `list_layers`, a `GDAL:`
  prefix and `--driver gdal`, GDAL dataset names and subdatasets, rasters written with any GDAL
  driver (ERDAS Imagine included). Still to do: vector layers written through GDAL, GDAL opening
  bytes in memory.
- Arrow and GeoArrow import and export for vector layers.

## 1.0: Stable

- A stable public API, documented with examples. Done: every public item documented
  (`#![warn(missing_docs)]`, rustdoc warnings denied in CI), a tour in the crate documentation,
  examples on the entry points, the API decisions written down (the `wbraster`/`wbvector` data
  model, `#[non_exhaustive]` enums, options built with `Default`), docs.rs metadata, and the crate
  packaging verified (`cargo publish --dry-run`). Still to do: the first crates.io release.
- A conformance suite of real-world files per format, with results compared against GDAL. Done:
  `scripts/conformance.py` runs every comparison (summaries and pixels, vector and raster round
  trips, windows, operations, terrain and algebra, vector operations, point clouds, CRS accuracy)
  and writes the report published as `docs/conformance.md`; CI runs it.
- Benchmarks against GDAL for reading, writing, warping and converting.
- A minimum supported Rust version policy. Done: Rust 1.90, raised only when needed and never to
  a release less than six months old, checked in CI.

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
  between dynamic datums (coordinate epochs). Done: PROJ's GeoTIFF grids (PROJ-data), NTv2 and GTX
  read in pure Rust and interpolated as PROJ does (`hgridshift`, `vgridshift`, NADCON5's
  biquadratic `gridshift`, `xyzgridshift`, `deformation`), looked up in `TERRIO_GRID_DIR`,
  `PROJ_DATA` and PROJ's user directory, or downloaded from the PROJ CDN when network access is on
  (or registered from bytes in WebAssembly); the transformations GDAL picks when the grids are
  available, else those without grids; geoid models for compound and geographic 3D CRSs
  (`EPSG:4326+5773`); time-dependent Helmert transformations with coordinate epochs as GDAL sets
  them (`--s_coord_epoch`, `--t_coord_epoch`, `TransformOptions`, Python and JavaScript). Checked
  against GDAL with the same grids: every grid of `scripts/proj_grids.txt` to the micrometre, and
  every EPSG CRS's reference points (`scripts/compare_crs_grids_with_gdal.py`,
  `epsg_accuracy_grids`). Still to do: PROJ's chains through several intermediate datums,
  vertical-to-vertical transformations (VERTCON, offsets), moving coordinates between epochs
  (point motion operations), and reading remote grids by range requests rather than whole.
- NetCDF and HDF5 in pure Rust. Done: classic and netCDF-4 reading, with variables in groups
  below the root (`NETCDF:"file.nc":/group/var`) listed as `gdalinfo` lists them; classic
  (CDF-1/2/5) and netCDF-4 writing (`FORMAT=NC4`/`NC4C`, chunked, deflate and shuffle, CF
  georeferencing, as GDAL writes them; terrio's own HDF5 writer); HDF5 rasters as GDAL's HDF5
  driver reads them (`HDF5:"file.h5"://group/dataset`, metadata, user blocks), checked on JPSS
  and Sentinel-5P products. Still to do: HDF-EOS5 grids and swaths and other product-specific
  georeferencing, compound and complex datasets, writing groups.
