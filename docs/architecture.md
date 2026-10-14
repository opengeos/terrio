# Architecture

```text
crates/terrio        the library
  dataset.rs         open, read and write; format detection; fixes applied after reading
  formats.rs         the driver registry, with GDAL's short names
  drivers/           terrio's own raster drivers; drivers/vector/ its vector drivers
  geotiff.rs, cog.rs GeoTIFF tags and decoding, lazy window and overview reads
  tiles/             PMTiles, MBTiles and MVT: archives, tile pyramids, vector tile clipping
  vfs.rs             locations: paths, URLs, s3://, gs://, az://, zip archives, /vsi prefixes
  http.rs, s3.rs     range requests with a block cache; AWS Signature Version 4
  crs.rs             parsing, identification and transformation of coordinate systems
  ops/               translate, warp, mosaic, vrt, vector filters and SQL, rasterize, polygonize
  memory.rs          datasets from and to bytes
  info.rs            summaries
crates/terrio-cli    the terrio command
python/              Python bindings (PyO3, NumPy)
js/                  WebAssembly bindings (wasm-bindgen)
scripts/             test data, comparisons with GDAL, benchmarks
```

## Data model

Rasters are held as `wbraster::Raster`: typed pixel buffers, band after band and row after row,
with the georeferencing and CRS. `RasterDataset` adds the driver and whether nodata is declared.
GeoTIFFs and tile archives (PMTiles, MBTiles) can also be opened lazily, to read windows and
overviews without loading the image.

Vector datasets are `wbvector::Layer`s: features with OGC simple-feature geometries (with Z and
M), typed attributes and a schema.

## Building on whitebox_next_gen

The `wbraster`, `wbvector` and `wbprojection` crates supply the data model, many codecs and the
projection engine. Testing them against GDAL found codecs that decoded wrong pixels or lost
georeferencing, so terrio has its own drivers for GeoTIFF, JPEG 2000, Zarr, netCDF, HFA,
GeoPackage (on its own SQLite file reader and writer), the Esri formats, FlatGeobuf, CSV,
GeoJSON, GeoParquet, GML, GPX, KML, TopoJSON, MapInfo, OSM and shapefile writing. Upstream bugs
are listed in the pull requests that replaced them.

## Parallelism

Native builds spread the heavy work over all cores with rayon: decoding and encoding GeoTIFF
tiles and strips, building COG overviews, warping and resampling (by output row), rasterizing (by
bands of rows), writing Zarr chunks, and decoding and encoding vector features. Each piece of
work is computed on its own and the results are combined in input order, so outputs are the same
bytes as a single-threaded run gives. WebAssembly builds run the same code sequentially.

## Pure Rust

There are no C dependencies apart from the Zstandard codec, which compiles for WebAssembly too.
JPEG 2000 uses a Rust port of OpenJPEG that is native only.
