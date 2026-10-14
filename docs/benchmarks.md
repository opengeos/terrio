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

## Running the benchmarks

```sh
scripts/fetch_test_data.sh ../terrio-data
cargo build --release
python scripts/benchmark.py ../terrio-data --runs 5 --remote --out benchmarks.md
```

The script needs the GDAL command-line tools on `PATH`.
