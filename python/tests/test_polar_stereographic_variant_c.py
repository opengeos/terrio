"""Polar Stereographic (variant C), EPSG method 9830, whose false origin lies on the standard
parallel rather than at the pole: IOGP Guidance Note 7-2's worked example on Petrels 1972 / Terre
Adelie Polar Stereographic (EPSG:2985), 66°36'18.820"S, 140°04'17.040"E, is 303169.52 E,
244055.72 N. EPSG:2986 is the same projection on Perroud 1950.
"""

import numpy as np
import pytest

import terrio
from terrio.rio.crs import CRS
from terrio.rio.warp import transform, transform_bounds

LON = 140 + 4 / 60 + 17.04 / 3600
LAT = -(66 + 36 / 60 + 18.82 / 3600)
# The CRSs and their geographic CRSs.
CODES = [("EPSG:2985", "EPSG:4636"), ("EPSG:2986", "EPSG:4637")]


@pytest.mark.parametrize("code, geographic", CODES)
def test_variant_c_gives_the_guidance_notes_example_point_and_back(code, geographic):
    xs, ys = transform(geographic, code, [LON], [LAT])
    assert abs(xs[0] - 303169.52) < 0.005 and abs(ys[0] - 244055.72) < 0.005, (xs, ys)
    lons, lats = transform(code, geographic, [303169.522], [244055.721])
    assert abs(lons[0] - LON) * 3600 < 0.0005 and abs(lats[0] - LAT) * 3600 < 0.0005, (lons, lats)
    # 20 km around the point lie around its latitude, not near the pole.
    bounds = transform_bounds(code, geographic, 290000.0, 230000.0, 310000.0, 250000.0)
    west, south, east, north = bounds
    assert -66.75 < south < north < -66.5 and 139.7 < west < east < 140.3, bounds


def test_variant_c_codes_keep_their_datum_and_method():
    wkt = CRS.from_epsg(2985).to_wkt()
    for part in (
        'DATUM["Petrels_1972"',
        'PROJECTION["Polar_Stereographic_(variant_C)"]',
        'AUTHORITY["EPSG","2985"]',
    ):
        assert part in wkt, wkt
    # From WGS 84, the same point directly and through the CRS's geographic CRS.
    x, y = transform("EPSG:4326", "EPSG:2985", [LON], [LAT])
    glon, glat = transform("EPSG:4326", "EPSG:4636", [LON], [LAT])
    gx, gy = transform("EPSG:4636", "EPSG:2985", glon, glat)
    assert np.hypot(x[0] - gx[0], y[0] - gy[0]) < 0.001, (x, y, gx, gy)


def test_a_variant_c_raster_warps_to_its_latitudes():
    gt = (290000.0, 1000.0, 0.0, 250000.0, 0.0, -1000.0)
    raster = terrio.Raster.from_array(np.ones((20, 20), "float32"), gt, crs="EPSG:2985")
    bounds = terrio.warp(raster, t_srs="EPSG:4636").bounds
    west, south, east, north = bounds
    assert -66.75 < south < north < -66.5 and 139.7 < west < east < 140.3, bounds
