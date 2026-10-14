# Benchmarks

How long terrio's commands take compared with the GDAL utilities doing the same work, on the
real-world files used for [testing](testing.md). Each command runs in a fresh process, once to warm
the file cache and then five times; the tables show the median wall time of the whole command,
including start-up, reading and writing.

!!! note "Read these numbers as a snapshot"
    terrio is young, and correctness against GDAL came first. The slower cases below are known and
    tracked: GeoTIFF writing (which dominates COG conversion, mosaicking and rasterizing to large
    grids) and warping have not been optimized yet. GDAL runs with its default settings, which
    include multi-threaded GeoTIFF compression where the driver supports it.

Machine: Intel Xeon w7-3445 (x86_64), 40 threads, Linux. GDAL 3.12.2 "Chicoutimi", released 2026/02/03; terrio 0.0.1. Median of 5 runs after a warm-up run; wall time of the whole command.


### Summaries

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Raster info: USGS 3DEP tile (3612×3612) | 2 ms | 38 ms | 21.2× faster |
| Raster statistics: SRTM 90 m (4269×2465) | 159 ms | 154 ms | 1.0× slower |
| Vector info: US counties GeoPackage | 65 ms | 38 ms | 1.7× slower |

### Raster conversion

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| GeoTIFF → COG (Deflate): USGS 3DEP tile | 2.18 s | 1.41 s | 1.5× slower |
| GeoTIFF → GeoTIFF (LZW, tiled): Landsat (4 bands) | 401 ms | 304 ms | 1.3× slower |
| GeoTIFF → Zarr: SRTM 90 m | 174 ms | 98 ms | 1.8× slower |

### Raster operations

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Translate: window + Int16: USGS 3DEP tile | 305 ms | 122 ms | 2.5× slower |
| Warp to UTM 13N, nearest: USGS 3DEP tile | 1.10 s | 454 ms | 2.4× slower |
| Warp to UTM 13N, bilinear: USGS 3DEP tile | 1.83 s | 1.69 s | 1.1× slower |
| Mosaic 4 USGS 3DEP tiles → GeoTIFF | 2.63 s | 1.61 s | 1.6× slower |
| Mosaic 4 USGS 3DEP tiles → VRT | 3 ms | 37 ms | 11.0× faster |

### Vector conversion

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Shapefile → GeoPackage: 7,342 populated places | 68 ms | 85 ms | 1.2× faster |
| GeoPackage → FlatGeobuf: US counties | 91 ms | 59 ms | 1.5× slower |
| GeoPackage → GeoParquet: US counties | 107 ms | 108 ms | 1.0× faster |
| GeoJSON → Shapefile: world cables | 13 ms | 44 ms | 3.4× faster |
| Filter + reproject: US counties → EPSG:5070 | 121 ms | 52 ms | 2.3× slower |

### Raster ↔ vector

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Rasterize US counties at 0.01° | 1.74 s | 333 ms | 5.2× slower |
| Polygonize NLCD land cover (966×731) | 476 ms | 636 ms | 1.3× faster |

### Remote data

| Task | terrio | GDAL | terrio vs GDAL |
|---|---|---|---|
| Info of a remote COG (USGS 3DEP on S3) | 491 ms | 585 ms | 1.2× faster |

## Notes

- **Raster info** reads only the GeoTIFF header and the tags it needs.
- **Mosaic → VRT** reads only the inputs' headers; GDAL opens each input fully.
- **Remote COG info** fetches one 64 KiB range; GDAL runs with
  `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`, as recommended for cloud data. Remote timings depend on
  the network.
- **Rasterize** burns US counties, whose extent crosses the antimeridian, into a 35894×5348 grid
  (192 MB): the time goes into writing the uncompressed GeoTIFF.
- `.aux.xml` statistics caches are disabled (`GDAL_PAM_ENABLED=NO`), and the statistics case uses a
  file without statistics stored in its metadata, so both tools compute them from the pixels.

## Running the benchmarks

```sh
scripts/fetch_test_data.sh ../terrio-data
cargo build --release
python scripts/benchmark.py ../terrio-data --runs 5 --remote --out benchmarks.md
```

The script needs the GDAL command-line tools on `PATH`.
