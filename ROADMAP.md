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

## Completed: Practical GDAL feature parity

All nine items in [#238](https://github.com/opengeos/terrio/issues/238) are complete
as of 2026-10-24. The [gap analysis](docs/gdal-parity-recommendations.md) records the
supported scope and validation criteria. These additions build on the operations
and drivers below.

- [x] Fill nodata: inverse-distance and nearest interpolation, validity masks, maximum
      pixel distance and smoothing, preserving valid source pixels. Done in
      [#239](https://github.com/opengeos/terrio/pull/239), through Rust, CLI/MCP, Python and JavaScript.
- [x] Overview-aware warping: explicit source overview selection, automatic selection for
      downsampling and a full-resolution override, reducing remote COG reads. Implemented in
      [#242](https://github.com/opengeos/terrio/pull/242), through Rust, CLI/MCP, Python and JavaScript.
- [x] Geolocation-array warping: reproject swaths using the longitude/latitude arrays already
      exposed by the scientific-format drivers, including subsampled arrays and pixel-center
      conventions. Done in [#241](https://github.com/opengeos/terrio/pull/241);
      subsampled/invalid-node swaths retain documented GDAL boundary-mask differences.
- [x] Richer VRT interoperability: serialized warped VRTs, then selected built-in derived bands
      and kernel-filtered sources, with lazy window reads and GDAL-compatible output.
      Rust, CLI/MCP, Python and WebAssembly; supported subsets are documented in the CLI guide.
- [x] Richer vector SQL: computed SELECT columns, DISTINCT, summary aggregates and limited
      attribute joins, with the supported OGR SQL subset documented explicitly. Done in
      [#245](https://github.com/opengeos/terrio/pull/245), with real-data OGR SQL comparisons.
- [x] Viewshed: single-observer visibility and minimum visible height, observer/target heights,
      maximum distance and curvature/refraction controls, with explicit input nodata behavior.
      Done in [#240](https://github.com/opengeos/terrio/pull/240), through Rust, CLI/MCP, Python
      and JavaScript; projected DEM comparisons are registered in conformance.
- [x] Indexed raster catalogs: GTI-compatible GeoPackage/FlatGeobuf tile indexes, spatial source
      selection, compositing order, mixed-CRS sources and a bounded source cache. Done in
      [#249](https://github.com/opengeos/terrio/pull/249), with nested VRT export protection
      and conformance reporting completed in [#255](https://github.com/opengeos/terrio/pull/255).
- [x] Pansharpening: weighted Brovey processing, spectral weights, resampling, nodata, bit depth
      and extent handling. Materialized output shipped in
      [#247](https://github.com/opengeos/terrio/pull/247), with lazy GDAL-compatible VRT output
      and window reads in [#250](https://github.com/opengeos/terrio/pull/250).
- [x] Native multidimensional API: groups, named dimensions, coordinate arrays, lazy time/depth
      slicing and transposition across netCDF/HDF5/Zarr, retaining the existing raster views.
      Done in [#252](https://github.com/opengeos/terrio/pull/252); the supported numeric/raw-CF
      subset is documented in the [multidimensional guide](docs/multidim.md).

The completed scope preserves pure-Rust/default WebAssembly builds, registers real-data
comparisons in the GDAL conformance workflow, and exposes applicable functionality through
Rust, CLI, Python, JavaScript and MCP. Supported subsets and numerical tolerances remain
documented; additional feature scope belongs in separate tracking issues.

## 0.1: Foundations

- [x] Data model: `Dataset`, raster `Band` with block access, vector `Layer` with `Feature`, field
      schemas and geometry (on `geo-types`), and a driver registry with format detection.
- [x] Virtual file system: local files, HTTP(S) with range requests, S3 (with SigV4 signing), Google
      Cloud Storage and Azure; zip archives.
- [x] Coordinate reference systems: EPSG codes, WKT and PROJ strings in and out; transformations that
      match GDAL's (units, axis orders, prime meridians and datum transformations) for every EPSG
      geographic and projected CRS. Done, and since: ESRI codes under the ESRI authority, WKT 1 and
      WKT 2 read by terrio's own reader and identified by their datum, rotated-pole and
      geostationary grids, PROJ's world projections, datum transformations between CRSs chosen as
      PROJ lists them, and every PROJ projection checked against PROJ as a PROJ string and as WKT.
- [x] GeoJSON and GeoTIFF read and write, so the model is proven on one vector and one raster format.
- [x] `terrio info` for any supported dataset.

## 0.2: Core formats

- [x] Raster: Cloud Optimized GeoTIFF (overviews, internal tiling, all common compressions), ASCII
      grids.
- [x] Vector: Shapefile (with `.prj`, `.cpg` and zipped), FlatGeobuf (with its spatial index),
      GeoParquet 1.1 (with bounding-box filtering), CSV with point or WKT columns.
- [x] Attribute and spatial filters pushed down to the drivers that support them.
- [x] `terrio convert` between any two supported formats.

## 0.3: Processing

- [x] Warp: reprojection and resampling (nearest, bilinear, cubic, average, mode), with output
      extent and resolution control.
- [x] Translate: subset by window or bounds, change data type, scale, add overviews, write COGs.
- [x] Rasterize vectors and polygonize rasters.
- [x] Mosaics: a virtual dataset (an equivalent of GDAL's VRT) over many rasters.
- [x] `terrio warp`, `terrio translate`, `terrio rasterize`, `terrio polygonize`.
- [x] Terrain analysis and raster algebra, matching GDAL pixel for pixel: `terrio dem` (`gdaldem`:
      hillshade, slope, aspect, color relief, TRI, TPI, roughness), `terrio calc` (`gdal_calc.py`,
      with NumPy's typing rules), `terrio proximity` (`gdal_proximity.py`) and `terrio sieve`
      (`gdal_sieve.py`).
- [x] Focal statistics and kernels (`neighbors`), collar cleaning (`clean-collar` / `nearblack`)
      and LUT contrast enhancement (`enhance`) across Rust, CLI/MCP, Python and WebAssembly
      ([#300](https://github.com/opengeos/terrio/pull/300)).
- [x] Geometry type conversion, simplify/segmentize, concave and convex hulls, grouped combine,
      spatial sorting and coordinate swapping across the public APIs
      ([#306](https://github.com/opengeos/terrio/pull/306)).
- [x] Centroids, multipart splitting, vertex extraction, exact valid-data raster footprints and
      point sampling across multiple rasters, through the Rust API, CLI and MCP.
- [x] Polygonization preserves exterior rings and holes on south-up and rotated grids, and
      classifies rings correctly for small pixels at large coordinate origins.
- [x] Contour lines with interval/fixed levels, nodata handling and optional elevations in Z;
      multi-raster zonal statistics with GDAL's centre/all-touched pixel rules and per-raster CRS
      transformations; a standalone read-only SQL command using the existing query engine.
- [x] Planar buffers, polygon overlay/dissolve, geometry validity/repair and explicit cleanup
      tolerances through the Rust API, CLI and MCP, completing the commands from
      [#211](https://github.com/opengeos/terrio/issues/211);
      tracked in [#227](https://github.com/opengeos/terrio/issues/227).

## 0.4: More formats

- [x] GeoPackage, vector and raster tiles. Done: terrio reads and writes the SQLite files itself
      (feature and attribute tables, R-tree spatial indexes, tile pyramids with PNG or JPEG tiles,
      gridded coverages for Int16, UInt16 and Float32 rasters, files holding both), from files or
      bytes; spatial filters through the R-tree; existing files updated (features appended, layers
      replaced or added, other tables kept), checked against `ogr2ogr` and `validate_gpkg.py`;
      files updated in place as SQLite updates them (b-tree page inserts and splits, R*-tree inserts
      and node splits, freed pages on the freelist and reused, only the changed pages written, in a
      transaction protected by a rollback journal SQLite plays back after a crash, under SQLite's
      locks), checked with `PRAGMA integrity_check`, `rtreecheck()`, `validate_gpkg.py` and terrio
      killed mid-commit; appending 100,000 points to a 2-million-point file takes 2.7 s
      (`ogr2ogr -append`: 6.7 s).
- [x] Zarr 2 and 3, and GeoZarr conventions. Done: GeoZarr's `spatial:`, `proj:` and `multiscales`
      conventions read (multiscales levels as overviews, read by window) and written
      (`GEOREFERENCING_CONVENTION=SPATIAL_PROJ`, overviews laid out as GDAL lays them out), and CF grid
      mappings written as rioxarray writes them; remote stores (HTTP(S), S3, Google Cloud Storage,
      Azure, and GDAL's `/vsicurl/`-style names) read in place, Zarr 2 and 3 with consolidated
      metadata, the v3 sharding codec (shard indexes, then only the inner chunks needed), float16 and
      the `scale_offset`/`cast_value` codecs, windows fetching only the chunks they touch in parallel,
      GeoZarr multiscales as overviews; identical to GDAL's Zarr driver and xarray on EOPF Sentinel-2
      (v2 and sharded GeoZarr v3), NOAA HRRR on S3, CMIP6 on GCS and Daymet on Azure
      (`scripts/compare_remote_zarr.py`); remote groups without consolidated metadata listed
      through the object store's listing (S3 and S3-compatible, Google Cloud Storage, Azure), and
      a group's arrays listed as GDAL's subdatasets, identical to `gdalinfo` on an OME-Zarr image
      of EBI's S3 and on CMIP6 on GCS; arrays of more than three dimensions read with their
      other dimensions as bands, as GDAL reads them, identical to GDAL on CMIP6's 4D air
      temperature on GCS (37,620 bands, `DIM_*` metadata included).
- [x] KML/KMZ, GPX, GML, TopoJSON.
- [x] PMTiles and MBTiles archives. Done: raster tiles (PNG, JPEG, WebP) read as rasters with their
      zoom levels as overviews, and vector tiles (MVT) as layers, as GDAL's MBTiles, PMTiles and MVT
      drivers read them; PMTiles read in place over HTTP; rasters written as tile pyramids and vector
      layers as MVT tiles (archives or directories), as GDAL writes them; invalid polygons repaired
      as GDAL's MVT writer repairs them with GEOS; AVIF tiles; existing archives updated (rasters
      and vector layers appended). Checked against GDAL (`scripts/compare_tiles_with_gdal.py`).
- [x] STAC items and collections as datasets. Done: items, item collections, collections and STAC
      API searches (local or remote, following `next` links) as footprint layers read as GDAL reads
      STAC GeoJSON; assets opened as `STAC:"item":asset` (COGs in place); mosaics of an asset over
      items as GDAL's STACIT driver builds them (`STACIT:"search":asset=red`), read by window;
      `POST` searches and pages (`method=POST`, `next` links with `body`, `merge` and `headers`),
      and Planetary Computer's assets signed with its SAS tokens, mosaics identical to GDAL's
      STACIT on the items pystac-client and `planetary_computer` return.
    - [x] STACTA (tiled assets). Done: STAC items of the tiled-assets extension read as one raster
      as GDAL's STACTA driver reads them (`item.json`, `STACTA:"item.json":asset:tms`, local or
      remote): the most detailed tile matrix over its limits, the others as overviews (cut to its
      extent as GDAL cuts them), windows reading only the tiles they touch, bands from
      `raster:bands` or a first tile, properties as metadata, GDAL's subdatasets, missing tiles
      as nodata with `GDAL_STACTA_SKIP_MISSING_METATILE`; summaries, whole rasters, windows and
      overviews identical to GDAL's on its STACTA test data and on `gdal raster tile` pyramids,
      local and over HTTP (`scripts/compare_stac_geozarr.py stacta`).
- [x] GRIB 1 and 2, read only. Done: a pure-Rust driver read as GDAL's GRIB driver reads files (a
      Float64 band per message or field, GDAL's `GRIB_*` band metadata and descriptions from its
      tables and degrib's naming rules, its grids, earth shapes and 0-360 rewrapping, units
      normalized, nodata where bitmaps mask points), GRIB2 packings decoded as g2clib decodes them
      (simple, complex, spatial differencing, IEEE, JPEG 2000 with the OpenJPEG port, PNG, and CCSDS
      with gribberish's pure-Rust decoder), GRIB1 simple packing; local files, bytes, and remote
      files read message by message (NOAA `.idx` and ECMWF `.index` sidecars for the offsets, the
      headers fetched in parallel); in WebAssembly too. Identical to GDAL, metadata and pixels, on
      GDAL's GRIB test files, ecCodes', every field of a GFS file, HRRR's and ECMWF's open data
      (`scripts/compare_grib_with_gdal.py`, the conformance suite's `grib`).

## 0.5: Bindings and the GDAL backend

- [x] Python bindings (PyO3), with NumPy arrays for rasters and Arrow for vector layers. Done:
      `open`, lazy window and overview reads (remote COGs included), `Raster.from_array`, vector
      layers as GeoJSON, WKB and GeoDataFrames, Arrow through the PyCapsule interface (pyarrow,
      GeoPandas, Polars, GDAL), and every operation, with arrays checked against GDAL's Python
      bindings; type stubs checked with `mypy --strict` and `stubtest`.
- [x] A rasterio-compatible Python API, `terrio.rio` (`import terrio.rio as rasterio`). Done:
      datasets read lazily by window (remote COGs included) and written by window, with rasterio's
      profiles, tags, band metadata, masks, overviews and resampled reads; `crs`, `transform`,
      `windows`, `features`, `warp`, `merge`, `mask`, `vrt.WarpedVRT`, `io.MemoryFile`, `env`,
      `plot`; every function compared with rasterio on synthetic and real files, and rasterio's
      documentation examples run unchanged but for the import.
- [x] An npm package built from the WebAssembly target. Done: datasets opened from bytes (GeoTIFF,
      ASCII grid and the text and FlatGeobuf vector formats), bands as typed arrays, GeoJSON out,
      conversion to bytes, CRS transforms, zipped Shapefile and MapInfo and GeoPackage from memory,
      Arrow IPC streams.
- [x] Datasets opened from and written to bytes (`terrio::memory`).
- [x] An optional `gdal` feature that opens anything GDAL can when it is installed, through the same
      data model. Done: fallback in `open`, `open_raster`, `open_vector` and `list_layers`, a `GDAL:`
      prefix and `--driver gdal`, GDAL dataset names and subdatasets, rasters and vector layers
      written with any GDAL driver (ERDAS Imagine, OpenFileGDB, DXF...), and bytes terrio can't read
      opened through GDAL's `/vsimem/`.
- [x] Arrow and GeoArrow import and export for vector layers.
- [x] The command line without a Rust toolchain: `pip install terrio` installs the `terrio`
      command, standalone binaries for Linux, macOS and Windows are attached to each release
      (`cargo binstall terrio-cli`), and one-line installers download them
      (`curl -LsSf https://terrio.opengeos.org/install.sh | sh`, `install.ps1` on Windows).
- [x] An MCP server (`terrio mcp`): AI agents run the commands as tools, each described by its
      `--help`.

## 1.0: Stable

- [x] A stable public API, documented with examples. Done: every public item documented
      (`#![warn(missing_docs)]`, rustdoc warnings denied in CI), a tour in the crate documentation,
      examples on the entry points, the API decisions written down (the `wbraster`/`wbvector` data
      model, `#[non_exhaustive]` enums, options built with `Default`), docs.rs metadata, and the crate
      packaging verified (`cargo publish --dry-run`); released: terrio 0.1.0 and 0.1.1 on
      crates.io and npm (`terrio-js`), 0.1.1 on PyPI.
- [x] A conformance suite of real-world files per format, with results compared against GDAL. Done:
      `scripts/conformance.py` runs every comparison (summaries and pixels, vector and raster round
      trips, windows, operations, terrain and algebra, vector operations, point clouds, CRS accuracy)
      and writes the report published as `docs/conformance.md`; CI runs it.
- [x] Benchmarks against GDAL for reading, writing, warping and converting.
- [x] A minimum supported Rust version policy. Done: Rust 1.90, raised only when needed and never to
      a release less than six months old, checked in CI.

## Point clouds

- [x] LAS 1.0 to 1.4 (point formats 0 to 10, extra bytes, VLRs and EVLRs, WKT and GeoKey CRSs),
      LAZ (LASzip, including its first unchunked compressor) and COPC, read and written; COPC files,
      local or remote, read in place by bounds, octree level, point spacing or point budget, with HTTP
      range requests; filters by bounds, classes and returns; reprojection; gridding to rasters as PDAL's
      `writers.gdal` does (min, max, mean, IDW, count, standard deviation, window filling); conversion to
      and from vector point layers; `terrio info`, `terrio convert` and `terrio grid`; Python (NumPy) and
      JavaScript (typed arrays) bindings. Every value is compared with laspy, and headers, COPC queries,
      grids and reprojections with PDAL (`scripts/compare_pointclouds.py`).
- [x] EPT read (LASzip, binary and Zstandard tiles; local, HTTP and S3, in place, by bounds,
      resolution and depth, as PDAL's `readers.ept`, checked on a USGS 3DEP EPT of 11.5 billion
      points); PLY (ASCII and binary) and E57 read and written, with PDAL's dimension names and values;
      reads and writes of LAS, LAZ, COPC and PLY in chunks for files larger than memory (`terrio
      convert`, `info --stats`, `grid`, `filter range`; COPC octrees built out of core), verified on a
      3 GB file of 101.7 million points in at most 251 MB; waveform data packets (point formats 4, 5, 9
      and 10, in the file or a `.wdp` file) read and kept on write; PDAL's `filters.smrf`,
      `filters.outlier`, `filters.range`, `filters.sample`, `filters.voxeldownsize` and
      `filters.voxelcenternearestneighbor`, identical to PDAL's on USGS 3DEP lidar (`terrio filter`);
      gridding by TIN (linear) and nearest-neighbour interpolation as `gdal_grid` does
      (`terrio grid --method`).
- [x] EPT writing (LASzip, binary and Zstandard tiles, the COPC octree built out of core, read by
      PDAL's `readers.ept` as it reads the source, checked on 4.07 million points of USGS 3DEP lidar);
      E57 written in chunks (packet by packet, 59 MB for 4.07 million points); PDAL's `filters.pmf`,
      `filters.csf`, `filters.elm`, `filters.hag_nn`, `filters.hag_delaunay`, `filters.crop` (boxes,
      polygons, circles and spheres), `filters.decimation`, `filters.expression` and `filters.assign`
      (PDAL's expression language), `filters.merge`, `filters.splitter`, `filters.chipper`,
      `filters.normal` and `filters.cluster`, identical to PDAL's on autzen and USGS 3DEP lidar
      (`terrio filter`, Python and JavaScript); `filters.reprojection` is `terrio convert --t-srs`.
- [x] PDAL's `filters.dbscan`, `filters.covariancefeatures`, `filters.eigenvalues`,
      `filters.approximatecoplanar`, `filters.estimaterank`, `filters.lof`, `filters.nndistance`,
      `filters.radialdensity`, `filters.reciprocity`, `filters.miniball`, `filters.zsmooth`,
      `filters.skewnessbalancing`, `filters.head`, `filters.tail`, `filters.sort`, `filters.locate`,
      `filters.iqr`, `filters.mad`, `filters.dem`, `filters.colorization` and `filters.overlay`,
      identical to PDAL's on autzen (nanoflann's neighbours, libstdc++'s sort and Eigen's
      eigensolver as PDAL runs them; `terrio filter`, Python and JavaScript).
- [ ] PDAL's long tail of filters (`filters.optimalneighborhood`, `filters.neighborclassifier`,
      `filters.radiusassign`, `filters.hag_dem`, `filters.voxelcentroidnearestneighbor`,
      `filters.farthestpointsampling`, `filters.transformation`, `filters.ferry`, `filters.returns`,
      `filters.groupby`, `filters.planefit`, `filters.litree`, `filters.delaunay`, `filters.h3` and
      others).
- [x] EPT datasets updated in place: points appended (`terrio convert --append`, `append_points`,
      Python's `append_points`), placed in the dataset's octree within its cube, leaves split as
      the writer splits them, the hierarchy (its pages kept), `ept.json` and the source list
      updated, staged beside the dataset and moved in with `ept.json` last, in bounded memory;
      terrio's datasets (LASzip, binary and Zstandard tiles) and Entwine's; datasets built with the
      same `--ept-bounds` merged by appending one to the other. Checked on autzen (10.65 million
      points in three parts): every point once, reads by bounds identical to a build from all the
      points at once, read by PDAL's `readers.ept` (and its `origin` option).

## Bindings: Python and WebAssembly

Python:

- [x] A rasterio-compatible API, `terrio.rio` (`import terrio.rio as rasterio`): datasets opened for
      reading, writing and update with profiles, windowed and incremental writes, masks, `windows`,
      `transform`, `features`, `warp`, `merge`, `mask`, `WarpedVRT` and `MemoryFile`, checked against
      rasterio on the same scripts. Done, and its known gaps closed: rotated and south-up rasters read,
      written (`ModelTransformationTag`) and warped as GDAL does, in the core raster model too
      (`RasterDataset::affine`); GDAL's internal TIFF masks read and written (`write_mask` saved to
      the file); GCPs read and written (GeoTIFF tie points), with GDAL's GCP transformers in the
      warper and in `transform.xy`/`rowcol` (a homography from 4 or 5 GCPs, polynomials of order 1 to
      3, thin plate splines; `gdalwarp -order`/`-tps` pixels in `terrio warp` too); RPCs read (the
      GeoTIFF tag, `_RPC.TXT` and `.RPB` sidecars) and written; per-band nodata from `.aux.xml`
      sidecars; `rasterio.shutil` and `rasterio.stack`; writers larger than memory kept in a mapped
      temporary file and written to GeoTIFF in rows of blocks (`terrio.GeoTiffWriter`, Rust's
      `GeoTiffWriter`); GDAL's `.msk` sidecars read (any local format, overviews included) and
      written (`GDAL_TIFF_INTERNAL_MASK=NO`, formats without internal masks, kept on `r+`), the
      same files as rasterio's on Copernicus DEM tiles.
    - [x] RPC transformers. Done: GDAL's RPC transformer (`terrio::ops::rpc`): the polynomials,
          the iterative inverse, `RPC_HEIGHT`, `RPC_HEIGHT_SCALE`, `RPC_DEM` with its
          interpolations, missing value and DEM CRS, `RPC_PIXEL_ERROR_THRESHOLD`,
          `RPC_MAX_ITERATIONS`, and its rows sampled whole from a WGS 84 DEM; in the warper, as
          `gdalwarp -rpc -to ...` (`terrio warp --rpc --to`, with gdalwarp's `SOURCE_EXTRA`,
          `SAMPLE_STEPS=ALL` and output window for RPCs) and Python's `warp(rpc=, to=)`;
          rasterio's `RPCTransformer`, `xy`/`rowcol` and `TransformMethod.rpcs` with
          `rpc_options`, `reproject(rpcs=)`, `calculate_default_transform(rpcs=)` and
          `WarpedVRT` by GCPs or RPCs; rioxarray's `reproject` through RPCs. Coordinates
          identical to rasterio's and pixels to rasterio's and gdalwarp's on ICEYE images with a
          Copernicus DEM and on synthetic RPCs with DEMs (WGS 84 and UTM, nodata holes).
    - [x] A `rio` command line. Done: `terrio-rio` (and `python -m terrio.rio`), rasterio's
          `rio` commands (`info`, `bounds`, `blocks`, `shapes`, `clip`, `convert`, `merge`,
          `overview`, `warp`, `sample`, `transform`, `mask`, `rasterize`, `stack`, `edit-info`,
          `env`, `gcps`, `calc` with its snuggs expressions, `create`, `rm`, `insp`) with their
          options, defaults and outputs, run with `terrio.rio` and compared with rasterio's `rio`
          on synthetic rasters and on Copernicus DEM and Sentinel-2 COGs.
- [x] xarray integration like rioxarray: a `terrio` backend engine with lazy windowed reads (remote
      COGs read tile by tile, dask chunks), and an accessor with rioxarray's methods (`reproject`,
      `reproject_match`, `clip`, `clip_box`, `pad_box`, `to_raster`, `merge_arrays`). Done:
      `terrio.xarray`, an xarray backend (`engine="terrio"`) and `open_rasterio` reading lazily by
      window (remote COGs fetch only the tiles needed; dask chunks read in parallel, thread-safe and
      picklable), with rioxarray's coordinates, `spatial_ref` grid mapping, attributes and encoding
      (masked, mask_and_scale, overviews, band_as_variable); the `.terrio` accessor with rioxarray's
      `.rio` methods (CRS, transform, bounds, nodata, `isel_window`, `clip_box`, `clip`, `pad_box`,
      `reproject`, `reproject_match`, `interpolate_na`, `to_raster` to GeoTIFF, COG and GeoZarr),
      `merge_arrays` and `merge_datasets`, and an opt-in `.rio` alias; compared with rioxarray on
      synthetic and real files (257 checks, none differing); subdatasets (`group=`,
      `variable=`), GCPs and RPCs as rioxarray opens, stores, writes and (GCPs) reprojects them,
      identical to rioxarray on an ESA CCI soil moisture netCDF and ICEYE GCP and RPC images.
- [x] Wheels with the GDAL backend as an optional extra, for the formats terrio doesn't read itself.
      Done: the Python package's `gdal` feature, built from source against an installed GDAL
      (`maturin build --features gdal`).
- [x] Streaming vector reads and writes, so layers larger than memory can be converted and filtered.
- [x] GeoPackage updates in place (appends without rewriting the file). Done: `write_vector(...,
      append=True)` and `convert(..., overwrite=True)` change the file in place, as GDAL does
      through SQLite, with a rollback journal.
- [x] Windowed decoding of JPEG 2000 and JPEG XL, so Sentinel-2 bands are read by window as COGs are.
      Done: JPEG 2000 read by window and resolution level (OpenJPEG's decode area and reduce factor,
      tiles decoded in parallel) through `LazyRaster`, `terrio convert --srcwin/--overview`, Python's
      lazy reads and `terrio.rio`, with GDAL's overviews and block sizes; a 1024 x 1024 window of a
      10980 x 10980 Sentinel-2 band takes 0.05 s and 81 MB where the whole band takes 0.5 s and 1.7 GB,
      and fetches 3.4 MB of the 26 MB file over HTTP; JPEG XL GeoTIFFs decoded tile by tile. Every
      window and overview is identical to GDAL's.
- [x] Wheels for more platforms (musl aarch64, Windows ARM64) and for Pyodide, to run in the browser.
      Done: musllinux aarch64 and Windows ARM64 wheels; a Pyodide 0.29 wheel (Python 3.13) attached
      to each GitHub release, tested in Pyodide under Node.js in the release workflow.

WebAssembly (the `terrio-js` npm package):

- [x] The operations: `warp` with all its options, `translate`, `rasterize`, `polygonize`, `dem`,
      `calc`, `mosaic`, `proximity` and `sieve`, as the command line and Python offer them.
- [x] Remote data read lazily with HTTP range requests (`fetch`), as native builds do: COGs by window
      and overview, PMTiles, COPC and FlatGeobuf by bounding box, GeoParquet by row group.
- [x] More formats in memory: Zarr and GeoZarr, netCDF and HDF5, point clouds (LAS, LAZ, COPC), and
      writing LERC, JPEG XL and JPEG GeoTIFFs.
- [x] terrio's own FlatGeobuf reader in place of the format library's, which overflowed on large
      files in 32-bit WebAssembly. Done: terrio's driver parses the files, with 64-bit offsets (the
      format library supplies only the schema types and the writer's spatial index); an 867 MB file
      of 2 million lines opens in Node.js in 3.3 s. A layer opened whole takes, with the file's
      bytes, about four times the file's size for lines and five for polygons, so a 1 GB file of
      polygons needs the memory64 item below, or `openUrl` to read it by bounding box (with a
      spatial index; a file without one is still read whole).
- [x] Multithreaded decoding and writing with WebAssembly threads, where the page allows them.
      Done: a threaded module, `terrio-js/threads` (WebAssembly atomics, a rayon pool of Web
      Workers or Node.js worker threads, `initThreadPool(n)`), which `terrio-js/load` picks in
      Node.js and on cross-origin isolated pages, the single-threaded module elsewhere. GeoTIFF
      tiles, JPEG 2000 tiles and Zarr chunks decode in parallel, and GeoTIFF/COG and Zarr writes
      compress in parallel, to the same bytes; with 16 threads, a 3612 x 3612 DEM decodes in 92 ms
      instead of 542 and writes as a DEFLATE COG in 302 ms instead of 2.5 s, and a Sentinel-2
      JPEG 2000 band decodes in 0.9 s instead of 6.8, in Node.js, Chromium and Firefox alike
      (tested in CI in all three). Vector work stays on one thread, as WebAssembly's allocator
      makes small parallel allocations slower.
- [x] A smaller download: optional formats split out or loaded on demand, and size-optimized builds.
      Done: the package holds a second module, `terrio-js/core`, without GeoParquet and Arrow,
      netCDF/HDF5, point clouds, JPEG 2000, Zarr, AVIF and OSM: 8.9 MB (3.0 MB gzipped, 2.3 MB
      with Brotli), against 16.4 MB (5.6 MB, 4.1 MB) for the full one, which a page can import when
      it needs those formats; formats left out say which module has them. Both are built with
      whole-program LTO, the codecs optimized for speed and the rest for size, and wasm-opt -Oz:
      the full module went from 20.6 MB (6.7 MB gzipped) to 16.4 MB, and decodes GeoTIFFs and JPEG
      2000 and writes COGs as fast in Node.js.
- [x] Datasets larger than 4 GB, with the 64-bit memory proposal (memory64) where runtimes support
      it. Done: a memory64 module, `terrio-js/memory64` (every format, up to 16 GB), which
      `load({ memory64: true })` picks where the runtime has 64-bit memories (Node.js 24+, Chrome
      133+, Firefox 134+; Safari falls back): a 1.27 GB FlatGeobuf of 289,890 polygons, which
      runs the 32-bit modules out of memory, opens in 4 s in Node.js with 7.5 GB of memory, and a
      2 GB raster is written and read back. Built on nightly Rust (`wasm64-unknown-unknown`,
      tier 3, `-Z build-std`) with cargo and wasm-bindgen, wasm-pack and its wasm-opt not
      supporting it yet, and with build-time patches of three crates that treat WebAssembly as
      wasm32 only (`tar`, `zstd-sys`, `arrow-buffer`); single-threaded.

## Later

- [x] CRS: datum shift grids (NTv2, GeoTIFF grids), vertical datums, time-dependent transformations
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
      `epsg_accuracy_grids`); CDN grids read in place by range requests, as PROJ reads them (four
      EGM2008 heights fetch 0.85 MB of 80 MB), identical to PROJ's values and to `gdalwarp` with
      `PROJ_NETWORK=ON` (or downloaded whole with `TERRIO_GRID_DOWNLOAD=YES`); transformations
      between vertical CRSs as PROJ lists them (offsets, offsets and slopes, VERTCON and other
      grids, chained through an intermediate vertical CRS, depths included), PROJ's heights on
      727 of 732 points over 604 pairs (`scripts/compare_vertical_with_proj.py`); point motion
      operations between two epochs of a datum (the Canada velocity grids of NAD83(CSRS),
      NKG_RF17vel), PROJ's coordinates to 4 nm.
    - [x] PROJ's chains through several intermediate datums. Done: between CRSs whose datums
          no transformation links directly, the transformations PROJ's database registers
          (concatenations of several among them included, NKG's too) chained through
          intermediate CRSs and datums as `proj_create_operations` finds them: through an
          intermediate CRS between the geographic and geocentric CRSs of the two datums in PROJ's
          order, then through an intermediate datum whatever its CRSs (when none is found, or
          only through PROJ's WGS 84 null transformations, over too small an area or time-dependent
          ones), without those another found replaces or whose grids are missing; sorted, pruned
          and picked per point as PROJ does (0 m ballparks between prime meridians, unknown
          areas, merged translations, heights through the chain, reversed for projected
          targets). The EPSG table carries the registered transformations and geodetic CRSs
          (`E` and `R` rows, 320 to 352 KB). Over 31,127 pairs of EPSG geographic CRSs and 1,204
          projected ones (`scripts/compare_chains_with_proj.py`), GDAL's coordinates within 1 cm
          for 31,037 and 1,179 of them before, for all but the 2 projected pairs PROJ links by
          transformations between projected CRSs after; with grids, 30,993 and 1,173 before,
          31,126 and 1,200 after.
- [x] NetCDF and HDF5 in pure Rust. Done: classic and netCDF-4 reading, with variables in groups
      below the root (`NETCDF:"file.nc":/group/var`) listed as `gdalinfo` lists them; classic
      (CDF-1/2/5) and netCDF-4 writing (`FORMAT=NC4`/`NC4C`, chunked, deflate and shuffle, CF
      georeferencing, as GDAL writes them; terrio's own HDF5 writer); HDF5 rasters as GDAL's HDF5
      driver reads them (`HDF5:"file.h5"://group/dataset`, metadata, user blocks), checked on JPSS
      and Sentinel-5P products; HDF-EOS5 grids and swaths, COSMO-SkyMed and ODIM georeferencing,
      netCDF geolocation arrays and complex (compound) datasets, as GDAL 3.12 reads them;
      rasters written into netCDF-4 groups (Rust, Python, JavaScript, and `terrio convert` with
      `GROUP` and `APPEND_SUBDATASET=YES`), read back identically by GDAL's classic and
      multidimensional APIs, netCDF4 and xarray on Copernicus DEM tiles and their terrain.
- [x] JPEG 2000 and other compressed raster formats. Done: JPEG 2000 (JP2 and J2K, GeoJP2 and
      GMLJP2 georeferencing) is read and written in WebAssembly too, with the same OpenJPEG port as
      native builds, and the npm package reads and writes it; GeoTIFFs compressed with LERC (alone,
      with Deflate or with Zstandard), JPEG XL and LZMA are read, natively and in WebAssembly; JPEG
      (GeoTIFF, GeoPackage and tile archive tiles, `.jpg` files) is decoded exactly as libjpeg decodes
      it for GDAL; ZSTD GeoTIFFs are written in WebAssembly. Every case is compared with GDAL on
      Sentinel-2 bands, GDAL's JPEG 2000 test files and GDAL-written GeoTIFFs. JPEG 2000 files are
      read by window and resolution level as GDAL reads them (their resolution levels as overviews,
      only the tiles, precincts and code blocks of a window decoded, tiles in parallel, locally or
      with HTTP range requests), and JPEG XL GeoTIFFs tile by tile; every window and overview is
      identical to GDAL's.
- [x] Band histograms as `gdalinfo -hist` computes them (`terrio info --hist`, identical to GDAL's on
      GeoTIFFs and scanline formats); color relief written as a VRT (`terrio dem color-relief -f
      VRT`, lookup tables identical to `gdaldem`'s). Done.

## CAD vector interchange (#262)

- [x] Native DWG and ASCII/binary DXF reading and DWG/ASCII DXF writing through acadrust,
      exposed by the existing Rust, CLI/MCP, Python and full JavaScript/WASM vector APIs.
      Common linework, curves, text, faces, polygon hatches and inserted blocks are supported;
      output includes horizontal polygon holes. Drawing coordinates have no inferred CRS.
      See [supported entities and limits](docs/formats.md#cad-drawings-dwg-and-dxf).
- [ ] Additional CAD entities (splines, dimensions, meshes, ACIS solids) and acadrust's optional
      STL, COLLADA, OBJ, glTF/GLB and FBX importers.

## Standalone CRS crate (#268)

- [x] Extract `terrio-crs` in this workspace with its tables, standalone tests and
      attribution, keeping `terrio::crs` compatible. Share grid configuration and
      decoded caches; keep network transport optional and lazy. Landed in #274.
- [x] Measure and reduce the standalone dependency footprint, retaining Terrio's
      existing projection coverage, and publish the crate with release wiring.
      Landed in #276; [`terrio-crs` 0.1.0](https://crates.io/crates/terrio-crs/0.1.0)
      is published with the release workflow's trusted publisher configured.
- The portable table decoder also affects native Terrio, including CLI and Python:
  measured first CRS use increases from about 10.6 ms to 17.9–18.1 ms. Repeated
  calls remain about 3.4–3.7 µs. This initialization tradeoff removes the duplicate
  table-decoder dependency; TIFF codecs remain supported. See
  [the measurement report](docs/crs-footprint.md#native-first-use-cost).
- [x] Geocentric Cartesian XYZ endpoints, EPSG/PROJ/WKT definitions, datum and
      projected composition, and Rust/CLI/MCP/Python/JavaScript/WASM validation
      against pinned and live GDAL/PROJ references (#269, #286).
- [x] PROJJSON v0.7 parsing for static 2D geographic, projected and bound CRSs,
      including units, GIS axis normalization and explicit Helmert operations.
      Rust/CLI/MCP/Python/JavaScript/WASM validation is tracked in #270.
- IAU and additional projection capabilities remain scoped separately in #271–#272.

## Decisions

- The core API is synchronous and reads datasets into memory; remote files are downloaded first.
  GeoTIFFs, local or remote, can instead be opened lazily (`LazyRaster`) and read by window and
  overview level, decoding only the tiles needed and fetching them with HTTP range requests.
  Terrain analysis, warping and mosaics read their inputs that way and write GeoTIFF outputs a
  strip of rows at a time (`ops::dem::dem_to_path`, `ops::warp::warp_to_path`,
  `ops::mosaic::mosaic_to_path`, which the command line uses), with the values of the in-memory
  operations.
- HTTP goes through `ureq` (with rustls) and S3 requests are signed by terrio itself, rather than
  through `object_store`, to keep the API synchronous and the dependencies small.
- GeoPackage is read and written by terrio's own SQLite file reader and writer (pure Rust): no
  C dependency, and files written in one go.
- terrio carries its own EPSG table (`crates/terrio-crs/data/epsg.tsv.zst`, about 220 KB, generated
  from GDAL and PROJ by `scripts/gen_epsg_table.py`): the PROJ strings of the CRSs, their WKT and
  the datum transformations GDAL lists for them, so that terrio picks the same transformation as
  GDAL for each point. The projection formulas are PROJ's, ported to Rust; `wbprojection` remains
  the fallback for projections EPSG CRSs do not use. WKT 1 and the geographic and projected
  CRSs of WKT 2 are read by terrio's own reader.
- NetCDF-4 and HDF5 are read with the pure-Rust `netcdf-reader` and `hdf5-reader` crates, and
  netCDF-4 is written by terrio's own small HDF5 writer (superblock 2, version 2 object headers,
  version 1 chunk B-trees, dimension scales), which the netCDF and HDF5 libraries read: no HDF5
  write crate exists in pure Rust.
- Point clouds use the `laz` crate for LASzip, and terrio's own LAS and COPC code rather than the
  `las` crate (whose header reader keeps only the first EVLR) or `wblidar` (which reads COPC points
  in hierarchy order and cannot read LASzip's unchunked compressor). Point clouds are not
  `Dataset`s: they have their own reader (`pointcloud::open_points`) and columnar `PointCloud`.
