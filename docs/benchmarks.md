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

Machine: Intel Xeon w7-3445 (x86_64), 40 threads, Linux. GDAL 3.12.2 "Chicoutimi", released 2026/02/03; terrio 0.0.1. Median of 5 runs after a warm-up run; wall time of the whole command.


### Summaries

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Raster info: USGS 3DEP tile (3612×3612) | 14 ms | 43 ms | 3.1× faster |
| Raster statistics: SRTM 90 m (4269×2465) | 111 ms | 139 ms | 1.2× faster |
| Vector info: US counties GeoPackage | 18 ms | 31 ms | 1.8× faster |

### Raster conversion

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| GeoTIFF → COG (Deflate): USGS 3DEP tile | 224 ms | 1.20 s | 5.3× faster |
| GeoTIFF → GeoTIFF (LZW, tiled): Landsat (4 bands) | 44 ms | 285 ms | 6.4× faster |
| GeoTIFF → Zarr: SRTM 90 m | 57 ms | 94 ms | 1.6× faster |

### Raster operations

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Translate: window + Int16: USGS 3DEP tile | 41 ms | 114 ms | 2.8× faster |
| Warp to UTM 13N, nearest: USGS 3DEP tile | 102 ms | 425 ms | 4.2× faster |
| Warp to UTM 13N, bilinear: USGS 3DEP tile | 142 ms | 1.10 s | 7.7× faster |
| Mosaic 4 USGS 3DEP tiles → GeoTIFF | 277 ms | 1.51 s | 5.5× faster |
| Mosaic 4 USGS 3DEP tiles → VRT | 2 ms | 35 ms | 16.5× faster |

### Vector conversion

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Shapefile → GeoPackage: 7,342 populated places | 39 ms | 82 ms | 2.1× faster |
| GeoPackage → FlatGeobuf: US counties | 49 ms | 61 ms | 1.2× faster |
| GeoPackage → GeoParquet: US counties | 73 ms | 105 ms | 1.4× faster |
| GeoJSON → Shapefile: world cables | 18 ms | 41 ms | 2.3× faster |
| Filter + reproject: US counties → EPSG:5070 | 39 ms | 54 ms | 1.4× faster |

### Raster ↔ vector

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Rasterize US counties at 0.01° | 158 ms | 314 ms | 2.0× faster |
| Polygonize NLCD land cover (966×731) | 431 ms | 570 ms | 1.3× faster |

### Remote data

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Info of a remote COG (USGS 3DEP on S3) | 451 ms | 587 ms | 1.3× faster |

## Large files

The same comparison on gigabyte-sized files, with the peak resident memory of each command
(measured with `wait4`): a 14412×14412 Float32 mosaic of 16 USGS 3DEP tiles (an 800 MB tiled
Deflate GeoTIFF), a Sentinel-2 L1C red band (10980×10980, JPEG 2000), and the OpenStreetMap
extract of Colorado (a 400 MB PBF, and the 6.2 M-feature GeoPackage `ogr2ogr` makes of it). Each
command runs once to warm the file cache, then three times; the tables show the median time and
the largest peak memory. Same machine, terrio 0.2.0.

### Large rasters

| Task | terrio | GDAL | terrio vs GDAL | terrio memory | GDAL memory |
|---|---|---|---|---|---|
| Statistics: 14412² Float32 mosaic | 1.82 s | 3.80 s | 2.1× faster | 143 MB | 859 MB |
| GeoTIFF → COG (Deflate): the mosaic | 1.54 s | 20.98 s | 13.6× faster | 1.9 GB | 1.2 GB |
| Window 2048² of the mosaic | 34 ms | 102 ms | 3.0× faster | 80 MB | 65 MB |
| Warp the mosaic to EPSG:3857, bilinear | 2.18 s | 16.44 s | 7.5× faster | 718 MB | 1.6 GB |
| Mosaic 16 USGS 3DEP tiles → GeoTIFF | 929 ms | 6.73 s | 7.2× faster | 521 MB | 1.9 GB |
| Hillshade of the mosaic | 593 ms | 4.55 s | 7.7× faster | 371 MB | 855 MB |
| Slope of the mosaic | 738 ms | 5.93 s | 8.0× faster | 611 MB | 857 MB |
| Sentinel-2 band: JPEG 2000 → GeoTIFF | 478 ms | 3.23 s | 6.8× faster | 753 MB | 517 MB |
| Statistics: Sentinel-2 band (15-bit samples) | 1.01 s | 2.80 s | 2.8× faster | 111 MB | 278 MB |

### Large vectors

| Task | terrio | GDAL | terrio vs GDAL | terrio memory | GDAL memory |
|---|---|---|---|---|---|
| Info: Colorado OpenStreetMap GeoPackage (6.2 M features) | 13 ms | 30 ms | 2.3× faster | 26 MB | 45 MB |
| Lines (2.0 M) → FlatGeobuf | 5.70 s | 6.18 s | 1.1× faster | 474 MB | 556 MB |
| Polygons (2.3 M) → GeoParquet | 2.63 s | 3.49 s | 1.3× faster | 1.4 GB | 960 MB |
| Lines → GeoJSON | 3.48 s | 42.69 s | 12.3× faster | 458 MB | 503 MB |
| Lines → Shapefile | 5.24 s | 5.38 s | 1.0× faster | 347 MB | 505 MB |
| Filter + reproject lines → GeoPackage | 4.36 s | 6.16 s | 1.4× faster | 528 MB | 186 MB |
| Polygons → GeoPackage | 6.09 s | 6.90 s | 1.1× faster | 981 MB | 770 MB |
| GeoPackage (5 layers) → GeoPackage | 10.91 s | 18.81 s | 1.7× faster | 999 MB | 782 MB |
| OpenStreetMap PBF (Colorado) → GeoPackage | 19.36 s | 45.21 s | 2.3× faster | 1.4 GB | 455 MB |

## Notes

- **Raster info** reads only the GeoTIFF header and the tags it needs.
- **Mosaic → VRT** reads only the inputs' headers; GDAL opens each input fully.
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
  inputs by windows, and write GeoTIFF outputs a strip of rows at a time, so neither is held
  whole. A COG is written whole, since its overviews need the full-resolution raster, and the
  JPEG 2000 band is decoded whole.
- **Mosaic of 16 tiles**: terrio copies each tile's pixels into place; `gdalwarp` warps them.
- **Large vectors** are converted a chunk of features at a time: at most 65,536 features, and
  about 64 MB of them, so that a run of large polygons is not held at once. Shapefile and GeoPackage
  outputs first read the input once to settle the column types and the header (see
  [#88](https://github.com/opengeos/terrio/issues/88)); GeoParquet outputs do so only for integer,
  date and date-time fields, and gather the geometry types and extent as they write. FlatGeobuf
  outputs read the input once, deciding the types as the features come.

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
