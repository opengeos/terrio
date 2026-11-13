"""Point failures stay in one native batch without changing successful results."""

from unittest.mock import patch

import numpy as np
import pytest

import terrio
from terrio.rio import warp


@pytest.mark.parametrize("index", [0, 500, 999])
@pytest.mark.parametrize("invalid", ["nan-x", "inf-x", "nan-y", "latitude"])
def test_failed_point_uses_one_batch(index, invalid):
    """Keep valid points exact and failed points infinite with one native call.

    Args:
        index: Position of the failing point.
        invalid: Non-finite coordinate or out-of-domain latitude selector.
    """
    xs = np.linspace(-170, 170, 1000)
    ys = np.linspace(-80, 80, 1000)
    expected = warp.transform("EPSG:4326", "EPSG:3857", xs, ys)
    if invalid == "nan-x":
        xs[index] = np.nan
    elif invalid == "inf-x":
        xs[index] = np.inf
    elif invalid == "nan-y":
        ys[index] = np.nan
    else:
        ys[index] = 95
    with patch.object(
        warp._terrio, "transform_points", wraps=warp._terrio.transform_points
    ) as calls:
        actual = warp.transform("EPSG:4326", "EPSG:3857", xs, ys)
    assert calls.call_count == 1
    keep = np.arange(1000) != index
    for before, after in zip(expected, actual, strict=True):
        np.testing.assert_array_equal(np.asarray(before)[keep], np.asarray(after)[keep])
        assert np.isposinf(after[index])


@pytest.mark.parametrize("with_z", [False, True])
def test_native_point_error_policy(with_z):
    """Native strict mode raises; permissive mode marks every failed ordinate.

    Args:
        with_z: Whether to exercise three-dimensional coordinate transformation.
    """
    args = ([0, np.nan, 1], [0, 0, 1], "EPSG:4326", "EPSG:3857")
    kwargs = {"zs": [10, 20, 30]} if with_z else {}
    with pytest.raises(terrio.CRSError):
        terrio.transform_points(*args, **kwargs)
    with pytest.raises(terrio.CRSError):
        terrio.transform_points(*args, errcheck=True, **kwargs)
    actual = terrio.transform_points(*args, errcheck=False, **kwargs)
    good = terrio.transform_points(
        [0, 1], [0, 1], args[2], args[3], **({"zs": [10, 30]} if with_z else {})
    )
    assert len(actual) == (3 if with_z else 2)
    for result, expected in zip(actual, good, strict=True):
        np.testing.assert_array_equal(result[[0, 2]], expected)
        assert np.isposinf(result[1])


def test_permissive_mode_keeps_input_and_crs_errors():
    """Skipping failed points must not hide invalid arrays or CRS definitions."""
    with pytest.raises(terrio.InvalidArgumentError, match="differ in length"):
        terrio.transform_points([0], [], "EPSG:4326", "EPSG:3857", errcheck=False)
    with pytest.raises(terrio.CRSError):
        terrio.transform_points([0], [0], "invalid CRS", "EPSG:3857", errcheck=False)
    with pytest.raises(terrio.InvalidArgumentError, match="differ in length"):
        terrio.transform_points([0], [0], "EPSG:4326", "EPSG:3857", zs=[], errcheck=False)


def test_geometry_point_failure_uses_one_batch():
    """The geometry helper shares the batch failure policy with coordinate lists."""
    geometry = {"type": "LineString", "coordinates": [[0, 0], [np.nan, 0], [1, 1]]}
    with patch.object(
        warp._terrio, "transform_points", wraps=warp._terrio.transform_points
    ) as calls:
        result = warp.transform_geom("EPSG:4326", "EPSG:3857", geometry)
    assert calls.call_count == 1
    assert all(np.isposinf(v) for v in result["coordinates"][1])
    assert result["coordinates"][0] == (0, 0)


def test_batch_setup_failure_is_not_retried_per_point():
    """Retain the helper's infinite result for a shared setup error without retries."""
    with patch.object(
        warp._terrio, "transform_points", wraps=warp._terrio.transform_points
    ) as calls:
        result = warp._transform_arrays("invalid CRS", "EPSG:3857", np.zeros(10), np.zeros(10))
    assert calls.call_count == 1
    assert all(np.isposinf(axis).all() for axis in result)


def test_unrelated_batch_errors_are_not_hidden():
    """Propagate programming errors rather than silently returning infinity."""
    with patch.object(warp._terrio, "transform_points", side_effect=TypeError("unexpected error")):
        with pytest.raises(TypeError, match="unexpected error"):
            warp._transform_arrays("EPSG:4326", "EPSG:3857", np.zeros(10), np.zeros(10))


@pytest.mark.parametrize("with_z", [False, True])
def test_empty_permissive_batch(with_z):
    """Empty batches retain float64 arrays with the requested dimension.

    Args:
        with_z: Whether the empty batch includes a height ordinate.
    """
    result = terrio.transform_points(
        [], [], "EPSG:4326", "EPSG:3857", errcheck=False, **({"zs": []} if with_z else {})
    )
    assert len(result) == (3 if with_z else 2)
    assert all(axis.dtype == np.float64 and axis.size == 0 for axis in result)


@pytest.mark.parametrize("target", ["EPSG:3857", "EPSG:3395"])
def test_real_places_batch_keeps_gdal_coordinates(data_dir, target):
    """Compare real successful coordinates to GDAL and preserve them after a failure.

    Args:
        data_dir: Real-data directory with Natural Earth populated places.
        target: Mercator target CRS.
    """
    ogr = pytest.importorskip("osgeo.ogr")
    ogr.UseExceptions()
    reference_warp = pytest.importorskip("rasterio.warp")
    path = data_dir / "ne_places/ne_10m_populated_places_simple.shp"
    if not path.exists():
        pytest.skip("Natural Earth populated places fixture is unavailable")
    source = ogr.Open(str(path))
    points = [feature.GetGeometryRef().GetPoint_2D() for feature in source.GetLayer(0)]
    assert len(points) >= 1000
    xs, ys = np.asarray(points[:1000]).T.copy()
    reference = reference_warp.transform("EPSG:4326", target, xs, ys)
    xs[500] = np.nan
    with patch.object(
        warp._terrio, "transform_points", wraps=warp._terrio.transform_points
    ) as calls:
        actual = warp.transform("EPSG:4326", target, xs, ys)
    assert calls.call_count == 1
    keep = np.arange(len(xs)) != 500
    for before, after in zip(reference, actual, strict=True):
        np.testing.assert_allclose(
            np.asarray(after)[keep], np.asarray(before)[keep], rtol=0, atol=1e-7
        )
        assert np.isposinf(after[500])
