"""UTM, WGS 84 and Web Mercator transformations bit-identical to GDAL's (PROJ 9.7.1).

terrio ports PROJ's exact transverse Mercator (Poder/Engsager, with PROJ's auxiliary latitude
series), PROJ's ellipsoid parameters, UTM central meridians and pipeline steps, and glibc's asinh
and atanh, so that the coordinates are PROJ's to the last bit: tiles warped from UTM sources then
sample the same source positions as GDAL's. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

import rasterio.warp as rw  # noqa: E402
from rasterio.crs import CRS as RCRS  # noqa: E402

import terrio.rio.warp as tw  # noqa: E402
from terrio.rio.crs import CRS as TCRS  # noqa: E402

N = 3000
# (EPSG code, central meridian, southern hemisphere)
ZONES = [
    (32601, -177, False),
    (32613, -105, False),
    (32618, -75, False),
    (32733, 15, True),
    (32760, 177, True),
]


def _same(src, dst, xs, ys):
    """Points transformed by terrio and by GDAL, compared exactly: the number that differ."""
    a = np.asarray(rw.transform(RCRS.from_epsg(src), RCRS.from_epsg(dst), list(xs), list(ys)))
    b = np.asarray(tw.transform(TCRS.from_epsg(src), TCRS.from_epsg(dst), list(xs), list(ys)))
    return int(((a[0] != b[0]) | (a[1] != b[1])).sum())


@pytest.mark.parametrize("epsg, lon0, south", ZONES, ids=[str(z[0]) for z in ZONES])
def test_utm_transforms_are_gdal_s(epsg, lon0, south):
    """UTM to and from EPSG:4326 and EPSG:3857, in the zone and up to 9 degrees beyond it."""
    rng = np.random.default_rng(epsg)
    lon = lon0 + np.concatenate([rng.uniform(-3, 3, N - 600), rng.uniform(-9, 9, 600)])
    lon = (lon + 180) % 360 - 180
    lat = rng.uniform(-80, 0, N) if south else rng.uniform(0, 84, N)
    ux, uy = rw.transform(RCRS.from_epsg(4326), RCRS.from_epsg(epsg), list(lon), list(lat))
    mx, my = rw.transform(RCRS.from_epsg(4326), RCRS.from_epsg(3857), list(lon), list(lat))
    differing = {
        f"4326->{epsg}": _same(4326, epsg, lon, lat),
        f"{epsg}->4326": _same(epsg, 4326, ux, uy),
        f"3857->{epsg}": _same(3857, epsg, mx, my),
        f"{epsg}->3857": _same(epsg, 3857, ux, uy),
    }
    assert all(v == 0 for v in differing.values()), differing


def test_wgs84_to_web_mercator_is_gdal_s():
    """EPSG:4326 to EPSG:3857 (and back), anywhere: the antimeridian and the latitude limit."""
    rng = np.random.default_rng(3857)
    limit = 85.0511287798066
    lon = np.concatenate(
        [rng.uniform(-180, 180, N - 400), [180.0, -180.0] * 100, rng.uniform(-180, 180, 200)]
    )
    lat = np.concatenate(
        [
            rng.uniform(-limit, limit, N - 400),
            rng.choice([-1, 1], 400) * rng.uniform(85.0, limit, 400),
        ]
    )
    assert _same(4326, 3857, lon, lat) == 0
    mx, my = rw.transform(RCRS.from_epsg(4326), RCRS.from_epsg(3857), list(lon), list(lat))
    assert _same(3857, 4326, mx, my) == 0
