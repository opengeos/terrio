"""Stored overview selection must survive paths, lazy objects and repeated inputs."""

import numpy as np
import pytest

import terrio

GT = (0.0, 1.0, 0.0, 32.0, 0.0, -1.0)


@pytest.fixture
def pyramid(tmp_path):
    y, x = np.mgrid[:32, :32]
    values = (y * y + x).astype("float32")
    path = tmp_path / "pyramid.tif"
    terrio.Raster.from_array(values, GT, crs="EPSG:3857").write(
        str(path), overviews=[2, 4, 8], overview_resampling="average"
    )
    return path


@pytest.mark.parametrize("mode", [None, "AUTO", 1, "1"])
def test_overviews_for_paths_objects_mixed_and_repeated_sources(pyramid, mode):
    lazy = terrio.open_raster(str(pyramid), lazy=True)
    expected = lazy.read(1, overview=1)
    for src in [
        str(pyramid),
        pyramid,
        lazy,
        [lazy, lazy],
        [str(pyramid), lazy],
        [lazy, str(pyramid)],
    ]:
        out = terrio.warp(src, ts=(8, 8), overview=mode)
        np.testing.assert_array_equal(out.read(1), expected)
        assert out.transform == (0.0, 4.0, 0.0, 32.0, 0.0, -4.0)
    full = terrio.warp(lazy, ts=(8, 8), overview="NONE")
    assert np.any(full.read(1) != expected)


def test_finer_auto_and_explicit_bounds(pyramid):
    lazy = terrio.open_raster(str(pyramid), lazy=True)
    fine = terrio.warp(lazy, ts=(8, 8), overview="AUTO-1")
    explicit = terrio.warp(lazy, ts=(8, 8), overview=0)
    np.testing.assert_array_equal(fine.read(1), explicit.read(1))
    cropped = terrio.warp(lazy, te=(4, 4, 20, 20), ts=(4, 4), overview="AUTO")
    expected = lazy.read(1, window=(1, 3, 4, 4), overview=1)
    np.testing.assert_array_equal(cropped.read(1), expected)
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.warp(lazy, overview="invalid")
