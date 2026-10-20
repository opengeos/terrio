<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark-dark.svg">
    <img src="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark.svg" alt="terrio" height="96">
  </picture>
</p>

# terrio

[![crates.io](https://img.shields.io/crates/v/terrio.svg)](https://crates.io/crates/terrio) [![docs.rs](https://img.shields.io/docsrs/terrio)](https://docs.rs/terrio) [![PyPI](https://img.shields.io/pypi/v/terrio.svg)](https://pypi.org/project/terrio/) [![npm](https://img.shields.io/npm/v/terrio-js.svg)](https://www.npmjs.com/package/terrio-js) [![CI](https://github.com/opengeos/terrio/actions/workflows/ci.yml/badge.svg)](https://github.com/opengeos/terrio/actions/workflows/ci.yml) [![Docs](https://github.com/opengeos/terrio/actions/workflows/docs.yml/badge.svg)](https://terrio.opengeos.org) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/opengeos/terrio/blob/main/LICENSE)

Pure-Rust geospatial raster, vector and point cloud data I/O.

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

Install the `terrio` command, a standalone binary, with:

```sh
curl -LsSf https://terrio.opengeos.org/install.sh | sh                                # Linux, macOS
powershell -ExecutionPolicy ByPass -c "irm https://terrio.opengeos.org/install.ps1 | iex"  # Windows
```

`pip install terrio` installs it too, and `cargo install terrio-cli` builds it from source (see
[Installation](https://terrio.opengeos.org/installation/)).

```sh
terrio info countries.gpkg                       # summarize a dataset (--json, --stats)
terrio info s3://bucket/image.tif                # remote: http(s)://, s3://, gs://, az://, /vsicurl/
terrio info "archive.zip!dir/roads.shp"          # zip archives, or /vsizip/archive.zip/dir/roads.shp
terrio convert countries.shp countries.gpkg      # convert between formats
terrio convert dem.tif dem_cog.tif -f COG --compress DEFLATE
terrio convert https://host/cog.tif w.tif --srcwin 1024 1024 512 512 --overview 0  # window of an overview
terrio convert roads.geojson roads.fgb --t-srs EPSG:3857
terrio convert countries.geojson europe.gpkg --where "CONTINENT = 'Europe'" --select NAME,POP_EST
terrio info https://host/tiles.pmtiles           # PMTiles read in place (header, then the tiles needed)
terrio info "ZARR:\"/vsis3/bucket/store.zarr\":/group/array"  # Zarr v2/v3 read in place (chunks needed only)
terrio convert landsat.tif landsat.pmtiles       # raster tiles in EPSG:3857, every zoom level (or .mbtiles)
terrio convert countries.gpkg countries.pmtiles --maxzoom 6   # vector tiles (MVT), or -f MVT for a directory
terrio convert counties.gpkg tn.gpkg --spat -90.3 34.9 -81.6 36.7  # through the GeoPackage's R-tree
terrio convert more.shp world.gpkg --append --nln countries          # update an existing GeoPackage
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
terrio info "https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=..."  # STAC: footprints, mosaics
terrio convert 'STACIT:"https://.../search?...":asset=red' red.tif --srcwin 0 0 1024 1024  # like GDAL's STACIT
terrio convert dem.tif dem.zarr --co GEOREFERENCING_CONVENTION=SPATIAL_PROJ --co OVERVIEW_LEVELS=AUTO  # GeoZarr
terrio formats                                   # list the drivers

# Point clouds: LAS 1.0-1.4 (formats 0-10, extra bytes, waveforms), LAZ, COPC and EPT (read in
# place, local or remote), PLY, E57; files larger than memory converted in chunks
terrio info tile.laz --stats                     # header, CRS, classes and returns (like pdal info)
terrio info https://host/lidar.copc.laz          # COPC octree depth and points per level
terrio convert tile.laz tile.copc.laz            # LAS <-> LAZ <-> COPC
terrio convert https://host/lidar.copc.laz sub.laz --bounds 637000 851000 637500 851500 --max-level 4
terrio convert tile.laz ground.laz --class 2 --returns last,only --t-srs EPSG:4326
terrio convert tile.laz points.gpkg --dims z,intensity,classification   # to a vector layer of points
terrio grid tile.laz dsm.tif --tr 1 --stat max                          # like PDAL's writers.gdal
terrio grid tile.laz dtm.tif --tr 1 --stat idw --class 2 --window-size 3
terrio grid tile.laz tin.tif --tr 1 --class 2 --method linear           # like gdal_grid -a linear
terrio filter smrf tile.laz classified.laz                              # PDAL's filters.smrf
terrio filter outlier tile.laz clean.laz --remove                       # filters.outlier
terrio filter csf tile.laz classified.laz                               # filters.csf, pmf, elm...
terrio filter hag_nn classified.laz hag.laz                             # filters.hag_nn, crop, normal...
terrio convert scan.e57 scan.laz                                        # E57, PLY, EPT
terrio convert tile.laz tile-ept/ept.json                               # write an EPT dataset
```

`terrio mcp` serves the commands to AI agents (Claude Code, Claude Desktop, Cursor, VS Code) over
the Model Context Protocol: `claude mcp add terrio -- terrio mcp` (see
[AI Agents](https://terrio.opengeos.org/mcp/)).

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

An optional GDAL backend (the `gdal` cargo feature, off by default and the only C dependency)
opens what terrio has no driver for (HDF4, GRIB, NITF and the rest of GDAL's formats)
into the same data model, and writes rasters with GDAL's drivers: `terrio::open("GDAL:file.hdf")`,
`terrio info --driver gdal`, or a plain path when terrio's own drivers cannot open it.

Point clouds are read into columns, one typed vector per dimension (`terrio::pointcloud`):

```rust,no_run
use terrio::pointcloud::{self, PointFilter, PointReadOptions};

// A COPC file is read in place: only the octree nodes in the bounds, down to level 4, are fetched.
let mut lidar = pointcloud::open_points("https://host/lidar.copc.laz")?;
let filter = PointFilter { bounds: Some([637000.0, 851000.0, 637500.0, 851500.0]), classes: vec![2], ..Default::default() };
let ground = lidar.read(&PointReadOptions { filter, max_level: Some(4), ..Default::default() })?;
let z = ground.coords("z").unwrap();
pointcloud::write_points(&ground, "ground.laz", &Default::default())?;
let options = pointcloud::grid::GridOptions { resolution: 1.0, ..Default::default() };
let dtm = pointcloud::grid::grid(&ground, &options)?; // a RasterDataset
# Ok::<(), terrio::Error>(())
```

LAZ compression comes from the pure-Rust [`laz`](https://crates.io/crates/laz) crate (the codec of
laspy's `lazrs` backend); terrio reads and writes the LAS container, extra bytes, CRS records and
COPC octrees itself.

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
table = pyarrow.table(layer.to_arrow())  # Arrow PyCapsule interface, GeoArrow geometries
layer = terrio.from_arrow(table)  # pyarrow, GeoPandas, Polars, GDAL layers...

points = terrio.read_points("lidar.copc.laz", bounds=(x0, y0, x1, y1), classes=[2])
points["z"], points.to_numpy(), points.grid(1.0, stat="idw"), points.write("ground.laz")

terrio.Raster.from_array(elevation * 2, dem.transform, crs=dem.crs).write("x2.tif")
terrio.convert("countries.shp", "europe.gpkg", where="CONTINENT = 'Europe'")
terrio.warp("dem.tif", "dem_4326.tif", t_srs="EPSG:4326", resampling="bilinear")
```

The operations (`convert`, `translate`, `warp`, `mosaic`, `rasterize`, `polygonize`, `dem`,
`calc`, `proximity`, `sieve`) take the command-line options as keyword arguments. See
[python/README.md](python/README.md).

rasterio scripts run with an import change, `import terrio.rio as rasterio`: datasets, windows,
CRSs, features, warping, merging, masking and memory files follow rasterio's API, compared with
rasterio on real files. See [Coming from rasterio](https://terrio.opengeos.org/rasterio/).

`terrio.xarray` is rioxarray's API without GDAL (`pip install "terrio[xarray]"`):
`terrio.xarray.open_rasterio` and `xr.open_dataset(path, engine="terrio")` open rasters lazily
(remote COGs fetch only the tiles a window needs, chunked with dask), and the `.terrio` accessor
has rioxarray's `.rio` methods (`clip_box`, `clip`, `reproject`, `reproject_match`, `pad_box`,
`to_raster`...), compared with rioxarray on real files. See
[xarray](https://terrio.opengeos.org/xarray/).

## JavaScript

The `terrio-js` npm package is the library compiled to WebAssembly, for browsers and Node.js:

```js
import init, { open, transformPoints } from "terrio-js";

await init();
const dem = open(new Uint8Array(await (await fetch("dem.tif")).arrayBuffer()), "dem.tif");
dem.info();                                   // the summary `terrio info --json` prints
const band = dem.readBand(1);                 // a Float32Array, Uint8Array...
const roads = open(geojsonBytes, "roads.geojson");
const fgb = roads.toBytes("roads.fgb", { tSrs: "EPSG:3857" });
const ground = readPoints(lazBytes, "tile.laz", { classes: [2] });   // LAS, LAZ, COPC
ground.get("z");                              // a Float64Array
```

See [js/README.md](js/README.md) for the formats available in WebAssembly.

## Testing against GDAL

`scripts/fetch_test_data.sh` downloads real-world files (Natural Earth, USGS 3DEP, Sentinel-2,
NASA ECOSTRESS and OPERA products, and more) and derives copies in other formats with GDAL.
`scripts/conformance.py` runs every comparison below and writes the conformance report
(`docs/conformance.md`, per format and operation: cases identical to GDAL, within tolerance or
different):

```sh
python scripts/conformance.py ../terrio-data --output docs/conformance.md
```

The comparisons, one by one:

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
python scripts/compare_stac_geozarr.py stac            # STAC footprints and STACIT mosaics vs GDAL (Earth Search)
python scripts/compare_stac_geozarr.py geozarr ../terrio-data  # GeoZarr stores read by GDAL and rioxarray
python scripts/compare_remote_zarr.py                  # public cloud Zarr stores read in place vs GDAL and xarray
python scripts/compare_tiles_with_gdal.py ../terrio-data  # PMTiles, MBTiles and MVT vs GDAL's drivers
python scripts/compare_pointclouds.py ../terrio-data   # point clouds: every value vs laspy, headers, COPC
                                                       # queries, grids and reprojection vs PDAL
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
