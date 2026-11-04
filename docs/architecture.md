# Architecture

```text
crates/terrio        the library
  dataset.rs         open, read and write; format detection; fixes applied after reading
  formats.rs         the driver registry, with GDAL's short names
  drivers/           terrio's own raster drivers; drivers/vector/ its vector drivers
  geotiff.rs, cog.rs GeoTIFF tags and decoding, lazy window and overview reads
  resampled_read.rs  overview-aware resampled window reads (GDAL RasterIO semantics)
  catalog.rs         GTI-compatible indexed raster catalogs with bounded source caching
  stac.rs, stacta.rs STAC items, API searches, STACIT mosaics and STACTA tiled assets
  tiles/             PMTiles, MBTiles and MVT: archives, tile pyramids, vector tile clipping
  vfs.rs, sparse.rs  locations: paths, URLs, s3://, gs://, az://, zip archives, /vsi prefixes
  http.rs, s3.rs     range requests with a block cache; AWS Signature Version 4
  crs.rs             terrio::crs, over crates/terrio-crs
  arrow.rs           Arrow and GeoArrow record batch import and export
  multidim/          native groups, dimensions, coordinate arrays and multidimensional mosaics
  pointcloud/        LAS, LAZ, COPC, EPT, PLY, E57 and PDAL filters
  vsi/               terrio vsi file management and seek-optimized ZIP (SOZip)
  par.rs             fork-safe process-local rayon thread pool
  ops/               translate, warp, mosaic, vrt, render, dem, calc, viewshed, pansharpen,
                     zonal, contour, footprint, vector filters/SQL, topology, coverages,
                     rasterize, polygonize, tiling/retiling and pipelines
  memory.rs          datasets from and to bytes
  info.rs            summaries
crates/terrio-crs    standalone coordinate systems: parsing, identification, transformation
crates/terrio-cli    the terrio command, MCP server, browser UI (ui, view) and usage catalog
python/              Python bindings (PyO3, NumPy, terrio.rio, terrio.xarray)
js/                  WebAssembly bindings (wasm-bindgen, core, threads and memory64 modules)
scripts/             test data, comparisons with GDAL, benchmarks
```

## Data model

Rasters are held as `wbraster::Raster`: typed pixel buffers, band after band and row after row,
with the georeferencing and CRS. `RasterDataset` adds the driver and whether nodata is declared.
GeoTIFFs, tile archives (PMTiles, MBTiles), JPEG 2000, Zarr, VRTs, STAC mosaics (`STACIT`), tiled
STAC assets (`STACTA`), GTI raster catalogs and raw-layout rasters can also be opened lazily
(`LazyRaster`), to read windows, overviews and resampled outputs (`read_resampled`) without loading
the whole image, and GRIB files to read a band at a time.

Vector datasets are `wbvector::Layer`s: features with OGC simple-feature geometries (with Z and
M), typed attributes and a schema. Point clouds use a columnar `PointCloud` model and chunked
`PointCloudReader`, and multidimensional scientific stores expose hierarchical `multidim::Group`
and `multidim::Array` views (`MdArray` in JavaScript) alongside their 2D raster views.

## Building on whitebox_next_gen

The `wbraster`, `wbvector` and `wbprojection` crates supply the data model, many codecs and a
fallback for projections terrio does not compute itself. Testing them against GDAL found codecs that decoded wrong pixels or lost
georeferencing, so terrio has its own drivers for GeoTIFF, JPEG 2000, GRIB, Zarr, netCDF, HDF5, HFA,
GeoPackage (on its own SQLite file reader and writer), PMTiles/MBTiles/MVT, the Esri formats, FlatGeobuf, CSV, XLSX,
GeoJSON, GeoParquet, GML, GPX, KML, CityJSON, CityGML, DWG/DXF, TopoJSON, MapInfo, OSM, WFS,
OGC API Features (`OAPIF`), HEC-RAS and shapefile writing. Upstream bugs
are listed in the pull requests that replaced them.

## Parallelism

Native builds spread the heavy work over all cores with rayon: decoding and encoding GeoTIFF
tiles and strips, building COG overviews, warping and resampling (by output row), rasterizing (by
bands of rows), writing Zarr chunks, and decoding and encoding vector features. Each piece of
work is computed on its own and the results are combined in input order, so outputs are the same
bytes as a single-threaded run gives. The default WebAssembly build runs the same code sequentially;
`terrio-js/threads` runs the raster and codec loops across a pool of Web Workers or Node.js worker
threads over shared WebAssembly memory.

All of it runs on one thread pool, terrio's own (`crates/terrio/src/par.rs`), never on rayon's
global pool. A `fork()`ed child, as Python's `multiprocessing` starts on Linux, has only the
thread that forked: a pool started before the fork has no threads there, and work given to it
would wait forever. terrio's pool is per process, so the first parallel call in a child starts a
new one. A fork also waits for terrio's background work in progress to finish. New parallel code
uses `par`'s functions: `par::map_range`, `par::for_each_owned` and the like for data parallelism,
`par::spawn` for work in the background, and `par::install` around rayon code they cannot express
or a dependency that uses rayon by itself. `clippy.toml` disallows rayon's global-pool functions
and parallel iterators everywhere else.

## Pure Rust

The default builds have no C dependencies, and CI builds them without a C compiler. Three choices
keep it so:

- Zstandard is decoded and written with `ruzstd`, GeoParquet's pages included. The optional
  `libzstd` feature uses the zstd C library natively instead, which decodes faster and compresses
  better: `ruzstd` writes at its fastest level, about zstd's level 1, whatever level is asked for.
- HTTPS uses `rustls` with `rustls-graviola`, whose cryptography is Rust and assembly and needs no
  C compiler. It requires `aes`, `pclmulqdq`, `ssse3`, `avx`, `avx2`, `adx`, `bmi1` and `bmi2` on
  x86-64 (most CPUs since about 2014), and `aes`, `sha2`, `pmull` and `neon` on AArch64: without the
  ones it checks, an HTTPS request panics, naming the missing feature. Other
  architectures, for which no release is built, use `ring`, which needs a C compiler.
- GeoParquet is written with Snappy, which needs no C. Reading also takes Zstandard, gzip, LZ4 and
  Brotli.

JPEG 2000 uses a Rust port of OpenJPEG, in WebAssembly too: terrio supplies the C allocator calls
the port makes. JPEG is decoded by terrio's own decoder, which reproduces libjpeg's arithmetic;
LERC, JPEG XL and LZMA by pure-Rust crates.
