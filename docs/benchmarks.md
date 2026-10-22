# Benchmarks

How long terrio's commands take compared with the GDAL utilities doing the same work, on the
real-world files used for [testing](testing.md). Each command runs in a fresh process, once to warm
the file cache and then five times; the tables show the median wall time of the whole command,
including start-up, reading and writing.

!!! note "Read these numbers as a snapshot"
    terrio is young, and correctness against GDAL came first. Both tools use the machine's cores:
    terrio decodes and encodes GeoTIFF tiles, warps, rasterizes and reads and writes vector
    features in parallel, with outputs identical to those of a single-threaded run, and GDAL runs
    with its default settings, which include multi-threaded GeoTIFF compression where the driver
    supports it. On machines with fewer cores, the parallel cases gain less.

Machine: Intel Xeon w7-3445 (x86_64), 40 threads, Linux. GDAL 3.12.2 "Chicoutimi", released 2026/02/03; terrio 0.4.1. Median of 5 runs after a warm-up run; wall time of the whole command.


### Summaries

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Raster info: USGS 3DEP tile (3612×3612) | 17 ms | 39 ms | 2.3× faster |
| Raster statistics: SRTM 90 m (4269×2465) | 108 ms | 149 ms | 1.4× faster |
| Vector info: US counties GeoPackage | 15 ms | 32 ms | 2.1× faster |

### Raster conversion

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| GeoTIFF → COG (Deflate): USGS 3DEP tile | 195 ms | 1.31 s | 6.7× faster |
| GeoTIFF → GeoTIFF (LZW, tiled): Landsat (4 bands) | 59 ms | 295 ms | 5.0× faster |
| GeoTIFF → Zarr: SRTM 90 m | 61 ms | 97 ms | 1.6× faster |

### Raster operations

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Translate: window + Int16: USGS 3DEP tile | 48 ms | 119 ms | 2.4× faster |
| Warp to UTM 13N, nearest: USGS 3DEP tile | 765 ms | 478 ms | 1.6× slower |
| Warp to UTM 13N, bilinear: USGS 3DEP tile | 642 ms | 1.16 s | 1.8× faster |
| Mosaic 4 USGS 3DEP tiles → GeoTIFF | 310 ms | 1.51 s | 4.9× faster |
| Mosaic 4 USGS 3DEP tiles → VRT | 20 ms | 37 ms | 1.8× faster |

### Vector conversion

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Shapefile → GeoPackage: 7,342 populated places | 89 ms | 83 ms | 1.1× slower |
| GeoPackage → FlatGeobuf: US counties | 59 ms | 61 ms | 1.0× faster |
| GeoPackage → GeoParquet: US counties | 79 ms | 106 ms | 1.3× faster |
| GeoJSON → Shapefile: world cables | 23 ms | 47 ms | 2.1× faster |
| Filter + reproject: US counties → EPSG:5070 | 47 ms | 55 ms | 1.2× faster |

### Raster ↔ vector

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Rasterize US counties at 0.01° | 147 ms | 311 ms | 2.1× faster |
| Polygonize NLCD land cover (966×731) | 462 ms | 572 ms | 1.2× faster |

### Remote data

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Info of a remote COG (USGS 3DEP on S3) | 543 ms | 592 ms | 1.1× faster |

## Large files

The same comparison on gigabyte-sized files, with the peak resident memory of each command
(measured with `wait4`): a 14412×14412 Float32 mosaic of 16 USGS 3DEP tiles (an 800 MB tiled
Deflate GeoTIFF), a Sentinel-2 L1C red band (10980×10980, JPEG 2000), and the OpenStreetMap
extract of Colorado (a 400 MB PBF, and the 6.2 M-feature GeoPackage `ogr2ogr` makes of it). Each
command runs once to warm the file cache, then three times; the tables show the median time and
the largest peak memory. Same machine, terrio 0.4.1.

### Large rasters

| Task | terrio | GDAL | terrio vs GDAL | terrio memory | GDAL memory |
|---|---|---|---|---|---|
| Statistics: 14412² Float32 mosaic | 1.84 s | 3.85 s | 2.1× faster | 144 MB | 859 MB |
| GeoTIFF → COG (Deflate): the mosaic | 1.88 s | 21.37 s | 11.4× faster | 696 MB | 1.2 GB |
| Window 2048² of the mosaic | 39 ms | 105 ms | 2.7× faster | 71 MB | 65 MB |
| Warp the mosaic to EPSG:3857, bilinear | 2.19 s | 16.30 s | 7.4× faster | 667 MB | 1.6 GB |
| Mosaic 16 USGS 3DEP tiles → GeoTIFF | 1.16 s | 5.98 s | 5.1× faster | 436 MB | 1.9 GB |
| Hillshade of the mosaic | 504 ms | 4.24 s | 8.4× faster | 312 MB | 858 MB |
| Slope of the mosaic | 777 ms | 5.56 s | 7.2× faster | 578 MB | 858 MB |
| Sentinel-2 band: JPEG 2000 → GeoTIFF | 666 ms | 3.16 s | 4.7× faster | 355 MB | 520 MB |
| Statistics: Sentinel-2 band (15-bit samples) | 1.00 s | 2.67 s | 2.7× faster | 106 MB | 277 MB |

### Large vectors

