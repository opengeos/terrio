"""Area methods other than `sum` summarize the source inside each output pixel, at its edges too."""

import numpy as np
import pytest

import terrio

METHODS = ("average", "rms", "mode", "min", "max", "med", "q1", "q3")


@pytest.mark.parametrize("n", [40, 100, 1000])
def test_area_methods_write_the_same_pixels_whatever_the_grid(n):
    """200 x 200 source pixels of 10 m, each holding its column number, whose edges cut 100 m output
    pixels in half: on grids of 40, 100 and 1000 such pixels, every area method writes the 21 x 21
    output pixels the source meets, and weighs an edge column once, for its own width."""
    ramp = np.tile(np.arange(200, dtype="float64"), (200, 1))
    src = terrio.Raster.from_array(ramp, (1050.0, 10.0, 0.0, 9050.0, 0.0, -10.0), crs="EPSG:32610")
    grid = dict(
        dst_transform=(0.0, 100.0, 0.0, 10000.0, 0.0, -100.0),
        ts=(n, n),
        ot="Float64",
        dstnodata=-1.0,
    )
    # Row 15's columns 10, 11 and 30: source columns 0 to 4, 5 to 14 and 195 to 199.
    expected = {
        "average": [2.0, 9.5, 197.0],
        "mode": [0.0, 5.0, 195.0],
        "min": [0.0, 5.0, 195.0],
        "max": [4.0, 14.0, 199.0],
    }
    for method in METHODS:
        a = np.asarray(terrio.warp(src, resampling=method, **grid).read(1))
        assert int((a != -1).sum()) == 441, method
        if method in expected:
            np.testing.assert_allclose(
                a[15, [10, 11, 30]], expected[method], atol=1e-9, err_msg=method
            )


def test_area_methods_of_output_pixels_larger_than_the_source_take_the_source_inside_them():
    """A 3 x 2 source of 10 m pixels across the line between two output pixels of 1 km: its first
    column, [7, 10], is west of the line, and [8, 9, 11, 12] east of it."""
    small = terrio.Raster.from_array(
        np.array([[7.0, 8, 9], [10, 11, 12]]),
        (990.0, 10.0, 0.0, 1800.0, 0.0, -10.0),
        crs="EPSG:32610",
    )
    grid = dict(
        dst_transform=(0.0, 1000.0, 0.0, 3000.0, 0.0, -1000.0),
        ts=(2, 2),
        ot="Float64",
        dstnodata=-1.0,
    )
    for method, west, east in [
        ("sum", 17.0, 40.0),
        ("average", 8.5, 10.0),
        ("min", 7.0, 8.0),
        ("max", 10.0, 12.0),
    ]:
        a = np.asarray(terrio.warp(small, resampling=method, **grid).read(1)).ravel()
        np.testing.assert_allclose(a, [-1, -1, west, east], atol=1e-9, err_msg=method)


def test_area_methods_leave_output_pixels_beside_the_source_alone():
    """4 x 4 source pixels of 1 m under 2 m output pixels from (-2, 6): the output's first and
    last rows and columns lie outside the source, and every area method leaves them alone."""
    src = terrio.Raster.from_array(
        np.arange(16, dtype="float64").reshape(4, 4),
        (0.0, 1.0, 0.0, 4.0, 0.0, -1.0),
        crs="EPSG:32610",
    )
    grid = dict(
        dst_transform=(-2.0, 2.0, 0.0, 6.0, 0.0, -2.0), ts=(4, 4), ot="Float64", dstnodata=-1.0
    )
    for method in ("sum", *METHODS):
        a = np.asarray(terrio.warp(src, resampling=method, **grid).read(1))
        assert (a[1:3, 1:3] != -1).all(), method
        a[1:3, 1:3] = -1
        assert (a == -1).all(), (method, a.tolist())


def test_area_methods_leave_pixels_whose_corners_do_not_transform_empty():
    """An orthographic source centred on (0, 0) under 10-degree pixels from 80 degrees north to 80
    south: the corners of the pixels past 90 degrees east or west lie beyond its horizon, with no
    position on its grid, and those pixels stay empty."""
    ortho = "+proj=ortho +lat_0=0 +lon_0=0 +datum=WGS84 +units=m +no_defs"
    src = terrio.Raster.from_array(
        np.ones((128, 128)), (-6.4e6, 1e5, 0.0, 6.4e6, 0.0, -1e5), crs=ortho
    )
    grid = dict(
        t_srs="EPSG:4326",
        dst_transform=(-180.0, 10.0, 0.0, 80.0, 0.0, -10.0),
        ts=(36, 16),
        ot="Float64",
        dstnodata=-1.0,
    )
    for method in METHODS:
        a = np.asarray(terrio.warp(src, resampling=method, **grid).read(1))
        assert (a[:, 9:27] == 1).all(), method
        assert (a[:, :9] == -1).all() and (a[:, 27:] == -1).all(), method
