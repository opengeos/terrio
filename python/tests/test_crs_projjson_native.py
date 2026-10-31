"""`CRS.to_dict(projjson=True)` without pyproj.

Applications call it on datasets' CRSs in environments without pyproj, where PROJ parameters
must not silently stand in for PROJJSON: terrio writes the PROJJSON GDAL writes (rasterio
1.5.0: GDAL 3.12.1, PROJ 9.7.1), or raises `CRSError`. Compared with rasterio, live, on: the CRSs of
EPSG codes (`CRS.from_epsg`: PROJ's database objects) for WGS 84 and its 3D CRS, Web Mercator and
every WGS 84 UTM zone; the same CRSs read from GDAL's WKT; and the CRSs of the GeoTIFFs of a
tile server's tests and of terrio's GRIB fixtures as each library opens them. pyproj is made
unimportable for each test, as in those environments.
"""

import sys
from pathlib import Path

import numpy as np
import pytest

import terrio.rio as trio
from terrio.rio.crs import CRS as TCRS
from terrio.rio.errors import CRSError

rasterio = pytest.importorskip("rasterio")
from rasterio.crs import CRS as RCRS  # noqa: E402

GRIB = sorted((Path(__file__).parents[2] / "crates/terrio/tests/data/grib").glob("*.grib2"))
UTM = [32600 + z for z in range(1, 61)] + [32700 + z for z in range(1, 61)]
CODES = [4326, 4979, 3857, *UTM]


@pytest.fixture(autouse=True)
def no_pyproj(monkeypatch):
    """Makes `import pyproj` fail, as in those services' environments."""
    monkeypatch.setitem(sys.modules, "pyproj", None)
    with pytest.raises(ImportError):
        import pyproj  # noqa: F401


@pytest.fixture(autouse=True)
def same_proj():
    """The native writer follows PROJ 9.7.1, which rasterio 1.5.0 bundles."""
    if not str(getattr(rasterio, "__proj_version__", "")).startswith("9.7."):
        pytest.skip(f"rasterio's PROJ is {getattr(rasterio, '__proj_version__', '?')}, not 9.7")


@pytest.mark.parametrize("code", CODES)
def test_code_crs_projjson_is_gdal_s(code):
    """`CRS.from_epsg`: PROJ's database object, with its ensemble, axes and usage."""
    assert TCRS.from_epsg(code).to_dict(projjson=True) == RCRS.from_epsg(code).to_dict(
        projjson=True
    )


@pytest.mark.parametrize("code", CODES)
def test_wkt_crs_projjson_is_gdal_s(code):
    """`CRS.from_wkt` of GDAL's WKT: PROJ's reading of that WKT."""
    wkt = RCRS.from_epsg(code).to_wkt()
    assert TCRS.from_wkt(wkt).to_dict(projjson=True) == RCRS.from_wkt(wkt).to_dict(projjson=True)


@pytest.mark.parametrize("code", [4326, 3857, 32613, 32633])
def test_dataset_crs_projjson_is_gdal_s(tmp_path, code):
    """A tile server's test GeoTIFFs and a Potsdam scene (32633), as each library reads them."""
    path = tmp_path / f"{code}.tif"
    with trio.open(
        path,
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="uint8",
        crs=f"EPSG:{code}",
        transform=trio.transform.from_origin(0, 0, 1, 1),
    ) as ds:
        ds.write(np.zeros((1, 4, 4), "uint8"))
    with trio.open(path) as t, rasterio.open(path) as r:
        assert t.crs.to_dict(projjson=True) == r.crs.to_dict(projjson=True)


# terrio identifies this grid as EPSG:3031 where GDAL leaves it unnamed: a difference in the CRS
# the GRIB driver reads (the same projection), not in the PROJJSON written for it.
IDENTIFIED = {"polar.grib2"}


@pytest.mark.parametrize(
    "path",
    [
        pytest.param(p, marks=pytest.mark.xfail(strict=True, reason="read as EPSG:3031 by terrio"))
        if p.name in IDENTIFIED
        else p
        for p in GRIB
    ],
    ids=[p.name for p in GRIB],
)
def test_grib_crs_projjson_is_gdal_s(path):
    """GRIB grids: spheres, Lambert conformal, polar stereographic and Mercator."""
    with trio.open(path) as t, rasterio.open(path) as r:
        if r.crs is None:
            # A grid GDAL leaves without a CRS (an earth shape without a radius): none either.
            assert t.crs is None
            return
        assert t.crs.to_dict(projjson=True) == r.crs.to_dict(projjson=True)


@pytest.mark.parametrize(
    "make",
    [
        lambda: TCRS.from_proj4("+proj=utm +zone=33 +datum=WGS84 +units=m +no_defs"),
        lambda: TCRS.from_user_input("ESRI:54009"),
        lambda: TCRS.from_epsg(5514),  # Krovak
        lambda: TCRS.from_epsg(4807),  # NTF (Paris): Paris prime meridian
        lambda: TCRS.from_wkt(TCRS.from_epsg(5514).to_wkt()),
    ],
    ids=["proj-string", "esri-code", "krovak-code", "paris-meridian", "krovak-wkt"],
)
def test_crss_terrio_does_not_model_raise(make):
    """Never PROJ parameters in place of PROJJSON."""
    crs = make()
    with pytest.raises(CRSError):
        crs.to_dict(projjson=True)


def test_proj_parameters_are_still_returned_when_asked_for():
    assert TCRS.from_epsg(32633).to_dict() == {
        "proj": "utm",
        "zone": 33,
        "datum": "WGS84",
        "units": "m",
        "no_defs": True,
    }


# The CRS terrio's GRIB driver reads from the fixture `two_bands_with_bitmap.grib2`: its inverse
# flattening has 17 significant digits; PROJ writes PROJJSON numbers with 15.
GRIB_TM = (
    'PROJCS["unnamed",GEOGCS["Coordinate System imported from GRIB file",DATUM["unnamed",'
    'SPHEROID["Spheroid imported from GRIB file",6378206.4,294.97869821391095]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]]],'
    'PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],'
    'PARAMETER["central_meridian",-117],PARAMETER["scale_factor",0.9996],'
    'PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["Metre",1],'
    'AXIS["Easting",EAST],AXIS["Northing",NORTH]]'
)


def test_projjson_numbers_have_proj_s_15_digits():
    """rasterio 1.5.0 on the GRIB file and on its WKT: an inverse flattening of 294.978698213911."""
    expected = RCRS.from_wkt(GRIB_TM).to_dict(projjson=True)
    assert expected["base_crs"]["datum"]["ellipsoid"]["inverse_flattening"] == 294.978698213911
    for crs in (TCRS.from_wkt(GRIB_TM), TCRS._from_dataset(GRIB_TM)):
        assert crs.to_dict(projjson=True) == expected
