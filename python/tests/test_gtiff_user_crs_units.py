"""GeoTIFFs whose CRS has no EPSG code and a unit other than the metre or the degree, written and
read by GDAL (rasterio): terrio.rio reads the CRS GDAL reads (GDAL reads terrio's WKT as its own
CRS), with its linear unit, its EPSG code (or none) and its coordinates.

GDAL writes a UTM zone in another unit than the metre (or on a datum of its own) as a
ProjectionGeoKey with a ProjLinearUnitsGeoKey: the zone's false easting is 500000 metres, in that
unit. Other conversions are written with their parameters, in the unit. Geographic CRSs are
written in their angular unit. Skipped without rasterio.
"""

import re

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.crs import CRS as RCRS  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402
from rasterio.warp import transform as rtransform  # noqa: E402

import terrio.rio as trio  # noqa: E402
import terrio.rio.warp  # noqa: E402,F401
from terrio.rio.crs import CRS as TCRS  # noqa: E402

CONVERSIONS = {
    "utm33n": ("+proj=utm +zone=33", (16.0, 48.0)),
    "utm23s": ("+proj=utm +zone=23 +south", (-44.0, -22.0)),
    # SPCS83 California zone 3 (meter), Lambert conic conformal (2SP).
    "spcs": (
        "+proj=lcc +lat_0=36.5 +lon_0=-120.5 +lat_1=38.4333333333333 +lat_2=37.0666666666667 "
        "+x_0=2000000 +y_0=500000",
        (-120.0, 37.5),
    ),
    "lcc1sp": (
        "+proj=lcc +lat_1=18 +lat_0=18 +lon_0=-77 +k_0=1 +x_0=250000 +y_0=150000",
        (-77.2, 18.1),
    ),
}
# GDAL's UNIT, for the units a PROJ string has no name for.
UNITS = {
    "m": ("+units=m", None),
    "ft": ("+units=ft", None),
    "us-ft": ("+units=us-ft", None),
    "km": ("+units=km", None),
    "clarke-ft": (
        "+to_meter=0.3047972654",
        'UNIT["Clarke\'s foot",0.3047972654,AUTHORITY["EPSG","9005"]]',
    ),
    "half-m": ("+to_meter=0.5", 'UNIT["half metre",0.5]'),
}
GRAD = 'UNIT["grad",0.015707963267949,AUTHORITY["EPSG","9105"]]'


def _projected(conv, units, datum):
    proj, unit = UNITS[units]
    crs = RCRS.from_proj4(f"{CONVERSIONS[conv][0]} {datum} {proj} +no_defs")
    if unit is None:
        return crs
    wkt = crs.to_wkt()
    start = wkt.rindex(',UNIT["unknown"')
    return RCRS.from_wkt(wkt[: start + 1] + unit + wkt[wkt.index("]", start) + 1 :])


CASES = {
    **{
        f"{c}-{u}": (lambda c=c, u=u: _projected(c, u, "+datum=WGS84"), CONVERSIONS[c][1])
        for c in CONVERSIONS
        for u in UNITS
    },
    **{
        f"{c}-{u}-nad83": (lambda c=c, u=u: _projected(c, u, "+datum=NAD83"), CONVERSIONS[c][1])
        for c in ("utm33n", "utm23s")
        for u in ("us-ft", "ft")
    },
    # A datum of its own, based on GRS 1980: no WGS 84 UTM zone.
    "utm33n-ft-grs80": (lambda: _projected("utm33n", "ft", "+ellps=GRS80"), (16.0, 48.0)),
    "utm33n-clarke-ft-grs80": (
        lambda: _projected("utm33n", "clarke-ft", "+ellps=GRS80"),
        (16.0, 48.0),
    ),
    "geog-grad": (
        lambda: RCRS.from_wkt(
            'GEOGCS["unknown",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
            f'PRIMEM["Greenwich",0],{GRAD}]'
        ),
        (2.0, 46.0),
    ),
    "geog-radian-grs80": (
        lambda: RCRS.from_wkt(
            'GEOGCS["unknown",DATUM["unknown",SPHEROID["GRS 1980",6378137,298.257222101]],'
            'PRIMEM["Greenwich",0],UNIT["radian",1,AUTHORITY["EPSG","9101"]]]'
        ),
        (2.0, 46.0),
    ),
}


def _write(path, crs, lonlat):
    (x,), (y,) = rtransform("EPSG:4326", crs, [lonlat[0]], [lonlat[1]])
    res = 1000.0 / crs.linear_units_factor[1] if crs.is_projected else 0.01
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="uint8",
        crs=crs,
        transform=from_origin(x, y, res, res),
    ) as dst:
        dst.write(np.zeros((1, 4, 4), "uint8"))


