# Provenance: approx_min_max_mask_blocks.tif, approx_min_max_mask_overview.tif

Generated locally with GDAL 3.12.1 (Python bindings, `GDAL_TIFF_INTERNAL_MASK=YES`) for the
`approx_min_max` mask tests; not downloaded. Both are Float32, 16x16 tiles, DEFLATE, no nodata,
with an internal per-dataset mask (`GetMaskFlags() == GMF_PER_DATASET`).

- `approx_min_max_mask_blocks.tif`: 128x128 of 50, with 20 at rows/columns 0..4 and 80 at (5,5);
  -1000 at (8,8) and 1000 at (9,9) are masked out. No overview, so GDAL samples blocks
  (block 0 is sampled). `ComputeRasterMinMax(True)` = (20, 80).
- `approx_min_max_mask_overview.tif`: 256x256 of 50, all valid. Internal 128x128 overview
  (`BuildOverviews('NEAREST', [2])`, then rewritten) of 60 with 30 at (0,0) and 90 at (1,1);
  -1000 at (2,2) and 1000 at (3,3) are masked out by the overview's own internal mask.
  `ComputeRasterMinMax(True)` = (30, 90) (the exact range is (50, 50)).

```
gdal.SetConfigOption('GDAL_TIFF_INTERNAL_MASK', 'YES')
ds = drv.Create(path, w, h, 1, GDT_Float32, ['TILED=YES','BLOCKXSIZE=16','BLOCKYSIZE=16','COMPRESS=DEFLATE'])
ds.GetRasterBand(1).WriteArray(pixels)
ds.CreateMaskBand(GMF_PER_DATASET); ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
# overview fixture: ds.BuildOverviews('NEAREST', [2]); overview band and its GetMaskBand()
# rewritten with the pixels and mask above.
```

Second set, same layout as `approx_min_max_mask_blocks.tif` plus an unmasked -5 at (1,1):

- `approx_min_max_mask_nodata_f32.tif`: Float32, nodata -5, internal mask.
  `ComputeRasterMinMax(True)` = (20, 80): a Float32 band with nodata is filtered by both nodata
  and mask, because `ComputeFloatNoDataValue` moves the nodata to the float nodata and clears
  `bGotNoDataValue`, so `ComputeRasterMinMax` reads `GetMaskBand()`.
- `approx_min_max_mask_nodata_f64.tif`: Float64, nodata -5, internal mask.
  `ComputeRasterMinMax(True)` = (-1000, 1000): filtered by nodata only.
- `approx_min_max_mask_sidecar.tif` + `.tif.msk`: Float32, no nodata, mask written with
  `GDAL_TIFF_INTERNAL_MASK=NO` (a `.msk` sidecar). `ComputeRasterMinMax(True)` = (-5, 80).
- `approx_min_max_mask_ovr_unmasked.tif` + `.tif.ovr`: 256x256 Float32 of 50 with 1000 at
  rows/columns 0..2, masked out by an internal mask; external overview built read-only with
  `BuildOverviews('NEAREST', [2])` and `COMPRESS_OVERVIEW=DEFLATE` (GDAL warns that the mask
  band gets no overview). The overview band reports `GMF_ALL_VALID`;
  `ComputeRasterMinMax(True)` = (50, 1000).
