"""Chunk-dependent warps (`grid_invariant=False`) with the 3.12 kernel scales of a world source
symmetric about the 180° meridian, into windows of UPS North (EPSG:5041) that mirror each other
across that meridian, get mirrored kernels through `reproject` and the block reads of a
`WarpedVRT`, whether the windows touch the meridian or reach across it (#1086).

The meridian runs up from the pole along x = 2,000,000 m, and the windows' points sampled on it
resolve to 180° E: the west window's samples lie beside both ends of the source's columns. Its
kernel scale comes from the columns they span across the meridian, as its mirror's does.
"""

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio import Env
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_bounds, from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.warp import reproject
from terrio.rio.windows import Window

# A world raster of 0.25° pixels from 180° W, zero but for the columns centred on 179.125° W and
# 179.125° E.
SOURCE = np.zeros((720, 1440), "float32")
SOURCE[:, [3, 1436]] = 1
SRC_TRANSFORM = from_origin(-180, 90, 0.25, 0.25)
# 256 pixels of 32,110.86 m, from 8,220 km to 16,441 km above the pole.
SIDE = 8_220_379.675126


def window(west, gap):
    """The 256 x 256 window west or east of the meridian, `gap` metres from it (across it when
    negative)."""
    left = 2e6 - gap - SIDE if west else 2e6 + gap
    return from_bounds(left, 2e6 + SIDE, left + SIDE, 2e6 + 2 * SIDE, 256, 256)


def row_128(api, tif, transform):
    """Row 128 of the bilinear chunk-dependent warp of the source into `transform` through `api`:
    `reproject`, or a `WarpedVRT` of `tif` read by a window of that row, from its warped blocks."""
    with Env(TERRIO_GDAL_COMPAT="3.12"):
        if api == "reproject":
            out = np.full((256, 256), np.nan)
            reproject(
                SOURCE,
                out,
                src_transform=SRC_TRANSFORM,
                src_crs="EPSG:4326",
                dst_transform=transform,
                dst_crs="EPSG:5041",
                resampling=Resampling.bilinear,
                dst_nodata=np.nan,
                grid_invariant=False,
            )
            return out[128]
        with rio.open(tif) as src:
            kw = dict(crs="EPSG:5041", transform=transform, width=256, height=256, nodata=-999)
            with WarpedVRT(src, resampling=Resampling.bilinear, grid_invariant=False, **kw) as vrt:
                return vrt.read(1, window=Window(0, 128, 256, 1))[0]


@pytest.mark.parametrize("api", ["reproject", "WarpedVRT"])
def test_mirror_windows_across_the_meridian_get_mirrored_kernels(tmp_path, api):
    """The column at 179.125° W reaches the same columns of row 128 of the west window touching
    the meridian, or reaching 1 km across it, as of the window 1 m short of it, whose samples do
    not cross the meridian; the column at 179.125° E, the mirrored columns of the east windows."""
    tif = tmp_path / "source.tif"
    if api == "WarpedVRT":
        profile = dict(driver="GTiff", width=1440, height=720, count=1, dtype="float32")
        with rio.open(tif, "w", crs="EPSG:4326", transform=SRC_TRANSFORM, **profile) as dst:
            dst.write(SOURCE[None])
    short = np.flatnonzero(row_128(api, tif, window(True, 1)) > 0)
    assert 0 < len(short) < 6, short
    for gap in (0, -1000):
        west, east = (row_128(api, tif, window(side, gap)) for side in (True, False))
        np.testing.assert_array_equal(np.flatnonzero(west > 0), short)
        np.testing.assert_array_equal(np.flatnonzero(east[::-1] > 0), short)
        np.testing.assert_allclose(west[::-1], east, atol=0.01)