def _units(crs):
    return crs.linear_units_factor if crs.is_projected else crs.units_factor


@pytest.mark.parametrize("name", sorted(CASES))
def test_user_defined_crs_units_are_gdals(tmp_path, name):
    make, lonlat = CASES[name]
    path = str(tmp_path / f"{name}.tif")
    with rasterio.Env():
        _write(path, make(), lonlat)
    with rasterio.open(path) as g, trio.open(path) as t:
        assert RCRS.from_wkt(t.crs.to_wkt()) == g.crs, (t.crs.to_wkt(), g.crs.to_wkt())
        assert t.crs.to_epsg() == g.crs.to_epsg()
        assert _units(t.crs)[0] == _units(g.crs)[0]
        assert _units(t.crs)[1] == pytest.approx(_units(g.crs)[1], rel=1e-12)
        corners = [g.transform * p for p in ((0, 0), (4, 0), (0, 4), (4, 4), (2, 2))]
        xs, ys = [list(v) for v in zip(*corners, strict=True)]
        theirs = rtransform(g.crs, "EPSG:4326", xs, ys)
        ours = trio.warp.transform(t.crs, "EPSG:4326", xs, ys)
        np.testing.assert_allclose(
            np.asarray(ours, float), np.asarray(theirs, float), rtol=0, atol=1e-9
        )


def test_utm_zone_in_us_survey_feet_is_not_the_zone_in_metres(tmp_path):
    """A UTM zone in US survey feet: GDAL reads a transverse Mercator in feet, without code."""
    path = str(tmp_path / "tmerc_ft.tif")
    crs = RCRS.from_string(
        "+proj=tmerc +lat_0=0 +lon_0=15 +k=0.9996 +x_0=500000 +y_0=0 +datum=WGS84 "
        "+units=us-ft +no_defs"
    )
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="uint8",
        crs=crs,
        transform=from_origin(1640000, 16400000, 30, 30),
    ) as dst:
        dst.write(np.zeros((1, 4, 4), "uint8"))
    with rasterio.open(path) as g, trio.open(path) as t:
        assert (g.crs.to_epsg(), g.crs.linear_units) == (None, "US survey foot")
        assert (t.crs.to_epsg(), t.crs.linear_units) == (None, "US survey foot")
        assert 'PARAMETER["false_easting",1640416.66666667]' in t.crs.to_wkt()


def test_inverse_flattening_and_esri_name_are_gdals(tmp_path):
    """GDAL computes an ellipsoid's inverse flattening from its axes when the citation does not
    name it, and names a CRS whose citation names an ESRI CRS by that CRS."""
    laea = str(tmp_path / "laea.tif")
    esri = str(tmp_path / "esri.tif")
    for path, crs in (
        (
            laea,
            "+proj=laea +lat_0=50 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80 "
            "+units=m +no_defs",
        ),
        (esri, "ESRI:102003"),
    ):
        with rasterio.open(
            path,
            "w",
            driver="GTiff",
            width=4,
            height=4,
            count=1,
            dtype="uint8",
            crs=RCRS.from_string(crs),
            transform=from_origin(0, 0, 1000, 1000),
        ) as dst:
            dst.write(np.zeros((1, 4, 4), "uint8"))
    with rasterio.open(laea) as g, trio.open(laea) as t:
        spheroid = [re.search(r"SPHEROID\[[^]]*\]\]", c.to_wkt()).group(0) for c in (g.crs, t.crs)]
        assert "298.257222101004" in spheroid[0]
        assert spheroid[1] == spheroid[0]
    with rasterio.open(esri) as g, trio.open(esri) as t:
        assert g.crs.to_authority() == ("ESRI", "102003")
        assert t.crs.to_wkt() == g.crs.to_wkt()
        assert (t.crs.to_epsg(), t.crs.to_authority()) == (g.crs.to_epsg(), g.crs.to_authority())


@pytest.mark.parametrize("code", [2227, 2263, 26946, 3857])
def test_proj_parameters_give_false_eastings_in_metres(code):
    """PROJ strings give false eastings and northings in metres, whatever the CRS's unit."""
    for crs in (TCRS.from_epsg(code), TCRS.from_wkt(RCRS.from_epsg(code).to_wkt())):
        ours, theirs = crs.to_dict(), RCRS.from_epsg(code).to_dict()
        for key in ("x_0", "y_0"):
            assert ours.get(key, 0) == pytest.approx(theirs.get(key, 0), rel=1e-12, abs=1e-9)
