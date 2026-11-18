"""`periodic_longitude=False`: a full-turn source's stored edge is an edge (rasterio not needed).

A geographic source covering one turn of longitude is periodic by default: bilinear
interpolation next to its stored edge blends its two edge columns. That is wrong for a field
that is not continuous there, such as each pixel's own longitude. The option keeps
interpolation inside the stored columns.
"""

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.warp import reproject

# Each pixel holds its centre longitude: -179.5 and 179.5 meet at the stored edge.
LON = np.broadcast_to(np.arange(360, dtype="float64") - 179.5, (180, 360)).copy()
SRC_TRANSFORM = from_origin(-180, 90, 1, 1)
# 21 x 2 pixels across the 180° meridian, their centres between the source's.
DST_CRS = "+proj=longlat +datum=WGS84 +lon_0=180"
DST_TRANSFORM = from_origin(-10.5, 60, 1, 1)


def turn(difference):
    """The absolute difference of two longitudes, in degrees."""
    return np.abs((difference + 180) % 360 - 180)


def warped(resampling, **options):
    out = np.full((2, 21), np.nan)
    reproject(
        LON,
        out,
        src_transform=SRC_TRANSFORM,
        src_crs="EPSG:4326",
        dst_transform=DST_TRANSFORM,
        dst_crs=DST_CRS,
        resampling=resampling,
        dst_nodata=np.nan,
        **options,
    )
    return out


@pytest.mark.parametrize("resampling", [Resampling.bilinear, Resampling.cubic])
def test_reproject_keeps_interpolation_inside_the_stored_columns(resampling):
    nearest = warped(Resampling.nearest)
    assert (nearest == warped(Resampling.nearest, periodic_longitude=False)).all()
    finite = warped(resampling, periodic_longitude=False)
    # The plain difference: a value from the other stored edge would be 359 away.
    assert np.abs(finite - nearest).max() <= 1
    # The periodic default blends the two edge columns.
    assert turn(warped(resampling) - nearest).max() > 90
    assert (warped(resampling, periodic_longitude=True) == warped(resampling)).all()


def test_warped_vrt_keeps_interpolation_inside_the_stored_columns(tmp_path):
    path = str(tmp_path / "lon.tif")
    profile = dict(driver="GTiff", width=360, height=180, count=1, dtype="float64")
    with rio.open(path, "w", transform=SRC_TRANSFORM, crs="EPSG:4326", **profile) as dst:
        dst.write(LON, 1)
    grid = dict(crs=DST_CRS, transform=DST_TRANSFORM, width=21, height=2)
    with rio.open(path) as src:
        with WarpedVRT(src, resampling=Resampling.nearest, **grid) as vrt:
            nearest = vrt.read(1)
        with WarpedVRT(src, resampling=Resampling.bilinear, **grid) as vrt:
            assert turn(vrt.read(1) - nearest).max() > 90
        options = dict(resampling=Resampling.bilinear, periodic_longitude=False)
        with WarpedVRT(src, **options, **grid) as vrt:
            assert np.abs(vrt.read(1) - nearest).max() <= 1
            assert vrt.periodic_longitude is False
            with pytest.raises(AttributeError):
                vrt.periodic_longitude = True
