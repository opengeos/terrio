"""Polar bounds against GDAL 3.12.1 via rasterio, including NSIDC grid extents."""

import warnings

import numpy as np
import pytest

from terrio.rio.warp import transform_bounds

POLAR_BOUNDS = [
    ("EPSG:3413", (-1e6, -1e6, 1e6, 1e6), 90.0),
    ("EPSG:3995", (-1e6, -1e6, 1e6, 1e6), 90.0),
    ("EPSG:3031", (-1e6, -1e6, 1e6, 1e6), -90.0),
    ("EPSG:3976", (-1e6, -1e6, 1e6, 1e6), -90.0),
    ("EPSG:3413", (-3850000, -5350000, 3750000, 5850000), 90.0),
    ("EPSG:3976", (-3950000, -3950000, 3950000, 4350000), -90.0),
]


@pytest.mark.parametrize("src,bounds,pole", POLAR_BOUNDS)
@pytest.mark.parametrize("dst", ["EPSG:4326", "OGC:CRS84"])
@pytest.mark.parametrize("densify", [0, 1, 2, 21, 40])
def test_bounds_contain_interior_pole(src, bounds, pole, dst, densify):
    """Include the interior pole even when edge sampling misses it.

    Args:
        src: Source CRS.
        bounds: Source box.
        pole: Expected pole latitude.
        dst: Geographic target CRS.
        densify: Interior samples per edge.
    """
    actual = transform_bounds(src, dst, *bounds, densify_pts=densify)
    assert (actual[0], actual[2]) == (-180.0, 180.0)
    assert actual[3 if pole > 0 else 1] == pole
    assert all(np.isfinite(actual))


@pytest.mark.parametrize("src,bounds,pole", POLAR_BOUNDS)
@pytest.mark.parametrize("dst", ["EPSG:4326", "OGC:CRS84"])
@pytest.mark.parametrize("densify", [2, 21, 40])
def test_polar_bounds_match_gdal(src, bounds, pole, dst, densify):
    """Compare all four bounds with GDAL's polar result.

    Args:
        src: Source CRS.
        bounds: Source box.
        pole: Expected pole latitude.
        dst: Geographic target CRS.
        densify: Interior samples per edge.
    """
    rw = pytest.importorskip("rasterio.warp")
    expected = rw.transform_bounds(src, dst, *bounds, densify_pts=densify)
    actual = transform_bounds(src, dst, *bounds, densify_pts=densify)
    assert (actual[0], actual[2], actual[3 if pole > 0 else 1]) == (-180.0, 180.0, pole)
    np.testing.assert_allclose(actual, expected, rtol=0, atol=1e-8)


@pytest.mark.parametrize(
    "src,dst,bounds",
    [
        ("EPSG:25830", "EPSG:4326", (500000, 4000000, 600000, 4100000)),
        ("EPSG:3413", "EPSG:4326", (1e5, -1e6, 1e6, -1e5)),
        ("EPSG:3031", "EPSG:4326", (1e5, 1e5, 1e6, 1e6)),
        ("EPSG:4269", "EPSG:5070", (-100, 30, -90, 40)),
    ],
)
@pytest.mark.parametrize("densify", [2, 21, 40])
def test_regional_bounds_match_gdal_without_probe_warnings(src, dst, bounds, densify):
    """Keep regional bounds and diagnostics unchanged by pole probing.

    Args:
        src: Source CRS.
        dst: Target CRS.
        bounds: Source box excluding the poles.
        densify: Interior samples per edge.
    """
    rw = pytest.importorskip("rasterio.warp")
    expected = rw.transform_bounds(src, dst, *bounds, densify_pts=densify)
    with warnings.catch_warnings(record=True) as seen:
        warnings.simplefilter("always")
        actual = transform_bounds(src, dst, *bounds, densify_pts=densify)
    assert not seen
    np.testing.assert_allclose(actual, expected, rtol=0, atol=1e-8)


def test_xarray_transform_bounds_includes_pole():
    """Use shared pole inclusion through the xarray accessor."""
    xr = pytest.importorskip("xarray")
    import terrio.xarray  # noqa: F401

    data = xr.DataArray(
        np.ones((2, 2)), dims=("y", "x"), coords={"x": [-5e5, 5e5], "y": [5e5, -5e5]}
    ).terrio.write_crs("EPSG:3413")
    actual = data.terrio.transform_bounds("EPSG:4326")
    assert (actual[0], actual[2], actual[3]) == (-180.0, 180.0, 90.0)


def test_raster_info_extent_includes_pole(tmp_path):
    """Include the pole through the raster information path used by the CLI.

    Args:
        tmp_path: Temporary output directory.
    """
    import terrio

    raster = terrio.Raster.from_array(
        np.ones((2, 2), dtype="uint8"),
        (-1e6, 1e6, 0, 1e6, 0, -1e6),
        crs="EPSG:3413",
    )
    path = tmp_path / "polar.tif"
    terrio.write_raster(raster, str(path))
    bounds = terrio.info(str(path))["wgs84_extent"]
    assert (bounds[0], bounds[2], bounds[3]) == (-180.0, 180.0, 90.0)
