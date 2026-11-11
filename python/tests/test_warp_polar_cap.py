"""A warp into a polar grid whose interior holds the pole writes every pixel its source covers, up
to the pole (#825), through `terrio.rio.warp.reproject`, `terrio.warp` and a `WarpedVRT`.
"""

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.warp import reproject, transform

# NSIDC's 25 km polar stereographic sea ice grids, each holding its pole: the CRS, the top-left
# corner and the size; and the latitudes of a longitude and latitude source of 1° pixels from the
# pole's cap to 30° N or 60° S.
GRIDS = {
    "north": ("EPSG:3413", (-3_850_000, 5_850_000), (304, 448), (90, 30)),
    "south": ("EPSG:3976", (-3_950_000, 4_350_000), (316, 332), (-60, -90)),
}


def source(north, south):
    """1° pixels from latitude `north` to `south`, at every longitude, each holding its row + 1."""
    values = np.repeat(np.arange(1, north - south + 1, dtype="uint8")[:, None], 360, axis=1)
    return values, from_origin(-180, north, 1, 1)


def centre_rows(crs, origin, size, north, south):
    """The source row + 1 that each output pixel's centre lies in, 0 for a centre outside."""
    w, h = size
    cols, rows = np.meshgrid(np.arange(w) + 0.5, np.arange(h) + 0.5)
    xs, ys = (origin[0] + cols * 25_000).ravel(), (origin[1] - rows * 25_000).ravel()
    _, lats = transform(crs, "EPSG:4326", xs.tolist(), ys.tolist())
    lats = np.asarray(lats).reshape(h, w)
    return np.where((lats <= north) & (lats >= south), np.floor(north - lats) + 1, 0)


def assert_written_up_to_the_pole(out, expected, label):
    """Every pixel the source covers is written, with its centre's row or the next."""
    covered = expected > 0
    unwritten = int((covered & (out == 0)).sum())
    assert covered.any() and unwritten == 0, f"{label}: {unwritten} of {int(covered.sum())}"
    assert (np.abs(out.astype(float) - expected)[covered] <= 1).all(), label


def warp(path, tmp_path, crs, origin, size, north, south):
    """The source warped by nearest neighbour into the grid through `path`."""
    w, h = size
    values, src_transform = source(north, south)
    dst_transform = from_origin(origin[0], origin[1], 25_000, 25_000)
    if path == "reproject":
        out = np.zeros((h, w), "uint8")
        reproject(
            values,
            out,
            src_transform=src_transform,
            src_crs="EPSG:4326",
            dst_transform=dst_transform,
            dst_crs=crs,
            resampling=Resampling.nearest,
            dst_nodata=0,
        )
        return out
    if path == "terrio.warp":
        raster = terrio.Raster.from_array(values, src_transform.to_gdal(), crs="EPSG:4326")
        bounds = (origin[0], origin[1] - h * 25_000, origin[0] + w * 25_000, origin[1])
        warped = terrio.warp(
            raster, t_srs=crs, te=bounds, tr=(25_000, 25_000), resampling="near", dstnodata=0
        )
        return np.asarray(warped.read(1))
    tif = tmp_path / "source.tif"
    profile = dict(driver="GTiff", width=360, height=north - south, count=1, dtype="uint8")
    with rio.open(tif, "w", crs="EPSG:4326", transform=src_transform, **profile) as dst:
        dst.write(values[None])
    with rio.open(tif) as src:
        kw = dict(crs=crs, transform=dst_transform, width=w, height=h, nodata=0)
        with WarpedVRT(src, resampling=Resampling.nearest, **kw) as vrt:
            return vrt.read(1)


@pytest.mark.parametrize("path", ["reproject", "terrio.warp", "WarpedVRT"])
@pytest.mark.parametrize("pole", list(GRIDS))
def test_a_polar_grid_holding_the_pole_is_written_up_to_it(tmp_path, pole, path):
    """By nearest neighbour, into the whole grid, through each way of warping."""
    crs, origin, size, (north, south) = GRIDS[pole]
    out = warp(path, tmp_path, crs, origin, size, north, south)
    assert_written_up_to_the_pole(out, centre_rows(crs, origin, size, north, south), path)
