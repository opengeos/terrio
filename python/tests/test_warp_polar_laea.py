"""The polar aspect of the Lambert azimuthal equal-area projection takes the whole Earth onto a
disk, whose rim is the opposite pole (#738). Coordinates beyond the rim are no point of the Earth:
they transform to infinity, as every point that fails, and a warp leaves them nodata.
"""

import math

import numpy as np
import pytest

import terrio
from terrio.rio.warp import transform

# The WGS 84 EASE-Grid 2.0 South and the WGS 84 North Pole LAEA Atlantic, on the ellipsoid, and
# the NSIDC EASE-Grid North and South, on a sphere: each CRS, and the latitude of its opposite
# pole.
POLAR = [("EPSG:6932", 90.0), ("EPSG:3574", -90.0), ("EPSG:3408", -90.0), ("EPSG:3409", 90.0)]


def rim(crs, opposite):
    """The radius of the CRS's disk: the distance from its origin of a point 9e-7° from the
    opposite pole."""
    x, y = transform("EPSG:4326", crs, [0.0], [opposite * 0.99999999])
    return math.hypot(x[0], y[0])


@pytest.mark.parametrize(("crs", "opposite"), POLAR)
def test_coordinates_beyond_a_polar_lambert_azimuthal_disk_transform_to_infinity(crs, opposite):
    """Points 1,000 km beyond the rim, in four directions, and an infinite point."""
    r = rim(crs, opposite) + 1e6
    lon, lat = transform(crs, "EPSG:4326", [0.0, r, 0.0, -r, math.inf], [r, 0.0, -r, 0.0, math.inf])
    assert np.isinf(lon).all() and np.isinf(lat).all(), (lon, lat)


@pytest.mark.parametrize(("crs", "opposite"), POLAR)
def test_a_world_warped_into_a_polar_lambert_azimuthal_disk_fills_the_disk_only(crs, opposite):
    """A world of 1° pixels (3 along 90° N, 2 along 90° S, 1 elsewhere) warped into the CRS's
    default grid, the square around the disk, with nearest: each pixel is the source pixel its
    centre falls in, and nodata (0) where its centre does not transform or lies on the source's
    excluded south edge (-90°). Every pixel more than a pixel beyond the rim fails to transform."""
    src = np.ones((180, 360), dtype="float32")
    src[0], src[-1] = 3.0, 2.0
    world = terrio.Raster.from_array(src, (-180.0, 1.0, 0.0, 90.0, 0.0, -1.0), crs="EPSG:4326")
    out = terrio.warp(world, t_srs=crs, resampling="nearest", dstnodata=0.0)
    a, t = np.asarray(out.read(1)), out.transform
    cx, cy = np.meshgrid(
        t[0] + (np.arange(out.width) + 0.5) * t[1], t[3] + (np.arange(out.height) + 0.5) * t[5]
    )
    lon, lat = transform(crs, "EPSG:4326", cx.ravel().tolist(), cy.ravel().tolist())
    lon, lat = np.reshape(lon, a.shape), np.reshape(lat, a.shape)
    inverts = np.isfinite(lon) & np.isfinite(lat)
    row = np.clip(np.floor(90.0 - np.where(inverts, lat, 0.0)), 0, 179).astype(int)
    col = np.clip(np.floor(np.where(inverts, lon, 0.0) + 180.0), 0, 359).astype(int)
    np.testing.assert_array_equal(a, np.where(inverts & (lat > -90.0), src[row, col], 0.0))
    beyond = np.hypot(cx, cy) > rim(crs, opposite) + abs(t[1])
    assert beyond.sum() > 16_000 and not inverts[beyond].any()
