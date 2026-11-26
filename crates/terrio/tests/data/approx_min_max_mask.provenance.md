# Provenance: approx_min_max_mask_* fixtures

Generated locally with GDAL 3.12.1 (Python bindings, numpy) for the `approx_min_max` mask tests;
not downloaded. Run each script below with an output directory as its argument; it writes the
fixtures and prints GDAL's `GetMaskFlags()`, overview mask flags and
`ComputeRasterMinMax(True)` / `ComputeRasterMinMax(False)` for each.

| Fixture | Contents | GDAL `ComputeRasterMinMax(True)` |
| --- | --- | --- |
| `approx_min_max_mask_blocks.tif` | 128x128 Float32, internal mask hides -1000 and 1000 in block 0, no overview | (20, 80) |
| `approx_min_max_mask_overview.tif` | 256x256 Float32 of 50, internal 128x128 overview whose own mask hides -1000 and 1000 | (30, 90) |
| `approx_min_max_mask_nodata_f32.tif` | blocks layout plus an unmasked -5, Float32, nodata -5 | (20, 80): nodata and mask both apply |
| `approx_min_max_mask_nodata_f64.tif` | same, Float64, nodata -5 | (-1000, 1000): nodata only |
| `approx_min_max_mask_sidecar.tif` + `.tif.msk` | same, Float32, no nodata, `.msk` sidecar | (-5, 80) |
| `approx_min_max_mask_ovr_unmasked.tif` + `.tif.ovr` | 256x256 Float32 of 50, masked 1000 corner, external overview without a mask (`GMF_ALL_VALID`) | (50, 1000) |

A Float32 band with nodata reads the mask because `ComputeFloatNoDataValue` moves its nodata to
the float slot and clears `bGotNoDataValue`, which `ComputeRasterMinMax` tests before reading
`GetMaskBand()`.

## First script: blocks and overview fixtures

```python
import sys

import numpy as np
from osgeo import gdal

gdal.UseExceptions()
gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "YES")
out = sys.argv[1]
drv = gdal.GetDriverByName("GTiff")
co = ["TILED=YES", "BLOCKXSIZE=16", "BLOCKYSIZE=16", "COMPRESS=DEFLATE"]

# Blocks: 128x128 Float32, 16x16 tiles, internal mask, no overview.
a = np.full((128, 128), 50.0, np.float32)
a[0:4, 0:4] = 20.0
a[5, 5] = 80.0
a[8, 8] = -1000.0
a[9, 9] = 1000.0
m = np.full((128, 128), 255, np.uint8)
m[8, 8] = 0
m[9, 9] = 0
ds = drv.Create(f"{out}/approx_min_max_mask_blocks.tif", 128, 128, 1, gdal.GDT_Float32, co)
ds.GetRasterBand(1).WriteArray(a)
ds.CreateMaskBand(gdal.GMF_PER_DATASET)
ds.GetRasterBand(1).GetMaskBand().WriteArray(m)
ds = None

# Overview: 256x256, internal 128x128 overview with its own mask overview.
ds = drv.Create(f"{out}/approx_min_max_mask_overview.tif", 256, 256, 1, gdal.GDT_Float32, co)
ds.GetRasterBand(1).WriteArray(np.full((256, 256), 50.0, np.float32))
ds.CreateMaskBand(gdal.GMF_PER_DATASET)
ds.GetRasterBand(1).GetMaskBand().WriteArray(np.full((256, 256), 255, np.uint8))
ds.BuildOverviews("NEAREST", [2])
ov = ds.GetRasterBand(1).GetOverview(0)
oa = np.full((128, 128), 60.0, np.float32)
oa[0, 0] = 30.0
oa[1, 1] = 90.0
oa[2, 2] = -1000.0
oa[3, 3] = 1000.0
ov.WriteArray(oa)
om = np.full((128, 128), 255, np.uint8)
om[2, 2] = 0
om[3, 3] = 0
ov.GetMaskBand().WriteArray(om)
ds = None

for f in ["approx_min_max_mask_blocks.tif", "approx_min_max_mask_overview.tif"]:
    ds = gdal.Open(f"{out}/{f}")
    b = ds.GetRasterBand(1)
    ovs = [(b.GetOverview(i).XSize, b.GetOverview(i).GetMaskFlags()) for i in range(b.GetOverviewCount())]
    print(f, b.GetMaskFlags(), ovs, "approx", b.ComputeRasterMinMax(True), "exact", b.ComputeRasterMinMax(False))
```