| Task | terrio | GDAL | terrio vs GDAL | terrio memory | GDAL memory |
|---|---|---|---|---|---|
| Info: Colorado OpenStreetMap GeoPackage (6.2 M features) | 14 ms | 32 ms | 2.3× faster | 28 MB | 46 MB |
| Lines (2.0 M) → FlatGeobuf | 5.18 s | 5.61 s | 1.1× faster | 286 MB | 561 MB |
| Polygons (2.3 M) → GeoParquet | 2.97 s | 3.25 s | 1.1× faster | 635 MB | 960 MB |
| Lines → GeoJSON | 3.57 s | 40.60 s | 11.4× faster | 287 MB | 504 MB |
| Lines → Shapefile | 5.30 s | 5.01 s | 1.1× slower | 236 MB | 504 MB |
| Filter + reproject lines → GeoPackage | 3.77 s | 5.99 s | 1.6× faster | 147 MB | 188 MB |
| Polygons → GeoPackage | 5.28 s | 6.48 s | 1.2× faster | 227 MB | 779 MB |
| GeoPackage (5 layers) → GeoPackage | 8.84 s | 17.39 s | 2.0× faster | 274 MB | 788 MB |
| OpenStreetMap PBF (Colorado) → GeoPackage | 18.33 s | 41.58 s | 2.3× faster | 681 MB | 455 MB |

## Notes

- **Raster info** reads only the GeoTIFF header and the tags it needs.
- **Mosaic → VRT** reads only the inputs' headers; GDAL opens each input fully.
- **Warp to UTM 13N**: the tile is in NAD83, so the warp also changes datum to WGS 84, which
  takes about 500 ms of terrio's time (into NAD83 / UTM 13N, the same warp takes 163 ms). That
  makes nearest-neighbour warping slower than GDAL's
  ([#184](https://github.com/opengeos/terrio/issues/184)).
- **Remote COG info** fetches one 64 KiB range; GDAL runs with
  `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`, as recommended for cloud data. Remote timings depend on
  the network.
- **Vector info** decodes every geometry of the GeoPackage (in parallel, without reading the
  attributes) to compute the extent; `ogrinfo` reports the extent stored in `gpkg_contents`.
- **Rasterize** burns US counties, whose extent crosses the antimeridian, into a 35894×5348 grid
  and writes it as an uncompressed 192 MB GeoTIFF.
- `.aux.xml` statistics caches are disabled (`GDAL_PAM_ENABLED=NO`), and the statistics case uses a
  file without statistics stored in its metadata, so both tools compute them from the pixels.
- **Large rasters**: statistics, translation, terrain products, warping and mosaicking read their
  inputs by windows, and write GeoTIFF and COG outputs a strip of rows at a time, so neither is
  held whole. A COG's levels are compressed by rows of tiles as the rows come, each overview
  averaged from pairs of rows of the level above, and the tiles wait in temporary files until
  the file is laid out (the IFDs first, then the smallest overview's tiles). The JPEG 2000 band
  is decoded a row of tiles at a time, four rows at once, so that tiles slow to decode overlap
  the next rows' tiles.
- **Memory**: on Linux with glibc, the `terrio` command fixes the allocator's mmap threshold at
  4 MiB (`mallopt`), so that blocks that large go back to the system when they are freed rather
  than stay in the per-thread arenas, where glibc's adaptive threshold puts them once large blocks
  have been freed. That lowers peak memory by 12-60% on these files, for a few percent of time
  (about 20% for the mosaic and GeoParquet); for the OpenStreetMap PBF, the threshold accounts
  for about 200 MB of its 290 MB drop, and decoding at most 20 blocks at once for the rest. A
  threshold set in the environment (`MALLOC_MMAP_THRESHOLD_`, or `glibc.malloc.mmap_threshold` in
  `GLIBC_TUNABLES`) is kept; the Python and JavaScript packages leave the allocator to their host.
- **Mosaic of 16 tiles**: terrio copies each tile's pixels into place; `gdalwarp` warps them.
- **Large vectors** are converted a chunk of features at a time: at most 65,536 features, and
  about 64 MB of them, so that a run of large polygons is not held at once. GeoPackage and
  FlatGeobuf outputs take chunks of about 16 MB, the next one read while one is written, as
  their rows are written one after another. Shapefile and GeoPackage
  outputs first read the input once to settle the column types and the header (see
  [#88](https://github.com/opengeos/terrio/issues/88)); GeoParquet outputs do so only for integer,
  date and date-time fields, and gather the geometry types and extent as they write. FlatGeobuf
  outputs read the input once, deciding the types as the features come.
  GeoPackage outputs keep only each geometry's box (24 bytes) for the R-tree, whose tables are
  written a row at a time once the boxes are packed (with each feature's leaf, 16 bytes, while
  its `_rowid` table is written).
- **OpenStreetMap PBF** files sorted by type then id are read a batch of blocks at a time. The
  nodes, packed to about 6 bytes each, and the ways relations use go to temporary files read back
  through memory maps, so the operating system's page cache rather than the process holds them.

## Running the benchmarks

```sh
scripts/fetch_test_data.sh ../terrio-data
cargo build --release
python scripts/benchmark.py ../terrio-data --runs 5 --remote --out benchmarks.md
```

The large-file cases download about 3.5 GB, and run alone when no test data directory is given:

```sh
scripts/fetch_large_data.sh ../terrio-large-data
python scripts/benchmark.py --large ../terrio-large-data --large-runs 3
```

The script needs the GDAL command-line tools on `PATH`.
