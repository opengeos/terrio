# Rust Library

```toml
[dependencies]
terrio = { git = "https://github.com/opengeos/terrio" }
```

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

## Operations

`terrio::ops` holds the operations behind the commands: `translate`, `warp`, `mosaic` and `vrt` for
rasters; `vector_filter` (with the `sql` parser), `rasterize` and `polygonize`. Each takes an
options struct mirroring the command-line options.

## Coordinate systems

```rust,no_run
use terrio::crs::{SpatialRef, Transformer};

let src = SpatialRef::parse("EPSG:4326")?;
let dst = SpatialRef::parse("EPSG:32613")?;
let (x, y) = Transformer::new(&src, &dst)?.transform(-105.5, 39.5)?;
# Ok::<(), terrio::Error>(())
```

## WebAssembly

The library builds for `wasm32-unknown-unknown`. Network access, temporary files and JPEG 2000 are
native only.

API documentation: run `cargo doc --open` in the repository.
