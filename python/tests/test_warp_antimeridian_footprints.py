"""Warps by `sum` and by the area methods into a grid across 180°, from a longitude and latitude
source, take the source inside each output pixel on both sides of 180° (#764), through
`terrio.warp`, `terrio.rio.warp.reproject` and a `WarpedVRT`, in the stable sampling and in the
chunk-dependent one (`grid_invariant=False`).
"""

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.warp import reproject

# A longitude and latitude grid centred on 180°: 555 x 139 pixels from 20° W to 20° E of it and
# from 5° N to 5° S. Column 277 is centred on 180°.
CRS = "+proj=longlat +datum=WGS84 +lon_0=180 +no_defs"
WIDTH, HEIGHT = 40 / 555, 10 / 139
# Its part west of 180°, and a whole pixel, in pixels of 0.05°.
PART = -(-20 + 277 * WIDTH) / 0.05 * HEIGHT / 0.05
WHOLE = WIDTH / 0.05 * HEIGHT / 0.05


def warp(path, tmp_path, values, west, resampling, grid_invariant):
    """`values`, 0.05° pixels from longitude `west` and from 5° N, warped into the grid."""
    src_transform = from_origin(west, 5, 0.05, 0.05)
    if path == "terrio.warp":
        raster = terrio.Raster.from_array(values, src_transform.to_gdal(), crs="EPSG:4326")
        grid = dict(t_srs=CRS, te=(-20.0, -5.0, 20.0, 5.0), ts=(555, 139), ot="Float64")
        out = terrio.warp(raster, resampling=resampling, grid_invariant=grid_invariant, **grid)
        return np.asarray(out.read(1))
    dst_transform = from_origin(-20, 5, WIDTH, HEIGHT)
    if path == "reproject":
        out = np.zeros((139, 555), "float64")
        reproject(
            values,
            out,
            src_transform=src_transform,
            src_crs="EPSG:4326",
            dst_transform=dst_transform,
            dst_crs=CRS,
            resampling=Resampling[resampling],
            dst_nodata=0,
            grid_invariant=grid_invariant,
        )
        return out
    tif = tmp_path / "source.tif"
    profile = dict(driver="GTiff", width=values.shape[1], height=values.shape[0], count=1)
    with rio.open(
        tif, "w", crs="EPSG:4326", transform=src_transform, dtype="float64", **profile
    ) as dst:
        dst.write(values[None])
    with rio.open(tif) as src:
        kw = dict(crs=CRS, transform=dst_transform, width=555, height=139, nodata=0)
        kw.update(resampling=Resampling[resampling], grid_invariant=grid_invariant)
        with WarpedVRT(src, **kw) as vrt:
            return vrt.read(1)


PATHS = pytest.mark.parametrize("path", ["terrio.warp", "reproject", "WarpedVRT"])
SAMPLINGS = pytest.mark.parametrize(
    "grid_invariant", [True, False], ids=["stable", "chunk-dependent"]
)


@SAMPLINGS
@PATHS
def test_a_sum_across_180_adds_the_regional_source_inside_it(tmp_path, path, grid_invariant):
    """From ones from 160° E to 180°, column 277 holds the area of its part west of 180°."""
    out = warp(path, tmp_path, np.ones((200, 400)), 160, "sum", grid_invariant)
    np.testing.assert_allclose(out[69, 274:278], [WHOLE, WHOLE, WHOLE, PART], rtol=1e-9)
    assert out[69, 278] == 0


@SAMPLINGS
@PATHS
def test_a_sum_across_180_adds_a_source_of_every_longitude_on_both_sides(
    tmp_path, path, grid_invariant
):
    """From ones at every longitude, column 277 holds its whole area, as its neighbours do."""
    out = warp(path, tmp_path, np.ones((200, 7200)), -180, "sum", grid_invariant)
    np.testing.assert_allclose(out[69, 274:281], [WHOLE] * 7, rtol=1e-9)


@SAMPLINGS
@PATHS
def test_an_average_across_180_takes_the_regional_source_inside_it(tmp_path, path, grid_invariant):
    """From the source's column numbers, 1 east of 160° E to 400 west of 180°: column 277 holds
    400, of the source's last column, which lies inside it west of 180°; column 278 lies beyond."""
    values = np.arange(1, 401, dtype="float64")[None, :].repeat(200, axis=0)
    out = warp(path, tmp_path, values, 160, "average", grid_invariant)
    assert out[69, 277] == 400
    assert out[69, 278] == 0


@SAMPLINGS
@PATHS
def test_an_average_across_180_takes_a_source_of_every_longitude_on_both_sides(
    tmp_path, path, grid_invariant
):
    """From |longitude| at every longitude, 179.975 on both sides of 180°: column 277 holds
    179.975, where the whole band averages 90."""
    lon = np.abs(np.arange(7200) * 0.05 - 180 + 0.025)[None, :].repeat(200, axis=0)
    out = warp(path, tmp_path, lon, -180, "average", grid_invariant)
    assert out[69, 277] == pytest.approx(179.975, abs=1e-9)
