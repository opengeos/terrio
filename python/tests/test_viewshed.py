"""Viewshed bindings, output persistence and invalid inputs."""

import numpy as np
import pytest

import terrio


def test_viewshed_ridge_and_height(tmp_path):
    src = terrio.Raster.from_array(
        np.array([[0, 0, 10, 0, 0]], dtype="float64"), (0, 10, 0, 10, 0, -10), crs="EPSG:32611"
    )
    path = str(tmp_path / "ground.tif")
    visible = terrio.viewshed(src, 5, 5, cc=0)
    np.testing.assert_array_equal(visible.read(1), [[255, 255, 255, 0, 0]])
    ground = terrio.viewshed(src, 5, 5, path, om="GROUND", cc=0)
    np.testing.assert_array_equal(ground.read(1), [[0, 0, 0, 14, 18]])
    np.testing.assert_array_equal(terrio.open(path).read(1), ground.read(1))
    np.testing.assert_array_equal(
        terrio.viewshed(src, 5, 5, cc=0, tz=14).read(1), [[255, 255, 255, 255, 0]]
    )
    assert terrio.viewshed(src, 5, 5, md=20).width == 3
    with pytest.raises(ValueError, match="mode"):
        terrio.viewshed(src, 5, 5, om="ACCUM")
    assert terrio.viewshed(src, -15, 5).width == 5
