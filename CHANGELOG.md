# Changelog

## Unreleased

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
- A `sum` now reads the source's full resolution unless it is given an overview level:
  `--overview AUTO`, the default, and `AUTO-n` choose no overview for it, and a reduced read of the
  whole of a `sum` `WarpedVRT` (`read(out_shape=...)`, of any of its bands) is warped from the full
  resolution: it took one of the VRT's implicit overviews, and a read of some of the bands, or of a
  VRT georeferenced by GCPs, resampled the VRT's pixels. An overview has one pixel per block of the
  source's pixels, so a `sum` of it gave a fraction of the total, about a quarter at factor 2. A
  coarse `sum` of a raster with overviews now reads more: from a remote 4096 × 4096 Float32 COG with
  overviews at 2, 4 and 8, a warp to EPSG:4326 at a sixteenth of its resolution takes 17 requests
  and 67.1 MB where its coarsest overview took 2 requests and 1.1 MB, and a `WarpedVRT` read at an
  eighth of its size takes 5 requests and 67.1 MB where it took 2 and 1.1 MB. `--overview n`
  (`overview=n` in Python) still reads level `n`, more cheaply, and the `sum` is then that level's.
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
  passes, and one of a 32768 × 32768 band (12 GiB) now errors, where it summed an overview (with
  overviews) or held the whole window in memory (without); a `sum` of several sources over 4 GiB in
  all now errors at any output size, where it held them all.
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
