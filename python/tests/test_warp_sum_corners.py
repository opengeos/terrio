"""A `sum` across a reprojection keeps the exact total: its corners are transformed exactly."""

import math

import numpy as np

import terrio
import terrio.rio
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.warp import reproject, transform

# A Web Mercator tile at z13 (13/4419/2617), 256 x 256, over 10 m UTM 33N pixels.
ORIGIN = math.pi * 6378137.0
SIZE = 2 * ORIGIN / 2**13
LEFT, TOP = -ORIGIN + 4419 * SIZE, ORIGIN - 2617 * SIZE
PX = SIZE / 256
SOURCE = (447090.0, 6022390.0, 400)


def band_area(cols, rows, t):
    """The area of the polygon `(cols, rows)` where `rows >= t`."""
    px, py = [], []
    n = len(cols)
    for i in range(n):
        c1, r1, c2, r2 = cols[i], rows[i], cols[(i + 1) % n], rows[(i + 1) % n]
        if r1 >= t:
            px.append(c1)
            py.append(r1)
        if (r1 >= t) != (r2 >= t):
            f = (t - r1) / (r2 - r1)
            px.append(c1 + f * (c2 - c1))
            py.append(t)
    if len(px) < 3:
        return 0.0
    x, y = np.array(px), np.array(py)
    return abs(float(np.dot(x, np.roll(y, -1)) - np.dot(y, np.roll(x, -1)))) / 2


def exact_total(value):
    """The tile's exact `sum` of a source whose row `r` holds `value(r)`: the tile's footprint is
    the polygon of its edge pixels' corners, transformed exactly, which its pixels' quadrilaterals
    tile, so the total is each source row's value times the footprint's area in that row."""
    i = np.arange(257, dtype=float)
    edge_c = np.concatenate([i[:-1], np.full(256, 256.0), i[::-1][:-1], np.zeros(256)])
    edge_r = np.concatenate([np.zeros(256), i[:-1], np.full(256, 256.0), i[::-1][:-1]])
    xs, ys = transform("EPSG:3857", "EPSG:32633", list(LEFT + edge_c * PX), list(TOP - edge_r * PX))
    x0, y0, _ = SOURCE
    cols, rows = (np.asarray(xs) - x0) / 10.0, (y0 - np.asarray(ys)) / 10.0
    first, last = math.floor(rows.min()), math.ceil(rows.max())
    areas = [band_area(cols, rows, float(t)) for t in range(first, last + 1)]
    return sum(value(r) * (areas[k] - areas[k + 1]) for k, r in enumerate(range(first, last)))


def test_a_sum_of_a_reprojected_tile_keeps_the_exact_total(tmp_path):
    # Values rising along y (1000 + the source row), so that footprints moved along y change the
    # total. Through `terrio.warp` and a WarpedVRT, the tile's sum is the exact total within
    # 1e-9. With its corners interpolated across 512 columns, a chord of each row's curve on the
    # source grid, it was -5.2e-5 off.
    x0, y0, n = SOURCE
    values = (1000.0 + np.arange(n, dtype="float64"))[:, None] * np.ones((1, n))
    path = str(tmp_path / "utm.tif")
    terrio.Raster.from_array(values, (x0, 10.0, 0.0, y0, 0.0, -10.0), crs="EPSG:32633").write(path)
    exact = exact_total(lambda r: 1000.0 + r)
    warped = terrio.warp(
        path,
        t_srs="EPSG:3857",
        te=(LEFT, TOP - SIZE, LEFT + SIZE, TOP),
        ts=(256, 256),
        resampling="sum",
        ot="Float64",
    ).read(1)
    grid = {"crs": "EPSG:3857", "transform": from_origin(LEFT, TOP, PX, PX)}
    with (
        terrio.rio.open(path) as src,
        WarpedVRT(src, resampling=Resampling.sum, width=256, height=256, **grid) as vrt,
    ):
        read = vrt.read(1)
    tiles = [("terrio.warp", warped), ("WarpedVRT", read)]
    reference = None
    for tolerance in (0.0, 0.125, 4.0):
        tile, _ = reproject(
            values,
            np.zeros((256, 256), dtype="float64"),
            src_transform=from_origin(x0, y0, 10.0, 10.0),
            src_crs="EPSG:32633",
            dst_transform=grid["transform"],
            dst_crs=grid["crs"],
            resampling=Resampling.sum,
            tolerance=tolerance,
        )
        if reference is None:
            reference = tile
        else:
            np.testing.assert_array_equal(tile, reference)
        tiles.append((f"reproject tolerance={tolerance}", tile))
    for name, tile in tiles:
        error = (float(np.asarray(tile, dtype="float64").sum()) - exact) / exact
        assert abs(error) < 1e-9, (name, error)
