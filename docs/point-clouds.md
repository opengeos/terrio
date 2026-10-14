# Point Clouds

terrio reads and writes lidar point clouds in pure Rust:

| Name | Format | Read | Write | Extensions |
|---|---|---|---|---|
| LAS | ASPRS LAS 1.0 to 1.4, point formats 0 to 10 | ✓ | ✓ | las |
| LAZ | LASzip-compressed LAS (chunked, layered, and LASzip 1.x's unchunked compressor) | ✓ | ✓ | laz |
| COPC | Cloud Optimized Point Cloud | ✓ | ✓ | copc.laz |

Every dimension is read, extra bytes included (every type, scaled, arrays, several extra-bytes
records), with the VLRs and EVLRs and the CRS from the OGC WKT record or the GeoTIFF GeoKeys
(compound CRSs are split into their horizontal and vertical parts). Points are held in columns,
one typed array per dimension, named as laspy names them: `x`, `y`, `z` (with the scale and
offset applied), `intensity`, `return_number`, `number_of_returns`, `classification`,
`scan_angle_rank` or `scan_angle`, `gps_time`, `red`, `green`, `blue`, `nir`...

LASzip comes from the pure-Rust [`laz`](https://crates.io/crates/laz) crate, the codec of laspy's
`lazrs` backend; terrio reads and writes the LAS container and the COPC structures itself.

## COPC, local or remote

A COPC file is a LAZ file whose points are organized in an octree, each node a separately
compressed chunk, with the hierarchy of nodes in pages. terrio reads COPC files in place:
opening one fetches its header and root hierarchy page, and a read fetches only the nodes that
intersect the bounds asked for, down to the level asked for, with HTTP range requests (in as few
requests as possible).

```sh
terrio info https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz
# 2 requests, 120 kB of an 81 MB file: header, CRS, dimensions, and the octree's depth and
# points per level
terrio convert https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz sub.laz \
  --bounds 637000 851000 637500 851500 --resolution 7.27
# 8 requests, 3.7 MB: the same 18901 points as PDAL's readers.copc
```

The levels read are chosen with `--max-level` (0 is the root), `--resolution` (the levels needed
for a point spacing, as PDAL's `readers.copc` chooses them) or `--max-points` (the deepest level
that keeps the count within it). Points come in file order.

Writing COPC builds the octree: each node keeps an even sample of its points (one per cell of a
128³ grid), down to nodes of at most 100 000 points, which keep all theirs. Within a node, points
are in GPS time order, which LASzip compresses best.

## Command line

```sh
terrio info tile.laz                      # header, CRS, dimensions, VLRs, points by return
terrio info tile.laz --stats              # and classification counts, each dimension's range
terrio convert tile.las tile.laz          # LAS <-> LAZ <-> COPC
terrio convert tile.laz tile.copc.laz
terrio convert tile.laz ground.laz --class 2 --returns last,only
terrio convert tile.laz sub.las --bounds 637000 851000 637500 851500 --z-range 0 500
terrio convert tile.laz wgs84.laz --t-srs EPSG:4326
terrio convert tile.las tile14.las --point-format 6 --las-version 1.4 --scale 0.001 0.001 0.001
terrio convert tile.laz points.gpkg --dims z,intensity,classification  # a layer of 3D points
terrio grid tile.laz dsm.tif --tr 1 --stat max
terrio grid tile.laz dtm.tif --tr 1 --stat idw --class 2 --window-size 3
terrio grid tile.laz density.tif --tr 5 --stat count --bin
```

`terrio grid` grids points into a raster as PDAL's `writers.gdal` does: each point counts towards
every cell whose centre is closer than `--radius` (by default the cell diagonal), or with `--bin`
towards its own cell only; the statistics (`--stat`, one band each) are `min`, `max`, `mean`,
`idw` (inverse distance weighted, power `--power`), `count` and `stdev`. Empty cells are nodata,
or are filled from their neighbours within `--window-size` cells. The grid starts at the points'
minimum (or `--te`), as PDAL's does.

## Library

```rust,no_run
use terrio::pointcloud::{self, PointFilter, PointReadOptions};

let mut f = pointcloud::open_points("lidar.copc.laz")?;
let options = PointReadOptions {
    filter: PointFilter { classes: vec![2], ..Default::default() },
    max_level: Some(4),
    ..Default::default()
};
let ground = f.read(&options)?;
let z: &[f64] = ground.coords("z").unwrap();
pointcloud::write_points(&ground, "ground.laz", &Default::default())?;
let grid = pointcloud::grid::GridOptions { resolution: 1.0, ..Default::default() };
let dtm = pointcloud::grid::grid(&ground, &grid)?; // a RasterDataset
terrio::write_raster_dataset(&dtm, "dtm.tif", &Default::default())?;
# Ok::<(), terrio::Error>(())
```

`pointcloud::vector::to_layer` and `from_layer` turn points into vector layers and back, and
`from_raster` turns raster cells into points.

## Python

```python
import terrio

f = terrio.open_points("https://host/lidar.copc.laz")
f.format, f.point_count, f.crs, f.bounds, f.dimensions
points = f.read(bounds=(x0, y0, x1, y1), max_level=4, classes=[2])
points["z"]            # numpy float64; points["intensity"] is uint16...
points.to_numpy()      # a structured array
points.write("ground.copc.laz")
dem = points.grid(1.0, stat="idw")       # a Raster
gdf = points.to_layer().to_geopandas()   # points with attributes
```

## JavaScript

```js
const pc = readPoints(lazBytes, "tile.laz", { classes: [2] });
pc.get("z");                     // Float64Array
pc.toBytes("tile.copc.laz");     // LAS, LAZ or COPC bytes
pc.grid(1, { stat: ["max"] });   // a raster Dataset
```

## Compared with laspy and PDAL

`scripts/compare_pointclouds.py` reads every test file (the PDAL and laspy test suites, USGS 3DEP
lidar over Madison as COPC, and laspy-written files of every point format with every extra-bytes
type) with terrio and compares:

| Check | Reference | Result |
|---|---|---|
| Every value of every dimension, 55 files | laspy 2.7 | identical |
| Header fields, counts by return, VLR and EVLR counts | laspy | identical |
| Count, version, format, scale, offset, bounds, EPSG code, COPC | `pdal info --metadata` | identical |
| LAS, LAZ and COPC written by terrio, read back | laspy, PDAL `readers.las`, `readers.copc` | identical values, 294 checks |
| COPC reads by bounds, resolution and both | PDAL `readers.copc` | the same points |
| Gridding: min, max, mean, IDW, count, stdev; radius, power, bin mode, window filling | PDAL `writers.gdal` | identical to 1e-9 |
| Reprojection (same datum: NAD83 to NAD83 geographic and Albers, RGF93 to WGS 84) | PDAL `filters.reprojection` | within 2e-7 degrees, 1 mm |

terrio also reads LASzip 1.2's first, unchunked compressor, which laspy's `lazrs` and PDAL's
`lazperf` cannot read. PDAL does not read point formats 4, 5, 9 and 10 (waveforms); those files are
compared with laspy only.
