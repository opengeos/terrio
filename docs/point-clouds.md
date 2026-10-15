# Point Clouds

terrio reads and writes lidar and laser scan point clouds in pure Rust:

| Name | Format | Read | Write | Extensions |
|---|---|---|---|---|
| LAS | ASPRS LAS 1.0 to 1.4, point formats 0 to 10, full waveforms | ✓ | ✓ | las |
| LAZ | LASzip-compressed LAS (chunked, layered, and LASzip 1.x's unchunked compressor) | ✓ | ✓ | laz |
| COPC | Cloud Optimized Point Cloud | ✓ | ✓ | copc.laz |
| EPT | Entwine Point Tile (LASzip, binary and Zstandard tiles), local or remote | ✓ | | ept.json |
| PLY | Polygon File Format, ASCII and binary (the vertices) | ✓ | ✓ | ply |
| E57 | ASTM E2807 laser scans (every scan, posed) | ✓ | ✓ | e57 |

Every dimension is read, extra bytes included (every type, scaled, arrays, several extra-bytes
records), with the VLRs and EVLRs and the CRS from the OGC WKT record or the GeoTIFF GeoKeys
(compound CRSs are split into their horizontal and vertical parts). Points are held in columns,
one typed array per dimension, named as laspy names them: `x`, `y`, `z` (with the scale and
offset applied), `intensity`, `return_number`, `number_of_returns`, `classification`,
`scan_angle_rank` or `scan_angle`, `gps_time`, `red`, `green`, `blue`, `nir`...

LASzip comes from the pure-Rust [`laz`](https://crates.io/crates/laz) crate, the codec of laspy's
`lazrs` backend; terrio reads and writes the LAS container and the COPC structures itself. E57
files are read and written with the pure-Rust [`e57`](https://crates.io/crates/e57) crate, and
triangulations come from [`delaunator`](https://crates.io/crates/delaunator).

Dimensions that are not LAS's are named as PDAL names them, in snake case: `normal_x`,
`spherical_range`, `origin_id`. PLY properties named like PDAL's dimensions, in any case (`red`,
`Intensity`, `returnnumber`, `nx`), take terrio's names; others keep theirs.

## EPT, local or remote

An EPT dataset is an octree of tiles with its hierarchy in JSON pages. terrio reads it as PDAL's
`readers.ept` does: a read walks the hierarchy through the nodes that overlap `--bounds`, down
to the depth `--resolution` needs (or `--max-level`), and fetches the tiles it selects in
parallel. Remote datasets (`https://`, `s3://`) are read in place:

```sh
terrio info s3://usgs-lidar-public/WI_8County_Columbia_2020/ept.json   # 11.5 billion points
terrio convert https://s3-us-west-2.amazonaws.com/usgs-lidar-public/WI_8County_Columbia_2020/ept.json \
  sub.laz --bounds -9951440.5 5382600 -9951140.5 5382900 --resolution 2
# the same 347 376 points as PDAL's readers.ept, in 10 s
```

## PLY and E57

PLY files are read and written as PDAL's `readers.ply` and `writers.ply` do: `x`, `y` and `z` as
float64, other properties with their PLY type; terrio writes PDAL's names (`returnnumber`,
`gpstime`) and sized types (`uint8`), in ASCII by default (`--ply-encoding
binary_little_endian` for binary). Unlike PDAL's ASCII writer, which keeps 6 significant digits
by default, terrio writes as many digits as needed to read the same values back
(`--ply-precision N` for N decimals).

E57 files are read as PDAL's `readers.e57` reads them: the points of every scan with its pose
applied, `x`, `y` and `z` from spherical coordinates when needed, colours and intensities scaled
to 16 bits by `65535 / (limit max - limit min)` (colours of 0 to 255 become 0 to 65535). Written
E57 files have one scan, intensities and colours of 0 to 65535, normals in the `nor` extension
and the other dimensions in terrio's own.

## Larger than memory

`terrio convert`, `terrio info --stats`, `terrio grid` (the statistics) and `terrio filter range`
read and write points in chunks (`--chunk-points`, a million by default), so their memory does
not grow with the file. On a 3.05 GB LAS file of 101.7 million points:

| Command | Time | Peak memory |
|---|---:|---:|
| `terrio info big.las --stats` | 18.5 s | 66 MB |
| `terrio convert big.las big.laz` | 139 s | 117 MB |
| `terrio convert big.laz big.copc.laz` | 267 s | 251 MB |
| `terrio grid big.las dsm.tif --tr 5 --stat max` | 25 s | 769 MB (the 5001 × 5001 grid) |
| `terrio filter range big.laz ground.laz --limits "Classification[2:2]"` | 73 s | 120 MB |
| PDAL `writers.gdal`, the same grid (identical) | 52 s | 6.3 GB |

COPC files written in chunks spill the points to a temporary file (in `TMPDIR`), then build the
octree one node of a coarse level at a time: the octree is the one built in memory. In Rust,
`PointCloudReader::chunks` and `PointWriter` do the same; in Python, `PointCloudFile.chunks()`
and `PointCloudWriter`.

## Waveforms

LAS point formats 4, 5, 9 and 10 locate each point's waveform in the waveform data packets, in
the file or in a `.wdp` file next to it. terrio reads the wave packet descriptors and a point's
samples (`PointCloudReader::waveform`, `PointCloudFile.waveform` in Python), and keeps the
packets when the points are written to LAS or LAZ (and the `.wdp` file next to a converted file).

## Filters

`terrio filter` (and the library's `pointcloud::filters`, Python's and JavaScript's
`PointCloud` methods) runs PDAL's filters, with PDAL's options and defaults:

| terrio | PDAL | What it does |
|---|---|---|
| `filter smrf` | `filters.smrf` | ground classification (Simple Morphological Filter): last and only returns become 2 (ground) or 1 |
| `filter outlier` | `filters.outlier` | statistical or radius outliers become 7 (noise); `--remove` drops them |
| `filter range` | `filters.range` | the points within ranges, such as `Classification[2:2],Z[0:100)` |
| `filter sample` | `filters.sample` | Poisson sampling: no two points closer than `--radius` |
| `filter voxeldownsize` | `filters.voxeldownsize` | the first point of each voxel (moved to its centre in `center` mode) |
| `filter voxelcenternearestneighbor` | `filters.voxelcenternearestneighbor` | the point nearest each voxel's centre |

They give PDAL's results exactly, PDAL's quirks included (`filters.sample` takes the first
point's `y` as the origin's `z`; SMRF fills empty cells with the mean of the 8 nearest as
nanoflann orders them).

```sh
terrio filter smrf tile.laz classified.laz
terrio filter outlier tile.laz clean.laz --method statistical --mean-k 8 --multiplier 2 --remove
terrio filter voxeldownsize tile.laz thin.laz --cell 1 --mode first
```

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
terrio grid tile.laz dtm.tif --tr 1 --class 2 --method linear   # a TIN, as gdal_grid -a linear
terrio grid tile.laz nn.tif --tr 1 --method nearest             # as gdal_grid -a nearest
terrio convert scan.e57 scan.laz                                # E57, PLY, EPT to LAS and back
terrio convert tile.laz tile.ply --ply-encoding binary_little_endian
```

`terrio grid` grids points into a raster as PDAL's `writers.gdal` does: each point counts towards
every cell whose centre is closer than `--radius` (by default the cell diagonal), or with `--bin`
towards its own cell only; the statistics (`--stat`, one band each) are `min`, `max`, `mean`,
`idw` (inverse distance weighted, power `--power`), `count` and `stdev`. Empty cells are nodata,
or are filled from their neighbours within `--window-size` cells. The grid starts at the points'
minimum (or `--te`), as PDAL's does.

`--method linear` interpolates linearly in the triangles of the points' Delaunay triangulation (a
TIN), as `gdal_grid -a linear`; cells outside it take a near point's value as GDAL finds it (the
nearest of the points in the smallest square around the cell that has some), within `--radius`,
or nodata with `--radius 0`. `--method nearest` takes the nearest point's value, as
`gdal_grid -a nearest` (within the square of half-side `--radius` with one).

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
tin = points.grid(1.0, method="linear")  # a TIN, as gdal_grid
gdf = points.to_layer().to_geopandas()   # points with attributes
ground = points.smrf()                   # PDAL's filters.smrf
clean = points.outlier().range("Classification![7:7]")

# Larger than memory: chunks in, chunks out.
f = terrio.open_points("huge.laz")
with terrio.PointCloudWriter("huge.copc.laz", f.template()) as w:
    for chunk in f.chunks(1_000_000):
        w.write(chunk)
dsm = terrio.grid_points("huge.laz", 1.0, stat="max")  # in bounded memory
```

## JavaScript

```js
const pc = readPoints(lazBytes, "tile.laz", { classes: [2] });
pc.get("z");                     // Float64Array
pc.toBytes("tile.copc.laz");     // LAS, LAZ or COPC bytes
pc.grid(1, { stat: ["max"] });   // a raster Dataset
pc.grid(1, { method: "linear" }); // a TIN
pc.smrf().range("Classification[2:2]");
readPoints(plyBytes, "scan.ply"); // PLY and E57 bytes too
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
| PLY (7 files), E57 (13 files), EPT (LASzip, binary, Zstandard): every value | PDAL `readers.ply`, `readers.e57`, `readers.ept` | identical |
| EPT reads by bounds and resolution, local and USGS 3DEP (11.5 billion points) | PDAL `readers.ept` | the same points |
| PLY and E57 written by terrio | PDAL `readers.ply`, `readers.e57` | identical |
| Waveform samples; packets kept through LAS and LAZ | the file's bytes (LASlib's offsets) | identical |
| SMRF, outliers (statistical, radius), range, sample, voxels, on autzen and USGS 3DEP Madison (4.07 million points) | PDAL's filters | identical classes and points |
| TIN (linear) grids | `gdal_grid -a linear` | 99.91% of cells identical (see below) |
| Nearest-neighbour grids | `gdal_grid -a nearest` | identical but for ties |
| A grid of 101.7 million points, read in chunks | PDAL `writers.gdal` | identical |

PDAL 2.9 rounds the coordinates of binary EPT tiles to integers (it writes the scaled values
back into their integer fields); terrio keeps them (and matches PDAL once rounded the same way).
TIN grids differ from `gdal_grid`'s where several points share an x and y (qhull and terrio's
triangulation keep different ones), and, at large coordinates, where qhull's triangulation loses
precision and is not Delaunay: on 568 119 points of USGS 3DEP lidar in state plane feet, 98.8% of
the cells are identical, and 99.996% once the coordinates are moved near the origin.

terrio also reads LASzip 1.2's first, unchunked compressor, which laspy's `lazrs` and PDAL's
`lazperf` cannot read. PDAL does not read point formats 4, 5, 9 and 10 (waveforms); those files are
compared with laspy only. terrio is faster than PDAL on these filters (SMRF on 4.07 million
points: 48 s, PDAL 164 s; statistical outliers: 0.8 s, PDAL 21 s).
