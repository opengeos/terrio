# terrio

Pure-Rust geospatial raster and vector data I/O.

terrio aims to read, write, convert and reproject the geospatial formats most workflows use, without
C dependencies, so that it builds anywhere Rust does, including WebAssembly.

**Status:** early development. Reading, writing and converting work; the processing operations are
in progress. See [ROADMAP.md](ROADMAP.md).

terrio builds on the pure-Rust format libraries of
[whitebox_next_gen](https://github.com/jblindsay/whitebox_next_gen) (`wbraster`, `wbvector`,
`wbprojection`) and adds format detection, remote and archived inputs, a GeoTIFF reader built on
the [`tiff`](https://crates.io/crates/tiff) crate, fixes for what the format libraries miss, and
GDAL-style operations.

## Command line

```sh
terrio info countries.gpkg                       # summarize a dataset (--json, --stats)
terrio info s3://bucket/image.tif                # remote: http(s)://, s3://, gs://, /vsicurl/
terrio info "archive.zip!dir/roads.shp"          # zip archives, or /vsizip/archive.zip/dir/roads.shp
terrio convert countries.shp countries.gpkg      # convert between formats
terrio convert dem.tif dem_cog.tif -f COG --compress DEFLATE
terrio convert roads.geojson roads.fgb --t-srs EPSG:3857
terrio crs EPSG:32610 --to EPSG:4326 699960 3900000
terrio formats                                   # list the drivers
```

## Library

```rust,no_run
let ds = terrio::open("countries.gpkg")?;
println!("{}", terrio::info::info(&ds, &Default::default()).to_text());
# Ok::<(), terrio::Error>(())
```

## Testing against GDAL

`scripts/fetch_test_data.sh` downloads real-world files (Natural Earth, USGS 3DEP, Sentinel-2,
NASA ECOSTRESS and OPERA products, and more) and derives copies in other formats with GDAL. Then:

```sh
TERRIO_TEST_DATA=../terrio-data cargo test             # tests with values checked against GDAL
python scripts/compare_with_gdal.py ../terrio-data     # compare every file's summary with GDAL's
```

## Layout

| Path | Contents |
|---|---|
| `crates/terrio` | The library |
| `crates/terrio-cli` | The `terrio` command-line tool |
| `python` | Python bindings |

See [ROADMAP.md](ROADMAP.md) for what is planned and in what order.

## License

MIT
