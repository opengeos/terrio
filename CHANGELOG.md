# Changelog

## 0.9.0 (2026-11-14)

### Breaking changes

- **Breaking:** an empty, blank or non-UTF-8 `TERRIO_ALLOWED_DRIVERS` now allows no driver; unset it
  to allow every driver (#751).
- **Breaking:** native GML byte output is a standalone XML document rather than a ZIP archive, and
  native MapInfo byte output takes explicit `.mif.zip` or `.tab.zip` names (#851).
- **Breaking (Python):** COG writers in `terrio.rio` report `COG`, not `GTiff`, in `driver`,
  `meta["driver"]` and `profile["driver"]` (#840).
- **Breaking (Python):** an oversized `terrio.rio` window read raises `ResourceLimitError`, a
  `MemoryError` subclass, instead of `RasterioIOError` (#862).
- **Breaking (Python):** anonymous SQLite buffers without a GeoPackage application id need a `.gpkg`
  or `.mbtiles` filename in `terrio.pyogrio` (#966).
- **Breaking (Rust API):** `FlatGeobufInfo` gains the public fields `has_z` and `has_m` (#880);
  `transform_bounds` samples 23 points per edge, endpoints included, instead of 21 (#908); a missing
  or failed velocity grid gives `MissingGrid` instead of `Crs` (#937).
- Krovak WKT 1 without `AXIS` or Esri `XScale`/`XYPlaneRotation` parameters now defaults to
  south-west axes rather than east-north, and WKT projected CRSs follow their `AXIS` nodes (#1015).

### Features

- `terrio.pyogrio` reads GeoParquet files: `read_arrow` and `open_arrow` hand out their Arrow
  record batches as pyogrio does through GDAL's Arrow stream, with GDAL 3.13.3's `meta` (the
  fields as OGR has them, structs flattened, the declared geometry type, `EPSG:4326` for a file
  without a CRS), and `read_dataframe(..., use_arrow=True)` builds its GeoDataFrame from them, as
  `geopandas.read_file(path, engine="pyogrio", use_arrow=True)` asks. Unlike GDAL, whose
  `skip_features` skips the file's rows before `bbox` filters them, the range is taken from the
  rows the rectangle keeps, as `read_dataframe` takes it. Arrow reads of other formats still
  raise `NotImplementedError`.
- `open_stream` takes `columns=[...]`, the fields to keep (a struct column by its name keeps its
  `name.member` fields), and the Arrow export of a GeoParquet stream (`__arrow_c_stream__`,
  `terrio::arrow::open_geoparquet` in Rust) hands out the record batches the file holds, without
  converting them to features, as GDAL's Arrow stream does: the columns keep their Arrow types
  (structs and lists included, where the export flattened them into fields and JSON text before),
  the geometry columns are tagged as GeoArrow extension types with their CRS (OGC:CRS84 when the
  file names none, as geopandas reads it, where GDAL names it EPSG:4326), and the bounding-box
  covering column is left out unless asked for. `bbox` skips row groups by their statistics, then
  rows by the covering column, read alone first (or by the geometries' bounding boxes without
  one). On 12 cores, the 664,000 polygons of fiboa's es-nc file (Brotli, 27 row groups) read
  into a pyarrow table in 0.79 s and 761 MB, against 1.56 s and 966 MB through GDAL 3.13's Arrow
  stream (pyogrio) and 2.33 s and 2.0 GB before; a small rectangle of a 533 MB Overture buildings
  file on S3 fetches 4.9 MB in 5 requests, where the whole file was downloaded.
- Commands that take any number of inputs (`mosaic`, `warp`, `concat`, `filter merge`,
  `raster-index`, `vector-index`, `gti-create` and `zonal-stats --raster`) now expand the wildcards
  `*` and `?` in an input's file name themselves, so `terrio mosaic 'tiles/*.tif' out.tif` mosaics
  every GeoTIFF of a folder from `terrio ui`, MCP clients and Windows shells, which pass patterns on
  unexpanded. The `terrio ui` file browser gains an **Add matching files** box for those inputs: it
  counts the files of the listed folder that match a pattern such as `*.tif` and adds one
  `folder/*.tif` line, instead of one path per click. (#776)
- OpenStreetMap PBF decoding now uses the native Rust decoder without osmpbfreader, preserving the
  five supported layer outputs and the existing XML reader. Malformed or truncated blobs and
  unsupported compression are rejected consistently, including informational headers. Metadata,
  dates and header bounding boxes remain unexposed. (#759)
- GeoTIFF readers opened without explicit limits now take their chunk decoding limit from the
  `TERRIO_TIFF_MAX_CHUNK_BYTES` option (a positive byte count, in the environment or an `Env`
  scope). Python, `terrio.rio`, `terrio.xarray` and the command line can therefore read trusted
  files whose tiles or strips decode to more than 256 MiB, such as pixel-interleaved GeoTIFFs of
  many bands. When the limit refuses a GeoTIFF chunk, the error message now names the option. (#799)
- `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN=NO` keeps a read on the origin of its URL. A redirect to another
  origin (scheme, host or port) is refused before anything is sent there, with an error naming the
  option; redirects within the origin are still followed. The default, `YES`, follows redirects to
  any origin, as before. Until now a read followed up to 10 redirects to any host, and no option
  kept it on the origin it was given (#723). (#793)
- Native HTTPS reads can now trust an additional PEM CA bundle through TERRIO_HTTP_CAINFO or its
  GDAL_HTTP_CAINFO alias. The bundle supplements the compiled Mozilla roots while preserving their
  applied name constraints, certificate-chain verification and hostname verification. Missing, empty
  or malformed explicit bundles refuse HTTPS with a configuration diagnostic; plain HTTP continues
  to work. (#1011)
- Python hosts can confine datum-grid lookup to their effective search directories with
  `terrio.confine_grid_search_paths()` and register encoded grids with `terrio.register_grid(name,
  data)`. Grid lookup refusals now reach the shared warning queue, so Python emits `UserWarning` and
  the CLI reports why a grid was refused, including when transformation fails. The shared warning
  drain also consumes pending standalone CRS diagnostics and deduplicates messages. (#1001)
- GeoTIFF and COG writes honor NBITS for unsigned samples, and compatible copies retain their stored
  width unless explicitly overridden. One-bit palette copies retain one-bit storage and a two-entry
  palette. Selected-overview/AUTO copies preserve their appropriate widths. Wider-domain clipping
  warnings reach the caller, including streamed CLI writes and Python output writes. Resampling
  before scaling or retyping follows the selected GDAL compatibility mode for both packed and
  unpacked sources. (#860)
- The npm package gains `terrio-js/zarr`, a module that reads Zarr arrays over HTTP and nothing
  else: `openZarr(url, { array })`, then `read({ window, bands })` returns one typed array per band.
  It is 0.53 MB (0.19 MB Brotli), against 2.56 MB Brotli for `terrio-js/core` and 337 KB for zarrita
  with numcodecs. It reads as fast as `terrio-js` and opens about 40% sooner. Its CRS is the EPSG
  code the store names. Remote file and object reads reject exposed Content-Range intervals that do
  not match the requested bytes, including overflowing intervals; hidden CORS headers retain the
  existing length/HEAD fallback. Oversized PROJJSON EPSG identifiers are rejected instead of
  truncating. The stricter _CRS URL matching also applies to native Python/CLI and the full JS
  module: noncanonical numeric-tail URLs no longer supply an EPSG code, while valid stored
  PROJJSON/WKT fallbacks remain available. (#900)
- `terrio_crs::Transformer` (and `terrio::crs::Transformer`) gain `inverse()`, which builds the
  transformer of the other direction with the same options and the coordinate epochs swapped. They
  also gain `transform_slice` and `transform_slice_or_nan`, which transform points in place. The
  second marks every point that fails as NaN and carries on, as PROJ's `proj_trans_generic` does
  (PROJ marks them `HUGE_VAL`). That includes points refused under the strict options. A point
  motion whose velocity grid is missing or failed to download now fails with `MissingGrid` instead
  of `Crs`. The docs now say to share a transformer through `Arc`, and a test asserts that it is
  `Send + Sync`. (#937)
- Added `SpatialRef::to_proj4()`, which writes a CRS's PROJ string as PROJ 9.8.1 writes it
  (`projinfo -o PROJ`, pyproj's `CRS.to_proj4()`), or `None` for a CRS PROJ has none for. It is also
  available as `terrio crs CRS --proj4`, Python `crs_to_proj4(crs)` and JavaScript
  `crsToProj4(crs)`. (#958)
- Added `SpatialRef::to_wkt1_strict()`, which writes a CRS's WKT 1 without GDAL's
  `EXTENSION["PROJ4", ...]`, as PROJ 9.8.1 exports `WKT1_GDAL`. It returns `None` where PROJ writes
  no WKT 1, or where the WKT 1 would need the extension. It is for formats whose readers ignore the
  extension, such as a shapefile `.prj`. It is also available as `terrio crs CRS --wkt1-strict`,
  Python `rio.CRS.to_wkt1_strict()` and `crs_to_wkt1_strict(crs)`, and JavaScript
  `crsToWkt1Strict(crs)`. `to_wkt()` keeps its API. (#974)
- `terrio-crs` reads and writes GeoTIFF GeoKeys without the `terrio` crate:
  `SpatialRef::from_geokeys(directory, doubles, ascii)` reads a CRS as GDAL does, and
  `SpatialRef::to_geokeys()` gives a CRS's keys. The GeoTIFF and LAS writers now write Gnomonic and
  Orthographic at their centre (ProjCenterLong/LatGeoKey), New Zealand Map Grid as method 26, a
  linear unit without an EPSG code as a user-defined unit with its size, and south-orientated
  Transverse Mercator as method 27; the last two were written as ESRI PE citations, and the first
  two at the natural origin. (#981)
- Added Eckert IV (`+proj=eck4`) and Natural Earth (`+proj=natearth`) to terrio-crs's default build,
  ported from PROJ 9.8.1's `eck4.cpp` and `natearth.cpp`. They no longer need the
  `wbprojection-fallback` feature, a feature that grows the optimized wasm build from 1.5 MB to 3.9
  MB (0.91 MB to 1.25 MB gzipped, `docs/crs-footprint.md`). ESRI:53012, 53077, 54012 and 54077
  (Sphere and World Eckert IV and Natural Earth) are now listed and computed in the default build.
  In `terrio`, which enables the fallback, both methods now use the native port instead of
  wbprojection's. (#988)
- `SpatialRef::axis_order()`, `projection_params()`, `body()` and `is_earth()` (in `terrio-crs`, and
  forwarded on `terrio::crs::SpatialRef`) describe a CRS as PROJ 9.8.1 reads it: the order of its
  first two axes (`AxisOrder::EastNorth`, `NorthEast`, `Other`, `Unknown`), the method and
  parameters of its PROJ string (`ProjParams`), and the celestial body it is on. `terrio crs CRS`,
  and so its MCP tool, prints them as `Axis order`, `Body` and `Method` lines. (#993)
- `terrio-crs` 0.2.4 adds GeoTIFF GeoKeys (`from_geokeys`, `to_geokeys`), PROJ-string and strict WKT
  1 export (`to_proj4`, `to_wkt1_strict`), `Transformer::inverse` and slice transforms, axis order,
  projection parameter and celestial body accessors, `confine_grid_search_paths`, and Eckert IV and
  Natural Earth in its default build. It fixes polar stereographic poles and variant C, the axes of
  WKT projected CRSs, PROJ-string parameter values PROJ refuses, datum shifts of latitudes beyond
  the poles, and grid reads from untrusted CRS definitions, and no longer turns on serde_json's
  `preserve_order` for dependents' builds.

### Performance

- A `Raster` read lazily from a file, such as a GeoTIFF (`open_raster(path, lazy=True)`,
  `rio.open`, the xarray backend), can be read from several Python threads at once: `read`,
  `read_mask` and `numpy.asarray` raised `RuntimeError: Already borrowed` when a read overlapped
  another. An overlapping read takes another reader of the same path, opened from the process-wide
  caches (its first bytes are not requested again) and kept, up to four, for the next overlapping
  read. Each reader has its own file handle, and their decoded chunks are cached separately under
  the one block-cache byte cap. A relative path is resolved when the raster opens, so a later
  `os.chdir` does not change the file read, and a reader opened on another thread takes the options
  of the thread that opened the raster (an `Env` such as `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`).
  Other rasters, including those decoded whole when they open (formats read whole and local VRTs),
  and the other methods, are still one call at a time.
- A remote read of a long range through the block cache, such as a GeoParquet row group or a run
  of COG tiles, is fetched as up to 8 parallel requests of at least 1 MiB each instead of one at
  the pace of a single connection (#741). `TERRIO_HTTP_MULTIRANGE=SERIAL` keeps one request.
- A selection of GeoParquet fields decodes only their columns and the geometry, and fetches only
  their column chunks from a URL: `open_stream(columns=...)`, `terrio convert --select` (without
  `--where` or `--sql`, which may name other fields), and JavaScript's `openUrl(...).read()`,
  which takes `columns` too.
  Breaking (Rust): `terrio::vector::read_geoparquet_sparse` takes a fifth argument, `columns`;
  pass `None` to read every field, as before.
- GeoParquet reads decode row groups in parallel, a window of as many as there are threads at a
  time, and read remote files in place: `open_vector` and `open_stream` on a URL fetch the footer,
  then only the column chunks of the row groups the filter keeps, by range requests, instead of
  downloading the whole file.
- Multi-chunk window reads of a GeoTIFF keep the decoders of their worker threads for the next
  read of the same open raster, instead of opening one per worker, which reads the image's whole
  chunk offset and byte count tables, on every read. 640 x 640 reads of a GeoTIFF in 481,000
  tiles went from 130 to 5.7 ms on 32 threads, with the block cache off. An open raster keeps up
  to one decoder per thread, each holding its image's tables (16 bytes a chunk).
- MBTiles archives are read in place (#693). Opening one reads its schema and metadata from the
  file, mapped into memory on native builds, and each tile is read when asked for, by a lookup in
  the index on `(zoom_level, tile_column, tile_row)` that MBTiles writers create (or, for the
  `map` and `images` layout, in `map`'s index and `images`' `tile_id` index). An archive was read
  whole and every tile copied again, so peak memory was several times the file's size. `terrio
  info` of a 2 GB archive of 10,240 tiles took 10 s, read the whole 2,019 MB from disk and peaked
  at 7.8 GB of memory; it now takes 0.17 s, reads 10.6 MB (with the kernel's read-ahead) and peaks
  at 83 MB. A 600 x 600 window read takes 0.29 s, 27.3 MB and 98 MB instead of 4.6 s, 2,019 MB and
  7.8 GB. An archive without such an index has the rowids of its tiles listed on opening, from
  their key columns only, without reading tile bytes, unless those columns follow `tile_data` in
  the table (as `tile_id` does in mbutil's unindexed `images`): the bytes before them, and a
  corrupt tile among them, are then read on opening. An index whose order the lookup cannot follow
  (a descending constraint, a column collation) is not used. Opening an MBTiles archive from
  memory keeps the bytes but no longer copies the tiles. Listing a zoom level's tiles now lists
  the rows of the `tiles` table, or of `map`, including a row whose `tile_data` is not a blob or
  whose `tile_id` names no image (reading it gives no tile); a raster opened from the archive
  takes its tile size from the first listed tile that has bytes. A NULL `tile_id` matches no
  image, as in the `tiles` view's join. Of rows with the same key, the last is read, even if it
  holds no tile; a row whose column is outside its zoom level's grid (`tile_column >=
  2^zoom_level`) is passed over. A raster opened from an archive without `bounds` takes its extent
  from the listed tiles, including rows without bytes.
- A `WarpedVRT` over a dataset terrio opened for reading now reads and warps each source window
  in one call that releases the GIL, so reads on several Python threads, such as a server
  answering tile requests on threads, no longer wait for each other's warps as in 0.8.0. The
  window is held once, in Rust, where no other thread can change it. These keep 0.8.0's read-only
  window, warped with the GIL held: datasets opened for writing or update, opened at an
  `overview_level`, or whose `_read_level` is replaced; another `WarpedVRT` as the source;
  complex bands; and float32 bands warped in a 32- or 64-bit integer type, or float64 bands in a
  64-bit one. The pixels are the same on both paths.
- `warp` of a `Raster` in memory releases the GIL for the whole warp: it takes the raster's
  shared pixels (no copy) and warps them after releasing it, so warps from Python threads
  (rioxarray-style `reproject` of xarray arrays on a thread pool) overlap. 96 nearest-neighbour
  warps of 3000 x 3000 on 12 threads of a 12-core machine took 2.36 s instead of 3.8 to 4.4 s.
  Other Python threads now run during the warp and may use the raster; the warp reads the
  pixels it was called with.
- Reads of a remote Zarr store keep up to 32 requests in flight, whatever the number of cores.
  Its chunks were fetched on terrio's thread pool, one blocked thread per request, so a read
  had as many requests in flight as the machine has cores. A 512 x 512 window of the 128 bands of
  one year of the OlmoEarth v1.3 embeddings store on source.coop (18 inner chunks, 66 MB, a server
  answering in about 0.8 s) took 6.6 to 7.2 s on 12 cores and 9.2 to 10.7 s on 4; it now takes
  5.3 to 5.7 s and 4.5 to 5.9 s. The bands and chunks of a read share one budget of requests,
  smaller when large chunks would take more than half the memory the process can use (per read:
  reads running at once each have their own), and no chunk is requested once one has failed.
  Local stores and WebAssembly are read as before.
- A remote read requests the separate byte runs of a window (a COG window over several tiles)
  in parallel, up to 8 at once, through the process-wide connection pool, as GDAL's default
  `GDAL_HTTP_MULTIRANGE=PARALLEL` does; `TERRIO_HTTP_MULTIRANGE=SERIAL` (or the GDAL name)
  requests them one after another. The requests carry the calling thread's options and STAC
  asset policy. A new request starts as each run is stored, so at most 8 responses are held at
  once, and one read's runs stay within half the block cache. A failed run ends the read as a
  serial read would: a request in flight waiting for its response is abandoned within 50 ms, even
  when the response has stalled (a connection being opened still waits for its connect timeout),
  and none is retried or sent after it. Peak memory can rise with many
  readers, from glibc's per-thread arenas (`MALLOC_ARENA_MAX=2` bounds them).
- A KML or KMZ document's layer names are made unique in time linear in its Folders and Documents.
  It took time quadratic in them, and cubic when they shared a name (#630): `terrio info` on a
  1,654-byte KMZ of 4,000 Folders without a name takes 0.10 s, where it took 28.7 s, and on one of
  100,000 named Folders 2.1 s, where it took 24.6 s. The names are the same, in the same order:
  `Folder`, `Folder (2)`, `Folder (3)`, and so on. (#763)
- Multi-chunk GeoTIFF window reads now share one budget of decode worker threads. (#755)
- Coarse sum warps read oversized source windows a strip at a time while retaining the whole-window
  sum result. Chunk planning and strip sizing use the same source/conversion memory estimate, so
  only unsplittable output chunks of at most 2x2 pixels allocate footprint accumulators. (#768)
- `terrio.rio.shutil.copy` writes a COG from its source as the file is written, a row of the COG's
  tiles at a time, its overviews computed as the rows come, instead of first copying the source's
  pixels into the dataset it creates, which held them until it closed. The copy stages bounded
  source slabs instead of unconditionally holding its source's image: copying the issue's 4096 x
  4096 RGB GeoTIFF to a Deflate COG grew the process's peak by 296 to 322 MiB, and grows it by 197
  to 206 MiB (268 to 283, and 145 to 166, with `TERRIO_CACHEMAX=0`), for the same file byte for
  byte. A GeoTIFF larger than `TERRIO_RIO_IN_MEMORY_LIMIT` is copied the same way, without the
  temporary file that held its pixels. A copy whose source has a dataset mask or overviews the COG
  copies, a COG compressed with JPEG, WebP, LERC or JPEG XL, and a smaller GeoTIFF still hold the
  source's pixels until they are written. (#797)
- The decoders that the worker threads of a GeoTIFF window read leave for the next read (#700) are
  kept within one budget for all the open readers of the process, 256 MiB by default, where each
  reader kept up to one per thread whatever the others held. Each spare decoder holds its image's
  chunk offset and byte count tables, 16 bytes a chunk, so 24 open rasters of 490,000 tiles held 6.1
  GiB on 32 threads; they now hold 0.6 GiB. A decoder given back past the budget closes the least
  recently used decoders of the other readers, which open theirs again on their next read. Readers
  whose decoders fit the budget together read as before. (#856)
- Zarr reads unshuffle Blosc bit-shuffled chunks 8 × 8 bits at a time instead of one bit at a time,
  in Python, the CLI and terrio-js: unshuffling 4.7 MB of int8 TESSERA embeddings took about 100 ms
  in terrio-js before. (#866)
- Zarr reads fill a region's missing chunks by doubling the filled part instead of copying the fill
  value one element at a time: with a non-zero fill such as NaN, the fill took 36 ms of a 40 MB read
  in terrio-js before. (#867)
- Zarr reads convert a window a row at a time when its columns are contiguous (the usual C-order
  layout) instead of one element at a time: the conversion took 12 ms of a 40 MB read in terrio-js
  before. (#868)
- GeoParquet reads decode WKB geometries straight from the column's bytes, without copying each
  value, in Python, the CLI and terrio-js: decoding took 0.82 s of a 3.0 s read of 90k polygons in
  terrio-js before. A geometry whose collections nest more than 64 deep (GeoParquet or GeoPackage)
  now reads as no geometry instead of overflowing the stack; real files nest a few levels at most.
  (#869)
- terrio-js `RemoteDataset.read` without a bbox takes the layers it read instead of copying every
  feature: the copy took 0.41 s of a 3.0 s read of a 90k-polygon GeoParquet file. It applies to
  GeoParquet, FlatGeobuf and PMTiles. (#874)
- terrio-js `toGeoJSON` is faster and uses less memory: 90k polygons take 0.65 s instead of 3.6 s,
  and layers whose GeoJSON text passed 4 GB (664k polygons) no longer run out of wasm memory. The
  objects are the same, except that strings with control characters are now escaped (they made the
  text invalid JSON before), property numbers that are not finite become `null` (coordinates keep
  them), and in a list of coordinates where only some have a height, the others get `NaN`. (#875)
- terrio-js in Node.js, Deno and Bun splits a remote Zarr shard read into as many requests as it
  keeps in flight (12), instead of runs of up to 1 MiB: 128 bands of a 192 × 192 TESSERA window (4.3
  MB in 36 inner chunks of one shard, from Europe to S3 us-west-2) now take 2.29 s instead of 4.86
  s, together with #871 and #872. Browsers keep the runs. (#873)
- terrio-js opens a remote Zarr v3 array that has no consolidated metadata in one round trip fewer:
  the array's `zarr.json` is now asked for together with the root's documents, instead of after they
  came back. A store with consolidated metadata, and every v2 store, pays one request in vain.
  (#872)
- terrio-js opens remote files with large headers in fewer round trips: while a file opens, each
  request is at least as long as the bytes read so far, up to 4 MiB. An OpenAerialMap COG of 93456 ×
  52031 pixels, whose IFDs and tile tables fill 655 KB, now opens in 4 sequential requests instead
  of 8. This applies to every format terrio-js opens by URL (COG, PMTiles, COPC, FlatGeobuf,
  GeoParquet); Zarr opens are unchanged. (#876)
- terrio-js keeps 12 remote requests in flight in a sliding window, the next starting as soon as one
  ends, instead of sending batches of 12 that each waited for their slowest request. When a request
  of a remote read fails while the other bodies are still streaming (a refused Zarr shard, for
  example), the read now returns the error and cancels those bodies, without keeping the bytes they
  had read; before, a Zarr read did not return. A server that ignores `Range` sends the file once:
  the other bodies in flight are cancelled. (#871)
- JPEG COGs and other pixel-interleaved (RGB) GeoTIFFs read faster: with a local Sentinel-2 TCI JPEG
  COG in terrio-js, first reads take 27% less time and warm reads 20% less. In `terrio-js/threads`,
  these loops were what made JPEG reads slower than in the single-threaded module (warm 76 against
  49 ms); the two now read at the same speed. (#896)
- GeoPackage feature pages materialize attributes only for requested records. Skip and limit are
  applied during native SQLite traversal. Spatially filtered pages inspect candidate geometry blobs
  and count matching features before decoding selected records' attributes. Feature values, FIDs,
  table order, layer selection, fields and CRS are preserved for file and in-memory reads. (#893)
- GeoTIFF and COG reads backed by a SparseFile decode prefetched tiles on the thread pool, including
  terrio-js/threads remote reads. Worker initialization can fall back to the primary decoder without
  fetching an evicted header again. Sparse cache misses retain Error::Io with a typed NotFetched
  cause instead of stringified InvalidData, allowing missing chunk bytes to be fetched and retried.
  (#898)
- terrio-js now fills unused request slots by fetching long merged ranges as parallel, block-aligned
  pieces in Node.js, Deno and Bun, preserving requested byte coverage. With default 64 KiB blocks,
  splitting can begin at 512 KiB and fill twelve concurrent request slots. More requests can add
  per-request cost or rate-limit pressure; there is currently no JavaScript disable switch. Initial
  opening probes and browser grouping remain unchanged. (#1017)
- Sparse nearest-neighbour GeoTIFF reads batch the sampled data, mask and UInt16 alpha chunks before
  decoding, reducing retry rounds while preserving lazy I/O and decoded-cache hits. Sparse buffering
  stops speculative read-ahead at the current sparse block boundary; explicit large reads retain
  batched missing-range discovery. (#828)

### Fixes

- A VRT source with fractional `SrcRect` or `DstRect` offsets is read within its rectangles (#702).
  A window with as many source pixels as output pixels was copied one for one whatever its offsets,
  so `bilinear`, `cubic` and `average` returned the source pixels a fraction of a pixel from where
  the rectangles place them, as in the mosaics `terrio mosaic` writes onto a grid a fraction of a
  pixel off a source's: it is now copied only at the same scale on whole-pixel offsets, and
  otherwise resampled by the source's method (`nearest` takes the nearest pixels). A fractional
  window was also widened to whole output pixels, and its source window with it, so pixels outside
  the `SrcRect` reached the output for every method. A source now reads only the pixels that meet
  its `SrcRect`; a method sampling a point writes only the output pixels whose centres are inside
  the `DstRect`, and an area method gives an output pixel partly inside it the part of the source
  inside it. A ramp `0, 10, …, 70` mosaicked a quarter pixel east with `bilinear` or `average` gives
  `2.5, 12.5, …, 62.5` instead of `0, 10, …, 60`, and the pixels `[10, 20]` of `[10, 20, 30]` placed
  a quarter pixel east sum to `[7.5, 17.5, 5]` instead of `[7.5, 17.5, 27.5]`. Reads of such a VRT
  by windows now give the pixels of a whole read: a window of one pixel at the source's resolution
  was copied whatever its offsets. The VRT `terrio mosaic` writes now gives each source its whole
  raster as `SrcRect`, with a `DstRect` that runs past the mosaic's edges where the source does, so
  that a kernel at the edges of a `--te` extent still reads the source's pixels past them.
- A feature-service query URL (`f=json`, `f=pjson`, `f=pbf` or `resultRecordCount=`) or a WFS
  `GetFeature` URL whose name points to no driver is refused before any request when no allowed
  driver (`set_allowed_drivers`, `TERRIO_ALLOWED_DRIVERS`) could read its answer: for `open_vector`
  and `list_layers`, none of the ESRIJSON, ESRIPBF, GeoJSON and GML drivers (nor GDAL, with the
  `gdal` feature); for `open` and `info`, which read a raster answer as a raster, no raster driver
  either. They requested it and read the start of its answer (6,000 bytes for a feature-service
  query) before refusing it (#722). Otherwise the answer is requested and checked against what it
  holds, as before.
- `terrio.rio` keeps a color interpretation other than red, green and blue on three Byte bands,
  or four compressed with WebP, where its GeoTIFF writer makes them RGB unless told otherwise:
  in a COG, set on the dataset or copied by `shutil.copy(..., driver="COG")` (#717); in an
  update (`r+`) of a GeoTIFF or a COG; and in a GeoTIFF written with `photometric="ycbcr"`.
  `terrio.rio` told the writer only the bands whose interpretation was not gray, then
  undefined, so `undefined` x 3 read back as `undefined, green, blue`, and
  `gray, undefined, undefined` as RGB. These bands are still red, green and blue by default, and
  a COG writer, or one in YCbCr, now reports that before the file is written. Other GeoTIFFs and
  COGs are written as before.
- Under `TERRIO_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`, or its other name
  `GDAL_DISABLE_READDIR_ON_OPEN`, a local GeoTIFF no longer takes overviews from an `.ovr` beside
  it. They served its reduced reads (`terrio translate --outsize`, a `terrio.rio` read with a
  smaller `out_shape`), so a file placed next to the dataset changed the pixels read (#720). The
  commands that manage an external `.ovr` (`terrio overviews --external`, and the Rust and Python
  overview functions) still read it under the option; the `.msk`, `.aux.xml` and `.ovr.msk` beside
  the GeoTIFF still follow the option, as before.
  `terrio.rio` follows both names of the option: under the `TERRIO_` name it still applied an
  `.aux.xml` sidecar's tags, band descriptions, scales, offsets, units, color interpretations and
  nodata, and read RPC sidecars. Under the option, its `files` no longer lists the `.aux.xml`,
  `.ovr`, `.msk`, world and RPC files, which are not read, nor a `.hdr` other than the header of an
  ENVI or EHdr raster.
- `terrio.rio` writes JPEG 2000 to a `.jp2` path opened for writing without a driver:
  `driver_from_extension` names the driver `JP2OpenJPEG`, which `open(..., "w")`,
  `shutil.copy` and `terrio.xarray`'s `to_raster` refused in 0.8.0 (`DriverCapabilityError`,
  `DriverRegistrationError`). Their driver names are now read as the driver allow-list reads
  them, so `JP2OpenJPEG`, `JPEG2000`, `JP2KAK`, `JP2ECW` and `JP2MrSID` write terrio's JP2, as
  `MemoryFile` already did with `JP2OpenJPEG`; the file reads back with `driver == "JP2"`.
  `terrio.rio`'s writers still refuse a name with whitespace around it. The other writers (`-f`
  on the command line, `Raster.write(driver=...)`) read names the same way, without the
  whitespace around them: `JP2MrSID`, the one of these names they refused, now selects JP2.
- A polar stereographic CRS on an ellipsoid projects the pole it is centred on to its false
  origin, and refuses the opposite pole. In 0.8.0 its own pole failed to project, and the opposite
  pole, which no coordinates represent, came out about 10²³ m away: EPSG:3413, 3031, 32661 and the
  other ellipsoidal polar stereographic CRSs, of variants A and B (spherical ones were right). A
  `sum` into a longitude and latitude grid whose edge is the pole now writes the row along it: from
  10,000 km of 10 km EPSG:3413 pixels centred on the North Pole into 1° pixels from 80° N to
  90° N, none of the 360 pixels along the pole was written, and the grid held 36,675.8 of the
  37,044.5 it now holds (1% less). The longitude and latitude grid suggested for a raster that
  covers its pole now reaches the North Pole exactly, over every longitude, and reaches the South
  Pole to within one pixel, as its height rounds down: for the same raster, it spanned 30.5° N to
  46.0° N, and now 30.4° N to 90° N.
- `terrio.rio` keeps a PixelIsPoint raster (`AREA_OR_POINT=Point`) PixelIsPoint when it writes a
  GeoTIFF or COG. `shutil.copy`, a writer given `update_tags(AREA_OR_POINT="Point")`, an `r+`
  update of a PixelIsPoint GeoTIFF and `terrio.xarray`'s `to_raster` wrote it as PixelIsArea in
  0.8.0, whole or in rows. The tag now reaches the GeoTIFF writer, which writes the raster type
  with the tie point at the centre of pixel (0, 0); the transform is unchanged. The raster type
  needs a CRS, the dataset's or its GCPs': without one, the raster is still written PixelIsArea,
  now with a `UserWarning` saying so.
- `terrio.xarray`: an object whose `.terrio` (or `.rio`) accessor was used is freed as soon as it
  is dropped. xarray's accessor registration keeps a reference cycle (the object caches its
  accessor, which refers to the object) that only the cyclic garbage collector frees, and that
  held the pixels of every object `.terrio` touched, including views of each variable that
  `write_crs` reads internally, so that a loop loading arrays could keep gigabytes. The object now
  caches only the accessor's state, and each access returns a new accessor over it, so
  `da.terrio is da.terrio` is False.
- A build with the `gdal` feature and without the default features
  (`--no-default-features --features gdal`) compiles. The GDAL backend formats the date-time
  fields it reads with chrono, whose formatting needs its `alloc` feature, and only the
  `parquet` feature turned it on (through arrow-array); the `gdal` feature now enables it. CI
  checks that build in the GDAL backend job (#732).
- `WarpedVRT` raises `WarpOptionsError` when `src_alpha` is not one of the source's bands. In
  0.8.0, a negative value could make another band the alpha, and a value out of range raised
  `IndexError` when the VRT was read.
- `TERRIO_ALLOWED_DRIVERS` set but empty, blank or not UTF-8 allows no driver: it allowed every
  driver, as the option unset does. `terrio.rio` no longer opens a `MEM:::DATAPOINTER=` name under a
  raster list that allows nothing, from this option or from `set_allowed_drivers([])`. A thread's
  scope (`terrio.rio.Env`, `config::push_scope`) now only narrows the list outside it on the calling
  thread. Work on Terrio's own pool does not inherit that scope yet; #761 / #769 tracks that
  separate fix. A scope that set the option to an empty value or to other drivers, or unset it,
  lifted or replaced the environment's list on its thread: a process limited to `GTiff` opened an
  Esri ASCII grid there (#721). The option is documented in `docs/remote-data.md` and
  `docs/python.md`. Breaking: an environment that sets the option empty (a blank templated variable,
  say) now opens nothing; unset it to allow every driver. (#751)
- netCDF-4 subdatasets now follow GDAL's definition order for tracked groups and name order for
  untracked groups. Unqualified multi-variable opens select the first data variable in that
  corrected order; explicitly selected variable pixels remain unchanged. (#752)
- Unreferenced raster outputs no longer gain georeferencing: JPEG 2000 omits its GeoJP2 box,
  PNG/JPEG/WebP omit world files, and GeoTIFF/COG omit tie points. Translate, convert and pipelines
  preserve absence unless bounds or explicit output options assign a transform; whole GDAL reads
  retain transform presence separately from fallback pixel coordinates. A CRS alone remains written
  as before. (#753)
- Area-based warp resampling now uses the valid source area inside each output footprint and its
  chunk window, fixing empty coarse thumbnails and biased edge weights (#703). Invalid transformed
  bounding corners leave pixels empty; zero-width footprints retain the documented half-open edge
  behavior. (#756)
- A local GeoJSON or GML file whose name points to no driver (`data`, `data.txt`, a GML saved as
  `.xml`) is read by its content, as a remote response under such a name is: `open_vector`, `open`,
  `list_layers`, `info`, `convert`, `terrio.open_vector` and `terrio.pyogrio` failed with "format
  not recognized". JSON without a GeoJSON root `type` is still not recognized, and an allow-list
  refuses such a file as before. Builds with the `gdal` feature read it with the GeoJSON or GML
  driver, and with GDAL only when that driver refuses it (#715). (#760)
- Shapefiles with a nonempty `.prj` that contains prose, a URL, malformed WKT or trailing
  non-definition text now open without a CRS and emit a sanitized warning. Python pyogrio reads also
  warn and supply no CRS when pyproj rejects a retained definition, allowing the features to load.
  (#762)
- Zonal statistics now opens and reads raster paths under the calling thread's scoped options on
  helper threads, including driver restrictions and sidecar-discovery settings. This closes the
  scope-propagation gap disclosed in #751. (#769)
- COGs written a strip at a time now spool compressed tiles beside the output, or in
  TERRIO_TMPDIR/CPL_TMPDIR when configured, avoiding space consumption on the system drive. The
  selected spool directory must permit creating files, even when replacing a writable output file;
  use an override when needed. (#770)
- `terrio.rio` no longer changes the process's warning filters when it multiplies affine transforms.
  `transform.compose` and `apply` entered `warnings.catch_warnings()` on every product, to silence
  affine 3's deprecation of `*`, and so did `windows.transform`, `windows.bounds`, the datasets'
  `window_transform` and `window_bounds`, and `array_bounds` of a rotated grid. Each change made
  Python forget the warnings it had shown, so a warning shown once per location (Python's default)
  was shown again on every tile request: 151 to 160 times for 192 requests on 8 threads, against
  once now. The filter list, which the whole process shares, was also replaced from reader threads,
  so concurrent reads could leave affine's `ignore` filter in place. The products are now computed
  from the transforms' coefficients, in affine's order, so they are unchanged to the bit and warn
  nothing. `WarpedVRT` of a given size no longer raises affine's `PendingDeprecationWarning` either.
  The xarray accessor checks a nodata value with NumPy's floating-point error state instead of a
  warning filter, and `terrio.pyogrio` changes the filters to parse date-times only on pandas 2, to
  silence its warning about mixed time zones. (#778)
- Datum shift and geoid grids named by a path in a CRS definition (`+nadgrids=/data/a.gsb`,
  `./a.gsb`, `../a.gsb`, including inside a WKT1 `PROJ4` extension) are no longer read once grid
  search paths are set from code (`set_grid_search_paths(Some(..))`, Rust and Python): they are
  treated as missing, so a CRS from untrusted input cannot make a transform read arbitrary files.
  Relative grid names must stay inside the search directories (`sub/grid.gsb` works,
  `a/../../etc/hosts` is refused), and only regular files of at most 2 GiB are read, so FIFOs and
  devices such as `/dev/zero` are never opened. Without search paths set from code, a grid named by
  a path is still read from it, as PROJ does. (#781)
- A KMZ archive is read from its first KML file at the root of the archive, in the order of its
  entries, whatever its name, and an archive holding two entries with the same name is refused as
  invalid data. With `a.kml` before `doc.kml` at the root, terrio read `doc.kml`; of two entries
  named `doc.kml`, it read the second (#725). (#789)
- With `TERRIO_WBVECTOR_FALLBACK=YES`, a vector file that only the fallback recognises by its
  content (a GPX, KML or GeoPackage file under a name of no driver, or a GML document that terrio's
  own GML reader refuses) now gives the features `skip_features` and `max_features` ask for. It was
  read whole: `terrio.pyogrio.read_dataframe(path, skip_features=1, max_features=1)` gave all 3 rows
  of a GPX or KML document of 3 points named `.bin`, against 1 under its own extension, and paging
  through such a file returned every row on each page. (#791)
- A GeoPackage read with `skip_features` or `max_features` now gives the features asked for, counted
  after the spatial filter when there is one. It gave every feature of its layers:
  `terrio.pyogrio.read_dataframe(path, skip_features=1, max_features=2)` returned all 5 rows of a
  GeoPackage of 5 points, where the same points in GeoJSON give 2, so paging through a GeoPackage
  layer returned the whole layer on every page. (#795)
- A VRT has the overviews of its first source, at its own scale, so a reduced read of it
  (`read_resampled` and `out_shape` in Python, `terrio translate --outsize`, `terrio warp` at its
  automatic overview) reads its sources' overviews instead of their full resolution. Each of its
  overviews reads every source from the source's own overview of the same scale or, when the source
  has none, from its nearest finer level, and only the first source is opened to list them. A VRT
  reported no overviews: a 256 x 256 read of a one-for-one VRT of a 16384 x 16384 COG decoded the
  whole COG, in 22.9 to 28.1 s and 1,477 to 1,479 MiB, and gave samples of its full resolution
  rather than an overview's pixels; it now takes 0.05 s and gives the COG's own pixels. A VRT with a
  kernel source has no overviews, nor have warped and pansharpened VRTs. (#796)
- EPSG:2985 and EPSG:2986, and WKT 1/WKT 2/PROJJSON definitions of Polar Stereographic variant C,
  now transform with the false origin on the standard parallel and preserve their datums.
  Previously, northings were approximately 2,499 km too large and the EPSG definitions lost their
  datum transformations. Both configurations of `terrio-crs` support the correction. (#803)
- EPT point-cloud reads now apply the calling thread’s scoped options to every node, including HTTP
  headers, user agent, driver restrictions and redirect policy. Previously, reads spanning several
  nodes lost options set through `terrio.rio.Env` or Rust configuration scopes. (#804)
- `terrio.rio.shutil.copy` no longer replaces its destination with a raster of zeros when a read of
  its source fails, in the copies that hold the source's pixels until they are written: a GeoTIFF
  below `TERRIO_RIO_IN_MEMORY_LIMIT`, a COG compressed with JPEG, WebP, LERC or JPEG XL, a source
  with a dataset mask, and a source whose overviews a COG copies. An existing destination now keeps
  its content, and no file is left where there was none. The copy raised, but its dataset, with
  zeros (or the nodata value) where the source had not been read, was written in place of the
  destination as a valid raster. (#821)
- On an ellipsoid, the inverse of the polar Lambert azimuthal equal-area projection now refuses
  coordinates beyond the rim of its disk, and infinite ones, as it already did on a sphere:
  `terrio.rio.warp.transform` returns them as infinity, and a warp leaves them nodata. This covers
  EPSG:3571 to 3576, 3973, 3974, 6931 and 6932, and `+proj=laea +lat_0=90` or `-90` on an ellipsoid.
  Before, such coordinates transformed to the opposite pole. A world warped into EPSG:6932 then
  wrote the value along 90° N into the 16,432 pixels of its grid's corners, and none into the 63,109
  pixels of the disk; in EPSG:6931 and 3574, the disk was empty or nearly so. (#822)
- `terrio.network_stats()` counts the bodies terrio sends as `sent_bytes`, in total, per host and
  per file: those of uploads (`PUT`), batch deletes and `POST` requests such as STAC searches. It
  reported 0 whatever was sent, so a 65,822-byte upload to S3 showed `"sent_bytes": 0` (#727). In
  Rust, `FetchStats` gains a `sent_bytes` field. Writes and `POST` requests are now counted per host
  and per file too, as reads are, and the CLI's `--stats` prints the bytes sent. (#823)
- The PNG and GeoTIFF writers now honour `ZLEVEL`, the Deflate level, from 1 (the least effort) to 9
  (the most), 6 by default: for PNG image data, and for GeoTIFF and COG chunks compressed with
  `DEFLATE` or `LERC_DEFLATE`. With the `libzstd` feature, `ZSTD_LEVEL`, from 1 to 22 (9 by
  default), sets the level of `ZSTD` and `LERC_ZSTD` chunks; without it, it has no effect, as the
  documentation now says. Every level gave the same bytes before. A level out of range is now
  refused, by these writers and by the netCDF writer, which used level 1 instead. (#827)
- `set_allowed_drivers` now restricts point clouds too. A list set without a kind applies to
  rasters, vectors and point clouds. After `set_allowed_drivers(["GTiff"])`, the point-cloud readers
  refuse LAS, LAZ, COPC, PLY, E57 and EPT files, LAS bytes and the LAZ tiles of an EPT dataset, as
  they do under `TERRIO_ALLOWED_DRIVERS=GTiff`, and `identify` no longer names LAS.
  `kind="pointcloud"` (Rust: `Kind::PointCloud`) sets a list for point clouds alone. Before, the
  process-wide list left point clouds unrestricted and `kind="pointcloud"` raised. An EPT dataset is
  now written under a list without the LAS family; before, `TERRIO_ALLOWED_DRIVERS` refused it.
  (#829)
- `terrio.pyogrio` now reads every layer of a source that has several. `list_layers` lists each
  layer with its geometry type, and `read_dataframe` and `read_info` read the layer that `layer=`
  names, or numbers from 0, with `skip_features`, `max_features`, `bbox`, `columns` and
  `fid_as_index` applied to that layer. Without `layer`, the first layer is read as before, now with
  a warning naming the other layers. Before, `list_layers` gave one row, and any layer but the first
  raised `DataLayerError` ("could not be opened"), so the second table of a GeoPackage, the later
  folders of a KML document and the routes and tracks of a GPX file could not be read. A vector
  MBTiles or PMTiles archive also keeps `skip_features` and `max_features` now; it ignored them.
  (#830)
- `translate` now resamples a `srcwin` or `projwin` at a fractional offset, read at its own size, at
  that offset by its method, whatever reads its source: Rust `ops::translate`, Python
  `terrio.translate` and `terrio translate` alike. A raster in memory, a dataset from `terrio.open`
  and a format read whole, such as an Esri ASCII grid, used to copy such a window one pixel for one
  by every method but `sum`. Its values sat a fraction of a pixel away from the output's
  georeferencing, and `bilinear` and `average` gave `nearest`'s values. With `srcwin=(0.25, 0, 7,
  4)` on a ramp of ten per column, they now write 2.5, 12.5, …, 62.5, as a GeoTIFF read by window
  already did, where they wrote 0, 10, …, 60. (#834)
- Standalone WebP writes now honor `LOSSLESS=NO` and `QUALITY` by encoding lossy VP8, preserving
  alpha exactly. With no lossy options, output remains lossless; explicit `LOSSLESS=YES` overrides
  quality. Unrecognized `LOSSLESS` values are refused, matching the strict GeoTIFF option. The
  standalone default deliberately differs from GeoTIFF WebP tiles, which default to lossy.
  Whitespace around `LOSSLESS` is ignored. Native path and memory writers share the same behavior,
  and GeoTIFF WebP tiles reuse the same lossy encoder and alpha container code. (#836)
- JPEG 2000 writes now honor `QUALITY` and `REVERSIBLE` on native paths, memory files and
  JavaScript/WASM. Quality follows GDAL's percentage-of-uncompressed-size convention, with
  increasing quality layers supported. With no creation options, output retains Terrio's lossless
  default; rate-limited or irreversible output is labelled lossy, including reversible streams whose
  final quality is below 100. (#837)
- Lazy GeoTIFF opens now retain the underlying read or seek failure while reading headers,
  directories and out-of-line metadata. HTTP status errors, disconnected responses, short range
  responses and refused cross-origin redirects keep their cause instead of appearing as `not a TIFF
  file`; translation stops rather than retrying the entire remote file. Full lazy opens refuse
  unreadable trailing directories instead of returning partial metadata, while inventory and file
  transfers use first-image metadata. (#838)
- EPT appends now apply the reader driver allow-list to the existing dataset and its LASzip tiles
  before acquiring a lock or recovering an interrupted update. Under `TERRIO_ALLOWED_DRIVERS=EPT`,
  both `append_points` and `convert_points(..., append=True)` refuse a LAZ-backed target without
  changing its files, instead of decoding its tiles and adding points. Refusals name the required
  LAZ driver. (#839)
- Terrio raster writers resolve driver aliases to a canonical name before choosing metadata, mask,
  buffering and streaming behavior. Case variants and padded names now produce the same TIFF as
  `GTiff` or `COG`, and writer profiles report the canonical spelling. COG writers now report `COG`
  in `driver`, `meta["driver"]` and `profile["driver"]` (previously `GTiff`), so reusing that
  profile retains COG output. Callers branching on those writer fields must accept `COG`; reopening
  the TIFF still reports `GTiff`. (#840)
- Terrio writes complex NumPy rasters and preserves complex TIFF sample types through streamed
  GTiff/COG writes, updates, copies, CLI conversion and translation. Integer-complex copies preserve
  Int32 real and imaginary components exactly, including values above 2²⁴. Both native Raster and
  read-only rio APIs provide precise complex128 output, including big-endian dtypes and explicit
  output arrays. (#841)
- Terrio retains the transparency mask of a TIFF whose first image is reduced-resolution, including
  attached external overview pyramids. Mask-presence checks, standalone reads, resampling and sparse
  prefetch associate masks with each image's own reduced-resolution flag. COG conversion copies
  masks belonging to external overviews instead of regenerating them. (#842)
- Python warnings recorded by Terrio's Rust operations now point to the first Python caller outside
  Terrio, including direct binding calls and calls through pyogrio wrappers. Module-specific warning
  filters and Python's default once-per-location suppression consequently apply to the caller's
  code. (#843)
- CRS parameter exports now preserve polar stereographic standard parallels, Mercator variant B and
  Web Mercator definitions, supported named datums and explicit WKT datum shifts. Boolean flags
  serialize as bare switches and retain normalized boolean values and first-occurrence precedence,
  including a bare duplicate after an explicit false. Python transforms preserve explicit WKT bound
  operations. (#844)
- Inverse projections now refuse non-finite source coordinates before evaluating their formulas. A
  failed transform's infinity marker stays a failure when passed through a polar stereographic
  inverse instead of becoming an apparently valid point at the opposite pole. Finite distant
  coordinates remain valid, including inputs whose intermediate conversion to metres overflows
  before normalization. (#845)
- ZIP archives with duplicate member names now fail as invalid data instead of silently reading a
  later entry over an earlier dataset or workbook part. The policy applies to whole archives,
  `/vsizip/` members, byte buffers and the XLSX vector reader, including duplicate names outside the
  selected dataset. KMZ retains its existing refusal and diagnostic, and the optional GDAL backend
  cannot retry an ambiguous archive. (#846)
- Warped rasters now retain their values and validity across aligned output tiles, VRT read windows
  and memory chunks. Stable sampling is the default for every resampler and both kernel versions,
  including the default approximate transform. Explicit GDAL reproduction mode preserves the
  previous chunk-dependent behavior for reference comparisons. (#847)
- Terrio's pyogrio adapter now reads anonymous vector buffers using the core's format identifiers,
  opens ZIP archives containing a dataset beside ancillary files, and parses datetime fields on
  pandas 2 without changing process-wide warning filters. GeoParquet Arrow dataframe reads also
  retain geometry when GDAL has registered its WKB extension type. (#848)
- JPEG 2000 writes preserve supplied undefined, RGB and alpha roles without reordering stored
  component samples. File and Python/WASM byte exports preserve full affine grids, including rotated
  and south-up transforms. Images with neither a transform nor a CRS omit georeferencing; CRS-only
  images retain their existing unit-pixel grid, as requested in #714. Native PNG, JPEG and WebP
  copies also preserve a source's absent georeferencing. This absence hook is limited to those image
  writers and JP2; GeoTIFF and COG retain their existing CRS-only unit-grid behavior. (#849)
- Date-time fields read through the optional GDAL backend now retain milliseconds and explicit UTC
  zones. Zoned values keep their offsets, UTC is written as `Z`, and unknown/local zones remain
  unqualified instead of being confused with UTC. Path, byte, CLI and Python reads share the
  correction. (#850)
- **Breaking:** Vector byte output now matches its filename: GML returns a standalone XML document
  on native and WebAssembly builds, and native MapInfo output uses explicit `.mif.zip` or `.tab.zip`
  archives. This changes the previous native GML byte contract from an implicit ZIP archive to
  standalone XML; callers that unpacked GML bytes as ZIP must consume the XML document directly.
  Without the XSD, typed byte round trips such as Integer64, dates and booleans are no longer
  guaranteed. ZIP input labeled as a standalone GML, MIF or TAB document raises a filename error
  instead of producing an empty or malformed layer. (#851)
- Compound OGC EPSG URNs retain horizontal and vertical CRS components across Rust, CLI/MCP, Python,
  WebAssembly, GML and CityGML. GML preserves point and line heights when a compound CRS supplies
  the implicit coordinate dimension, while honoring explicit srsDimension overrides. Native GML
  refuses unsupported selected compound references and conflicting geometry CRSs within a layer.
  (#852)
- Shapefile reads now warn with the sidecar path and reason when an existing `.prj` cannot be read
  or decoded as UTF-8. For undecodable text, whole, range, streaming and archive byte reads keep the
  geometries and attributes, leave the CRS absent, and report the problem instead of silently
  treating the sidecar as missing. The WASM memory codec also diagnoses optional projection members
  that fail to read. Python stream opening also forwards native warnings through the existing
  caller-aware warning bridge. (#853)
- KML and KMZ layer names are now made unique across the complete dataset, so folders in the main
  document and linked documents can reuse names without hiding layers or blocking GeoPackage
  conversion. The existing ` (2)`, ` (3)` suffix rule runs once in main-document order followed by
  link order, keeping every layer selectable by name. ASCII case variants also collide, matching
  GeoPackage table naming, while original spelling is preserved. (#854)
- Vector feature ranges now apply once per selected layer across streamed readers, WebAssembly byte
  codecs, STAC vector assets and MVT directories or tiles. Bounded streams stop requesting feature
  batches once their limit is satisfied. JavaScript openVector accepts skipFeatures and maxFeatures,
  and in-memory readers consistently refuse OAPIF service options. (#857)
- Deflate GeoTIFF and COG writes at the default compression level now keep long matches across
  repeating categorical rows, producing compact outputs for classifications and masks. Levels 6–9
  add a bounded run-boundary probe; levels 1–5 retain their existing search and all levels preserve
  decoded samples. (#858)
- GeoTIFF and COG `DEFLATE` chunks of rasters made of uniform blocks are smaller at `ZLEVEL` 1 to 7:
  each search over a chunk's rows also examines the position one row back, after the hash chains and
  the periodic-run probe of #858, so rows that repeat the row above are coded as copies of whole
  rows. Pixel-interleaved RGB blocks, whose 3-byte pixels the probe does not take, were still 2.6
  times their 0.7.0 size (#712): at the default level, the issue's 37 × 53 blocks as RGB now take
  18,911 bytes, against 70,222 before and 27,206 in 0.7.0. Float32 blocks are 40 % smaller, Byte
  blocks, stripes and their COG 12.5 to 15 %, and noise keeps its size. (#861)
- Raster opens can enforce declared pixel-byte and band-count budgets through Rust, CLI/MCP, Python
  and JavaScript/WASM. GeoTIFF raster/window buffers, decoded chunks and streaming writer buffers
  are checked before allocation against usable memory where available. Native eager reads,
  metadata/lazy opens and info report unsupported TIFF sample layouts rather than guessing their
  type. Oversized requests return a resource error; Python exposes `ResourceLimitError` through both
  native and `terrio.rio` APIs. This changes oversized rio-window failures from RasterioIOError to a
  MemoryError subclass, so callers handling resource refusals must catch ResourceLimitError or
  MemoryError. (#862)
- A warp from a geographic source into a grid across the antimeridian now gives an output pixel
  across 180° the source inside it on both sides of the line, for `sum` and the area methods
  (`average`, `rms`, `mode`, `min`, `max`, `med`, `q1`, `q3`). Such a pixel was bounded by the box
  between its corners, which spans the source's whole width: `sum` added the whole band of source
  rows the pixel covers (574.5 in the issue's case, where the source inside the pixel adds up to
  1.04), and the area methods summarized that band on a source of every longitude (89.98 where the
  source holds 179.975 on both sides of 180°) and left the pixel empty on a source that ends at
  180°. (#870)
- `export-schema` reports the Z and M that a FlatGeobuf header declares, so an empty layer declared
  `PointZ` is reported as `PointZ` instead of `Point`. This holds for local files, remote files,
  members of local or remote zip archives, and bytes, including GDAL-written bytes, and for layers
  with no geometry type or CRS. Other drivers and datasets already held in memory keep the feature
  scan. With a driver allow-list, opening a remote zip member whose name points only to disallowed
  drivers is now refused without requesting the archive. For Rust callers, `FlatGeobufInfo` gains
  the public fields `has_z` and `has_m`, so code that builds it with a struct literal must set them.
  (#880)
- Engineering CRS definitions using LOCAL_CS, ENGCRS or ENGINEERINGCRS and vertical definitions
  using VERTICALCRS now parse through the normal WKT reader across Rust, CLI/MCP, Python and
  JavaScript. Vertical CRS identity follows the outer CRS identifier instead of incorrectly treating
  a datum or unit identifier as the coordinate system. (#892)
- On glibc Linux, Shapefile whole-file reads now build DBF record vectors, strings and dates on the
  calling thread, allowing a host to reuse that storage after dropping a dataset. This reduces the
  peak of a subsequent allocation-heavy step on Linux/glibc without changing the host's allocator
  settings. DBF decoding runs serially, which trades read throughput for memory reuse. (#894)
- CRS definition-file arguments now read regular files only, including symlinks to regular files,
  and refuse pipes and devices without reading them. Native MCP tool processes confine datum/geoid
  grids to the host's effective search directories, so a client-supplied CRS cannot name a grid
  through an absolute or dot-relative path. Ordinary direct CLI and library grid-path behavior is
  preserved. (#895)
- GeoPackage and FlatGeobuf readers now reject stored text that is not a CRS definition with a
  warning, matching Shapefile behavior. FlatGeobuf silently discards malformed redundant text when a
  declared EPSG code already supplies the CRS. Python GeoPandas and Arrow exports warn and omit a
  retained but unparseable CRS instead of failing the whole layer; geometries, attributes and the
  original layer definition are preserved. Valid custom, engineering, dynamic and WKT2-only
  definitions remain usable, including dynamic frame reference epochs. Static non-EPSG authority
  strings and stored WKT are normalized to WKT1_GDAL for GeoPandas, matching pyogrio; this can
  change the CRS representation and authority identification. Dynamic and WKT2-only definitions
  retain their original representation. (#901)
- Fix transformed bounds for projected boxes containing an interior pole: geographic output now
  includes the exact pole latitude and the full longitude range. Rust callers, raster information,
  processing operations, and Python's rasterio and xarray interfaces share the same implementation,
  with 21 interior edge points by default and an explicit densification method for callers that need
  another count. The Rust default changes from 21 to 23 samples per edge including endpoints,
  matching Python's existing 21 interior points. (#908)
- Under the chunk-dependent warp policy (`TERRIO_WARP_GRID=GDAL`, `grid_invariant=False`, `terrio
  warp --grid-invariant false`), a warp into a grid that holds a pole now writes the polar cap, and
  a grid whose edge passes near a pole is written up to that edge. Each chunk's source window was
  bounded by points sampled along the chunk's edges, which pass far from a pole inside the chunk and
  can miss the pixels nearest a pole just outside it. So every resampling method but `sum` left the
  cap unwritten: by nearest neighbour, 67,752 of the 136,192 pixels that a 90-30° N source covers in
  NSIDC's 25 km EPSG:3413 grid, and all 55,512 that a 60-90° S source covers in EPSG:3976. In a
  chunk that holds a pole or lies near one, values the policy wrote before can change too, as its
  window now reaches further: near the window's former edge, and, where the chunk holds the pole,
  anywhere in it by bilinear, cubic, cubic spline and Lanczos, which by default take their
  horizontal scale from the window's width. The default policy is unchanged. (#917)
- Python operations that emit warnings now succeed when invoked directly by `atexit` or `_thread`
  callbacks without a Python caller frame. Their warnings use `sys:1`, while ordinary calls retain
  caller attribution and module-specific filtering. (#918)
- Python Arrow exports of `terrio.open_stream()` now share layer exports' optional CRS validation.
  With working pyproj, an unusable retained definition warns once and is omitted from GeoArrow
  metadata, so GeoPandas can read the geometries and attributes. Valid definitions and usable stored
  WKT fallbacks remain available, while the original layer and stream CRS are preserved. (#919)
- `terrio.pyogrio.read_dataframe` now consistently treats naive timestamps as UTC when a field mixes
  naive and timezone-aware values and uses the default UTC conversion. On pandas 2, a naive
  `2024-02-01T12:00:00` following a `+01:00` timestamp previously became `11:00 UTC`; it now remains
  `12:00 UTC` in either row order. (#920)
- Writing a layer to a `.tsv` filename now defaults to tabs across whole-file writes, streamed
  conversions and native/WebAssembly byte exports. These files read back with their original fields
  and geometries instead of one combined text column. Explicit CSV separator options still take
  precedence, and `.csv` keeps its comma default. (#922)
- GeoJSON exports now name retained ESRI and IAU_2015 CRSs alongside EPSG CRSs, keeping coordinates
  in their original CRS. A retained definition that cannot be named, such as an unnamed custom
  projection or engineering grid, is refused with guidance to reproject to WGS 84 or use a format
  that stores the definition. This prevents unchanged projected coordinates from silently reading
  back as WGS 84. (#923)
- GeoTIFF and COG now preserve full engineering CRS definitions through Rust, Python and WebAssembly
  writes and reads, including LOCAL_CS, ENGCRS and ENGINEERINGCRS definitions with EPSG:5800
  identifiers. Local grid coordinates remain local, and attempts to warp them to an Earth CRS fail
  with a CRS error. Readers also retain supplied PE/sidecar WKT ahead of a registered EPSG summary,
  preserving the complete definition for existing files as well as engineering grids. (#924)
- Standalone WebP writes now reject malformed QUALITY values, including empty strings, arbitrary
  words and numbers with trailing text, instead of silently encoding at quality zero. Complete
  numeric values from 0 to 100, decimal values, surrounding whitespace and the historical NaN
  default of 75 keep working. (#925)
- MIF reads now reject wrong-format content and missing Version, Columns or Data declarations with
  an InvalidData error that identifies MIF. Valid headers with no objects still open as empty layers
  with their declared fields, including zero-column headers. The fix applies to standalone paths,
  byte inputs, ZIP members and WebAssembly. (#926)
- Rio coordinate and geometry transforms now process mixed-validity coordinates in one native batch,
  retaining valid results and setting failed points to infinity. A single missing or invalid
  coordinate no longer sends the entire batch through one native call per point. (#927)
- The file browser of `terrio ui` can now reach every drive on Windows: **Up** from the root of a
  drive (`C:`) lists the drives, and choosing one opens its root. Before, **Up** was disabled at a
  drive's root, so a folder on `D:` could only be entered by typing its path. (#928)
- Pyogrio now reads KMZ, XLSX, newline-delimited GeoJSON and DXF buffers through the same format
  readers as named files. KMZ buffer layer listings retain linked layers, and named file objects
  retain their filename extensions, allowing CSV streams to open without guessing that arbitrary
  unsigned bytes are CSV. (#929)
- terrio.rio now returns TIFF BLOCK_OFFSET_x_y and BLOCK_SIZE_x_y metadata instead of None for
  GeoTIFF tiles and strips. Queries support base images, internal and external overviews, planar or
  pixel interleaving, complex bands and MemoryFile sources. Missing items return None, stored zero
  offsets/counts return '0', and real metadata or I/O errors propagate. (#930)
- Global geographic rasters now sample every longitude regardless of whether their stored grid
  starts at 0° or 180° W. Point kernels fill the +180° meridian, interpolation uses neighbors across
  the stored cut, and area methods include both sides of a footprint crossing that cut. A pixel
  covering 0.1° west and 0.4° east of zero now gives the expected sum of 1.8 or average of 3.6 for
  the reported field. (#933)
- Configuration options set in an inner `terrio.rio.Env` now override outer scopes and process
  environment values even when the settings use different aliases. For example,
  `Env(GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR")` now suppresses TIFF sidecars when the environment
  sets `TERRIO_DISABLE_READDIR_ON_OPEN=NO`. Scoped HTTP user-agent and retry aliases likewise
  control the actual requests. (#934)
- RegionWarp now returns typed empty results for zero-width or zero-height destinations instead of
  panicking with 'chunk size must be non-zero'. For Rust and Python, source_window now returns an
  empty window for every zero-sized region instead of planning a transform or returning None, and
  the private Python binding preserves the configured band count in empty output arrays. (#935)
- Shapefiles with dotted basenames now read and write their own SHP/SHX/DBF/PRJ/CPG components.
  Writing roads.v2.shp preserves neighboring roads.* files, and reading roads.v5.shp uses its own
  attributes, encoding and CRS. Explicit Shapefile output also preserves unknown dotted suffixes:
  roads.v2 becomes roads.v2.shp. Requested .SHP extension case is retained on case-sensitive
  filesystems. Converting to an alias of the source reads the input whole before writing, preserving
  its records. (#949)
- CSV and TSV reads now retain every repeated column under a distinct case-sensitive name instead of
  shifting values into another field. Self-conversion and append preserve original attributes and
  geometry when generated geometry columns collide with attribute names. (#951)
- JPEG 2000, Zarr and GeoPackage rasters now retain a stored CRS definition when its EPSG identifier
  is absent from Terrio's registry. Engineering CRS names, axes and units survive eager and lazy
  reads, and those local definitions continue to refuse reprojection to Earth CRSs. Canonical
  registered EPSG round trips and identifier fallback remain supported. When a file holds a
  different definition alongside a registered identifier, that stored definition is authoritative
  too. (#954)
- Standalone JPEG and MBTiles/PMTiles QUALITY options now require a complete finite number from 1 to
  100. Scientific notation is interpreted completely, so 1e2 means 100 and 7.5e1 means 75. Malformed
  text, NaN, infinities and out-of-range values are refused before replacing an existing output,
  instead of accepting a numeric prefix or silently clamping to another quality. (#955)
- Successful Python reads that record native warnings now retain their result during interpreter
  shutdown, even after Python disables imports. Late __del__ callbacks with default or ignored
  warnings no longer lose an opened dataset to ImportError from importing the warning helper. (#956)
- Polar output pixels with a corner exactly on the 180-degree meridian now use their local
  geographic footprint. Bilinear and cubic no longer average a whole source parallel, and sum no
  longer counts a world-wide strip for those pixels. UPS pixels whose corner is both the pole and
  the meridian retain the same sum as their mirror across the meridian. (#959)
- Missing local inputs to translate now report the input path followed by 'does not exist'.
  Unrecognized-format errors name the input once and retain the optional vector-reader hint. (#960)
- Two WKT 1 forms terrio cannot identify by code are read as PROJ reads them: a
  `PRIMEM["Paris",2.33722917]` in a CRS whose unit is grads is in degrees (it was read as grads,
  which put longitudes about 0.23° off), and ESRI's `Behrmann` projection, as in the EASE-Grid 2.0
  `.prj`, is cylindrical equal area, where it was refused. (#961)
- Chunk-dependent warps now retain covered pixels beside either pole when adjacent source-window
  samples cross the longitude cut. North-up and sheared polar grids no longer omit the meridian side
  of a full-turn geographic source merely because the sampled min/max bounds stop short of its
  stored edges. (#962)
- A window with zero width and a large height panicked with `capacity overflow` in the raw-raster
  reader. The row list was sized by `height * bands`, and the earlier size check multiplies width,
  height and bands, so it passes when the width is 0 whatever the height is. The raster was read
  through the shared raw path (`drivers/common.rs`). (#963)
- PNG, JPEG and GeoTIFF rasters now open by their signatures when their names supply no raster
  driver, through both whole-raster and rio entry points, locally and over HTTP. Unknown local files
  now report an unrecognized format without a generic fallback hint. This also removes that hint for
  formats the optional fallback could identify; the fallback option remains available explicitly.
  (#964)
- GeoParquet Arrow stream exports and terrio.pyogrio Arrow dataframe reads now warn and omit CRS
  declarations that the optional Python consumer parser rejects, preserving geometries, attributes
  and stored definitions. Valid declarations retain their original GeoArrow metadata, and
  environments without pyproj retain the stored metadata. (#965)
- Pyogrio reads named MBTiles and geometry-sequence buffers through their intended formats and
  retains their geometry types. Archives without dataset layers report a refusal naming the input.
  Anonymous SQLite buffers without a GeoPackage application id now require a .gpkg or .mbtiles
  filename, including previously accepted non-conformant GeoPackages. (#966)
- TopoJSON writers now name EPSG, ESRI and IAU_2015 coordinate systems using the GeoJSON authority
  rules and refuse retained definitions they cannot name. GeoJSON and TopoJSON readers reject
  unresolvable explicit CRS members, preserve explicit null as unknown, and no longer replace a
  TopoJSON reader's unknown CRS with WGS 84. (#967)
- GML exports now name EPSG, ESRI and IAU_2015 coordinate systems and refuse retained definitions
  they cannot name. CSV geometry exports warn when a known CRS will be omitted across Python, CLI
  and JavaScript writer entry points. (#968)
- Regional geographic rasters now contribute at equivalent longitude numberings during warps,
  including sources stored past 180 degrees east and adjacent regional tiles meeting at the
  meridian. Rounded near-global grids use their actual angular period, and true uncovered gaps
  remain nodata. (#972)
- CSV and TSV exports preserve their actual geometry when attributes use coordinate names such as
  lon/lat, x/y or easting/northing. Explicit GEOM_POSSIBLE_NAMES selections prefer valid WKT
  geometry and retain XY fallback for non-WKT columns. (#975)
- KML, GPX, OSM and GeoJSON sequence readers now refuse documents of the wrong kind instead of
  returning misleading empty layers. HTML error pages, cross-format XML, missing OSM headers and
  invalid sequence records produce document-specific errors across path, memory and streaming opens.
  (#976)
- Replacing a Shapefile removes superseded lowercase, uppercase and mixed-case component spellings,
  preventing an old PRJ file from assigning its CRS to new geometry. Filesystem readers also
  discover mixed-case DBF, PRJ and CPG sidecars. (#977)
- MapInfo MIF/MID, Shapefile DBF, FlatGeobuf, Parquet and Arrow imports retain every declared column
  when names repeat, preventing later values from shifting into the wrong fields or disappearing.
  Repeated names receive distinct case-sensitive suffixes while preserving original names, field
  order and metadata. (#978)
- Lossy WebP files and WebP-compressed GeoTIFF tiles now signal every skipped macroblock whose
  coefficient tokens are omitted. This prevents the decoder from losing token alignment and damaging
  the following blocks, including GeoTIFF's default lossy level of 75. Requested quality and exact
  alpha handling are preserved. (#979)
- Warping an unrotated geographic raster wider than one longitude turn now includes the uncovered
  portion of a second footprint placement for area resampling and exact `sum`, without counting
  duplicated longitudes twice. The 361-column example in #884 now returns 20 instead of 15. (#980)
- terrio-crs no longer enables serde_json's `preserve_order` feature. Cargo enables a dependency's
  features for the whole build, so every crate built with terrio-crs got insertion-ordered JSON
  objects, which changed the key order of their own JSON output (RustGIS's project files and command
  results, for example). `SpatialRef::to_projjson()` now writes PROJJSON's keys in PROJ's order
  itself, and its text is unchanged. (#994)
- Dropping a `Transformer` while its thread's locals are destroyed no longer aborts the process.
  This happens when a transformer is kept in a `thread_local!` cache and its thread exits. The drop
  reports the transformer's pending warnings, and terrio-crs's and terrio's warning lists are
  thread-locals that may already be gone by then. Reaching one with `LocalKey::with` panicked
  ("cannot access a Thread Local Storage value during or after destruction"), and a panic in a
  thread-local destructor aborts. Such a warning is now dropped, since nobody is left to take it.
  (#995)
- A latitude beyond a pole is no longer folded back to a valid place by a datum shift.
  `+proj=longlat +datum=OSGB36` to EPSG:4326 turned (10, 95) into (-169.98, 85.00), and a projected
  coordinate given as degrees, (451219.33, 5411550.2), into (139.33, 30.21). terrio now gives PROJ
  9.8.1's results: the point unchanged where PROJ falls back to its ballpark, and a refusal where a
  `+towgs84` shift is the only operation. (#996)
- Terrio now rejects unreadable PROJ-string values for parameters consumed by a CRS method, rather
  than silently substituting defaults. Angular values with an `r` suffix are read in radians,
  recognized linear and vertical units take precedence over explicit conversion factors, and
  nonpositive unit conversions and selected scale factors are rejected. In literal PROJ strings,
  unreadable `towgs84` lists, including a trailing comma, and lists with counts other than three or
  seven are rejected. (#997)
- `vsi sozip validate` and other diagnostics again redact credential-bearing query strings (`?sig=`,
  `token=`, `key=`, …) in locations without a URL scheme, such as `/vsiaz/container/blob?sig=…`;
  URLs keep having their whole query redacted. `CatalogInput::bytes` holding a VRT documents that it
  resolves no references outside a source map. (#998, #999)
- Diagnostics now redact a credential parameter that follows a URL-valued parameter in a schemeless
  location (`/vsiaz/c/b.tif?next=https://h/a&sig=…`), and recognize `api-key`, `x-api-key`,
  `apiKey`, `authorization`, `code` and `session` parameters; a Windows verbatim path (`\\?\C:\…`)
  is no longer read as having a query. A catalog source supplied as bytes that holds a VRT behind an
  XML declaration, under a name without `.vrt`, now resolves references only from the source map
  instead of reading host files. (#1000)
- Catalog VRT bytes are identified by content regardless of their supplied name, including XML
  declarations and `GDAL:`/`NETCDF:` prefixes. Reads and overwrite inventory both use the closed
  source-map opener, so unmapped references are refused before reading host files or opening
  destinations. Direct paths and already-open native handles retain filesystem lookup. (#1002,
  #1005)
- CRSs read from PROJJSON now report the EPSG or ESRI identity identified from their complete
  definition, matching the WKT reader. Projected geographic bases accept an omitted `type`, as
  allowed by PROJJSON v0.7. Python round trips of EPSG:4326 and EPSG:32633 retain their authority,
  string form and equality. Identified definitions now use registry datum operations and installed
  grids, as WKT does, so coordinates can change from the previous WKT-only PROJJSON path. (#1003)
- GeoParquet Arrow exports now preserve every repeated column's own values, types and metadata.
  Stream exports, `terrio.pyogrio.read_arrow` and `read_dataframe(use_arrow=True)` assign distinct
  top-level names before projection, so a file declaring `count, count, total` yields `count,
  count_2, total` with the original positional values, including when selecting `count_2` alone or
  filtering by bbox. (#1004)
- Archive refusals now name the caller's ZIP or explicit /vsizip/.../member instead of temporary
  extraction and buffer-storage paths. Pyogrio dataframe reads, metadata reads and layer listings
  consistently report no dataset found for unsupported archives, naming the original archive or
  buffer. Malformed datasets retain their parsing reason. Native diagnostics redact remote URL
  userinfo and hide extracted sidecar directories, and converted Python errors suppress private
  paths in displayed exception chains. Catalog-supplied VRT bytes remain confined to their source
  map even when long XML prologs place the root beyond the identification header. (#1006)
- GeoParquet metadata and dataframe reads retain their validated CRS after a registered Arrow
  geometry type consumes the standard extension metadata. In particular, loading GDAL's
  `geoarrow.wkb` type no longer changes a valid EPSG:4326 dataframe to `crs=None`. (#1007)
- GeoJSON and TopoJSON now refuse writes whose retained authority name Terrio cannot resolve,
  preventing output that its own readers reject. GML warns when writing an unresolved short
  authority name and when reading an unresolved simple `srsName`, while preserving features and
  coordinate values. (#1009)
- Failed native remote Zarr reads now cancel sibling HTTP response waits and retry delays, so a
  failed chunk can return its error while another server response is stalled. Nested shard reads
  inherit cancellation, and subsequent reads on the same dataset can succeed after the server
  recovers. (#1013)
- Native parallel range workers are now cancelled and joined before any fork copies the process. A
  fork from any thread can therefore give concurrent readers on other threads an I/O cancellation
  error; callers may retry after the fork. DNS and connection setup cannot be interrupted: a fork
  can wait for DNS completion (no Terrio-configured timeout) or the default 30-second connection
  timeout. (#1014)
- A projected CRS given by WKT now has the axes of its `AXIS` nodes, as GDAL 3.10 and PROJ 9.8.1
  read them in GIS order. WKT 1 ignored them: "Hartebeesthoek94 / Lo19" (Transverse Mercator,
  `AXIS["Y",WEST],AXIS["X",SOUTH]`) put (46246.675, 3752681.8425) at 19.5°E 33.9°N instead of 18.5°E
  33.9°S, 7,540 km away. WKT 2 always put the east-west axis first, so a south-west coordinate
  system had its coordinates swapped. WKT 2 Krovak (EPSG 9819) is now read too. Krovak WKT 1 without
  AXIS or Esri XScale/XYPlaneRotation parameters now defaults to south-west rather than east-north
  coordinates. PROJ-string export follows PROJ 9.8.1 even when coordinate-system axes are lost;
  retain WKT when those axes must round-trip. (#1015)
- Area resampling from unrotated geographic grids now uses local polygonal source footprints,
  keeping pole-corner pixels in the correct longitude sector and fixing the reported average/min/max
  mirror asymmetry. Average, min, max, mode, RMS and quantiles share the corrected geometry under
  both grid policies. Floating mode now lets a newly encountered value with greater overlap become
  the leader while preserving equal-weight source-order ties. (#1016)

## 0.8.0 (2026-11-08)

- `terrio.rio`'s `subdatasets`, and so `terrio.xarray`'s open, list a file's subdatasets from the
  raster already open instead of opening the file again. A remote store's metadata was fetched
  twice: opening a UTM zone of the OlmoEarth v1.3 embeddings Zarr store on source.coop with
  `xr.open_dataarray(..., engine="terrio")` took 5.2 to 6.0 s in 6 requests, and now takes
  3.2 s in 3. GeoPackage raster tables are still listed by `info`.
- `terrio translate` reads the bands of a window it copies pixel for pixel together, whichever
  bands they are. It used to read bands that are not the source's first ones in order one by one,
  as `gdal_translate`'s virtual dataset does, and without GDAL's block cache a Zarr chunk holding
  many bands was then fetched and decoded once per band. One year (128 bands) of a 192 x 192
  window of the TESSERA embeddings store on source.coop, its chunks holding 128 bands, read from
  Europe: a band alone took 4.8 to 7 s and about 4.4 MB, and the 128 bands were stopped
  unfinished after 2 minutes (about 560 MB at that rate). The 128 bands and their per-pixel scales
  now take a median 6.2 s (4.4 to 8.1 s, 5 runs), 6.5 MB in 21 requests, opening included.
- Opening a remote Zarr store requests its v2 consolidated metadata (`.zmetadata`) and its root
  `zarr.json` at once, and, in a Zarr v3 store, no longer looks for v2 documents (`.zarray`,
  `.zattrs`, `.zgroup`). The coordinate arrays that describe the band dimensions are read in
  parallel. Opening that TESSERA array went from 13 requests in a row to 4 round trips.
- A sharded Zarr array keeps the indexes of the shards it has read while it is open (up to 64 MB
  of entries), as zarrita does: a later window in the same shard fetches only its inner
  chunks, where it fetched the shard's index again first (256 KB and a round trip for a TESSERA
  shard of 128 x 128 inner chunks).
- A window of a sharded remote Zarr array coalesces nearby inner chunks with a
  1 MiB target, in parallel (a larger individual chunk is fetched whole), previously 16 MiB:
  a connection far from the server delivers
  a large range slowly; local stores keep reads of up to 16 MiB. Reading 34.5 MB of the TESSERA
  embeddings from Europe took a median 6.3 s instead of 10.0 s (8 runs each), and the 192 x 192
  window above 5.2 s instead of 6.4 s.
- A `sum` warp transforms the corners of each output pixel exactly, whatever the error threshold
  (`--et`, `et`, or `tolerance` in `terrio.rio`), which still bounds how the source window a
  chunk reads is found. The corners outline each source pixel's share of an output pixel.
  Interpolated within the threshold along runs of 512 corners, as before, they were off by up to
  0.09 source pixels on a 256 x 256 tile reprojected from UTM to Web Mercator at the source's
  resolution, and the tile's total was 5.2e-5 off the integral of the source over the tile;
  it is now within 1e-9 relative error. Source windows include every exact corner, including
  extrema between edge samples. Exact corners and bounds require additional transformation work;
  see `docs/testing.md` for measured costs and the intentional difference from GDAL's sum kernel.
  The other resamplings are unchanged.

- Python raster creation-option mappings reject conflicting names that differ only by case,
  including `Raster.write`, grouped netCDF output and streaming GeoTIFF/COG writers.

- A `WarpedVRT` read holds each source window it warps once: the warp reads the window where it
  is, with the GIL held, instead of copying it first (`RegionWarp::warp` takes the bands borrowed,
  a `RasterDataRef`, which a `&RasterData` converts to). The copy doubled what a read held: a
  256 x 256 `nearest` tile of a 16384 x 16384 UInt16 raster without overviews, whose blocks each
  warp half the raster, peaked at 702 MiB and now peaks at 456 MiB (2,237 and 1,469 MiB read as
  Float64, whose conversion still holds the window in both types). The pixels are the same. The
  window a read warps from is read-only; arrays given to the private `RegionWarp.warp` binding
  are read in place when C-contiguous and read-only, and copied first otherwise.

- HDF5 raster reads check declared raw and expanded pixel sizes before decoding whole datasets,
  refusing buffers beyond the address space or known usable-memory limit with a terminal resource
  error. Coexisting pixel buffers are checked together; hosts without memory discovery use a
  512 MiB combined pixel-buffer budget. This intentionally refuses previously readable
  datasets exceeding that budget on macOS, Windows and WASM; the refusal remains terminal
  even with GDAL enabled, so a fallback cannot bypass the allocation limit. HDF-EOS5 structure text is limited to 16 MiB before
  decoding, including fill-only files.
  This metadata cap applies to every HDF5 open, even when selecting an unrelated subdataset.

- Python and JavaScript `translate` read paths, lazy rasters and byte inputs by window and take an
  `overview` option (`AUTO` by default, `AUTO-n`, `NONE` or a level), as `terrio translate` and
  `gdal_translate -ovr` do. Resized outputs of rasters with overviews can therefore change: they
  are read from the overview GDAL picks for the output size, not full resolution; pass
  `overview="NONE"` for the previous result. Python `rasterize` and `vector_grid` take
  `tile_size` and `bigtiff`.

- `translate --overview AUTO-n` without `--outsize` or `--tr` keeps the full-resolution
  output size while reading the selected overview, as `gdal_translate` does. A 400 x 300
  source window now stays 400 x 300 instead of shrinking to the overview's resolution.
  `AUTO-0` is now equivalent to `AUTO` across all resampled reads, including Python and
  JavaScript readers, VRTs and translate, and permits the same whole-read fallbacks.

- `warp -r sum` without `--te` rounds the suggested output extent's size up, as gdalwarp does
  for a sum (`GDAL_SWO_ROUND_UP_SIZE`), so that the output covers the whole source. Without
  `--ts` or `--tr` this sets the output size; with `--ts` alone, the pixel size spans the
  rounded-up extent; with `--tr` alone, the extent the resolution divides. Rounded to the
  nearest, a 41 x 37 UTM raster reprojected to EPSG:4326 lost its last row and 0.4 % of its
  total. Sizes and geotransforms match GDAL 3.13.3 in all three cases; per-pixel values of a
  reprojected sum still differ from GDAL's.

- GML and WFS normalize northing-first projected authority identifiers to GIS XY, including
  EPSG:6708. Standalone layers with conflicting coordinate-axis declarations are now refused.
  Older Terrio GML files that wrote XY under northing-first EPSG URNs need their `srsName`
  changed to a short `EPSG:` name before reading. Unknown EPSG codes are written with short
  names and XY coordinates, preserving coordinates without reconstructing a custom CRS (#681).

- `ui` and `view` open the browser through a private HTML redirect file, keeping the session
  token out of process arguments. If a sandboxed browser cannot read the file, paste the
  printed address manually or use `--no-open`.

- Bilinear, cubic, cubic spline and Lanczos reads of VRT and mosaic sources with nodata
  average each source row over its valid pixels, then the rows over the valid ones, as GDAL's
  `RasterIOResampled` does, rather than all valid weights at once. For 8- and 16-bit and
  Float32 sources, with or without nodata, these reads also round the result to `float`, GDAL's
  work type. Next to a nodata pixel, a bilinear value could differ from GDAL's by 20. Reads of
  GDAL-built VRTs now agree with GDAL 3.13.3 on the conformance DEM within an absolute 1e-5
  (the registered tolerance; the runs were exact). Known differences remain: a valid 8-bit
  cubic or Lanczos result clamped onto the nodata value is not moved off it as GDAL does, and a
  cubic or Lanczos mosaic written directly to a file at 2.8x upsampling differs from GDAL's VRT
  read next to some isolated nodata pixels (under 0.1 % of them).

- Rasterize initializes uncovered pixels to the specified nodata value unless explicit
  initialization values are given. This follows GDAL's GeoTIFF behavior for all Terrio
  output formats and in-memory rasters, so uncovered pixels remain invalid by default.
  This changes existing `--add --a-nodata` calls that omitted `--init`. Integer nodata values
  must be representable in the output type; finite Float32 nodata must be within its range.
  Additive burns start
  from the background (nodata -1 plus burn 7 yields 6); use explicit zero initialization
  to accumulate from zero.
  The CLI catalog is now version 0.1.2 to describe this initialization default accurately.

- The `--json-usage` catalog (`schema_version` 0.1.1) records whether each argument is
  `repeatable` (clap Append/Count) and lists its `conflicts_with` arguments in both directions,
  including members of exclusive argument groups. A CI step requires a newer `schema_version`
  whenever the catalog digest changes relative to the pull request's base.

- JavaScript's `openUrl` reads remote Zarr stores (v2 and v3, sharded or not) by window, as
  `terrio` reads them natively: `openUrl(store, { format: "Zarr", array: "embeddings" })` (or a
  URL ending in `.zarr`) opens the array as a raster, fetching its metadata documents, and a
  window fetches the shard indexes and chunks it needs. `LazyRaster::from_zarr_objects` and
  `terrio::sparse::SparseObjects` read a store whose objects the caller fetches, as
  `LazyRaster::from_sparse` and `SparseFile` read one file. 128 bands of a 192 x 192 window of the
  TESSERA embeddings read in a median 3.9 s in Chrome from Europe (3 runs, 19 requests, 5 MB),
  where the TESSERA Zarr Explorer (zarrita) took 9.0 s.

- Python raster operations accept format-specific `options` dictionaries when writing a
  destination, as `Raster.write` and CLI `--co` do. Creation options also reach streamed
  terrain/warp/pansharpen outputs, mask-writing paths, palette operations and `create_raster`.
  In `calc`, `options` is now the output-options keyword; rename any raster input called `options`.
  Virtual VRT outputs reject creation-option mappings in warp, mosaic, color-relief and
  pansharpen; direct XML outputs no longer silently discard them.

- Calc disambiguates `-f` between the output format and a referenced `f` input, and preserves
  literal arguments and fixed-arity option values. Translate handles zero- or two-value
  `--scale` options before input/output paths without consuming the paths as scale values.

- Translate's lazy-open and whole-read fallback policy now lives in the Rust core and is shared
  by the CLI, Python path inputs and JavaScript byte inputs. Terminal opening errors and
  overview refusals remain consistent; lazy destinations retain streaming writes.
  Overview refusal diagnostics now use shared wording and quote the input name across the
  CLI and bindings; callers should not match the previous surface-specific message text.
  Whole-read fallback for bytes remains TIFF-only: GDAL-only non-TIFF files may translate
  by path but fail from bytes.

- `rio stack --co KEY=VALUE` now applies creation options to the output instead of silently
  discarding them.

- Remote STAC, STACIT and STACTA documents require
  `TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS=YES` to select assets using configured cloud credentials
  or cross-origin custom HTTP headers. Checks include declared assets during metadata reads and
  concrete deferred tile opens, nested native sources and cached readers after settings change.
  Export and overview reads enforce the same policy, including parallel tile workers. Asset
  checks use the final response origin after redirects. Actual clients check resolved credentials
  before requests; cached parts retain consistent provenance. Layer listing rejects raster pointers
  from metadata, without reading their pixels or traversing catalog children.
  Native public unsigned cloud reads remain supported, including S3 with
  `AWS_NO_SIGN_REQUEST=YES`. GDAL asset reads selected by remote documents always require the
  opt-in because GDAL can follow nested sources and use additional credentials. Named STAC,
  STACIT and STACTA documents, local or remote, are not retried through GDAL after any native error.
  STACTA also preserves invalid-argument and resource refusals instead of treating them as
  missing tiles or metadata-only container fallbacks, including for local items. Remote STAC
  errors that could otherwise trigger pointer fallback become `InvalidArgument` with the redacted
  native cause. Malformed remote JSON containing a STAC marker is conservatively terminal.

- Public `build_vrt` and same-CRS mosaic entry points reject unselected mixed band counts and
  target extents intersecting no source. These strict checks deliberately differ from GDAL
  `gdalbuildvrt`; calc retains zero/nodata-filled alignment outside its input extents.

- Datum-grid hierarchy uses the first duplicate parent name and nests unparented subgrids under
  the first containing extent. Existing ambiguous grid files can select a different datum-shift
  grid. Lazy TIFF reads require malformed IFD chains to be repaired or rewritten before opening.

- `terrio.rio.coords.disjoint_bounds` rejects opposite nondegenerate orientations in either
  argument order; zero-height bounds remain boundary lines. Calc window alignment retains
  blank nodata/zero-filled inputs outside the window rather than rejecting the calculation.

- JavaScript `listUrl` rejects invalid `depth` values, including strings and `Infinity`;
  omit `depth` or pass `null` for unlimited traversal. Native lazy TIFF opens now reject unreadable or
  malformed directory/tag tables discovered during mask scanning, including files without masks,
  instead of silently treating them as maskless.
  Only tags relevant to image layout and masks are decoded; unrelated private tags are skipped.

- Whole-file Shapefile reads, streaming reads and conversions omit deleted DBF records,
  preserving the physical feature IDs of live records.

- Shapefile `skip_features` and `max_features` count live rows: deleted DBF records are passed
  over and count toward neither bound, while FIDs still number physical slots. pyogrio's
  `skip_features` (and GDAL's fast seek) can count deleted slots instead, so `terrio.pyogrio`
  pages a Shapefile with deleted rows differently from pyogrio. Files without deleted rows
  page the same in both.

- Native FlatGeobuf readers reject incomplete records, mismatched nonzero feature counts and
  invalid coordinate dimensions/part bounds. GDAL-enabled eager reads retain GDAL fallback
  admission behavior; native range and stream checks apply in either build.

- Python's `Raster.read_resampled`, `read_resampled_masks` and `read_resampled_with_masks`, and
  JavaScript's `readResampled`, `readResampledMasks` and `readResampledWithMasks` (on `Dataset`
  and, as promises, on `openUrl`'s `RemoteDataset`), read a window of some bands into an output
  of another size as GDAL's `RasterIO` reads it, through
  `LazyRaster::read_resampled`: from the overview it chooses or the level `overview` names
  (`AUTO`, `AUTO-n`, `NONE` or a number, as `gdal_translate -ovr`), with fractional windows, so a
  remote COG read into a small output fetches only an overview's tiles (#429).
- `terrio.rio` reads `out_shape` reads of GeoTIFFs and other rasters read by window through that
  core read (nearest, bilinear, cubic, cubic spline, Lanczos, average, RMS and mode; Gaussian,
  paletted and complex bands, datasets opened with `overview_level` and written datasets keep
  their own path), and resamples a per-dataset mask from the mask's own overviews (#429).
- Changed default: resampled reads (`LazyRaster::read_resampled`, `terrio translate --outsize`
  and `--tr`) with the convolution kernels (bilinear, cubic, cubic spline and Lanczos) sum the NaN
  values of masked floating-point chunks, as GDAL 3.12 does, so an output pixel whose footprint
  holds one is NaN. Average, RMS, Gaussian, mode and nearest reads are unaffected. In 0.7.0
  the convolution kernels always skipped them, as GDAL 3.13 does. `TERRIO_GDAL_COMPAT=3.13` restores that, as for
  the other `RasterIO` kernels.
- The registry CRSs terrio resolves can be listed, for searchable CRS pickers, without GDAL,
  network access or a PROJ installation (#613): `terrio::crs::list_crs` (`terrio_crs::list_crs`),
  `terrio.list_crs` in Python, `listCrs` in JavaScript and `terrio list-crs` (also an MCP tool),
  printing as `projinfo --list-crs` does. Each entry has its authority and code, name, kind,
  deprecation, area of use and its bounding box, projection method and celestial body, as GDAL's
  `OSRGetCRSInfoListFromDatabase` gives them; queries filter by authority, kind, deprecation and
  words of the name or code (`Albers` and `3577` both find EPSG:3577). The list holds 7,681 EPSG
  CRSs (EPSG v12.029, `terrio::crs::EPSG_VERSION`), 2,519 ESRI CRSs and 2,201 IAU_2015 CRSs from
  PROJ 9.8.1's database (`scripts/gen_crs_catalog.py`, 160 KB compressed), each one a code
  `SpatialRef::parse` resolves; the conformance suite `crs-list` finds every entry identical to
  GDAL 3.13.2's.
- A netCDF snapshot (`terrio mdconvert`, `Dataset.write`) writes a netCDF-4 compound attribute
  as a compound, as GDAL 3.13.3's `gdalmdimtranslate -of netCDF` does (#565): `station_t
  z:station = {7, 12.5}` stays `station_t z:station = {7, 12.5}` where it became the text
  `{"id":7,"elevation":12.5}`. The type is committed under its source name, nested compound
  types too, with enum fields as their base integer type and char fields as unsigned bytes; an
  enum attribute is written as its base type (`z:flag = 1b`, where it was a 64-bit integer).
  `ncdump -h` prints the same types and attributes as for GDAL's copy. Zarr object attributes are
  still written as JSON text.
- A netCDF snapshot now refuses an attribute whose message exceeds the 64 KiB an HDF5 object
  header message can hold, such as a long `history` text, with an error naming it. The writer
  used to truncate the message's 16-bit size and write a file that readers reject. A link name
  over 255 bytes is now written with a two-byte length instead of a truncated one.
- KMZ archives are read within limits (#579). Their main document and the layer documents it
  links to may take 512 MiB of decompressed text together (`TERRIO_KMZ_MAX_SIZE` changes it),
  and the archive is checked against `TERRIO_ZIP_MAX_ENTRIES`, `TERRIO_ZIP_MAX_RATIO` and
  `TERRIO_ZIP_MAX_SIZE` as `/vsizip/` checks it, so a 1 MiB KMZ whose `doc.kml` inflates to 1 GiB
  is refused at once instead of taking 2 GiB of memory. A KMZ file is read from its directory and
  the members needed rather than loaded whole. The same archive limits apply to zip archives
  opened from memory and to the `TERRIO_WBVECTOR_FALLBACK` KMZ read. A style document that breaks
  these limits now refuses the whole archive; a style document that cannot be read, or that takes
  the style budget past its size, still leaves its links as URLs.
- A KMZ document that `doc.kml` links to several times is read, counted toward
  `TERRIO_KMZ_MAX_SIZE` and parsed once, and emits its existing layers once. Document and
  folder names are kept; the first link supplies the default name for an unnamed root. A
  link to `doc.kml` itself adds no layer. Each link used to read the document again and add a
  layer with the same name: 100,000 links to one document, in a 25 KB archive, gave 100,000
  identical layers and took 702 MiB. Linked documents are also found without scanning the
  archive's names for each link, a scan that made 99,999 links to distinct documents take 25 s.
- A remote read whose tiles span more than half the HTTP block cache's blocks
  (`TERRIO_HTTP_CACHE_SIZE`, 64 MiB by default) fetches them in batches of at most half the cache
  as it decodes them: each byte once when the tiles are decoded in file order, and at most one
  batch again otherwise. Before, it fetched them all in one request, evicted most of them before
  decoding, and fetched them again a block per request. `terrio convert` of a remote 8192 x 8192
  4-band COG (425.8 MB) with 50 ms of latency per request: 210.7 s, 4,099 requests and 851.7 MB ->
  3.1 s, 14 requests and 425.8 MB, pixels identical. Reads whose tiles span at most half the
  cache's blocks make the same requests as before.
- With `TERRIO_WBVECTOR_FALLBACK=YES`, a zip archive named for no driver is read by the KMZ
  driver, as the same archive named `.kmz` is, so the layer documents its `doc.kml` links to with
  `NetworkLink` are read (#580). A KMZ that terrio wrote, which keeps each layer in its own
  document under `layers/`, opened as one empty layer; it now gives its layers with their
  features. The fallback's KMZ dataset has the KMZ driver's layer names, fields and style table,
  and `KMZ` as its driver instead of `GeoJSON`.
- Lazy `translate` (`translate_lazy`, `translate_lazy_to_file` and the CLI's `translate`) and
  reads by window (`LazyRaster::read_resampled`) copy pixels fewer times, as do GeoTIFFs other
  than COGs written in rows (`GeoTiffWriter::write_rows`, which streamed GeoTIFF outputs use)
  when the rows given end a block row or the raster, with byte-identical results. Translating a
  14412 x 14412 float32 Deflate GeoTIFF to an uncompressed GeoTIFF on one CPU takes 67% fewer
  page faults. `GeoTiffWriter::write_rows` refuses rows of another type, even none, before taking
  their mask rows, which a refused call left queued. Once a block row of a GeoTIFF other than a
  COG fails to be written, the writer refuses more rows and `finish`, which could write a file
  missing it.
- External overviews are built, refreshed and deleted beside a read-only GeoTIFF, as
  `gdaladdo -ro` builds them (#612): `add_overviews`, `refresh_overviews` and `delete_overviews`
  with `external` (and `terrio overviews --external`) write only the `.ovr` sidecar and no longer
  refuse a file that cannot be written. Internal overviews still need a writable file.
- `add_overviews_with_progress` (Rust) and `add_overviews(..., progress=callback)` (Python) report
  the fraction of the overviews computed, from 0 to 1 and never decreasing, to a callback that can
  cancel (#612). A cancellation returns `Error::Cancelled` (Python
  `terrio.errors.CancelledError`) and changes no file: an existing `.ovr` stays as it was.
- `terrio tile` (Python `terrio.raster_tiles`, JavaScript `rasterTiles`) builds each lower-zoom
  tile from the four tiles below it, as GDAL does, instead of rendering it from the source (#574).
  Tiles are generated depth first, with at most four tiles per zoom level in memory, and every
  source window is the size of one maximum-zoom tile (except when `--resume` rebuilds a missing
  parent from the source, as it does for GeoTIFF tiles), so a VRT mosaic without overviews tiles from
  zoom 0: a 12000 x 12000 EPSG:4326 VRT of 36 GeoTIFFs, which failed with "the source tile window
  exceeds the pixel-buffer budget", now gives its 5390 WebMercatorQuad tiles of zooms 0 to 14.
  With nearest resampling the pyramid equals GDAL 3.13.3's (a raster grid and six reprojected
  WebMercatorQuad zooms); with bilinear, lower zooms differ from GDAL's by a few levels on some
  pixels. This changes the default output of lower zooms with interpolating kernels (bilinear,
  cubic, etc.), which are now resampled from the 8-bit or typed tiles below them;
  `--overview-source source` (`overview_source`, `overviewSource`) keeps the previous rendering
  from the source. `--resume` can rebuild a missing parent from the existing PNG or WebP tiles
  below it; with GeoTIFF tiles or `--no-alpha`, it renders the parent from the source instead,
  which can exceed the source-window budget.
- `terrio tile --format WEBP` writes lossless WebP directory tiles (gray tiles as RGB), equal in
  pixels to the PNG tiles; Leaflet and OpenLayers viewers accept them.
- Zonal statistics that keep values (median, quantiles, mode, minority, variety) count the values of
  a zone of an 8- or 16-bit integer band with at least as many pixels as the type has values (256 or
  65,536) in a histogram (2 or 512 KiB) rather than holding and sorting them; values it does not
  count (-0, fractions, values outside the type's range) are still listed. Without `fractional`, a
  zone whose listed values, 256 or more, are all integers from -32,768 to 65,535 (other than -0),
  whatever the band's type, is sorted by radix. The bands of 8- and 16-bit rasters are then usually
  reduced together rather than one per pass. The median of each of 10 UInt16 bands (10980 x 10980,
  DEFLATE, 32 threads) over one zone covering the tile takes 4.9 s, 10.7 s of CPU and 1.06 GB, down
  from 24.1 s, 70 s and 2.88 GB (0.7.0: 47.8 s, 47 s and 5.34 GB); over 10,000 fields of about 5,000
  pixels, 2.1 s, 20 s of CPU and 1.0 GB, down from 2.3 s, 25 s and 1.03 GB (0.7.0: 18.6 s, 41 s and
  2.58 GB). Without `fractional`, results are unchanged. With it, a zone counted in a histogram sums
  its coverages in another order, so their rounding can change its quantiles in the last digits, or
  which of two values whose coverage sums tie within rounding is its mode or minority; on both
  benchmarks, with mode, minority, quantiles and variety, it changed no value and outputs are byte
  for byte the same.
- GeoTIFF and COG writers compress Deflate chunks with a pure-Rust port of libdeflate 1.26's
  level 6 compressor, in place of zlib-rs: the level GDAL uses for them, and the library it uses
  when built with libdeflate. The chunks are libdeflate's, byte for byte: every 512 x 512 tile of
  six rasters (float32 DEM, int16 classes, two Sentinel-2 uint16 bands, float32 reflectance and
  NDVI), 1,616 tiles, compared equal outside the test suite, whose tests check round trips and libdeflate's output for four inputs. On one core, the DEM's 165 tiles compress in 1.36 s, where zlib-rs took 2.6 s and
  libdeflate 1.65 s (a Sentinel-2 band: 1.98 / 3.6 / 2.24 s), to 136.46 MB, as libdeflate's,
  where zlib-rs's took 136.09 MB. Writing the 5610 x 7533 DEM as a Deflate COG takes 2.75 s of
  CPU, down from 3.7 s (rioxarray: 2.95 s); its pixels and overviews equal GDAL's, and its size
  is within 0.005% of GDAL's. The port is MIT-licensed and credited in `NOTICE` and
  `docs/acknowledgements.md`.
- Raster writers write over an existing file in place, where they truncated it first and waited for
  the kernel to write its old pages back: GeoTIFF and COG (whole, streamed and built from
  overviews), EHdr, ENVI, ERS, AAIGrid, SAGA, WebP, GSBG, GeoPackage, netCDF, JPEG 2000,
  PNG, JPEG and RST, with their `.hdr`, ENVI and ERS headers, `.sgrd`, `.rdc` and `.wld` files
  (the other writers are still truncated, among them `.prj` and `.aux.xml` sidecars, XYZ, HGT, VRT,
  GRIB2, MBTiles, PMTiles, Zarr, the formats wbraster writes and those written through GDAL; on
  Windows the in-place writers still truncate the file when they open it, so a file another
  process maps is refused unchanged). The file
  stays the same file: a symlink is written through, it keeps its permissions, and its hard links
  and readers holding it open see the new content; a shorter output drops the old tail. The file's
  first 16 bytes, where formats with a signature put it, are zeroed at once and written last, and
  the header of a headerless data file (EHdr, ENVI, ERS, SAGA, RST) is emptied before the data is
  written, so a write interrupted partway (the process killed, the disk full) leaves no such file
  that opens as a raster of old and new bytes (the ENVI writer writes its header over a
  `name.ext.hdr` already there, which the reader prefers to `name.hdr`), and one
  that fails with an error leaves it empty; one that fails before the file is opened (a
  whole-raster encoding error, a data file or header sidecar it cannot open)
  leaves it as it was, and a sidecar failing after its data file is finished leaves the new data.
  A path that is not a regular file (a device such as `/dev/null`, a FIFO) is written as
  before, without truncation. Rewriting a 170 MB GeoTIFF eight times takes a steady 0.22 s each,
  where truncating took 0.07 to 0.8 s with stalls; a 170 MB Float32 raster written eight times over
  the same path takes 0.10 to 0.11 s per write as EHdr (was 0.15 to 0.16 s), 0.11 to 0.12 s as ENVI
  (0.15 to 0.76 s), 0.10 to 0.11 s as SAGA (0.15 to 0.39 s) and 0.25 s as RST (0.30 to 0.67 s). A
  slope file-to-file benchmark takes 0.53 s, where it took 1.01 s (`gdaldem`: 0.90 s).
- Code-only raster CRSs now export terrio's registry WKT, preserving the EPSG authority and
  datum identity (#610). This corrects false northing in GDA94 MGA zones, US survey-foot State
  Plane definitions and Web Mercator's spherical model. Explicit WKT stored in a file is kept.
- Python operations given a `terrio.Raster` held in memory share it instead of copying it; the
  `mask=` raster of `footprint` and `polygonize`, the rasters of the point-cloud functions and
  the rasters written as netCDF groups are still copied. An operation does not change
  a raster in memory, so the sharing is not visible. On a large DEM in memory, one thread, with
  nodata / without: TPI takes 0.068 / 0.034 s (was 0.086 / 0.052 s), slope 0.192 / 0.153 s
  (0.212 / 0.172 s), hillshade 0.128 / 0.092 s (0.146 / 0.111 s), with the same outputs.
- On Linux, the Python extension's allocator advises buffers of 4 MiB or more to use transparent
  huge pages (`madvise(MADV_HUGEPAGE)`), as NumPy does for its arrays; under THP's `madvise`
  mode a raster buffer otherwise takes a page fault every 4 KiB on first write. The `terrio`
  command a wheel installs runs in the extension and so advises too; other platforms, the Rust
  crate, the `terrio-cli` binary and WebAssembly are unchanged. For a 170 MB float32 raster, one
  thread: `Raster.from_array` 0.052 -> 0.020 s, `read()` 0.050 -> 0.021 s, `terrio.dem` slope
  0.27 -> 0.20 s. The allocator ignores the null pointer openjp2 0.6 deallocates when decoding
  JPEG 2000, as `free` does.
- `terrio.rio.features.rasterize` burns shapes that all take one value (`geometry_mask`,
  `default_value`, and the touched pixels of `MergeAlg.add`) as a byte coverage, not as an Int64
  raster of feature numbers gathered back into values: the same pixels with an eighth of the
  memory. On the 5610 x 7533 DEM, one thread, `terrio.xarray` masking with 2,000 polygons takes
  0.32 s (was 0.53 s) and cropping to a 2,000-vertex polygon 0.29 s (was 0.59 s).
- `clip` in `terrio.xarray` (DataArrays and Datasets) and `terrio.rio.features.geometry_mask`
  take a `terrio.Layer`, burnt without a round trip through GeoJSON on north-up grids in memory
  (other grids, `from_disk=True` and a CRS-less Layer reprojected from `crs=` still go through
  GeoJSON), with the pixels of the GeoJSON path for valid geometries (a polygon ring of fewer
  than four positions, which the GeoJSON path skips, is burnt closed). `clip` reprojects the
  Layer from its own CRS, or from `crs=` when it has none; a `crs=` differing from the Layer's
  own CRS raises `ValueError`. Masking the 5610 x 7533 DEM with 2,000 polygons from a
  GeoDataFrame, one thread: 0.32 -> 0.14 s.
- `terrio.rio.windows.get_data_window` reduces each axis instead of listing every valid pixel,
  with the same windows: `.terrio.clip` of the 5610 x 7533 DEM to one polygon takes 0.13 s, where
  it took 0.247 s (rioxarray: 0.21 s).
- `terrio dem` (Python `terrio.dem`, JavaScript `dem`) computes most of its 3×3 products a row at a
  time, with AVX2 where the CPU has it, and most rows holding nodata no longer fall back to one
  pixel at a time. Some products take their arctangents from fdlibm (credited in `NOTICE`,
  `LICENSES/SunPro.txt` and the acknowledgements) and recompute with the platform's functions any
  pixel whose result could round differently. Outputs are bit-identical to the previous build (144
  products and options on four DEMs: Float32 and Int16, with and without nodata), and slope (with
  and without `compute_edges`) and hillshade equal gdaldem 3.8.4's on all 42,260,130 pixels. Slope
  on a 5610 x 7533 Float32 DEM, one thread: 1.45 -> 0.27 s.
- `terrio neighbors` (`focal`; Python `terrio.neighbors`, JavaScript `neighbors`) and VRT
  `KernelFilteredSource` reads compute focal statistics and convolutions a row at a time (medians up
  to 24 taps, modes up to 121), with AVX2 where the CPU has it, and the per-pixel median selects
  instead of sorting. Values are unchanged: `terrio neighbors` outputs equal GDAL's VRT for mean,
  gaussian, u, max and median, with and without nodata, and the previous build's for 20 kernels and
  methods (40 outputs). Mean 3x3 on a 5610 x 7533 DEM, one thread: 1.31 -> 0.15 s.
- Remote files read over HTTP(S) share one connection pool per process (up to 64 idle connections;
  the /vsi file operations keep one pool per operation, as before), so files opened one after
  another or from many threads reuse warm connections rather than each opening its own. The pool
  is rebuilt when the timeout, connect timeout, user agent or proxy variables change, and in a
  forked child, which never uses or closes the parent's connections. Opening a STAC item set of 10
  Sentinel-2 dates x 4 bands over https on 32 threads took 18.6 s instead of 27.2 s (one run).
- GeoTIFF: Deflate tiles and strips are decoded by terrio with `fdeflate` (pure Rust) when the file
  and the host are little-endian and the samples are 8-, 16-, 32- or 64-bit integers (with or
  without the horizontal predictor) or Float32 or Float64 without a predictor. YCbCr and WhiteIsZero
  images, Float16 and complex samples, and RGB or CMYK images whose extra samples the `tiff` crate
  misreads keep their previous decoders. A stream ending before the chunk's samples, or with a wrong
  Adler-32 right after them, is an error; one cut in its trailer after the samples is read, as
  libtiff with zlib reads it. All 121 tiles of a Sentinel-2 L2A band decode as the `tiff` crate
  decodes them.
- Dependencies with RustSec vulnerabilities leave the build. ctutils, zstd and zstd-safe move to
  releases that fix them (0.4.8, 0.13.4, 7.4.0). wbvector is built without its KMZ and OSM PBF
  readers, which terrio did not use for its drivers, so zip 0.6 and protobuf 2 (through
  osmpbfreader 0.16) are no longer compiled; terrio reads OSM PBF with osmpbfreader 0.19 as
  before. With `TERRIO_WBVECTOR_FALLBACK=YES`, a zip archive named for no driver (`.xml`
  included) is still read as KMZ, now opened with terrio's zip 9: from the member the KMZ driver
  reads (its first `doc.kml` in any case, else its first `.kml` member), decoded as the KMZ
  driver decodes it, with wbvector's KML reader. An archive with two `doc.kml` members reads the
  last one, where it read the first. Three kinds of archive that failed are read: one named
  `.xml` with its members stored, one with its document in Latin-1, and one with an encrypted or
  bzip2-compressed member before its document.
- GRIB, netCDF, HDF5, WebP and SRTM tiles have no wbraster format any more, so wbraster's Zarr
  reader never reads them. In a build with the `gdal` feature and without `grib`, a `.grib2` path
  that is a Zarr store was decoded by wbraster's Zarr reader (ruzstd 0.8, RUSTSEC-2026-0344) with
  no option set; the GDAL backend reads it now, as it reads GRIB files in such a build.
- Raster operations use each selected band's nodata, and overview mask discovery reports I/O
  failures instead of silently losing validity (#627). Mosaics reject inconsistent band counts
  and target extents intersecting no source, including separate-band mosaics. Reclassification
  mapping-file inputs are restricted to regular files.
- Python's `write_raster` forwards dataset masks and streams eligible PMTiles/MBTiles writes.
  Tile archives that cannot represent a dataset mask are refused before creating output (#627).
- CI checks `Cargo.lock` against the RustSec advisory database (`cargo audit`, the `audit` job)
  on every pull request and push to main. `.cargo/audit.toml` ignores the five advisories of zip
  8.6.0 and ruzstd 0.8.3, which wbraster 0.2.1 requires, until a wbraster release depends on zip
  9 and ruzstd 0.9.1; since an ignore covers every version of a crate, a second step fails when
  the lockfile has any other vulnerability. Unmaintained and unsound crates are listed without
  failing the check.
- Warps with nearest or `average` resampling are faster, and so are bilinear warps past GDAL's
  4-tap threshold (output pixels over 1/0.95 source pixels across in either direction, 2 or more
  with `TERRIO_GDAL_COMPAT=3.13`); the arithmetic and its order are unchanged. Nearest from
  EPSG:32615 to EPSG:32616 of an 1100 x 1100 uint16 Sentinel-2 window takes 12 ms of kernel time
  instead of 29 (single-threaded).
- A zero-length numeric, enum or compound attribute of a multidimensional dataset is `null`, as
  GDAL 3.13.3's `gdalmdiminfo` prints it, where it was an empty list `[]`, in Rust, Python,
  JavaScript and CLI results and in Zarr copies. A zero-length string attribute stays `[]`. A
  netCDF snapshot leaves out these attributes and any empty list with no recorded native type,
  as `gdalmdimtranslate -of netCDF` does, where it wrote an empty int64 attribute that read back
  as `null`; a typed empty list is still written with its type.
- `terrio.xarray` (`engine="terrio"`, `open_rasterio`) skips applying a band's scale and offset
  when they are 1 and 0, and applies them in place otherwise: reads with `mask_and_scale` take
  19% less time (8000 x 8000 float32).
- `Raster.from_array` copies arrays 2.6 times faster, 6.9 times for views that are not C-ordered
  (169 MB float32): C-ordered arrays as one slice, others made contiguous by numpy, and the
  raster no longer filled with nodata before the data replaces it. `terrio::raster_from_data`
  builds a `Raster` from its data that way, as JavaScript's `rasterFromArray` now does.
- `terrio.rio.features.rasterize` looks burn values up in one array operation, not per pixel in
  Python: 9 times faster for 2,000 shapes on an 8000 x 8000 grid (`crop`/`mask` in
  `terrio.xarray`). Into an integer dtype, integer burn values stay exact (IDs above 2**53 and
  2**63 included) and are clamped to its range, where values outside it wrapped; other values
  are rounded half away from zero and clamped, NaN as 0, as rasterio 1.5.2 (GDAL 3.12.2) burns
  them, where fractions were truncated. Edge cases match rasterio but for integers above 2**52,
  which GDAL rounds through float64 and terrio keeps exact, and values of 2**64 and above into
  uint64, which burn its maximum where GDAL burns 0.
- `terrio translate` and `terrio convert` read a VRT of GeoTIFFs (or other files read by window,
  copied or picked by nearest neighbour) a strip of rows at a time when writing a GeoTIFF or COG,
  so a mosaic larger than memory can be copied or converted. Both read the whole
  VRT into memory as 64-bit values first: translating a 98304 x 114688 RGB mosaic to a COG asked
  for 270 GB and failed (#558). On a 16384 x 16384 RGB mosaic of 16 GeoTIFFs written as a JPEG
  COG, `translate` now peaks at 0.5 GB and `convert` at 0.8 GB, down from 10.3 GB, and the files
  are the same, byte for byte. Strips are made of the source tiles' rows. The input is read
  within 5% as much as before on a tiled GeoTIFF and a 4-tile VRT mosaic, and 40% more on the
  16-tile JPEG mosaic (168 to 235 MB), and a VRT source copied
  pixel for pixel is read without a margin around each window. `translate` streams any input read by window this way when the output
  copies a window of the input or is resized by nearest neighbour or a bilinear, cubic or cubic
  spline kernel from the overview `--overview AUTO` chooses; outputs resized by average, RMS,
  mode or Lanczos, or from a level read alone, are still made whole first.
  A strip holds about 64 MB of values, or one row of the source's tiles when that is larger and
  at most 2 GB (1.2 GB for the reporter's mosaic), and a few strips are in flight.
  `translate_lazy_to_file` is the Rust entry point.

- Warps with `sum` resampling give each output pixel the sum of the valid source pixels it
  overlaps, each weighted by the share of its area inside the output pixel (on the source grid, a
  quadrilateral when the output grid is turned or reprojected), so that an output grid covering a
  source keeps its total, up to floating-point rounding in a floating-point output (a Float32 one
  rounds each pixel to 32 bits), and up to the rounding of each pixel to a whole number and its
  clamping to the type's range in an integer one (a Byte output stores 255 for a sum of 400). An
  upsampled output pixel took the whole value of its source pixel, an output pixel overhanging the
  source's edge counted as covered whole, and one much larger than the source was left empty; a
  reprojected `sum` also changed with the memory limit it was warped under. VRT sources resampled
  with `sum` share their pixels by area too, also from a source window with fractional edges, which
  was copied pixel for pixel when it spanned as many pixels as its destination, and their sums reach
  the band's type unrounded: they were rounded to the source's type, which dropped a Byte source's
  fractions and capped its sums at 255.
- Changed default: `sum` now uses overview estimates under `AUTO`/`AUTO-n`, instead of reading
  full resolution. This is a numerical compatibility change. Totals and nodata footprints can
  change: partly nodata overview blocks count
  their valid-pixel mean for the entire block and can assign values to otherwise nodata output
  pixels. Use `--overview NONE` for exact full-resolution validity and sums.
  A `sum` that reads an overview counts each of its pixels for the source pixels it stands for,
  `(W / w) × (H / h)` for a `w × h` overview of a `W × H` source (exact ratios, also where the
  factor does not divide the size), so that it estimates the source's total at the overview's
  cost, whether the automatic choice gives the overview (`--overview AUTO`, the default, and
  `AUTO-n`; see `terrio::ops::warp::SUM_AUTO_OVERVIEW`) or a level is given (`--overview n`,
  `overview=n` in Python, a `terrio.rio` dataset opened with `overview_level=n`, the source of a
  warped VRT opened at `OVERVIEW_LEVEL`). It added the overview's pixels, each the mean of a block,
  which gives the total divided by the factor squared: 0.39% of it from an overview at 16, 0.024%
  at 64. `--overview NONE` (`overview="NONE"`) sums the full resolution exactly, reading all of it.
  A reduced read of the whole of a `sum` `WarpedVRT` (`read(out_shape=...)`, of any of its bands)
  is warped into the buffer's grid from the source's overview that the read's implicit overview is
  warped from, counted the same way: a read of some of the bands, or of a VRT georeferenced by
  GCPs, resampled the VRT's pixels. The estimate is the total, up to rounding, for an `average`
  overview of floating-point data without nodata when the footprint covers complete overview
  cells; crops through those cells can have substantial total error. Over 256 × 256 outputs of a
  16384 × 16384 raster at 16, 32 and 64 times its pixel size, it was within 2 × 10⁻⁷ of the
  total of Float32 and Float64 data (windows whose edges cut overview pixels), 0.4% to 0.6%
  above it for UInt16 data, whose averages are rounded to whole numbers at each level, and 1.4%
  to 2.6% above it for Float32 data 7% nodata, in two corners and in holes 40 pixels wide: an
  overview pixel of a block partly nodata counts its mean for the whole block (a block wholly
  nodata stays nodata). An overview resampled otherwise than by average gives an estimate from a
  sample of the pixels. A sum outgrows small integer types: give a coarse `sum` a floating-point
  output type (`--ot`, `ot=`, a `WarpedVRT`'s `dtype`), as a UInt16 output stores 65535 for any
  larger sum.
- Rust `WarpOptions` adds `sum_scale` (default `1.0`) for sources opened at an overview
  level. Exhaustive struct literals must add `sum_scale: 1.0`; literals using
  `..Default::default()` need no change.
- A `sum` is refused before its source is read when it would hold more than 4 GiB, estimated as the
  source window an output chunk reads and 8 bytes per pixel of it for the pixels' shares (12 bytes
  per pixel for one Float32 band), with the copies a conversion to another working type holds at
  once (a `WarpedVRT`'s `dtype`, a warped VRT's working type): a coarse output reads the whole
  source in one chunk, and a `sum` with several sources or a cutline holds every source whole. The
  error, `Error::ResourceLimit` (Python `ResourceLimitError`), names how to sum an overview level
  instead (`--overview n`, `overview=n` for `terrio.warp`, or a source opened with
  `overview_level=n` in `terrio.rio`) and `TERRIO_WARP_SUM_MAX_BYTES`, a positive byte count, which
  changes the budget; a `sum` read of a `WarpedVRT` and a `sum` `terrio.rio.warp.reproject` of a
  band or dataset are refused the same way. A 1 × 1 `sum` of a 16384 × 16384 Float32 band (3 GiB)
  passes, and one of a 32768 × 32768 band without overviews, or with `--overview NONE` (12 GiB),
  now errors, where it held the whole window in memory; a `sum` of several sources over 4 GiB in
  all now errors at any output size, where it held them all.
- A `sum` read of a `terrio.rio` `WarpedVRT` warps a region whose source window's working memory
  exceeds its `warp_mem_limit` (64 MB by default) a part at a time, halved along its longer side
  until each part's window fits, down to single pixels, and each part from its own window: the
  pixels are the same, bit for bit. A reduced read of the whole VRT over a large source without
  overviews held the source window of half its output at once, whatever `warp_mem_limit`; it now
  holds one part's: a 256 × 256 Float64 read of a 16384 × 16384 UInt16 band without overviews
  peaked at 2,231 MiB of resident memory, and now at 226 MiB. `TERRIO_WARP_SUM_MAX_BYTES` bounds
  each part's window, refused before any is read: a part fits `warp_mem_limit` unless it cannot be
  split (one output pixel) or is kept whole, so the budget no longer refuses a read for its footprint alone.
  Several bands with source and output nodata values are warped whole, as whether a sum stored on
  the output nodata value is moved off it depends on whether the window holds a nodata pixel.
  A region one of whose halves cannot be transformed is also kept whole.
- A `sum` read of a `terrio.rio` `WarpedVRT` whose `dtype` holds every value of the source's type
  (uint16 or float32 in float64) takes each source window in its own type instead of converting
  it to the VRT's: the kernel reads each value as a float64 either way, so the pixels are the same,
  bit for bit, and a uint16 window of a float64 VRT is held in 2 + 2 bytes per pixel (the window
  and the copy the warp takes) instead of 8 + 8 once converted: the 256 × 256 read above now peaks
  at 226 MiB. `TERRIO_WARP_SUM_MAX_BYTES` counts such a window in the source's type only. A
  `dtype` that does not hold the values (float64 in float32, floats in an integer type) still
  converts the window first, and the budget counts both types.
- With `GDAL_INGESTED_BYTES_AT_OPEN` (or `TERRIO_INGESTED_BYTES_AT_OPEN`) above a block (64 KiB
  by default), opening a remote file that the process-wide cache holds takes the ingested blocks
  from there and requests only those it no longer holds. Every reader of a file requested them
  again (twice for each `terrio.rio.open`), so that opening a URL again was never served from the
  cache.
- netCDF: a `Byte`, `Short` or `Int` variable flagged `_Unsigned` reads its `valid_range` (or
  `valid_min` and `valid_max`) as unsigned too, as GDAL does. Such a range holding negative bounds,
  as GOES ABI L2 products' `[9200, -9536]` does, made every pixel nodata.
- netCDF: `Int64` variables flagged `_Unsigned` are read as `Int64`, as GDAL reads them, where they
  were read as `UInt64` without nodata, and with every pixel `0` given a `valid_range`; without
  `_FillValue`, they take netCDF's default `Int64` nodata (`-9223372036854775806`). An
  inverted `valid_range` (or `valid_min` above `valid_max`) is ignored, as GDAL ignores it, where
  it made every pixel nodata (#553).
- Python and JavaScript: `ZonalReducer` computes zonal statistics from arrays you produce strip
  by strip (after your own per-pixel transforms, say), with the results of `zonal_stats` for a
  raster holding the same values: `windows()` lists the strips to supply, `stream()` reduces one
  band and `finish()` returns one array per statistic. The CLI and MCP take rasters, which
  `zonal-stats` already reduces this way.
- Zonal statistics at datacube scale: zones are computed once per distinct raster grid and
  shared by every raster on it; rasters are read lazily (local, remote, STAC, Zarr) in strips of
  block rows, only where zones lie, and in parallel, and the memory a reduced raster freed is
  returned to the system (Linux, glibc). `zonal-stats` on 100 Sentinel-2 COGs and 10,000 fields
  (`mean`, 32 threads) takes 4.0 s and 0.8 GB of peak memory, where 0.7.0 took 80 s and 12.7 GB,
  with the same results (relative differences under 2e-14). Median, quantiles, mode, minority
  and variety reduce a multi-band raster's bands in groups whose values fit 1 GiB: `median` over
  a zone covering a whole 10-band Sentinel-2 tile takes 24 s and 2.8 GB, where 0.7.0 took 46 s
  and 5.2 GB; on 10,000 fields, 2.6 s and 1.0 GB, where it took 18 s and 2.5 GB.
  `ops::zonal::zonal_stats` takes
  `ZonalSource`s (a `LazyRaster`, or a path opened only while it is read so that thousands of
  files stay within the open-file limit, and its bands); `zonal_stats_with` hands each finished
  band to a sink instead of building a layer. `ops::zonal::{ZonalReducer, ZonalStream}`
  reduce arrays supplied strip by strip on an `ops::rasterize::Grid`.
- **Breaking (Rust API, `terrio::ops::zonal`):** the CLI, Python and JavaScript are unchanged.
  - `zonal_stats(zones, &mut sources, &options)` takes `ZonalSource`s instead of a slice of
    `ZonalInput`s (removed). Wrap a raster in memory with `LazyRaster::from(dataset)`, open one
    with `open_raster_lazy` (then `.into()` a `ZonalRaster`), or give its path
    (`ZonalRaster::Path`), and name each band with `ZonalBand::new(band, prefix)`:

    ```rust
    // 0.7.0
    zonal_stats(&zones, &[ZonalInput { prefix: "dem".into(), raster: &dem, band: 1 }], &options)?;
    // now
    let mut sources = [ZonalSource { raster: LazyRaster::from(dem).into(), bands: vec![ZonalBand::new(1, "dem")] }];
    zonal_stats(&zones, &mut sources, &options)?;
    ```

  - `ZonalWeight::raster` is an `Arc<Mutex<LazyRaster>>`: build it with
    `ZonalWeight::new(raster, band)`.
  - `Statistic` has new variants, so exhaustive `match`es need the new arms; it no longer
    implements `Eq` (`Quantile` holds an `f64`), and `Statistic::name` returns a `String`.
- New zonal statistics `variance`, `cv`, `quantile_P`, `mode` (`majority`), `minority` and
  `variety`, checked against GDAL masks + NumPy and exactextract 0.3.0.
- Labelled zonal inputs: `zonal-stats --inputs FILE.csv` (`path,time,label,band`) and
  `--raster @FILE`; dicts or a DataFrame in Python, `{raster, time, label, band}` objects in
  JavaScript. Labelled fields are named `TIME_LABEL_STATISTIC` (`TIME_LABEL_bN_STATISTIC` for
  a label over several bands). Outputs that would exceed GeoPackage's 2000 columns fail before
  rasters are read.
- Zonal statistics reduce each run of pixels in a loop of its sample type, and track the running
  mean and M2 only when stddev, variance or cv is requested: about four times less CPU than
  before. `mean` is the sum over the count, as exactextract computes it (within 3e-12 of 0.7.0's
  running mean on 10^7 float32 pixels), whatever other statistics are requested.
- Fractional zonal coverage clips each ring to rows and boundary cells (axis-aligned
  Sutherland–Hodgman and shoelace areas) instead of general polygon overlays: about ten times
  less coverage time, with the same exact cell areas within floating-point roundoff.

- terrio Web runs the terrio command line in the browser at https://terrio.dev/web/: the CLI
  compiled to `wasm32-wasip1` runs in a Web Worker on an in-memory workspace, with forms built
  from `terrio --json-usage` as in `terrio ui`, and reads `https://` URLs (and public `s3://`
  and `gs://` objects) by range (#576).
- Typing in the `terrio ui` file browser, or the terrio Web workspace picker, jumps to the next
  entry whose name starts with the typed text; the arrow keys, Home and End move between
  entries (#600).
- `terrio-crs` 0.2.3 adds `list_crs`, the registry CRS listing above, and hardens grid parsing.

## 0.7.0 (2026-11-03)

- Visualization: `terrio::ops::render` draws a raster or vector dataset as 8-bit RGBA, reading a
  raster from the coarsest overview that covers the output, with stretches, eight colormaps,
  palettes and transparency for nodata, masks and alpha. `terrio preview` writes a PNG or draws in
  the terminal (Kitty, iTerm2, sixels or half blocks), the MCP `preview` tool returns the image,
  and `terrio view` serves a local pan-and-zoom map. Python and JavaScript expose the same
  rendering (#513).
- `viewshed --om ACCUM --os N` sums visibility from a regular grid of observers and normalizes it
  to Byte as GDAL 3.13 does, through Rust, CLI/MCP, Python and JavaScript/WASM (#288).
- SQL: attribute `GROUP BY` with per-group aggregates, ordering and limits, and the spatial
  functions `ST_Area`, `ST_Length`, `ST_Intersects`, `ST_Buffer`, `ST_Transform` and
  `ST_Centroid` (#288).
- KML and KMZ keep shared style ids in a style table (`style_table` in Rust and Python,
  `styleTable` in JavaScript), as GDAL's LIBKML driver does, and write it back; `fill` and
  `outline` flags and inline styles merged over shared ones follow KML (#393).
- CAD drawings read polyface meshes and MESH entities as 3D multipolygons, and leave out
  entities they do not support with a warning instead of failing the drawing (#501).
- A dataset with a single named layer accepts its file stem as the layer name too, through native
  readers, in-memory codecs and the GDAL backend, with one diagnostic for a missing layer (#529).
- The WebAssembly package can be built with a selection of scientific formats, such as NetCDF and
  HDF5, as a single module (#503).
- `terrio-crs` 0.2.2 adds the two-point form of oblique Mercator, and
  `SpatialRef::parse_literal`, which parses a CRS definition without reading it as a file path.
- `Workbook::open_bytes` now enforces the same compressed-size limit as `Workbook::open`.
  Workbook authoring and editing are checked against openpyxl in CI, including retained
  chart, drawing, table and theme parts (#405).
- Opening a vector dataset with layer names refuses a name no layer has, rather than reading
  the layers the other names match, as GDAL refuses a layer it cannot fetch (#521).
- A local `.aux.xml` sidecar's band `ColorInterp` overrides a GeoTIFF's and applies to other
  rasters read whole, as GDAL reads it, so `preview`, `render` and `_repr_png_` draw an alpha
  band it names. The renderer now reads a lazily opened GeoTIFF's color interpretations too:
  its own alpha band is drawn, and without `bands` the bands it names Red, Green and Blue are
  drawn as RGB, in that order and whatever their type, where it drew the first three bands of a
  Byte file, or band 1 otherwise (#521).
- Rendering scales alpha as GDAL's warper does: 2^NBITS - 1, 65535 for UInt16, 32767 for
  Int16 (was 255) and 255 otherwise, floating point included (#521).
- Rendering a level without a fitting overview reads its sampled rows a row of blocks at a
  time, decoding each block once (a 4000 x 4000 tiled GeoTIFF drew in a twelfth of the time),
  and map views of vector layers index the features once instead of scanning and cloning them
  for every tile (#521).
- `LazyRaster::read_resampled` reads a window of some bands into an output of another size as
  GDAL's `RasterIO` reads it (`gdal_translate -outsize`, rasterio's `out_shape` reads): from the
  overview GDAL chooses, or the level `gdal_translate -ovr` names, by nearest neighbour or with
  the kernels of `RasterIOResampled`, pixels weighted by the level's mask band, nodata values or
  alpha band. `read_resampled_masks` reads their mask bands the same way (#429).
- `terrio translate --outsize/--tr` reads a raster read by window from its overviews, as
  `gdal_translate` does, rather than reading it whole at full resolution: a 512 x 512 output of a
  16384 x 16384 COG reads its 512 x 512 overview. `--overview` (`--ovr`) chooses the level as
  `gdal_translate -ovr` does (#429).
- Mode overviews and resampled reads of a raster whose nodata value is NaN fill a footprint
  without a valid pixel with 0, as GDAL does (`GDALIsValueInRange`), rather than NaN.
- Zonal statistics use each selected band's own nodata value in every membership mode,
  fixing multi-band inputs whose nodata differs from band 1. Fractional coverage and
  aligned weighting rasters are available across Rust, CLI/MCP, Python and Node/WASM (#288).
  `ZonalOptions` remains a concrete type without a lifetime parameter; new weights use
  shared `Arc<RasterDataset>` ownership. Rust callers with exhaustive struct literals
  or `Statistic` matches must account for the new fields and weighted variants;
  struct updates using `..Default::default()` retain the defaults.
- `terrio::xlsx::Workbook::open` and `open_bytes` read a workbook to edit and save. Parts the model
  does not hold, such as charts and macros, a sheet's other elements, and unedited cells and settings
  are written back as they were. The workbook is held in memory whole, its parts expanding to at most 8 GiB,
  and cells are charged by the memory they take, which keeps opening and saving near 8 GiB at most
  (measured: 2 million cells peak at 0.9 to 1 GB).
  `Workbook::layer` and `add_layer` view sheets as layers (#405).
- Read-only OGC API Features discovery and bounded paging through Rust, CLI/MCP,
  Python and JavaScript/WASM. Explicit `OAPIF:` URLs support collection selection,
  spatial/temporal queries, advertised CRS and axis rules, and scalar JSON schemas.
  Nested properties are preserved as JSON text; unsupported schema references fail
  explicitly. Incomplete pagination fails when `numberMatched` or a next link
  reveals missing features; otherwise a final page ends the read (#288).
- Native JSON requests send a caller- or document-supplied `Accept` header in place of
  the default `application/geo+json, application/json` instead of sending both, so STAC
  link `headers` that set `Accept` now control content negotiation (#288).
- `terrio::xlsx::Workbook` writes workbooks laid out for people: sheets, typed cells at their
  positions, merged ranges, column and row settings, cell formats, frozen panes, filters, list
  validations, defined names, hyperlinks, comments, print setup and document properties (#405).
- XLSX layout options beyond GDAL: `HEADER_ROWS`, `RANGE` and `FIRST_ROW` when reading, and
  `HEADER_STYLE`, `FREEZE_HEADER`, `AUTOFILTER`, `COLUMN_WIDTH`, `DATE_FORMAT`, `DATETIME_FORMAT`
  and `CREATION_TIME` when writing. A workbook's sheets may expand to 8 GiB together (#405).
- The `XLSX` driver reads and writes `.xlsx` and `.xlsm` workbooks in pure Rust, natively and in
  WebAssembly, as GDAL's driver does: a layer per worksheet, with the `HEADERS` and `FIELD_TYPES`
  open options. Naming coordinate or WKT columns reads geometries from them, and the `GEOMETRY`
  layer creation option writes them (#405).

- Remote GeoJSON and GML responses open by detected content when their URLs have no
  file extension, including WFS GetFeature requests (#494).
- XML element-count and nesting budget refusals report `ResourceLimitError` in Python,
  including palette XML reads, consistently with other parser resource limits.
- Native HTTP builds read WFS 1.0/1.1/2.0 services by `WFS:` endpoint or
  GetCapabilities URL, list layers without downloading features, and read selected
  simple-feature layers with schemas, CRS and configurable paging (#495).
- GML source IDs resolve namespace aliases. Unrelated prefixed `id`/`fid`
  attributes are no longer treated as source IDs.
- The `ESRIPBF` driver reads the protocol buffer of ArcGIS feature-service queries (`f=pbf`), which
  GDAL has no driver for: files recognized by content, and query URLs read page by page as
  `ESRIJSON` reads them, with the `ESRIPBF:` prefix (#475).

- Resource and allocation refusals are terminal across native and optional GDAL reads. Python
  reports them as `ResourceLimitError` (a `MemoryError`), replacing `InvalidArgumentError`
  for GRIB/ESRIJSON limits, `TerrioError` for decoded-byte limits, and `TerrioIOError` for
  I/O allocation failures. The audited native parser, expansion, chunk,
  archive, listing and metadata budgets also replace prior `InvalidDataError`/`UnsupportedError`
  refusals with the resource class. Update exception handlers accordingly.
  GDAL vector reads, including fallback
  reads, now have a default 1 GiB retained-data estimate; previously successful large reads can
  require raising `TERRIO_GDAL_MAX_VECTOR_BYTES` to a positive byte count (#485).

### Projections

- `+proj=omerc` with two points (`+lat_1`/`+lon_1`, `+lat_2`/`+lon_2`) and no
  azimuth is now computed as PROJ does; it was refused. GDAL's WKT1
  `Hotine_Oblique_Mercator_Two_Point_Natural_Origin` (with GDAL's or ESRI's
  parameter names) and WKT2 `Hotine Oblique Mercator Two Point Natural Origin`
  are read, and the former is written.

## 0.6.1 (2026-11-01)

- CityJSON 1.1 and 2.0 and CityGML 2.0 and 3.0 are read and written as flat XYZ features through
  Rust, the CLI and MCP, Python and JavaScript. They do not keep lossless city-model topology,
  textures or materials (#464, #470, #473, #480).
- `terrio [command ...] --json-usage` prints a versioned, still unstable JSON catalog of the
  commands (#461).
- CADRG `A.TOC` indexes can be created from existing frame files through every API (#446).
- Python and JavaScript expose contour, footprint, buffer, grouped dissolve, geometry validity and
  repair, zonal statistics and multipart splitting directly (#445, #447).
- `stream::GpkgLayersWriter` writes interleaved multi-layer GeoPackages from Rust (#458).
- Python `to_bytes` and JavaScript `toBytes` reject several layers for a single-layer format
  rather than writing only the first (#445).
- xarray `to_raster()` exports reprojected masked integer rasters as floating point with NaN
  nodata when their integer encoding cannot hold the new borders (#463).
- `terrio-crs` 0.2.1 adds `Transformer::with_reverse_degree_division`, which `terrio` needs, and
  accepts antimeridian rounding in the Mollweide and Wagner IV and V inverses (#458, #472).
- Rust: `VectorOpenOptions` has a new `feature_server_paging` field and `VectorWriteOptions` new
  `cityjson_version`, `citygml_version` and `citygml_lod0_surface` fields, so a struct literal of
  either type without `..Default::default()` no longer compiles.
- The `ESRIJSON` driver reads the JSON of ArcGIS feature-service queries as GDAL's does: files
  recognized by content, and query URLs read page by page, with the `FEATURE_SERVER_PAGING` open
  option and the `ESRIJSON:` prefix.
- `terrio ui` serves a local page with a form for every command, a file browser, the equivalent
  command line and cancellation. It listens on 127.0.0.1 only and needs a per-session token. The
  `--json-usage` catalog records whether each option is `hidden`.
- Warps map output pixels to the source with GDAL's degree rounding on every platform, except
  between Web Mercator and WGS 84, so warped pixels can change in the last bits.
- On glibc Linux, projections call the installed C library's `asinh` and `atanh`, so coordinates can
  change in the last bits and are no longer reproducible across glibc releases. With the warp
  change, the #253 bilinear warp and county cases match GDAL 3.13.3 exactly.
- `terrio info` of a rotated raster reports the extent of its four corners, and prints the
  geotransform in place of a pixel size. South-up rasters lazily read report ordered bounds (#468).
- `terrio formats` lists EPT as written, and the compression help and errors list LERC and JPEG XL
  (#467).
- The command line page opens with every command in alphabetical order, with its `--help`
  summary, and a test keeps the list current. The rest of the site was corrected against the code.
- The documentation site runs the notebooks in the browser with JupyterLite, with
  terrio's wheel served by the site and installed by each notebook, at
  <https://terrio.dev/notebooks/>.
- `terrio.rio.MemoryFile` given PNG, JPEG or WebP bytes without a filename or
  `ext` names its file for their format, so they open (#451). Other bytes are
  named `.tif`, as before.
- `open_raster` (eager or lazy) and `validate_cog` name a missing local file, or
  a directory, in their errors, as in `'dem.tif': No such file or directory (os
  error 2)` (#451). The error's kind and its Python class are unchanged.
- A first range request answered 206 with an unknown total length
  (`Content-Range: bytes 0-16383/*`) fails with its own message, distinct from
  that of a 206 without `Content-Range` (#451).
- `terrio --help` and the nested command lists are sorted alphabetically, with a
  one-line summary of at most 60 characters per command. Details such as the
  matching GDAL or PDAL tool moved to each command's own `--help`.
- Homebrew installs the `terrio` command on macOS and Linux, with `brew install
  --cask opengeos/terrio/terrio`. Each release tests the cask and updates the tap.

## 0.6.0 (2026-10-31)

### Standalone CRS crate

- `terrio-crs` 0.2.0 carries the CRS work since 0.1.0: geocentric and IAU CRSs,
  PROJJSON, and more projections. `TransformOptions` has a new public field,
  `promote_to_3d`: struct literals need it or `..Default::default()`.
- The crate's default build has no C dependency, on native targets as well as
  WebAssembly. Its default features changed: Zstandard TIFF grids are no longer
  read unless the optional `libzstd` feature (the zstd C library) is enabled, and
  such a grid is treated as unavailable, so a transformation that needs it falls
  back to the next operation, possibly a ballpark one, without an error. Set
  `only_best` (`ONLY_BEST=YES`) to get an error instead. PROJ's grids are
  DEFLATE-compressed.

### No C dependencies

- The default builds of the CLI, the Python package, the Rust crates and the JavaScript
  package have no C dependencies, and CI builds them without a C compiler.
- `libzstd` is no longer a default feature of `terrio`. Zstandard GeoTIFF and EPT tiles are decoded
  and written with `ruzstd`, which decodes more slowly and writes at its fastest level (about zstd's
  level 1) whatever level is asked for. Enable `libzstd` to use the zstd C library.
- Zstandard GeoParquet is read with the same codec, rather than through the `parquet` crate's zstd.
- HTTPS uses `rustls-graviola` rather than `ring` on x86-64 and AArch64. An HTTPS request panics on
  a CPU without the features it needs; see [Architecture](docs/architecture.md#pure-rust).

### Raster dataset masks

- Preserve internal GeoTIFF and `.msk` validity masks in `convert`, including
  extracted windows and overviews. Stream masks alongside GeoTIFF/COG pixels
  and use them when generating COG overviews. Lazy warps, CLI output and Python
  inputs now apply source masks, including multiple sources and cutlines (#431).
- Masked inputs are refused, rather than written without their mask, by
  `convert` to any output other than GeoTIFF and COG (PMTiles, MBTiles and GDAL
  drivers included), by `convert --append` and by `convert --t-srs`. Write
  GeoTIFF or COG to keep the validity, or use `warp` to apply it. The library,
  Python and JavaScript writers refuse tile archives but keep the pixel, alpha
  and nodata serialization of other non-GeoTIFF formats, which do not store the
  mask.

### Docker image

- Publish the `terrio` command-line tool as a Docker image for linux/amd64 and
  linux/arm64 to the GitHub Container Registry (`ghcr.io/opengeos/terrio`) with
  each release.

### Licensing

- Geometry routines are ports of JTS or independent implementations (#402).
- Point cloud radius searches sort with Rust's standard library (#414).
- The repository follows the [REUSE specification](https://reuse.software): files
  adapted from other projects carry SPDX headers naming their sources, and
  `NOTICE` lists every upstream project and its copyright holders (#415, #435).
- `LICENSE` names its copyright holders again (#416).
- Package metadata declares MIT AND MPL-2.0: the point cloud eigensolver
  (`pointcloud/filters/selfadjoint.rs`) is MPL-2.0 (#435).
- Earlier releases ship older notices; upgrading is recommended.

### Geometry implementation provenance

- Replace the GEOS-derived simplification, hull, tile-validity, spatial-packing
  and clipping-envelope kernels with attributed JTS 1.20.0 implementations under
  EDL-1.0. Replace the tile writer's linework repair independently, preserving its
  isolated implementation and behavioral evidence. The complete old repair
  module and helpers are removed. Public signatures are unchanged; simplification,
  hull and tile-validity outputs can differ from earlier releases, including
  ring starts, orientation and some geometries. The
  [compatibility record](docs/jts-provenance.md) describes the observed behavior
  and numeric limits. Polygon hulls can return an error when no non-degenerate
  finite frame can be constructed. Repeated mesh edges and unclosed boundary
  walks also return errors instead of overwriting adjacency or losing components.
  Polygon hulls require valid, non-overlapping input; mixed degenerate components
  can error, and invalid-input output is not guaranteed to match GEOS.
  The previous upfront ring-crossing rejection is not preserved: invalid inputs
  are not guaranteed to return the former "run make-valid first" error.
- Polygon hulls keep holes on their containing outer shell when an island
  touches a hole vertex, rather than assigning the hole to that smaller island.
- Tile conversion now fails explicitly on repair invariant violations instead
  of silently dropping a feature or emitting unrepaired geometry. One such
  failure stops the conversion, including a multi-zoom run.
  Tile encoding errors identify the layer, feature ID and index, and tile Z/X/Y.
- Polygon tile encoding reports an error when a signed cursor delta cannot fit
  in 32 bits. The [provenance inventory](docs/geos-retirement-audit.md) records
  reference comparisons, collinear-ring ordering differences and the existing
  approximate clipping-failure predictor.

### Terrain

- `terrio dem color-relief` and Python's `terrio.dem(path, ..., dst=path)` read the DEM and write
  the output a strip of rows at a time, as `terrio dem` already did for the other products
  (Rust: `color_relief_to_path`, and `ColorMap::parse_lazy` for color files with percentages).
  A path-to-path `terrio.dem` now returns the result opened lazily from `dst` instead of in
  memory; the pixels are unchanged. On a 15,000 x 12,000 Float32 DEM, a color relief with alpha
  peaks at about 560 MiB (160 MiB with `max_memory_mib=256`) instead of 7,569 MiB.
- `--max-memory` (CLI), `max_memory_mib` (Python) and `DemOptions::max_memory_mib` set the strip
  height from a memory budget, like `gdalwarp -wm` (by default, strips of about 64 MiB of input
  values, as before). The values do not depend on it.
- The in-memory `color_relief` colors the band directly into the Byte output, in parallel,
  instead of holding a Float64 copy of the band and four Float64 output planes.
- `dem_to_path` and `color_relief_to_path` read a DEM that is also the output whole first,
  instead of truncating it while it is read.
- On Unix, an output that is a hard link to an input now counts as that input wherever an output
  may replace its input: the DEM paths, `warp`, `mosaic`, `LazyRaster::write` and pansharpening
  read the input whole first (or refuse, for a VRT output), as for the same path. On Windows, a
  hard link is not detected.
- `terrio dem` names `--max-memory`, not `max_memory_mib`, when it refuses a budget for an output
  that is a hard link to the input.

### Formats

- GeoJSON and GeoJSONSeq outputs write a field as each feature's `id` member with GDAL's
  `ID_FIELD` and `ID_TYPE` creation options (`--lco`, `GeoJsonOptions::id_field` and
  `id_type`), so the string identifiers read from a GeoJSON `id` go back out as RFC 7946 `id`
  members. `GeoJsonOptions` is no longer `Copy`.
- KMZ archives can carry files such as the icon images of `SYMBOL` styles (#394):
  `VectorWriteOptions::kmz` (`KmzOptions::resources`), `terrio convert --kmz-resource`, Python's
  `kmz_resources=` and JavaScript's `kmzResources` pack them beside the layer documents, where
  relative icon links resolve.
- HEC-RAS geometry (`.g##.hdf`) and plan (`.p##.hdf`) HDF files are read as
  vector layers (the `HECRAS` driver, the `hecras` feature, on by default), through the
  [`ras-commander-fast-hdf`](https://crates.io/crates/ras-commander-fast-hdf)
  engine: 2D mesh cells, cell centres, faces, face points and perimeters (with
  the stored 2D flow area table); cross sections (with station-elevation,
  Manning's n and ineffective areas as JSON fields), river lines, storage
  areas, structures, boundary-condition lines, breaklines, refinement regions,
  reference lines and points, junctions, pump stations, and pipe conduits and
  nodes, each with its stored attribute columns. Stored results join through
  open options (summary maxima and minima with their times, time-step values,
  series extremes, cross-section, structure, boundary and reference results).
  The CRS is the stored `Projection` WKT. A file is claimed only when its root
  `File Type` is `HEC-RAS Geometry` or `HEC-RAS Results`; other HDF5 and
  NetCDF files are not affected. Compared with h5py on the HEC-RAS 7.0.1 files
  of a public corpus: every value identical. See `docs/hecras.md`.
- `hdf5-reader` and `netcdf-reader` are the crates.io 0.10.1 releases; the
  `[patch]` to a netcdf-rust branch is gone (its fix, roteiro-gis/netcdf-rust#95,
  is in 0.10). The HEC-RAS engine uses the same `hdf5-reader`, so a build
  compiles one HDF5 reader.

- SRTM HGT tiles are read and written (the `SRTMHGT` driver), as GDAL's SRTMHGT
  driver does: 3", 1" and 0.5" tiles georeferenced from their name
  (`N37W120.hgt`), `.hgt.zip` archives, and the NASADEM and water body variants.
  Tiles are written as Int16 with -32768 for nodata; a grid of another size is
  refused.
- SRTM HGT tiles and the other raw-layout rasters (EHdr, ENVI, SAGA, ERS,
  Idrisi, Surfer 6 binary grids) are read in place (#333): `open_raster_lazy`
  reads only the header, a window reads only the rows it covers, and `info`
  decodes no pixels. Remote SRTM tiles are read with HTTP range requests, and
  `convert` to a GeoTIFF or COG writes these inputs a strip at a time.
- Lazy GeoTIFF reads keep the tile or strip offsets and byte counts of each
  image (full resolution or overview) after its first window read (#348): later
  windows of the image no longer read and decode both tables again, nor does
  terrio's own codec (LZW and the other compressions it decodes) read them a
  second time, and mask reads no longer copy the mask's tables. Windows decoded
  on the calling thread (one chunk, or remote and sparse files) no longer cost
  more as the image has more tiles; the threads decoding the chunks of larger
  windows of local files still each read the tables, as does moving to another
  overview. The tables kept take 16 bytes per chunk of each image read.

### GeoPackage

- `terrio gpkg-repack` (CLI, and the `gpkg-repack` MCP tool) compacts a native GeoPackage in
  place, rebuilding every B-tree from its encoded records and preserving rowids, SQL objects,
  text encodings, rasters and auto-vacuum pointer maps. Python `gpkg_repack(path)` and
  `gpkg_repack_bytes(data)`, and JavaScript `gpkgRepack` and `gpkgRepackBytes`, expose the same
  operation. Scope and storage safety are in [docs/cli.md](docs/cli.md#geopackage-repacking).

### Remote I/O

- Signed S3 uploads of local files over 8 MiB sent over HTTPS read the file once
  per attempt instead of twice (#335), as a retried, redirected or
  region-rerouted request reads it again: they are signed with
  `UNSIGNED-PAYLOAD`, as the AWS SDKs sign S3 uploads over HTTPS, and the body
  is streamed. The signature no longer covers the body of these uploads; TLS
  protects it in transit. Over plain HTTP, and for smaller or staged uploads,
  the payload hash is signed as before.

### Breaking changes

- Dataset/VSI paths retain leading and trailing whitespace across CLI, Python, JavaScript
  and MCP (#372). Native filenames and virtual keys with edge whitespace are distinct from their
  trimmed neighbors; callers must normalize pasted paths/URLs themselves. `@FILE` lines
  still apply their file-list whitespace rules.
- Native Unix SQLite in-place append/update and recovery propagate parent-directory open/fsync
  failures. Filesystems without directory fsync support can fail before page writes. An error
  after journal deletion reports committed changes or completed recovery with unconfirmed
  durability; do not blindly retry an append. Native append also refuses linked/special
  journal, WAL and shared-memory sidecars before recovery, and refuses a database file with
  hard-link aliases (Unix `nlink > 1`).
- VRT `SourceFilename` values retain trailing whitespace and newlines, matching GDAL.
  Remove formatting whitespace from inside the element when it is not part of the filename;
  encoded leading whitespace and CDATA remain significant.
- Signed S3 uploads of local files over 8 MiB over HTTPS use `UNSIGNED-PAYLOAD`
  (#335). An S3-compatible service, or a bucket policy, that refuses it now
  rejects those uploads; they can be sent over plain HTTP to keep a signed
  payload, with `AWS_HTTPS=NO`, or an `http://` `AWS_S3_ENDPOINT` when the
  endpoint names its scheme (an `https://` endpoint ignores `AWS_HTTPS`). Plain
  HTTP does not encrypt the request body or headers, including
  `x-amz-security-token` for temporary credentials; use this workaround only
  over a trusted connection.
- A path-to-path Python `terrio.dem(src_path, ..., dst=path)` (other than to a VRT) returns the
  result opened lazily from `dst` instead of in memory: the returned `Raster` reads `dst`, which
  must stay in place and unchanged while the `Raster` is in use.
- `DemOptions` has a new public field, `max_memory_mib`: struct literals need it or
  `..Default::default()`.
- `mosaic`, `warp`, `concat` and `filter merge` now treat an input beginning
  with `@` as a file list, so a file named `@name.tif` must be written
  `./@name.tif`.
- WKT2 `Goode Homolosine` (PROJ's uninterrupted `goode`) is refused; it was read
  as `igh`.

### Command line

- `mosaic`, `warp`, `concat` and `filter merge` read inputs from a file list:
  `@FILE` stands for the paths listed in FILE, one per line, as GDAL's
  `--input @filelist.txt` does (#323). MCP tool calls refuse `@FILE`, so that a
  client cannot read a file back through the errors naming its lines.
- `validate_cog` (Rust, Python) and `terrio validate-cog` (and MCP) check that a GeoTIFF is a
  Cloud Optimized GeoTIFF with the checks and messages of rio-cogeo's `cog_validate` (GDAL's
  `validate_cloud_optimized_geotiff.py`), reading only headers and chunk offsets: local files,
  `/vsizip/` paths, and HTTP(S) or S3 URLs by range requests. The command exits with status 1
  when the file is not a valid COG.
- Dataset file operations (`dataset-copy`, `dataset-rename`, `dataset-delete` and
  `dataset-files`) use paths and URLs exactly
  as given, across CLI, Python, JavaScript and MCP. Native filenames and virtual keys with
  edge whitespace are distinct from their trimmed neighbors, so callers must trim pasted
  paths/URLs themselves.

### Point clouds

- PDAL's `filters.hexbin`: the boundary of points on a grid of hexagons (the hexagons with at
  least a threshold of points, as polygons with holes), with its area, density and point
  spacing, and the hexagons with their counts. `terrio filter hexbin` (and MCP) writes them as
  vector datasets, reading the points in chunks; Python's `PointCloud.hexbin` returns PDAL's
  metadata (`boundary` as WKT, `boundary_json`...) with both as layers; JavaScript's
  `PointCloud.hexbin` returns both as a vector dataset; Rust has `pointcloud::filters::hexbin`
  and `HexBinner`. PDAL's options (`edge_length`/`edge_size` or its estimate from
  `sample_size` points, `threshold`, `smooth`, `hole_cull_area_tolerance`, `precision`) and a
  grid `origin`; H3 grids are not implemented. Hexagons, counts and boundaries (smoothed or not)
  are identical to PDAL 2.10's on autzen; points are assigned to hexagons as hexer assigns them
  (a port, attributed in `NOTICE`).
- Reprojection (`terrio convert --t-srs`, `convert_points(t_srs=...)`,
  `PointCloud.reproject`, `append_points`) now transforms `z` when the points'
  CRS and the target CRS both have heights, as PDAL's `filters.reprojection`
  does: vertical units, geoid models and vertical datum transformations apply.
  It previously transformed only `x` and `y`, so 300 ftUS in
  `EPSG:6588+6360` stayed 300 in `EPSG:6588+5703` (now 91.44018 m). The
  vertical CRS a LAS file declares is now part of the CRS reprojected from
  (it was dropped) unless a CRS is assigned with `--a-srs`, and LAS files
  written with GeoKeys record a compound CRS's vertical CRS in
  `VerticalCSTypeGeoKey`. When only the target has heights, `z` is kept and a
  warning says so.

### CRS compatibility

- Transformer construction now checks celestial-body compatibility for all CRS
  pairs, including custom PROJ definitions without IAU identifiers. Different
  known bodies are rejected. When either body is unidentified, semi-major axes
  must agree within 0.7% of the target radius. For example, `+R=1` to `+R=2`, or a
  lunar/custom small sphere to EPSG:4326, now returns an error; matching custom
  figures remain supported.
- Earth datum operations (`+datum`, `+towgs84`, or datum grids) require an Earth
  figure, including for otherwise matching unidentified custom radii. They are
  rejected on planetary/custom small figures rather than interpreted as valid
  Earth shifts. This intentionally also applies without IAU identifiers. There
  is no `PROJ_IGNORE_CELESTIAL_BODY` override in Rust, CLI/MCP, Python or
  JavaScript. Use a CRS on the correct body; changes of arbitrary coordinate
  units should use the applicable CRS unit parameters.
- Geographic WKT2 definitions with west- or south-positive axes now preserve
  those directions and their declared order, including Earth CRSs. They were
  previously treated as ordinary east/north coordinates. This matches GDAL's
  traditional GIS axis strategy; ordinary north/east Earth axes still use
  longitude, latitude order.
- Vertical CRSs without geoid models of their own now use those of the vertical
  CRSs of their datum, as PROJ does. NAVD88 height in US survey feet
  (EPSG:6360) or feet (EPSG:8228) and NAVD88 depth (EPSG:6357, EPSG:6358) go
  through GEOID18 like NAVD88 height (EPSG:5703), in both directions, with the
  unit and direction converted. They previously only converted the unit,
  an error of the geoid undulation (about 21 to 28 m across Texas):
  `EPSG:6318+6360` to `EPSG:6319` at 30 N, 97 W turns 328.0833 ftUS
  (100 m) into 73.4074 m, as PROJ 9.9.0, instead of 100 m.

### Projections

- `+proj=igh` (interrupted Goode homolosine) is computed as PROJ does; it was
  refused. GDAL's WKT1 `Interrupted_Goode_Homolosine`, ESRI's
  `Goode_Homolosine` with `Option` 1 and WKT2 `Interrupted Goode Homolosine`
  are read, and the first is written.
- `+proj=ob_tran` with an `+o_proj` other than `longlat` (a projection on a
  rotated sphere or ellipsoid) is computed as PROJ does; it was refused. The
  pole may be given by `+o_lat_p`/`+o_lon_p`, by `+o_lon_c`/`+o_lat_c`/`+o_alpha`
  or by `+o_lon_1`/`+o_lat_1`/`+o_lon_2`/`+o_lat_2`, for rotated-pole grids too.
  PROJ's WKT2 `PROJ ob_tran o_proj=...` method is read.
- `+proj=qsc` (quadrilateralized spherical cube) is computed as PROJ does, on
  each of the six cube faces and on a sphere or an ellipsoid; it was refused.
  GDAL's and ESRI's WKT1 `Quadrilateralized_Spherical_Cube` and WKT2
  `Quadrilateralized Spherical Cube` are read, and the WKT1 form is written.
- `+proj=healpix` and `+proj=rhealpix` are computed as PROJ does, with
  `+rot_xy` and `+north_square`/`+south_square`; they were refused. On an
  ellipsoid they use the authalic latitude, as PROJ 9.8.1. GDAL's WKT1 (which
  carries the PROJ string), ESRI's `PROJ_healpix` and `PROJ_rhealpix` and WKT2
  `PROJ healpix` and `PROJ rhealpix` are read, and the first is written.
- `+proj=isea` is computed as PROJ 9.8.1 does, with `+orient`, `+azi`, `+lon_0`,
  `+lat_0`, `+resolution` and `+aperture`; it was refused. As in PROJ 9.8.1, the
  inverse is defined for the default grid in its two orientations only, and on an
  ellipsoid the forward takes the geodetic latitude on the semi-major axis while
  the inverse uses the authalic radius. `+mode=di`, `dd` and `hex` (grid
  addresses) are refused. GDAL's WKT1 (which carries the PROJ string), ESRI's
  `PROJ_isea` and WKT2 `PROJ isea` are read, and the first is written.
- WKT2 `PROJ name` methods take only the projection's string-valued parameters
  in the name (`o_proj` for `ob_tran`, `orient` and `mode` for `isea`), and refuse
  `R_C`, `geoc`, `vunits` and `vto_meter` among their PARAMETERs, as they refused
  the figure, datum and units before.

### Warp

- With bilinear, cubic, cubic spline and Lanczos, an output pixel whose source
  pixel under the sample point is nodata (or cut out) is now left unwritten
  (at the output's nodata value, when it has one) for a single-band source too,
  as GDAL does: it took a value renormalised over the valid neighbours, which
  moved the edge of the data about a pixel into nodata areas. Several bands
  already behaved so, with unified nodata.

### Shapefile

- The rings of polygon records can be organized by any of the methods of GDAL's
  `OGR_ORGANIZE_POLYGONS`, chosen with the `ORGANIZE_POLYGONS` open option (`--oo`,
  `VectorOpenOptions::shapefile`) or the `OGR_ORGANIZE_POLYGONS` (or `TERRIO_ORGANIZE_POLYGONS`)
  environment variable. `DEFAULT` groups rings by containment, whatever their winding, so files
  that wind every ring one way keep their holes. `ONLY_CCW` stays the default, as in GDAL. With
  every method, the self-touching rings below are split first, as GDAL 3.13 does.
- In polygon records of several parts, rings that touch themselves at a vertex
  to enclose a hole, which Esri allows, are now split into an exterior and
  holes when read, wherever the stored ring starts, as GDAL 3.13 does (GDAL
  3.12 keeps them as stored). A ring whose loops are not holes (a figure-eight,
  a loop outside the ring, a spike), and a record of a single ring, are still
  read as stored, also as GDAL does. The split rings start at the vertex the
  rings GEOS rebuilds for GDAL start at. The `shapefile-rings` conformance
  suite compares 18 such cases with GDAL.
- A hole whose first vertex lies on the exterior now belongs to that exterior.
  It was read as a polygon of its own.

### Hardening and GDAL parity

- `driver=` and the process-wide allow-lists (`TERRIO_ALLOWED_DRIVERS`) are
  enforced; under them, VRT, STAC, STACIT and Zarr open only when named.
- Every entry point that opens data judges the driver it opens it with against
  the allow-lists before the data is read or downloaded: formats recognised by
  their content or by a file beside them (a Surfer 6 grid under any name, ENVI
  and EHdr data by their `.hdr`), GTI catalogs, the multidimensional, tile
  archive, VRT, TIFF, sparse and point cloud readers, and the GDAL backend.
- Format identification (`identify`) names only drivers the allow-lists allow,
  as GDAL identifies only with its registered drivers, and reads a remote
  file's first 16 KiB through the range reader: cached, and refused from a
  server that ignores ranges for a file over ten times that, as GDAL refuses
  it. The content check (`check_dataset`) reads a dataset only through openers
  that judge its driver, a GeoPackage only with GPKG allowed.
- `terrio.identify_drivers(path)` gives what the allow-lists judge, the drivers
  a file's name and its first bytes point to, beside `terrio.identify`'s format
  identification.
- A format no terrio driver recognises is refused; the `wbraster` and `wbvector`
  fallbacks need `TERRIO_WBRASTER_FALLBACK` or `TERRIO_WBVECTOR_FALLBACK`.
- Zip archives, on raster opens and in the `vsi` functions, JSON documents
  fetched whole and STAC links are limited (`TERRIO_ZIP_MAX_*`,
  `TERRIO_JSON_MAX_SIZE`, same-origin STAC links).
- Outside STAC API paths, a remote file whose name says no format, or one of
  most formats read whole (VRT, netCDF, HDF5, GeoPackage...), is refused after
  its first bytes unless they show a format the read takes under that name.
- Zarr chunks past 1 GiB (Fortran order counting twice) are refused, as in GDAL,
  others decoded no further than they need; Zstandard GeoTIFF chunks, decoded
  by terrio, stop at their size.
- Zarr decimated reads read only picked chunks; windows past memory are errors.
- A Zarr chunk is held once; big ones decode in parallel within half the memory.
- Zarr stores of more than 65,536 bands are refused at open, as in GDAL.
- `zarr_format`, GeoTIFF tile sizes and codecs are checked at open, as in GDAL.
- A compressed GeoTIFF chunk running past the end of the file is an error.
- `validate_cog` refuses a file whose images' tag arrays together take more
  bytes than the file (images pointing to one array), before reading them.
- Raster PMTiles and MBTiles pyramids built a tile row at a time are refused,
  before anything is read, when the process cannot hold what they hold at once
  (each level's rows, the filtered rows, the tiles an MBTiles archive gathers),
  and allocate their rows fallibly: an error, not an abort.
- WhiteIsZero GeoTIFF samples of 8 bits and more are read as stored, as in GDAL.
- Panics in bindings that release the GIL raise `terrio.errors.InternalError`.
- `MEM:::DATAPOINTER=` names open only when `GDAL_MEM_ENABLE_OPEN` allows it.
- WEBP files are read, and written lossless; JPEG files are written with GDAL's
  `QUALITY` and 4:2:0 chroma subsampling.
- GRIB is read by a new pure-Rust reader, in place of the driver of #232, and
  GRIB2 written, as GDAL does.
- Web Mercator to WGS 84 longitudes are GDAL's, bit for bit, in bounded time.
- A remote SRTM tile is known by its name and size from its first request, before
  anything more of it is read (by range); where a tile is recognised by its name
  and size, locally or remotely, the allow-list applies to SRTMHGT.
- GeoTIFF and COG writes carry a color table as GDAL 3.12.1's `CreateCopy` does: a
  single Byte or UInt16 band's.
- Parallel work runs on a thread pool of terrio's own, which survives `fork()`.
- `scripts/build_wheel_manylinux_2_28.sh` builds the Linux wheel for glibc 2.28
  and later without Docker.
- `terrio.rio.MemoryFile` removes its files when it is dropped unclosed, as
  rasterio's leaves nothing.
- The internal mask overviews that `overviews` adds and refreshes keep the tile
  height of the overview they mask in whole tiles, as GDAL writes them. Mask
  overviews smaller than a tile had a `TileLength` that was not a multiple of 16,
  and refreshed or re-added ones short tiles that GDAL failed to read.
