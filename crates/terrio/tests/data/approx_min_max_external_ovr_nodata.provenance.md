# Provenance: approx_min_max_external_ovr_nodata.tif (+ .tif.ovr)

Generated locally with GDAL 3.13.3 (Python bindings) for the `approx_min_max` overview-nodata
test; not downloaded. Base: 128x128 Float32, 16x16 tiles, DEFLATE, nodata -9999, pixels 150 with
one -9999. External overview `tif.ovr`: 64x64 Float32, DEFLATE, nodata -32768, pixels 500 with
12.5 at (0,0) and -32768 at (1,1) — `gdalinfo -approx_stats` reports Minimum=12.500 Maximum=500.000.

```
drv.Create('base.tif', 128, 128, 1, GDT_Float32, ['TILED=YES','BLOCKXSIZE=16','BLOCKYSIZE=16','COMPRESS=DEFLATE'])
# band nodata -9999; BuildOverviews('NEAREST', [2]) on a read-only handle -> external .ovr;
# then .ovr band rewritten with nodata -32768 and the pixels above; recompressed with
# `gdal_translate -co COMPRESS=DEFLATE`.
```
