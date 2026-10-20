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

- [x] Data model: `Dataset`, raster `Band` with block access, vector `Layer` with `Feature`, field
      schemas and geometry (on `geo-types`), and a driver registry with format detection.
- [x] Virtual file system: local files, HTTP(S) with range requests, S3 (with SigV4 signing), Google
      Cloud Storage and Azure; zip archives.
- [x] Coordinate reference systems: EPSG codes, WKT and PROJ strings in and out; transformations that
      match GDAL's (units, axis orders, prime meridians and datum transformations) for every EPSG
      geographic and projected CRS. Done.
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
      (`scripts/compare_remote_zarr.py`).
    - [ ] Listing remote groups without consolidated metadata.
    - [ ] Arrays of more than three dimensions.
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
      items as GDAL's STACIT driver builds them (`STACIT:"search":asset=red`), read by window.
    - [ ] POST pagination.
    - [ ] STACTA (tiled assets).
    - [ ] Signed URLs (Planetary Computer).

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
- [ ] PDAL's other filters (`filters.dbscan`, `filters.covariancefeatures`, `filters.lof`,
      `filters.dem`, `filters.colorization`, `filters.overlay` and the long tail).
- [ ] EPT datasets updated in place (appending points, as Entwine's `--subset` and merge do).

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
      `GeoTiffWriter`).
    - [ ] RPC transformers.
    - [ ] `.msk` sidecars.
    - [ ] A `rio` command line.
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
      synthetic and real files (257 checks, none differing).
    - [ ] Subdatasets, GCPs and RPCs.
- [x] Wheels with the GDAL backend as an optional extra, for the formats terrio doesn't read itself.
      Done: the Python package's `gdal` feature, built from source against an installed GDAL
      (`maturin build --features gdal`).
    - [ ] Prebuilt `terrio-gdal` wheels bundling GDAL (vcpkg and cibuildwheel, as rasterio's), or a conda-forge package linking `libgdal-core` (see docs/gdal.md).
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
- [ ] terrio's own FlatGeobuf reader in place of the format library's, which overflows on large files
      in 32-bit WebAssembly.
- [ ] Multithreaded decoding and writing with WebAssembly threads, where the page allows them.
- [ ] A smaller download: optional formats split out or loaded on demand, and size-optimized builds.
- [ ] Datasets larger than 4 GB, with the 64-bit memory proposal (memory64) where runtimes support
      it.

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
      `epsg_accuracy_grids`).
    - [ ] PROJ's chains through several intermediate datums.
    - [ ] Vertical-to-vertical transformations (VERTCON, offsets).
    - [ ] Moving coordinates between epochs (point motion operations).
    - [ ] Reading remote grids by range requests rather than whole.
- [x] NetCDF and HDF5 in pure Rust. Done: classic and netCDF-4 reading, with variables in groups
      below the root (`NETCDF:"file.nc":/group/var`) listed as `gdalinfo` lists them; classic
      (CDF-1/2/5) and netCDF-4 writing (`FORMAT=NC4`/`NC4C`, chunked, deflate and shuffle, CF
      georeferencing, as GDAL writes them; terrio's own HDF5 writer); HDF5 rasters as GDAL's HDF5
      driver reads them (`HDF5:"file.h5"://group/dataset`, metadata, user blocks), checked on JPSS
      and Sentinel-5P products; HDF-EOS5 grids and swaths, COSMO-SkyMed and ODIM georeferencing,
      netCDF geolocation arrays and complex (compound) datasets, as GDAL 3.12 reads them.
    - [ ] Writing groups.
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
- terrio carries its own EPSG table (`crates/terrio/data/epsg.tsv.zst`, about 220 KB, generated
  from GDAL and PROJ by `scripts/gen_epsg_table.py`): the PROJ strings of the CRSs, their WKT and
  the datum transformations GDAL lists for them, so that terrio picks the same transformation as
  GDAL for each point. The projection formulas are PROJ's, ported to Rust; `wbprojection` remains
  the fallback for projections EPSG CRSs do not use and for WKT 2 parsing.
- NetCDF-4 and HDF5 are read with the pure-Rust `netcdf-reader` and `hdf5-reader` crates, and
  netCDF-4 is written by terrio's own small HDF5 writer (superblock 2, version 2 object headers,
  version 1 chunk B-trees, dimension scales), which the netCDF and HDF5 libraries read: no HDF5
  write crate exists in pure Rust.
- Point clouds use the `laz` crate for LASzip, and terrio's own LAS and COPC code rather than the
  `las` crate (whose header reader keeps only the first EVLR) or `wblidar` (which reads COPC points
  in hierarchy order and cannot read LASzip's unchunked compressor). Point clouds are not
  `Dataset`s: they have their own reader (`pointcloud::open_points`) and columnar `PointCloud`.