## Second script: nodata, sidecar and unmasked-overview fixtures

```python
import sys

import numpy as np
from osgeo import gdal

gdal.UseExceptions()
out = sys.argv[1]
drv = gdal.GetDriverByName("GTiff")
co = ["TILED=YES", "BLOCKXSIZE=16", "BLOCKYSIZE=16", "COMPRESS=DEFLATE"]


def blocks(path, nodata=None, internal=True, dtype=gdal.GDT_Float32):
    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "YES" if internal else "NO")
    a = np.full((128, 128), 50.0, np.float32)
    a[0:4, 0:4] = 20.0
    a[5, 5] = 80.0
    a[8, 8] = -1000.0
    a[9, 9] = 1000.0
    m = np.full((128, 128), 255, np.uint8)
    m[8, 8] = 0
    m[9, 9] = 0
    a[1, 1] = -5.0
    ds = drv.Create(path, 128, 128, 1, dtype, co)
    if nodata is not None:
        ds.GetRasterBand(1).SetNoDataValue(nodata)
    ds.GetRasterBand(1).WriteArray(a)
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    ds.GetRasterBand(1).GetMaskBand().WriteArray(m)
    ds = None


# Float32 nodata plus mask: GDAL filters by both. Float64: by nodata only.
blocks(f"{out}/approx_min_max_mask_nodata_f32.tif", nodata=-5.0)
blocks(f"{out}/approx_min_max_mask_nodata_f64.tif", nodata=-5.0, dtype=gdal.GDT_Float64)
# .msk sidecar.
blocks(f"{out}/approx_min_max_mask_sidecar.tif", internal=False)

# Overview without its own mask: 256x256 with an internal mask, external .ovr built read-only.
gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "YES")
p = f"{out}/approx_min_max_mask_ovr_unmasked.tif"
a = np.full((256, 256), 50.0, np.float32)
a[0:2, 0:2] = 1000.0
m = np.full((256, 256), 255, np.uint8)
m[0:2, 0:2] = 0
ds = drv.Create(p, 256, 256, 1, gdal.GDT_Float32, co)
ds.GetRasterBand(1).WriteArray(a)
ds.CreateMaskBand(gdal.GMF_PER_DATASET)
ds.GetRasterBand(1).GetMaskBand().WriteArray(m)
ds = None
gdal.SetConfigOption("COMPRESS_OVERVIEW", "DEFLATE")
ds = gdal.Open(p)
ds.BuildOverviews("NEAREST", [2])
ds = None

for f in ["approx_min_max_mask_nodata_f32.tif", "approx_min_max_mask_nodata_f64.tif", "approx_min_max_mask_sidecar.tif", "approx_min_max_mask_ovr_unmasked.tif"]:
    ds = gdal.Open(f"{out}/{f}")
    b = ds.GetRasterBand(1)
    ovs = [(b.GetOverview(i).XSize, b.GetOverview(i).GetMaskFlags()) for i in range(b.GetOverviewCount())]
    print(f, ds.GetFileList(), b.GetMaskFlags(), ovs, "approx", b.ComputeRasterMinMax(True), "exact", b.ComputeRasterMinMax(False))
```

## Third script: the nodata rule for Float32 (no fixture)

`gdal_min_max_reads_mask` is unit-tested against this GDAL 3.12.1 run, which prints
(50, 50) for nodata -inf, +inf and NaN (the mask is read) and (50, 1000) for nodata 1e39,
outside the float range (it is not). GDAL 3.13.3's `gcore/gdalrasterband.cpp` has the same
`ComputeRasterMinMax` gate and `ComputeFloatNoDataValue` (checked by reading the source).

```python
import numpy as np
from osgeo import gdal

gdal.UseExceptions()
# Float32 MEM band of 50 with 1000 at (9,9) masked out by a per-dataset mask, for each nodata:
# (50, 50) means GDAL read the mask, (50, 1000) means it did not.
for nodata in [float("-inf"), float("inf"), float("nan"), 1e39]:
    ds = gdal.GetDriverByName("MEM").Create("", 128, 128, 1, gdal.GDT_Float32)
    a = np.full((128, 128), 50.0, np.float32)
    a[9, 9] = 1000.0
    b = ds.GetRasterBand(1)
    b.SetNoDataValue(nodata)
    b.WriteArray(a)
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    m = np.full((128, 128), 255, np.uint8)
    m[9, 9] = 0
    b.GetMaskBand().WriteArray(m)
    print(nodata, b.ComputeRasterMinMax(False))
```
