"""Warps by `sum` and by the area methods into a grid across 180°, from a longitude and latitude
source, take the source inside each output pixel on both sides of 180° (#764), each side weighed
by the width the pixel covers of it, through `terrio.warp`, `terrio.rio.warp.reproject` and a
`WarpedVRT` (read whole, and by windows), in the stable sampling and in the chunk-dependent one
(`grid_invariant=False`).
"""

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.warp import reproject
from terrio.rio.windows import Window

# A longitude and latitude grid centred on 180°: 555 x 139 pixels from 20° W to 20° E of it and
# from 5° N to 5° S. Column 277 is centred on 180°.
CRS = "+proj=longlat +datum=WGS84 +lon_0=180 +no_defs"
WIDTH, HEIGHT = 40 / 555, 10 / 139
GRID = (-20.0, WIDTH, 555, 5.0, HEIGHT, 139)
# Its part west of 180°, and a whole pixel, in pixels of 0.05°.
PART = -(-20 + 277 * WIDTH) / 0.05 * HEIGHT / 0.05
WHOLE = WIDTH / 0.05 * HEIGHT / 0.05


def warp(api, tmp_path, values, src_transform, resampling, grid_invariant, grid=GRID):
    """`values`, georeferenced by `src_transform` in EPSG:4326, warped through `api` into `grid`:
    the west edge, pixel width and columns, then the north edge, pixel height and rows, in CRS.
    "WarpedVRT windows" reads the VRT by windows of one row and four columns."""
    west, width, columns, north, height, rows = grid
    if api == "terrio.warp":
        raster = terrio.Raster.from_array(values, src_transform.to_gdal(), crs="EPSG:4326")
        bounds = (west, north - rows * height, west + columns * width, north)
        out = terrio.warp(
            raster,
            resampling=resampling,
            grid_invariant=grid_invariant,
            t_srs=CRS,
            te=bounds,
            ts=(columns, rows),
            ot="Float64",
        )
        return np.asarray(out.read(1))
    dst_transform = from_origin(west, north, width, height)
    if api == "reproject":
        out = np.zeros((rows, columns), "float64")
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
        kw = dict(crs=CRS, transform=dst_transform, width=columns, height=rows, nodata=0)
        kw.update(resampling=Resampling[resampling], grid_invariant=grid_invariant)
        with WarpedVRT(src, **kw) as vrt:
            if api == "WarpedVRT":
                return vrt.read(1)
            out = np.zeros((rows, columns), "float64")
            for row in range(rows):
                for col in range(0, columns, 4):
                    n = min(4, columns - col)
                    out[row, col : col + n] = vrt.read(1, window=Window(col, row, n, 1))[0]
            return out


def fine(west):
    """0.05° pixels from longitude `west` and from 5° N."""
    return from_origin(west, 5, 0.05, 0.05)


APIS = pytest.mark.parametrize("api", ["terrio.warp", "reproject", "WarpedVRT"])
SAMPLINGS = pytest.mark.parametrize(
    "grid_invariant", [True, False], ids=["stable", "chunk-dependent"]
)


@SAMPLINGS
@APIS
def test_a_sum_across_180_adds_the_regional_source_inside_it(tmp_path, api, grid_invariant):
    """From ones from 160° E to 180°, column 277 holds the area of its part west of 180°."""
    out = warp(api, tmp_path, np.ones((200, 400)), fine(160), "sum", grid_invariant)
    np.testing.assert_allclose(out[69, 274:278], [WHOLE, WHOLE, WHOLE, PART], rtol=1e-9)
    assert out[69, 278] == 0


@SAMPLINGS
@APIS
def test_a_sum_across_180_adds_a_source_of_every_longitude_on_both_sides(
    tmp_path, api, grid_invariant
):
    """From ones at every longitude, column 277 holds its whole area, as its neighbours do."""
    out = warp(api, tmp_path, np.ones((200, 7200)), fine(-180), "sum", grid_invariant)
    np.testing.assert_allclose(out[69, 274:281], [WHOLE] * 7, rtol=1e-9)


@SAMPLINGS
@APIS
def test_an_average_across_180_takes_the_regional_source_inside_it(tmp_path, api, grid_invariant):
    """From the source's column numbers, 1 east of 160° E to 400 west of 180°: column 277 holds
    400, of the source's last column, which lies inside it west of 180°; column 278 lies beyond."""
    values = np.arange(1, 401, dtype="float64")[None, :].repeat(200, axis=0)
    out = warp(api, tmp_path, values, fine(160), "average", grid_invariant)
    assert out[69, 277] == 400
    assert out[69, 278] == 0


@SAMPLINGS
@APIS
def test_an_average_across_180_takes_a_source_of_every_longitude_on_both_sides(
    tmp_path, api, grid_invariant
):
    """From |longitude| at every longitude, 179.975 on both sides of 180°: column 277 holds
    179.975, where the whole band averages 90."""
    lon = np.abs(np.arange(7200) * 0.05 - 180 + 0.025)[None, :].repeat(200, axis=0)
    out = warp(api, tmp_path, lon, fine(-180), "average", grid_invariant)
    assert out[69, 277] == pytest.approx(179.975, abs=1e-9)


@SAMPLINGS
@pytest.mark.parametrize("api", ["terrio.warp", "reproject", "WarpedVRT", "WarpedVRT windows"])
@pytest.mark.parametrize("west, east", [(0.1, 0.4), (0.4, 0.1)])
@pytest.mark.parametrize("resampling", ["average", "rms", "mode"])
def test_a_pixel_across_180_weighs_each_side_by_the_width_it_covers(
    tmp_path, api, grid_invariant, west, east, resampling
):
    """From 1° pixels of every longitude holding 2 west of 180° and 4 east of it, a pixel across
    180° (81 columns of 0.5°, 5 rows of 2°: two source rows each) covering `west`° of the one and
    `east`° of the other: `average`, `rms` and `mode` weigh each side by the width it covers,
    read whole and, through a `WarpedVRT`, by windows, the pixel first in its window."""
    values = np.ones((10, 360))
    values[:, 359], values[:, 0] = 2, 4
    # Column 40 spans from `west`° west of 180° to `east`° east of it.
    grid = (-20 - west, 0.5, 81, 5.0, 2.0, 5)
    out = warp(api, tmp_path, values, from_origin(-180, 5, 1, 1), resampling, grid_invariant, grid)
    expected = {
        "average": (2 * west + 4 * east) / (west + east),
        "rms": np.sqrt((4 * west + 16 * east) / (west + east)),
        "mode": 2 if west > east else 4,
    }[resampling]
    assert out[1, 40] == pytest.approx(expected, abs=1e-9)
