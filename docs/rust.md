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

| Feature | Default | What it adds |
|---|---|---|
| `gdal` | off | The optional GDAL backend: formats terrio has no driver for, through GDAL (needs GDAL installed). See [the GDAL backend](gdal.md#the-optional-gdal-backend). |

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

The library builds for `wasm32-unknown-unknown`; network access and temporary files are native
only. JPEG 2000 works there too, with two settings in the program that is built: the global
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
