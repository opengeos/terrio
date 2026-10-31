"""Web Mercator to WGS 84 as GDAL computes it, and back, against `rasterio.warp`.

GDAL transforms EPSG:3857 to EPSG:4326 with its own spherical formula rather than with PROJ
(`OGRProjCT::DetectWebMercatorToWGS84()`): its results differ from PROJ's in the last bits, and
every warped tile goes through them. The other way, GDAL uses PROJ. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

import rasterio.warp as rw  # noqa: E402
from rasterio.crs import CRS as RCRS  # noqa: E402

import terrio.rio.warp as tw  # noqa: E402
from terrio.rio.crs import CRS as TCRS  # noqa: E402

# Half the width of the Web Mercator world.
H = 20037508.342789244


def _mercator_points():
    """Web Mercator points: anywhere, at the antimeridian and beyond it (one or more turns),
    and at the latitude limit and beyond it."""
    rng = np.random.default_rng(3857)
    edge = rng.uniform(H * (1 - 1e-9), H * (1 + 1e-15), 300) * rng.choice([-1, 1], 300)
    exact = [H, -H, np.nextafter(H, 0), np.nextafter(H, 2 * H), H * (1 + 2e-14)]
    xs = np.concatenate(
        [rng.uniform(-H, H, 2400), edge, exact, rng.uniform(-3.5 * H, 3.5 * H, 300)]
    )
    pole = rng.uniform(H * 0.999, 2 * H, 400) * rng.choice([-1, 1], 400)
    ys = np.concatenate([rng.uniform(-H, H, 2205), pole, rng.uniform(-H, H, 400)])
    return xs, ys


def test_web_mercator_to_wgs84_is_gdal_s():
    """`transform` (with and without heights), `transform_geom` and `transform_bounds` from
    EPSG:3857 to EPSG:4326 give GDAL's coordinates exactly."""
    xs, ys = _mercator_points()
    assert len(xs) == len(ys) >= 3000
    src, dst = (
        (RCRS.from_epsg(3857), RCRS.from_epsg(4326)),
        (TCRS.from_epsg(3857), TCRS.from_epsg(4326)),
    )
    zs = list(np.linspace(-100, 100, len(xs)))
    a = rw.transform(*src, list(xs), list(ys))
    b = tw.transform(*dst, list(xs), list(ys))
    assert np.array_equal(a, b), f"{(np.asarray(a) != np.asarray(b)).mean():.3%} differ"
    a = rw.transform(*src, list(xs), list(ys), zs)
    b = tw.transform(*dst, list(xs), list(ys), zs)
    assert np.array_equal(a, b)
    line = {
        "type": "LineString",
        "coordinates": [[float(x), float(y)] for x, y in zip(xs, ys, strict=True)],
    }
    a = rw.transform_geom(*src, line)["coordinates"]
    b = tw.transform_geom(*dst, line)["coordinates"]
    assert np.array_equal(np.asarray(a), np.asarray(b))
    for box in [(-H, -H, H, H), (-1.2e7, 4.0e6, -1.1e7, 5.0e6), (1.9e7, -1e6, H, 1e6)]:
        assert rw.transform_bounds(*src, *box) == tw.transform_bounds(*dst, *box)
    # ESRI's Web Mercator, which GDAL recognizes by its PROJ string.
    a = rw.transform(RCRS.from_user_input("ESRI:102100"), src[1], list(xs), list(ys))
    b = tw.transform(TCRS.from_user_input("ESRI:102100"), dst[1], list(xs), list(ys))
    assert np.array_equal(a, b)


def test_wgs84_to_web_mercator_is_close_to_gdal_s():
    """From EPSG:4326 to EPSG:3857 GDAL uses PROJ, whose `asinh(tan(phi))` is the C library's;
    terrio's Mercator uses Rust's `asinh`, which differs in the last bit for a tenth of the
    latitudes: northings within 1e-8 m, eastings exact."""
    rng = np.random.default_rng(4326)
    limit = 85.0511287798066
    lon = np.concatenate(
        [rng.uniform(-180, 180, 2600), [180.0, -180.0] * 100, rng.uniform(-180, 180, 200)]
    )
    lat = np.concatenate(
        [rng.uniform(-limit, limit, 2600), rng.choice([-1, 1], 400) * rng.uniform(85.0, limit, 400)]
    )
    a = np.asarray(rw.transform(RCRS.from_epsg(4326), RCRS.from_epsg(3857), list(lon), list(lat)))
    b = np.asarray(tw.transform(TCRS.from_epsg(4326), TCRS.from_epsg(3857), list(lon), list(lat)))
    assert np.array_equal(a[0], b[0])
    assert np.abs(a[1] - b[1]).max() <= 1e-8
