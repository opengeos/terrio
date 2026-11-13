"""Polar pixels with a corner on the meridian use their local geographic footprint."""

import json

import numpy as np
import pytest

import terrio
from terrio.rio import Env
from terrio.rio.warp import transform

SOURCE_GRID = (-180.0, 1.0, 0.0, 90.0, 0.0, -1.0)
# NSIDC pixels (row 180, columns 99..101), centered west of, on and east of the cut.
DESTINATION_GRID = (-1_375_000.0, 25_000.0, 0.0, 1_350_000.0, 0.0, -25_000.0)


def footprint_area(column):
    """Compute the exact transformed quadrilateral's area in source-pixel units.

    Args:
        column: Destination column in the cropped NSIDC grid.

    Returns:
        float: Shoelace area after placing corners beside the pixel center.
    """
    x0, dx, _, y0, _, dy = DESTINATION_GRID
    corners = [(column, 0), (column + 1, 0), (column + 1, 1), (column, 1)]
    lon, lat = transform(
        "EPSG:3413",
        "EPSG:4326",
        [x0 + x * dx for x, _ in corners],
        [y0 + y * dy for _, y in corners],
    )
    center, _ = transform("EPSG:3413", "EPSG:4326", [x0 + (column + 0.5) * dx], [y0 + 0.5 * dy])
    longitude = center[0] + (np.asarray(lon) - center[0] + 180) % 360 - 180
    latitude = np.asarray(lat)
    return (
        abs(
            float(
                np.dot(longitude, np.roll(latitude, -1)) - np.dot(latitude, np.roll(longitude, -1))
            )
        )
        / 2
    )


@pytest.mark.parametrize("compatibility", ["3.12", "3.13"])
@pytest.mark.parametrize("invariant", [True, False])
@pytest.mark.parametrize("method", ["bilinear", "cubic", "sum"])
def test_meridian_corner_keeps_a_local_footprint(compatibility, invariant, method):
    """Keep interpolation local and sum proportional to the true footprint area.

    Args:
        compatibility: Public GDAL kernel selector.
        invariant: Stable per-pixel or chunk-dependent grid policy.
        method: Resampling method affected by a long-way-round longitude span.
    """
    values = (
        np.ones((180, 360))
        if method == "sum"
        else np.repeat(100 * np.cos(np.radians(-179.5 + np.arange(360)))[None, :], 180, axis=0)
    )
    source = terrio.Raster.from_array(values, SOURCE_GRID, crs="EPSG:4326")
    with Env(TERRIO_GDAL_COMPAT=compatibility):
        actual = terrio.warp(
            source,
            t_srs="EPSG:3413",
            dst_transform=DESTINATION_GRID,
            ts=(3, 1),
            resampling=method,
            ot="Float64",
            dstnodata=-999,
            grid_invariant=invariant,
            et=0,
        ).read(1)[0]
    if method == "sum":
        expected = [footprint_area(column) for column in range(3)]
        np.testing.assert_allclose(actual, expected, rtol=1e-6, atol=1e-8)
    else:
        assert np.all(actual < -99.9), actual
        assert np.all(actual >= -100.1), actual


@pytest.mark.parametrize("compatibility", ["3.12", "3.13"])
@pytest.mark.parametrize("invariant", [True, False])
def test_pole_corner_sum_matches_its_meridian_mirror(compatibility, invariant):
    """Keep equal-area polar footprints equal when a corner also lies on the cut.

    Args:
        compatibility: Public GDAL kernel selector.
        invariant: Stable per-pixel or chunk-dependent grid policy.
    """
    source = terrio.Raster.from_array(
        np.ones((4, 3600)), (-180, 0.1, 0, 90, 0, -0.1), crs="EPSG:4326"
    )
    with Env(TERRIO_GDAL_COMPAT=compatibility):
        result = terrio.warp(
            source,
            t_srs="EPSG:5041",
            dst_transform=(1_975_000, 25_000, 0, 2_025_000, 0, -25_000),
            ts=(2, 1),
            resampling="sum",
            ot="Float64",
            dstnodata=-999,
            grid_invariant=invariant,
            et=0,
        ).read(1)[0]
    assert np.all(result > 0)
    assert result[0] == pytest.approx(result[1], rel=1e-8)


@pytest.mark.parametrize("method", ["bilinear", "sum"])
def test_real_country_coverage_at_the_polar_meridian(data_dir, method, record_property):
    """Compare real country coverage across the cut with GDAL and rotated storage.

    Args:
        data_dir: Real-world test dataset directory.
        method: Interpolation or footprint-area sampling.
        record_property: Record the GDAL version and measured difference.
    """
    rasterio = pytest.importorskip("rasterio")
    from rasterio.enums import Resampling
    from rasterio.features import rasterize
    from rasterio.transform import Affine, from_origin
    from rasterio.warp import reproject

    path = data_dir / "countries.geojson"
    if not path.exists():
        pytest.skip("needs countries.geojson")
    collection = json.loads(path.read_text())
    values = rasterize(
        [(feature["geometry"], 1) for feature in collection["features"]],
        out_shape=(180, 360),
        transform=from_origin(-180, 90, 1, 1),
        fill=0,
        dtype="uint8",
    ).astype("float64")
    expected = np.full((1, 3), -999.0)
    reproject(
        values,
        expected,
        src_transform=Affine.from_gdal(*SOURCE_GRID),
        src_crs="EPSG:4326",
        dst_transform=Affine.from_gdal(*DESTINATION_GRID),
        dst_crs="EPSG:3413",
        resampling=Resampling[method],
        dst_nodata=-999,
        ERROR_THRESHOLD=0,
    )
    difference = 0.0
    for invariant in (True, False):
        for west in (-180, 0):
            field = values if west == -180 else np.roll(values, -180, axis=1)
            source = terrio.Raster.from_array(field, (west, 1, 0, 90, 0, -1), crs="EPSG:4326")
            actual = terrio.warp(
                source,
                t_srs="EPSG:3413",
                dst_transform=DESTINATION_GRID,
                ts=(3, 1),
                resampling=method,
                ot="Float64",
                dstnodata=-999,
                grid_invariant=invariant,
                et=0,
            ).read(1)
            difference = max(difference, float(np.max(np.abs(actual - expected))))
            np.testing.assert_allclose(actual, expected, rtol=0, atol=1e-8)
    record_property("gdal_reference", rasterio.__gdal_version__)
    record_property("maximum_absolute_difference", difference)
