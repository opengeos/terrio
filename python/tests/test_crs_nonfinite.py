"""Non-finite projection inputs retain the public transform failure contract."""

import math

import pytest

import terrio
from terrio.rio.warp import transform

POLAR = [
    *[f"EPSG:{code}" for code in [3413, 3995, 3411, 32661, 3031, 3976, 3412, 32761, 2985]],
    "+proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +R=6371000 +units=m",
    "+proj=stere +lat_0=-90 +k=0.994 +R=6371000 +units=m",
]


@pytest.mark.parametrize("crs", POLAR)
@pytest.mark.parametrize(
    "x,y",
    [
        (math.inf, math.inf),
        (math.inf, 0),
        (-math.inf, math.inf),
        (0, -math.inf),
        (math.nan, math.nan),
    ],
)
def test_nonfinite_inverse_remains_a_failure(crs, x, y):
    """Native failures raise and the rasterio-compatible wrapper returns infinity.

    Args:
        crs: Polar projection definition.
        x: Non-finite or finite easting.
        y: Non-finite or finite northing.
    """
    with pytest.raises(ValueError):
        terrio.transform_points([x], [y], crs, "EPSG:4326")
    lons, lats = transform(crs, "EPSG:4326", [x], [y])
    assert lons == [math.inf] and lats == [math.inf]


@pytest.mark.parametrize("crs,pole", [("EPSG:3413", -90), ("EPSG:3031", 90)])
def test_failed_forward_transform_stays_failed_after_inverse(crs, pole):
    """Chaining a failed forward projection must not recover a valid-looking pole.

    Args:
        crs: Destination polar stereographic CRS.
        pole: Pole opposite the projection's center.
    """
    xs, ys = transform("EPSG:4326", crs, [10], [pole])
    assert xs == [math.inf] and ys == [math.inf]
    lons, lats = transform(crs, "EPSG:4326", xs, ys)
    assert lons == [math.inf] and lats == [math.inf]


def test_finite_kilometre_coordinates_keep_their_direction_after_normalization():
    """Finite inputs retain the analytic polar limit despite intermediate metre overflow."""
    source = "+proj=stere +lat_0=90 +R=6371000 +units=km"
    target = "+proj=longlat +R=6371000"
    xs, ys = terrio.transform_points([1e306], [2e306], source, target)
    assert xs[0] == pytest.approx(math.degrees(math.atan2(1, -2)), abs=1e-12)
    assert ys[0] == pytest.approx(-90, abs=1e-12)
