# terrio

Pure-Rust geospatial raster and vector data I/O.

terrio aims to read, write, convert and reproject the geospatial formats most workflows use, without
C dependencies, so that it builds anywhere Rust does, including WebAssembly.

**Status:** early development. There is no usable API yet.

## Planned scope

- Raster: GeoTIFF / Cloud Optimized GeoTIFF, Zarr
- Vector: GeoJSON, Shapefile, GeoPackage, FlatGeobuf, GeoParquet
- Cloud and HTTP I/O
- Coordinate reference system transformations
- Common operations: translate, warp, rasterize, polygonize

## Layout

| Path | Contents |
|---|---|
| `crates/terrio` | The library |
| `crates/terrio-cli` | The `terrio` command-line tool |
| `python` | Python bindings |

See [ROADMAP.md](ROADMAP.md) for what is planned and in what order.

## License

MIT
