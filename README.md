# terrio

Pure-Rust geospatial raster and vector data I/O.

Documentation: <https://terrio.opengeos.org>

terrio aims to read, write, convert and reproject the geospatial formats most workflows use, without
C dependencies, so that it builds anywhere Rust does, including WebAssembly.

**Status:** early development. Reading, writing and converting work; the processing operations are
in progress. See [ROADMAP.md](ROADMAP.md).

terrio builds on the pure-Rust format libraries of
[whitebox_next_gen](https://github.com/jblindsay/whitebox_next_gen) (`wbraster`, `wbvector`,
`wbprojection`) and adds format detection, remote and archived inputs, a GeoTIFF reader built on
the [`tiff`](https://crates.io/crates/tiff) crate, fixes for what the format libraries miss, and
GDAL-style operations.

Coordinate transformations match GDAL's: terrio carries the EPSG CRSs with the coordinate
operations GDAL uses for them (generated from GDAL and PROJ by `scripts/gen_epsg_table.py`) and
pure-Rust ports of the projections they need. For each of the 6,354 EPSG geographic and projected
CRSs GDAL 3.12 transforms, transformations from and to WGS 84 agree with GDAL's within a
centimetre, and 99 % of a sample of transformations between two CRSs within a metre (see
`crates/terrio/tests/epsg_accuracy.rs`).

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
terrio translate dem.tif sub.tif --projwin -105.7 39.8 -105.3 39.5 --ot Int16   # like gdal_translate
terrio translate img.tif small.tif --outsize 25% 25% -r average --scale 0 4000 0 255 --ot Byte
terrio warp dem.tif dem_utm.tif --t-srs EPSG:32613 -r bilinear              # like gdalwarp
terrio warp a.tif b.tif out.tif --t-srs "+proj=lcc +lat_1=33 +lat_2=45 +lon_0=-96" --tr 100 100 --tap
terrio warp dem.tif clip.tif --cutline area.geojson --crop-to-cutline
terrio mosaic tiles/*.tif mosaic.vrt                                        # like gdalbuildvrt
terrio mosaic tiles/*.tif mosaic.tif --resolution highest --nodata -9999   # like gdal_merge.py
terrio dem hillshade dem.tif shade.tif -z 2 --multidirectional              # like gdaldem
terrio dem slope dem.tif slope.tif -s 111120 -p
terrio dem color-relief dem.tif colors.txt relief.tif --alpha
terrio calc -A red.tif -B nir.tif --calc "(B - A * 1.0) / (B + A * 1.0)" --type Float32 ndvi.tif  # gdal_calc.py
terrio proximity roads.tif dist.tif --values 1 --distunits GEO --maxdist 5000  # like gdal_proximity.py
terrio sieve landcover.tif clean.tif --st 10 -8                            # like gdal_sieve.py
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

Datasets can also be opened from bytes and written to bytes (`terrio::memory`), for data
already in memory and for WebAssembly, which has no file system.

## Python

```sh
pip install terrio
```

```python
import terrio

dem = terrio.open_raster("dem.tif")
elevation = dem.read(1)  # a NumPy array of the raster's own dtype
dem.crs, dem.transform, dem.bounds, dem.nodata

cog = terrio.open_raster("https://example.com/cog.tif", lazy=True)  # read in place
tile = cog.read(1, window=(1024, 1024, 512, 512), overview=0)

layer = terrio.open_vector("countries.gpkg").layers[0]
layer.to_geojson(), layer.to_wkb(), layer.to_geopandas()

terrio.Raster.from_array(elevation * 2, dem.transform, crs=dem.crs).write("x2.tif")
terrio.convert("countries.shp", "europe.gpkg", where="CONTINENT = 'Europe'")
terrio.warp("dem.tif", "dem_4326.tif", t_srs="EPSG:4326", resampling="bilinear")
```

The operations (`convert`, `translate`, `warp`, `mosaic`, `rasterize`, `polygonize`, `dem`,
`calc`, `proximity`, `sieve`) take the command-line options as keyword arguments. See
[python/README.md](python/README.md).

## JavaScript

The `terrio` npm package is the library compiled to WebAssembly, for browsers and Node.js:

```js
import init, { open, transformPoints } from "terrio";

await init();
const dem = open(new Uint8Array(await (await fetch("dem.tif")).arrayBuffer()), "dem.tif");
dem.info();                                   // the summary `terrio info --json` prints
const band = dem.readBand(1);                 // a Float32Array, Uint8Array...
const roads = open(geojsonBytes, "roads.geojson");
const fgb = roads.toBytes("roads.fgb", { tSrs: "EPSG:3857" });
```

See [js/README.md](js/README.md) for the formats available in WebAssembly.

## Testing against GDAL

`scripts/fetch_test_data.sh` downloads real-world files (Natural Earth, USGS 3DEP, Sentinel-2,
NASA ECOSTRESS and OPERA products, and more) and derives copies in other formats with GDAL. Then:

```sh
TERRIO_TEST_DATA=../terrio-data cargo test             # tests with values checked against GDAL
python scripts/compare_with_gdal.py ../terrio-data     # compare every file's summary with GDAL's
python scripts/compare_with_gdal.py --roundtrip ../terrio-data countries.geojson fgb gpkg shp
                                                       # convert with terrio, compare GDAL's reading
python scripts/raster_roundtrip.py                     # raster drivers: GDAL writes, terrio reads, and back
python scripts/compare_windows_with_gdal.py local ../terrio-data  # windowed and overview reads
python scripts/compare_windows_with_gdal.py remote     # the same on public COGs, with bytes fetched
cargo test -- --ignored                                # tests that read public cloud files
python scripts/compare_vector_ops_with_gdal.py ../terrio-data  # convert options, rasterize, polygonize
python scripts/compare_ops_with_gdal.py ../terrio-data # compare translate, warp and mosaic with GDAL's tools
python scripts/compare_dem_calc_with_gdal.py ../terrio-data  # dem, calc, proximity and sieve vs GDAL
cd python && TERRIO_TEST_DATA=../../terrio-data pytest # Python bindings, arrays compared with osgeo.gdal's
TERRIO_TEST_DATA=../terrio-data node --test js/tests/node.test.mjs  # WebAssembly build (after wasm-pack build, see js/)
TERRIO_TEST_DATA=../terrio-data cargo test --release --test epsg_accuracy -- --nocapture
                                                       # every EPSG CRS's transformations against GDAL's
```

## Layout

| Path | Contents |
|---|---|
| `crates/terrio` | The library |
| `crates/terrio-cli` | The `terrio` command-line tool |
| `python` | Python bindings (PyO3, NumPy), the `terrio` package on PyPI |
| `js` | WebAssembly bindings (wasm-bindgen), the `terrio` package on npm |

See [ROADMAP.md](ROADMAP.md) for what is planned and in what order.

## License

MIT

terrio's EPSG table (`crates/terrio/data/epsg.tsv.zst`) is derived from the
[EPSG Geodetic Parameter Dataset](https://epsg.org), copyright the International Association of
Oil & Gas Producers (IOGP), whose [terms of use](https://epsg.org/terms-of-use.html) permit its
redistribution with this attribution; the table was produced with [PROJ](https://proj.org) and
[GDAL](https://gdal.org). The projection formulas in `crates/terrio/src/crs/` are ported from
PROJ (MIT licence).
