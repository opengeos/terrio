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
terrio info s3://bucket/image.tif                # remote: http(s)://, s3://, gs://, az://, /vsicurl/
terrio info "archive.zip!dir/roads.shp"          # zip archives, or /vsizip/archive.zip/dir/roads.shp
terrio convert countries.shp countries.gpkg      # convert between formats
terrio convert dem.tif dem_cog.tif -f COG --compress DEFLATE
terrio convert https://host/cog.tif w.tif --srcwin 1024 1024 512 512 --overview 0  # window of an overview
terrio convert roads.geojson roads.fgb --t-srs EPSG:3857
terrio convert countries.geojson europe.gpkg --where "CONTINENT = 'Europe'" --select NAME,POP_EST
terrio rasterize counties.gpkg pop.tif -a POPULATION --tr 0.01 0.01 --ot Int32
terrio polygonize landcover.tif landcover.gpkg --8
terrio crs EPSG:32610 --to EPSG:4326 699960 3900000
terrio formats                                   # list the drivers
```

## Library

```rust,no_run
let ds = terrio::open("countries.gpkg")?;
println!("{}", terrio::info::info(&ds, &Default::default()).to_text());

// Cloud Optimized GeoTIFFs are read in place: opening fetches the headers, and a window read
// fetches only the tiles it needs, with HTTP range requests.
let mut cog = terrio::open_raster_lazy("s3://bucket/image.tif")?;
let options = terrio::WindowOptions { bands: vec![1], overview: Some(0) };
let window = cog.read_window(terrio::Window::new(0, 0, 512, 512), &options)?;
# Ok::<(), terrio::Error>(())
```

Remote reads are configured with environment variables: `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_PROFILE`, `AWS_REGION`, `AWS_NO_SIGN_REQUEST`
and `AWS_S3_ENDPOINT` for S3; `AZURE_STORAGE_ACCOUNT` and `AZURE_STORAGE_SAS_TOKEN` for Azure;
`TERRIO_HTTP_TIMEOUT`, `TERRIO_HTTP_MAX_RETRY` and `TERRIO_HTTP_LOG` (and more, see the `http`
module) for HTTP.

## Testing against GDAL

`scripts/fetch_test_data.sh` downloads real-world files (Natural Earth, USGS 3DEP, Sentinel-2,
NASA ECOSTRESS and OPERA products, and more) and derives copies in other formats with GDAL. Then:

```sh
TERRIO_TEST_DATA=../terrio-data cargo test             # tests with values checked against GDAL
python scripts/compare_with_gdal.py ../terrio-data     # compare every file's summary with GDAL's
python scripts/compare_windows_with_gdal.py local ../terrio-data  # windowed and overview reads
python scripts/compare_windows_with_gdal.py remote     # the same on public COGs, with bytes fetched
cargo test -- --ignored                                # tests that read public cloud files
python scripts/compare_vector_ops_with_gdal.py ../terrio-data  # convert options, rasterize, polygonize
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
