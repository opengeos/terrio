# Benchmarks

How long terrio's commands take compared with the GDAL utilities doing the same work, on the
real-world files used for [testing](testing.md). Each command runs in a fresh process, once to warm
the file cache and then five times; the tables show the median wall time of the whole command,
including start-up, reading and writing.

!!! note "Read these numbers as a snapshot"
    terrio is young, and correctness against GDAL came first. Both tools run with the same number
    of threads, all of the machine's: terrio decodes and encodes GeoTIFF tiles, warps, rasterizes
    and reads and writes vector features in parallel (`RAYON_NUM_THREADS`), with outputs identical
    to those of a single-threaded run, and GDAL runs with `GDAL_NUM_THREADS` set (GeoTIFF and COG
    compression and decompression, JPEG 2000 decoding) and `gdalwarp -multi -wo NUM_THREADS`, as
    most of GDAL's multithreading is off by default; the other GDAL commands use whatever
    `GDAL_NUM_THREADS` turns on for them. On machines with fewer cores, the parallel cases gain
    less. Some operations use fewer threads than they are given: terrio's terrain products
    (hillshade, slope) and `calc` at most 16. Thanks to
    [#212](https://github.com/opengeos/terrio/issues/212) for pointing out that earlier runs left
    GDAL's multithreading off. While these ran, another process used about 6 of the machine's 40
    threads, which slows the parallel cases of both tools.

## Small files

Machine: Intel Xeon w7-3445 (x86_64), 40 threads, Linux; 40 threads for each tool. GDAL 3.12.2 "Chicoutimi", released 2026/02/03; terrio 0.4.2. Median of 5 runs after a warm-up run; wall time of the whole command.


### Summaries

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Raster info: USGS 3DEP tile (3612×3612) | 16 ms | 38 ms | 2.3× faster |
| Raster statistics: SRTM 90 m (4269×2465) | 107 ms | 153 ms | 1.4× faster |
| Vector info: US counties GeoPackage | 15 ms | 30 ms | 2.0× faster |

### Raster conversion

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| GeoTIFF → COG (Deflate): USGS 3DEP tile | 207 ms | 357 ms | 1.7× faster |
| GeoTIFF → GeoTIFF (LZW, tiled): Landsat (4 bands) | 58 ms | 88 ms | 1.5× faster |
| GeoTIFF → Zarr: SRTM 90 m | 62 ms | 66 ms | 1.1× faster |

### Raster operations

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Translate: window + Int16: USGS 3DEP tile | 49 ms | 54 ms | 1.1× faster |
| Warp to UTM 13N, nearest: USGS 3DEP tile | 158 ms | 2.01 s | 12.7× faster |
| Warp to UTM 13N, bilinear: USGS 3DEP tile | 203 ms | 2.00 s | 9.8× faster |
| Mosaic 4 USGS 3DEP tiles → GeoTIFF | 310 ms | 619 ms | 2.0× faster |
| Mosaic 4 USGS 3DEP tiles → VRT | 19 ms | 37 ms | 2.0× faster |

### Vector conversion

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Shapefile → GeoPackage: 7,342 populated places | 58 ms | 89 ms | 1.6× faster |
| GeoPackage → FlatGeobuf: US counties | 54 ms | 61 ms | 1.1× faster |
| GeoPackage → GeoParquet: US counties | 80 ms | 109 ms | 1.4× faster |
| GeoJSON → Shapefile: world cables | 24 ms | 46 ms | 1.9× faster |
| Filter + reproject: US counties → EPSG:5070 | 49 ms | 58 ms | 1.2× faster |

### Raster ↔ vector

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Rasterize US counties at 0.01° | 143 ms | 356 ms | 2.5× faster |
| Polygonize NLCD land cover (966×731) | 470 ms | 600 ms | 1.3× faster |

### Remote data

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Info of a remote COG (USGS 3DEP on S3) | 557 ms | 613 ms | 1.1× faster |

## Large files

The same comparison on gigabyte-sized files, with the peak resident memory of each command
(measured with `wait4`): a 14412×14412 Float32 mosaic of 16 USGS 3DEP tiles (an 800 MB tiled
Deflate GeoTIFF), a Sentinel-2 L1C red band (10980×10980, JPEG 2000), and the OpenStreetMap
extract of Colorado (a 400 MB PBF, and the 6.2 M-feature GeoPackage `ogr2ogr` makes of it). Each
command runs once to warm the file cache, then three times; the tables show the median time and
the largest peak memory. Same machine and threads, terrio 0.4.2.

### Large rasters

| Task | terrio | GDAL | terrio vs GDAL | terrio memory | GDAL memory |
|---|---|---|---|---|---|
| Statistics: 14412² Float32 mosaic | 1.84 s | 4.07 s | 2.2× faster | 145 MB | 859 MB |
| GeoTIFF → COG (Deflate): the mosaic | 2.45 s | 2.35 s | 1.0× slower | 695 MB | 1.7 GB |
| Window 2048² of the mosaic | 37 ms | 71 ms | 1.9× faster | 71 MB | 70 MB |
| Warp the mosaic to EPSG:3857, bilinear | 2.11 s | 4.27 s | 2.0× faster | 670 MB | 2.2 GB |
| Mosaic 16 USGS 3DEP tiles → GeoTIFF | 1.21 s | 2.20 s | 1.8× faster | 441 MB | 987 MB |
| Hillshade of the mosaic | 503 ms | 5.35 s | 10.6× faster | 316 MB | 855 MB |
| Slope of the mosaic | 739 ms | 7.00 s | 9.5× faster | 578 MB | 855 MB |
| Sentinel-2 band: JPEG 2000 → GeoTIFF | 666 ms | 3.22 s | 4.8× faster | 363 MB | 510 MB |
| Statistics: Sentinel-2 band (15-bit samples) | 1.02 s | 2.75 s | 2.7× faster | 105 MB | 277 MB |

### Large vectors

| Task | terrio | GDAL | terrio vs GDAL | terrio memory | GDAL memory |
|---|---|---|---|---|---|
| Info: Colorado OpenStreetMap GeoPackage (6.2 M features) | 14 ms | 32 ms | 2.3× faster | 28 MB | 45 MB |
| Lines (2.0 M) → FlatGeobuf | 5.11 s | 5.93 s | 1.2× faster | 287 MB | 563 MB |
| Polygons (2.3 M) → GeoParquet | 3.01 s | 3.33 s | 1.1× faster | 642 MB | 965 MB |
| Lines → GeoJSON | 3.49 s | 42.95 s | 12.3× faster | 277 MB | 502 MB |
| Lines → Shapefile | 5.21 s | 7.90 s | 1.5× faster | 222 MB | 490 MB |
| Filter + reproject lines → GeoPackage | 3.86 s | 6.52 s | 1.7× faster | 149 MB | 187 MB |
| Polygons → GeoPackage | 5.54 s | 7.06 s | 1.3× faster | 230 MB | 771 MB |
| GeoPackage (5 layers) → GeoPackage | 9.74 s | 18.73 s | 1.9× faster | 269 MB | 791 MB |
| OpenStreetMap PBF (Colorado) → GeoPackage | 20.11 s | 42.95 s | 2.1× faster | 533 MB | 457 MB |

## Notes

- **Raster info** reads only the GeoTIFF header and the tags it needs.
- **Mosaic → VRT** reads only the inputs' headers; GDAL opens each input fully.
- **Warp to UTM 13N**: the tile is in NAD83, so the warp also changes datum to WGS 84. The
  machine had PROJ's grids in terrio's download cache, which GDAL does not search, so terrio
  shifted the points with a grid and GDAL did not.
- **Warps and GDAL's thread count**: `gdalwarp` gets slower past a few threads on these files. The
  small tile warps fastest with 4 threads (330 ms nearest, 516 ms bilinear, against 158 ms and
  203 ms for terrio with 40), and the large mosaic with 16 (2.93 s, against 2.11 s); the tables
  show both tools at 40 threads. terrio's warp of the tile takes 166 ms from 16 threads up, and
  303 ms with 4.
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
- **Mosaics to GeoTIFF**: terrio copies each tile's pixels into place, and so does GDAL here,
  with `gdalbuildvrt` then `gdal_translate` (timed together); the outputs are identical. Earlier
  versions of this page timed `gdalwarp`, which resamples the tiles.
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

The script needs the GDAL command-line tools on `PATH`. `--threads N` runs both tools on `N`
threads (all of the machine's by default).
