# Rust Library

```toml
[dependencies]
terrio = { git = "https://github.com/opengeos/terrio" }
```

The API reference, with an example on each entry point, is on [docs.rs](https://docs.rs/terrio)
once terrio is published; until then, `cargo doc -p terrio --open` builds it. The crate
documentation opens with a tour of the API.

## Opening datasets

```rust,no_run
use terrio::Dataset;

let ds = terrio::open("countries.gpkg")?;
match &ds {
    Dataset::Raster(r) => println!("{} x {}", r.raster.cols, r.raster.rows),
    Dataset::Vector(v) => println!("{} layers", v.layers.len()),
}
println!("{}", terrio::info::info(&ds, &Default::default()).to_text());
# Ok::<(), terrio::Error>(())
```

- `terrio::open(path)` detects the format and returns a raster or a vector dataset.
- `terrio::open_raster(path)` returns a `RasterDataset`: a `wbraster::Raster` (band-major typed
  pixels, georeferencing, CRS) with its driver and whether nodata is declared.
- `terrio::open_vector(path, &VectorOpenOptions)` returns a `VectorDataset` of `wbvector::Layer`s.
- `terrio::list_layers(path)` lists layers without reading features where the format allows.
- `terrio::open_raster_lazy(path)` opens a GeoTIFF or COG by its header; `read_window` decodes only
  the tiles a window touches, from full resolution or an overview.
- `terrio::memory` opens datasets from bytes and writes them to bytes.

`RasterDataset::band_nodata`, when nonempty, overrides the shared
`has_nodata` / `raster.nodata` fields, including explicit `None` entries. Read
nodata with `band_nodata(band)` (one-based) or `nodata()` (band 1). To replace it
with one shared value, clear the per-band vector and update both shared fields.

## Writing

```rust,no_run
let ds = terrio::open_raster("dem.tif")?;
let options = terrio::RasterWriteOptions {
    driver: Some(terrio::formats::RasterDriver::Cog),
    compression: Some("DEFLATE".into()),
    ..Default::default()
};
terrio::write_raster_dataset(&ds, "dem_cog.tif", &options)?;
# Ok::<(), terrio::Error>(())
```

`RasterWriteOptions::overview_resampling` accepts `nearest`, `average`, `min`, `max` or
`mode` for GeoTIFF and COG. The defaults are `nearest` for GeoTIFF and `average` for COG.
COGs build factors 2, 4, 8, … until the image fits in one tile. Each level reduces the original
source blocks independently: `nearest` samples the block center (clamped at image edges), and
`average` skips nodata and rounds only the final result. Categorical methods retain native
sample precision, including UInt64 values above 2⁵³. With `ALPHA=YES`, color averages are
weighted by the last band's alpha; alpha itself is averaged normally. Row-streamed COG output
uses the same reduction, independently of the input row chunk size, without retaining the whole
source image. Independent overview levels and complete average output rows run in parallel on
native builds and sequentially in WebAssembly. Each output cell still accumulates its source
samples in row-major order, including across streamed input slabs; block-local sums avoid
per-sample updates to persistent row buffers. Reading the source for each level still costs
O(levels × source pixels) work, more than recursively reducing already-rounded overviews.
Mode counts native sample values with hash tables, retaining the first value to reach the
highest count when frequencies tie; floating-point signed zeros count as the same value.

Set the creation option `COG_OVERVIEW_POLICY=GDAL_3_13` to recursively reduce
previously rounded levels and truncate halved dimensions as GDAL 3.13 does. This
profile supports nearest and average, with nodata, without alpha bands or explicit
masks. It retains bounded row streaming; `SOURCE` remains the default policy above.
Recursive levels consume the previous level in order, instead of reducing levels
independently in parallel. Input slabs and pending rows are copied between stages;
this has a throughput cost that depends on the workload. Compression and each
level's row reductions still use native parallelism. No speed advantage is implied.

Native WebP TIFF output supports pixel-interleaved Byte RGB and RGBA.
`WEBP_LOSSLESS=YES` preserves all samples, including transparent colors;
`WEBP_LEVEL=0..100` controls lossy quality (default 75). Lossy RGBA retains alpha
losslessly, using an uncompressed ALPH chunk; files can be larger than libwebp's
compressed-alpha output. Lossless RGBA preserves RGB under transparent pixels,
which differs from libwebp/GDAL defaults that may discard hidden colors.
Compressed TIFF/WebP bytes are not claimed identical across encoders.
The same codecs and creation options are available in WebAssembly.

`write_vector(&layers, path, &VectorWriteOptions)` writes layers; GeoPackage holds several.
`ops::vector_filter::write_vector_with_mode` appends to or overwrites the layers of an existing
output; GeoPackages are updated keeping their other tables.

## Streams

`terrio::stream` reads and writes vector layers a chunk of features at a time, for layers larger
than memory: `open_layers` gives a `FeatureReader` per layer (an iterator of features, or chunks
with `read_chunk`), `FeatureWriter` writes chunks, `ArrowStream` gives Arrow record batches (an
`arrow_array::RecordBatchReader`), and `convert` combines them with the `ogr2ogr`-style options
(`ops::vector_filter::Translator` applies them chunk by chunk), as `terrio convert` does.

```rust,no_run
use terrio::stream::{ConvertOptions, open_layers};

let mut readers = open_layers("buildings.gpkg", &Default::default(), 65536)?;
while let Some(chunk) = readers[0].read_chunk(10_000)? {
    println!("{} features", chunk.features.len());
}
terrio::stream::convert("buildings.gpkg", "buildings.fgb", &ConvertOptions::default())?;
# Ok::<(), terrio::Error>(())
```

Writers whose headers or column types depend on the values (CSV, Shapefile, FlatGeobuf,
GeoParquet) take a `LayerProfile` of all the features, which `convert` computes in a first pass
over the input, so that a streamed file is identical to the one `write_vector` writes.

## Arrow

`terrio::arrow` converts layers to and from Arrow record batches with a GeoArrow geometry column
(WKB or native encodings, the CRS as PROJJSON), and reads and writes Arrow IPC streams. The
`arrow-array` and `arrow-schema` versions are re-exported as `terrio::arrow::arrow_array` and
`terrio::arrow::arrow_schema`.

```rust,no_run
use terrio::arrow::{ArrowWriteOptions, GeometryEncoding, layer_from_arrow, layer_to_arrow};

let layer = terrio::open_vector("countries.gpkg", &Default::default())?.layers.remove(0);
let options = ArrowWriteOptions { geometry_encoding: GeometryEncoding::GeoArrow, ..Default::default() };
let table = layer_to_arrow(&layer, &options)?; // schema and record batches
let back = layer_from_arrow(&table.schema, &table.batches, "countries")?;
# Ok::<(), terrio::Error>(())
```

## Reading windows lazily

```rust,no_run
let mut cog = terrio::open_raster_lazy("s3://bucket/image.tif")?;
println!("{} x {}, overviews {:?}", cog.width(), cog.height(), cog.overviews());
let options = terrio::WindowOptions { bands: vec![1], overview: Some(0) };
let window = cog.read_window(terrio::Window::new(0, 0, 512, 512), &options)?; // a RasterDataset
# Ok::<(), terrio::Error>(())
```

## Bytes in memory

```rust,no_run
let bytes = std::fs::read("countries.geojson")?;
let ds = terrio::memory::open_vector_bytes(bytes, "countries.geojson", &Default::default())?;
let fgb: Vec<u8> = terrio::memory::write_vector_bytes(&ds.layers, "countries.fgb", &Default::default())?;
# Ok::<(), terrio::Error>(())
```

## Operations

`terrio::ops` holds the operations behind the commands: `translate`, `warp`, `mosaic` and `vrt` for
rasters; `vector_filter` (with the `sql` parser), `rasterize` and `polygonize`; `dem`, `calc`
(with its expression parser and NumPy-typed evaluator), `proximity` and `sieve`. Each takes an
options struct mirroring the command-line options.

`ops::neighbors::{neighbors, neighbors_to_path}` computes focal statistics (`gdal raster
neighbors`) from `NeighborsOptions` (kernels, `NeighborMethod`s, size, band, output type and
`NeighborsNodata`); the `_to_path` form reads a `LazyRaster` and writes GeoTIFF/COG strips.
`ops::nearblack::{nearblack, nearblack_in_place}` cleans collars (`nearblack`, `gdal raster
clean-collar`) from `NearblackOptions` (colors, `white`, `near`, `max_non_black`, alpha, mask and
`CollarAlgorithm`), returning a `Collar` (the Byte raster and its mask);
`keep_alpha_or_mask` applies clean-collar's defaults. `ops::enhance` computes
`equalization_luts` (or `_from_path`, with GDAL's stored histograms), reads and writes LUT
files with `parse_luts`/`format_luts`, and applies `BandLut`s with `enhance` or
`enhance_to_path` (`gdalenhance`).

`ops::geometry::{centroids, singleparts, vertices}` transforms vector layers while preserving
their CRS and attributes. `ops::footprint::footprint` extracts an exact raster valid-data
footprint with `FootprintOptions` (bands, intersection, optional mask, `singlepart`, and
`attributes: Vec<(String, String)>` for text fields copied to every feature). The Rust
operation does not infer a source path from an in-memory raster; supply a `location`
attribute explicitly when needed. Field names must be nonempty, have no surrounding whitespace, and be unique ignoring case.
`ops::sample::sample` accepts a point layer and a slice of `SampleInput` values, each naming a
raster, a band and an output field. It transforms points into each raster's CRS for sampling,
retains their original geometries, and records missing samples as nulls.
`sample_with_options` additionally accepts `SampleOptions { dst_srs, decimal_places }` to
reproject output geometries after sampling and round only newly sampled numeric fields.
`sample` keeps its existing behavior. These operations are
available in the core crate, including WebAssembly builds, without additional features.

`ops::contour::contour` accepts a raster and `ContourOptions` (interval/fixed levels, band,
nodata override and optional Z coordinates), returning a line layer with IDs and elevations.
`ops::zonal::zonal_stats` accepts a polygon layer, `ZonalInput` raster bands and `ZonalOptions`
(statistics, pixel inclusion, optional `dst_srs` and `decimal_places`); the result retains the
source features and adds statistic fields. Output reprojection occurs after analysis, and
rounding (0–15 decimal places, ties away from zero after binary64 scaling) leaves source
attributes and counts intact. Zonal statistics reuse a cropped membership mask across
bands of the same raster.
`ops::sql::query` runs the [documented OGR SQL subset](sql.md), including computed columns,
DISTINCT, summaries and attribute joins, with strict input-layer lookup. These operations
are also available in the core crate without extra features.

`ops::topology` provides `buffer` (`BufferOptions`), `overlay` (`OverlayOperation`),
`dissolve` (group field names), `validity`, `make_valid` and `clean` (`CleanOptions`). These
pure-Rust operations are included in WebAssembly builds without extra features. Distances
and snap grids use planar source CRS units, including degrees. Overlay and dissolve accept
polygons only; overlay requires matching CRSs. Buffer/overlay/dissolve construct XY geometry;
validity and repair of already-valid features preserve Z/M. See [Vector topology](cli.md#vector-topology)
for null, attribute, ID and collapsed-part handling.

```rust,no_run
use terrio::ops::topology::{self, BufferOptions, CleanOptions, OverlayOperation};

let ds = terrio::open_vector("parcels_utm.gpkg", &Default::default())?;
let layer = &ds.layers[0];
let repaired = topology::make_valid(layer)?;
let buffers = topology::buffer(&repaired, &BufferOptions { distance: 100.0, ..Default::default() })?;
let dissolved = topology::dissolve(&buffers, &["zone".into()])?;
let overlaps = topology::overlay(&repaired, &dissolved, OverlayOperation::Intersection)?;
let report = topology::validity(&overlaps);
let cleaned = topology::clean(&repaired, &CleanOptions { snap_grid: 0.01 })?;
# Ok::<(), terrio::Error>(())
```

The commands are also available through the Python package's `terrio` CLI and MCP. Dedicated
Python and JavaScript operation bindings are not yet provided for these additions.

`ops::vector_geom` holds the equivalents of GDAL 3.13's `gdal vector simplify`, `segmentize`,
`convex-hull`, `concave-hull`, `swap-xy`, `set-geom-type`, `combine` and `sort` (see
[Vector geometry commands](cli.md#vector-geometry-commands)). The per-feature operations are
`GeometryOp` values (`Simplify`, `Segmentize`, `ConvexHull`, `ConcaveHull(ConcaveHullOptions)`,
`SwapXy`, `SetGeomType(SetGeomTypeOptions)`), applied to a layer with `vector_geom::apply`, or
to layers read and written as streams through `VectorTranslateOptions::geometry_op` (which is
also `ogr2ogr`'s `-simplify`/`-segmentize`). `vector_geom::combine` (`CombineOptions`) and
`vector_geom::sort` (`SortMethod`) take whole layers; `vector_geom::force_to` is OGR's `forceTo`.
The single-geometry algorithms are `ops::simplify::{simplify_preserve_topology, segmentize}`
and `ops::hull::{convex_hull, concave_hull, concave_hull_of_polygons}`. They are pure Rust and
included in WebAssembly builds.

```rust,no_run
use terrio::ops::vector_geom::{self, CombineOptions, ConcaveHullOptions, GeometryOp, SortMethod};

let ds = terrio::open_vector("places.gpkg", &Default::default())?;
let groups = vector_geom::combine(&ds.layers[0], &CombineOptions { group_by: vec!["adm0_a3".into()], ..Default::default() })?;
let hulls = vector_geom::apply(&groups, &GeometryOp::ConcaveHull(ConcaveHullOptions { ratio: 0.3, ..Default::default() }))?;
let simple = vector_geom::apply(&hulls, &GeometryOp::Simplify { tolerance: 0.01 })?;
terrio::write_vector(&[vector_geom::sort(&simple, SortMethod::Hilbert)], "hulls.gpkg", &Default::default())?;
# Ok::<(), terrio::Error>(())
```

```rust,no_run
use terrio::ops::{Resampling, translate, warp};

let dem = terrio::open_raster("dem.tif")?;
let o = warp::WarpOptions {
    dst_srs: Some(terrio::crs::SpatialRef::parse("EPSG:32613")?),
    resampling: Resampling::Bilinear,
    ..Default::default()
};
let utm = warp::warp(&[&dem], &o)?;
let o = translate::TranslateOptions { src_win: Some((0.0, 0.0, 512.0, 512.0)), ..Default::default() };
let corner = translate::translate(&utm, &o)?;
# Ok::<(), terrio::Error>(())
```

### Pansharpening

```rust
use terrio::ops::pansharpen::{pansharpen, PansharpenOptions, SpectralBand};
let pan = terrio::open_raster("pan.tif")?;
let rgb = terrio::open_raster("rgb.tif")?;
let spectral: Vec<_> = (1..=3).map(|band| SpectralBand { dataset: &rgb, band }).collect();
let sharp = pansharpen(&pan, &spectral, &PansharpenOptions::default())?;
use terrio::ops::pansharpen::vrt::PansharpenVrt;
let description = PansharpenVrt::from_paths("pan.tif", &[("rgb.tif".into(), 1)], &Default::default())?;
description.write("sharp.vrt")?; // source headers only
let virtual_raster = terrio::open_raster_lazy("sharp.vrt")?;
# Ok::<(), terrio::Error>(())
```

Each `SpectralBand` can reference a separate raster. See [pansharpen](cli.md#pansharpen)
for working types, interpolation, nodata, bit depth and extent handling.

### Viewshed

```rust
use terrio::ops::viewshed::{viewshed, ViewshedOptions};
let dem = terrio::open_raster("dem_utm.tif")?;
let visible = viewshed(&dem, &ViewshedOptions {
    observer_x: 500000.0, observer_y: 4400000.0, max_distance: 5000.0,
    ..Default::default()
})?;
# Ok::<(), terrio::Error>(())
```

`ViewshedMode::Dem` and `Ground` return minimum visible elevation or height above the DEM.
See [viewshed](cli.md#viewshed) for units, curvature, radius cropping and nodata behavior.

### Vector gridding and linear referencing

```rust
use terrio::ops::vector_grid::{vector_grid, GridAlgorithm, VectorGridOptions};
use terrio::ops::lineref::{create_reference, position, LineRefOptions};
let points = terrio::open_vector("stations.gpkg", &Default::default())?;
let grid = vector_grid(&points.layers, &VectorGridOptions {
    algorithm: GridAlgorithm::parse("invdist:power=2:radius=50000:max_points=12")?,
    z_field: Some("temp".into()),
    size: Some((200, 200)),
    ..Default::default()
})?;
let route = terrio::open_vector("path.shp", &Default::default())?;
let stones = terrio::open_vector("mstones.shp", &Default::default())?;
let parts = create_reference(&route.layers[0], &stones.layers[0], &LineRefOptions {
    position_field: "pos".into(), step: 1000.0, ..Default::default()
})?;
let pos = position(&parts, -1.4345, 51.9497)?;
# Ok::<(), terrio::Error>(())
```

`grid_points` grids coordinate arrays directly; `coordinates` and `subline` are the other
lookups. See [vector-grid](cli.md#vector-grid) and [lineref](cli.md#lineref).


### Compare, pixel info and XYZ

```rust,no_run
use terrio::ops::compare::{compare, CompareOptions};
use terrio::ops::pixel_info::{pixel_info, PixelInfoOptions, Position, PositionCrs};
use terrio::ops::xyz::{write_xyz, XyzOptions};

let report = compare("reference.tif", "output.tif", &CompareOptions { skip_binary: true, ..Default::default() })?;
print!("{}", report.to_text()); // gdal raster compare's lines
println!("{} difference(s)", report.difference_count());

let mut dem = terrio::open_raster_lazy("dem.tif")?;
let options = PixelInfoOptions { position_crs: Some(PositionCrs::parse("EPSG:4326")?), ..Default::default() };
let info = pixel_info(&mut dem, &[Position::new(-105.5, 39.5)], &options)?;
print!("{}", info.to_text()); // gdallocationinfo's report
write_xyz(&mut dem, &mut std::io::stdout(), &XyzOptions::default())?;
# Ok::<(), terrio::Error>(())
```

`compare_rasters` compares two open `LazyRaster`s (without the binary, RPC and subdataset
comparisons), `pixel_info_layer` queries at a layer's points, and `PixelInfo` prints GeoJSON,
CSV, text and `-valonly` output or builds a point layer. See [compare](cli.md#compare),
[pixel-info](cli.md#pixel-info) and [xyz](cli.md#xyz).

### Vector datasets

```rust,no_run
use terrio::ops::concat::{concat, ConcatMode, ConcatOptions, ConcatSource};
use terrio::ops::partition::{partition_to_dir, PartitionOptions, PartitionWriteMode};
use terrio::ops::vector_schema::{export_schema_path, set_field_type, FieldSelector, OgrType};

let ds = terrio::open_vector("countries.gpkg", &Default::default())?;
let sources = vec![ConcatSource { name: "countries.gpkg".into(), layers: ds.layers.clone() }];
let merged = concat(&sources, &ConcatOptions { mode: ConcatMode::Single, ..Default::default() })?;
let typed = set_field_type(&merged[0], &FieldSelector::Name("POP_EST".into()), OgrType::parse("Integer64")?)?;
let options = PartitionOptions { fields: vec!["CONTINENT".into()], ..Default::default() };
let write = terrio::VectorWriteOptions { driver: Some(terrio::formats::VectorDriver::GPKG), ..Default::default() };
partition_to_dir(&[typed], "out", &options, &write, PartitionWriteMode::Create)?;
println!("{}", export_schema_path("countries.gpkg", &[])?.to_json()); // OGR_SCHEMA
# Ok::<(), terrio::Error>(())
```

`ops::concat`, `ops::partition` (`partition` plans the parts in memory, `partition_to_dir`
writes them), `ops::vector_schema` (`set_field_type`, `export_schema`, `export_schema_path`,
`layer_from_schema`, `create`, `parse_field_spec`, `parse_geometry_type`, `OgrSchema`) and
`ops::make_point` are `terrio concat`, `partition`, `set-field-type`, `export-schema`,
`create` and `make-point`; see [Vector datasets](cli.md#vector-datasets).

### Pixel-wise operations

```rust
use terrio::ops::as_features::{as_features, AsFeaturesOptions, PixelGeometry};
use terrio::ops::pixel::{nodata_to_alpha, reclassify, unscale, ReclassifyOptions};
let packed = terrio::open_raster("packed.tif")?;
let physical = unscale(&packed, &Default::default())?; // value * scale + offset, Float32
let rgba = nodata_to_alpha(&terrio::open_raster("landsat.tif")?, &Default::default())?;
let nlcd = terrio::open_raster("nlcd.tif")?;
let groups = reclassify(&nlcd, &ReclassifyOptions { mapping: "11=1;[21,25)=2;DEFAULT=0".into(), output_type: None })?;
let pixels = as_features(&nlcd, &AsFeaturesOptions { geometry: PixelGeometry::Polygon, ..Default::default() })?;
# Ok::<(), terrio::Error>(())
```

`terrio::ops::pixel::Reclassifier` parses and applies GDAL's mapping syntax on its own, and
`mapping_from_file_text` reads mapping files. See [unscale](cli.md#unscale),
[nodata-to-alpha](cli.md#nodata-to-alpha), [reclassify](cli.md#reclassify) and
[as-features](cli.md#as-features).

## Coordinate systems

```rust,no_run
use terrio::crs::{SpatialRef, Transformer};

let src = SpatialRef::parse("EPSG:4326")?;
let dst = SpatialRef::parse("EPSG:32613")?;
let (x, y) = Transformer::new(&src, &dst)?.transform(-105.5, 39.5)?;
# Ok::<(), terrio::Error>(())
```

## Point clouds

`terrio::pointcloud` reads LAS, LAZ and COPC into columns, writes them and grids them into
rasters: see [Point Clouds](point-clouds.md).

## Features

Every driver is on by default. A program that needs only part of terrio turns the default
features off and names the ones it uses, for a smaller dependency tree, faster builds and a
smaller WebAssembly module. A COG writer, say:

```toml
terrio = { version = "0.3", default-features = false, features = ["geotiff"] }
```

The core, always built, is GeoTIFF and COG (deflate, LZW, PackBits, ZSTD, WebP, JPEG and JPEG
XL), the raw and ASCII rasters (AAIGrid, EHdr, ENVI, ERS, HFA, RST, SAGA, GSBG, XYZ...), PNG,
JPEG, VRT, STAC, GeoJSON, TopoJSON, Shapefile, KML/KMZ, GML, GPX, CSV, MapInfo, the CRS engine,
the operations and the streams. The other drivers are features:

| Feature | Default | What it adds |
|---|---|---|
| `geotiff` | on | GeoTIFF and COG: always built, the feature exists so that `features = ["geotiff"]` says what a build needs. |
| `parquet` | on | GeoParquet, the `arrow` module and Arrow streams of vector layers (arrow, parquet). |
| `remote` | on | URLs, `s3://`, `gs://` and `az://` objects, range reads of remote COGs, COPC, PMTiles and Zarr, and datum grids from the PROJ CDN (native builds only). |
| `netcdf-hdf5` | on | NetCDF, HDF5 and HDF-EOS reading, netCDF writing. |
| `sqlite` | on | GeoPackage (vector and raster tiles) and SQLite/SpatiaLite. |
| `tiles` | on | MBTiles, PMTiles and MVT, raster and vector (the `tiles` module's archives; needs `sqlite`). |
| `avif` | on | AVIF tiles of tile archives (needs `tiles`). |
| `zarr` | on | Zarr v2 and v3. |
| `jp2` | on | JPEG 2000. |
| `flatgeobuf` | on | FlatGeobuf. |
| `osm` | on | OpenStreetMap PBF and XML. |
| `pointcloud` | on | LAS, LAZ, COPC, EPT, PLY and E57 (the `pointcloud` module). |
| `lerc` | on | LERC-compressed GeoTIFFs, read and written. |
| `lzma` | on | LZMA-compressed GeoTIFFs. |
| `gdal` | off | The optional GDAL backend: formats terrio has no driver for, through GDAL (needs GDAL installed). See [the GDAL backend](gdal.md#the-optional-gdal-backend). |

The drivers stay in the API whatever the features (`formats::RasterDriver`,
`formats::VectorDriver`): those a build leaves out are listed as neither read nor written, and
opening or writing their files fails with "built without the `zarr` feature". Modules and types
that belong to one feature (`terrio::arrow`, `terrio::pointcloud`, `terrio::http`,
`terrio::s3`, `tiles::TileArchive`) exist only with it.

The `geotiff` build still pulls the JPEG XL decoder and encoder (`jxl-oxide`, `zune-jpegxl`):
`wbraster`, terrio's raster data model, depends on them through its GeoTIFF reader.

## API stability

terrio is heading for a stable 1.0 API. What to rely on, and the decisions behind it:

- **The data model is `wbraster`'s and `wbvector`'s.** Rasters are `wbraster::Raster`s and vector
  layers `wbvector::Layer`s, re-exported by terrio (`terrio::Raster`, `terrio::Layer`,
  `terrio::raster::RasterData`, `terrio::vector::Coord`...). Wrapping them in terrio types would
  copy every pixel and coordinate through the bindings and the operations for no gain, so terrio
  exposes them as they are: they are part of terrio's API, and a breaking release of either crate
  that terrio adopts is a breaking release of terrio. Use them through terrio's re-exports rather
  than depending on those crates yourself, so that the versions always match.
- **Options structs** (`RasterWriteOptions`, `WarpOptions`, `PointReadOptions`...) all implement
  `Default`: build them with `..Default::default()`. New options are added as new fields with a
  default that keeps the old behaviour, which is not considered a breaking change.
- **Enums that grow** (`Error`, `formats::RasterDriver`, `formats::VectorDriver`,
  `formats::Kind`) are `#[non_exhaustive]`: match them with a wildcard arm.
- **Errors** carry a message for display; `Error::Raster` and `Error::Vector` wrap the format
  libraries' errors.
- **Minimum supported Rust version**: 1.90, raised only when needed, never to a release less
  than six months old (see [Contributing](contributing.md#minimum-supported-rust-version)).

## WebAssembly

The library builds for `wasm32-unknown-unknown`, with any features; network access and
temporary files are native only. No C compiler is needed, except for the `parquet` feature, whose
Zstandard codec is the zstd C library (it needs clang for WebAssembly); Zstandard GeoTIFFs are
decoded in pure Rust there. JPEG 2000 (the `jp2` feature) works there too, with two settings in the program that is built: the global
allocator `terrio::NullSafeAllocator` (the OpenJPEG port releases null pointers, which the
WebAssembly allocator does not tolerate), and the link flag `-C link-arg=--allow-undefined` for
`wasm32-unknown-unknown`, as in the repository's `.cargo/config.toml` (the port is built as a C
library too, whose C allocator calls terrio defines):

```rust
#[global_allocator]
static ALLOCATOR: terrio::NullSafeAllocator = terrio::NullSafeAllocator;
```

API documentation: [docs.rs/terrio](https://docs.rs/terrio), or `cargo doc -p terrio --open` in
the repository.

## Native multidimensional arrays

See [Native multidimensional arrays](multidim.md) for scientific groups, named dimensions,
lazy coordinate arrays, time/depth slicing, transposition and netCDF/Zarr numeric snapshots.
