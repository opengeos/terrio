"""The WKT terrio writes for a CRS given as a PROJ string (`CRS.to_wkt()`, and the CRS of the
GeoTIFFs it writes) describes the same CRS (#124): PROJ (pyproj) reading the WKT gives PROJ's
coordinates for the PROJ string, terrio reading it terrio's, and rasterio (GDAL) reads a GeoTIFF
terrio writes as the PROJ string. Each engine is compared with itself: for some projections terrio
and PROJ differ on the PROJ string itself. The tests are skipped when pyproj or rasterio is not
installed.
"""

import math

import numpy as np
import pytest

import terrio
import terrio.rio as trio

W = "+ellps=WGS84 +units=m"
B = "+ellps=bessel +units=m"
# name: (PROJ string, a point near which it is checked)
CASES = {
    # The issue's.
    "merc lat_ts": (f"+proj=merc +lat_ts=33 +lon_0=-100 {W}", (-95.0, 40.0)),
    "lcc 1SP k_0": (f"+proj=lcc +lat_1=40 +lat_0=40 +lon_0=-96 +k_0=0.999 {W}", (-90.0, 45.0)),
    "somerc": (f"+proj=somerc +lat_0=46.95 +lon_0=7.44 {B}", (8.0, 47.0)),
    "krovak": (f"+proj=krovak {B}", (15.0, 50.0)),
    "krovak swu": (f"+proj=krovak +axis=swu {B}", (15.0, 50.0)),
    "hammer": (f"+proj=hammer {W}", (30.0, 40.0)),
    "nzmg": ("+proj=nzmg +ellps=intl +units=m", (174.0, -41.0)),
    "merc k": (f"+proj=merc +k=0.99 +lon_0=-100 {W}", (-95.0, 40.0)),
    "eqc lat_ts": (f"+proj=eqc +lat_ts=30 {W}", (10.0, 40.0)),
    "omerc A": (
        f"+proj=omerc +lat_0=4 +lonc=115 +alpha=53.31 +gamma=53.13 +k=0.99984 +no_uoff {W}",
        (116.0, 5.0),
    ),
    "omerc gamma": (
        f"+proj=omerc +lat_0=4 +lonc=115 +alpha=53.31 +gamma=40 +k=0.99984 {W}",
        (116.0, 5.0),
    ),
    "loxim": (f"+proj=loxim +lat_1=40 {W}", (30.0, 50.0)),
    "collg": (f"+proj=collg {W}", (30.0, 40.0)),
    "us-ft": (
        "+proj=tmerc +lon_0=-100 +k=0.9996 +x_0=500000 +ellps=WGS84 +units=us-ft",
        (-95.0, 40.0),
    ),
    "towgs84": (f"+proj=tmerc +lon_0=-100 +towgs84=100,50,-30 {W}", (-95.0, 40.0)),
    "pm ferro": (f"+proj=tmerc +lon_0=10 +pm=ferro {W}", (28.0, 48.0)),
    "etmerc": (f"+proj=etmerc +lon_0=10 {W}", (12.0, 48.0)),
    # Every other projection terrio computes, with parameters other than the defaults.
    "tmerc": (f"+proj=tmerc +lat_0=5 +lon_0=10 +k=0.9995 +x_0=1000 +y_0=2000 {W}", (12.0, 8.0)),
    "tmerc south": (f"+proj=tmerc +axis=wsu +lon_0=29 {W}", (28.0, -26.0)),
    "utm south": (f"+proj=utm +zone=33 +south {W}", (14.0, -30.0)),
    "datum": ("+proj=utm +zone=33 +datum=WGS84 +units=m", (14.0, 30.0)),
    "datum OSGB36": (
        "+proj=tmerc +lat_0=49 +lon_0=-1 +k=0.9996 +x_0=400000 +y_0=-100000 +datum=OSGB36",
        (-2.0, 53.0),
    ),
    "lcc 2SP": (
        f"+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +x_0=1000 +y_0=2000 {W}",
        (-90.0, 41.0),
    ),
    "lcc 2SP k": (
        f"+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +k_0=0.999 {W}",
        (-90.0, 41.0),
    ),
    "aea": (
        f"+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=1000 +y_0=2000 {W}",
        (-90.0, 41.0),
    ),
    "eqdc": (
        f"+proj=eqdc +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=1000 {W}",
        (-90.0, 41.0),
    ),
    "laea": (f"+proj=laea +lat_0=52 +lon_0=10 +x_0=1000 +y_0=2000 {W}", (12.0, 50.0)),
    "aeqd": (f"+proj=aeqd +lat_0=40 +lon_0=20 +x_0=1000 {W}", (25.0, 45.0)),
    "stere": (f"+proj=stere +lat_0=40 +lon_0=10 +k=0.999 +x_0=1000 {W}", (12.0, 42.0)),
    "stere polar": (f"+proj=stere +lat_0=90 +lon_0=-45 +k=0.994 +x_0=1000 {W}", (-40.0, 75.0)),
    "stere polar lat_ts": (
        f"+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +x_0=1000 {W}",
        (10.0, -75.0),
    ),
    "ups": (f"+proj=ups {W}", (30.0, 85.0)),
    "sterea": (
        f"+proj=sterea +lat_0=52.15 +lon_0=5.38 +k=0.9999 +x_0=155000 +y_0=463000 {W}",
        (6.0, 52.0),
    ),
    "cass": (f"+proj=cass +lat_0=10 +lon_0=-61 +x_0=1000 {W}", (-60.0, 11.0)),
    "poly": (f"+proj=poly +lat_0=10 +lon_0=-61 +x_0=1000 {W}", (-60.0, 11.0)),
    "eqc lat_0": (f"+proj=eqc +lat_ts=30 +lat_0=10 +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "sinu": (f"+proj=sinu +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "bonne": (f"+proj=bonne +lat_1=40 +lon_0=20 +x_0=1000 {W}", (25.0, 45.0)),
    "mill": (f"+proj=mill +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "vandg": (f"+proj=vandg +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "gnom": (f"+proj=gnom +lat_0=40 +lon_0=20 +x_0=1000 {W}", (25.0, 42.0)),
    "cea": (f"+proj=cea +lat_ts=30 +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "eqearth": (f"+proj=eqearth +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "omerc B": (
        f"+proj=omerc +lat_0=4 +lonc=115 +alpha=53.31 +k=0.99984 +x_0=1000 {W}",
        (116.0, 5.0),
    ),
    "mod_krovak": (f"+proj=mod_krovak {B}", (15.0, 50.0)),
    "labrd": (
        f"+proj=labrd +lat_0=-18.9 +lon_0=46.43 +azi=18.9 +k_0=0.9995 +x_0=400000 +y_0=800000 {W}",
        (47.0, -19.0),
    ),
    "col_urban": (
        f"+proj=col_urban +lat_0=4.68 +lon_0=-74.14 +x_0=92334.879 +y_0=109320.965 +h_0=2550 {W}",
        (-74.0, 4.7),
    ),
    "ortho": (f"+proj=ortho +lat_0=40 +lon_0=20 +x_0=1000 {W}", (25.0, 42.0)),
    "geos": (f"+proj=geos +h=35786023 +lon_0=-75 +sweep=x {W}", (-70.0, 10.0)),
    "robin": (f"+proj=robin +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "wintri": (f"+proj=wintri +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "wink1": (f"+proj=wink1 +lat_ts=30 +lon_0=20 {W}", (25.0, 40.0)),
    "wink2": (f"+proj=wink2 +lat_1=30 +lon_0=20 {W}", (25.0, 40.0)),
    "gall": (f"+proj=gall +lon_0=20 {W}", (25.0, 40.0)),
    "gstmerc": (
        f"+proj=gstmerc +lat_0=-21.11666667 +lon_0=55.53333333 +x_0=160000 +y_0=50000 {W}",
        (55.5, -21.0),
    ),
    "fouc_s": (f"+proj=fouc_s +n=0.5 +lon_0=20 {W}", (25.0, 40.0)),
    "mbtfps": (f"+proj=mbtfps +lon_0=20 {W}", (25.0, 40.0)),
    "putp4p": (f"+proj=putp4p +lon_0=20 {W}", (25.0, 40.0)),
    "murd2": (f"+proj=murd2 +lat_1=30 +lat_2=50 +lon_0=20 {W}", (25.0, 40.0)),
    "tissot": (f"+proj=tissot +lat_1=30 +lat_2=50 +lon_0=20 {W}", (25.0, 40.0)),
    # Projections terrio computes with wbprojection.
    "moll": (f"+proj=moll +lon_0=20 +x_0=1000 {W}", (25.0, 40.0)),
    "eck4": (f"+proj=eck4 +lon_0=20 {W}", (25.0, 40.0)),
    "eck6": (f"+proj=eck6 +lon_0=20 {W}", (25.0, 40.0)),
    "aitoff": (f"+proj=aitoff +lon_0=20 {W}", (25.0, 40.0)),
    "natearth": (f"+proj=natearth +lon_0=20 {W}", (25.0, 40.0)),
    "wag3": (f"+proj=wag3 +lon_0=20 {W}", (25.0, 40.0)),
    "kav7": (f"+proj=kav7 +lon_0=20 {W}", (25.0, 40.0)),
    # Units, datum shifts and prime meridians.
    "km": (
        "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80 +units=km",
        (12.0, 50.0),
    ),
    "to_meter": ("+proj=tmerc +lon_0=10 +R=6371000 +to_meter=2.5", (12.0, 48.0)),
    "towgs84 7": (
        "+proj=tmerc +lon_0=10 +ellps=bessel +units=m"
        " +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7",
        (12.0, 48.0),
    ),
    "pm number": ("+proj=tmerc +lon_0=10 +pm=3.5 +a=6378000 +rf=300 +units=m", (16.0, 48.0)),
    "longlat pm": ("+proj=longlat +ellps=bessel +pm=paris +towgs84=1,2,3", (8.0, 48.0)),
}


def _points(name):
    """Points around a case's point, in degrees."""
    lon, lat = CASES[name][1]
    return [(lon + dx, lat + dy) for dx in (-1.0, 0.0, 1.5) for dy in (-1.0, 0.0, 1.5)]


def _dist(a, b):
    """The largest distance between two lists of points."""
    return max(math.hypot(p[0] - q[0], p[1] - q[1]) for p, q in zip(a, b, strict=True))


@pytest.mark.parametrize("name", sorted(CASES))
def test_terrio_reads_its_wkt_as_the_proj_string(name):
    """terrio reading the WKT gives terrio's coordinates for the PROJ string."""
    p = CASES[name][0]
    wkt = trio.crs.CRS.from_string(p).to_wkt()
    lons, lats = zip(*_points(name), strict=True)
    a = list(zip(*terrio.transform_points(list(lons), list(lats), "EPSG:4326", p), strict=True))
    b = list(zip(*terrio.transform_points(list(lons), list(lats), "EPSG:4326", wkt), strict=True))
    assert _dist(a, b) < 1e-3, wkt


@pytest.mark.parametrize("name", sorted(CASES))
def test_proj_reads_terrio_wkt_as_the_proj_string(name):
    """PROJ reading the WKT gives PROJ's coordinates for the PROJ string."""
    pyproj = pytest.importorskip("pyproj")
    p = CASES[name][0]
    wkt = trio.crs.CRS.from_string(p).to_wkt()
    to_p = pyproj.Transformer.from_crs("EPSG:4326", pyproj.CRS(p), always_xy=True)
    to_wkt = pyproj.Transformer.from_crs("EPSG:4326", pyproj.CRS(wkt), always_xy=True)
    pts = _points(name)
    a, b = [to_p.transform(*q) for q in pts], [to_wkt.transform(*q) for q in pts]
    assert _dist(a, b) < 1e-3, wkt


@pytest.mark.parametrize("name", sorted(CASES))
def test_rasterio_reads_terrio_geotiff_as_the_proj_string(name, tmp_path):
    """GDAL reads a GeoTIFF terrio writes with the CRS's PROJ string as that PROJ string."""
    pytest.importorskip("rasterio")
    import rasterio.warp
    from rasterio.transform import from_origin

    if not trio.crs.CRS.from_string(CASES[name][0]).is_projected:
        pytest.skip("a geographic CRS")
    p = CASES[name][0]
    path = tmp_path / "crs.tif"
    profile = dict(
        driver="GTiff", width=2, height=2, count=1, dtype="uint8", transform=from_origin(0, 0, 1, 1)
    )
    with trio.open(path, "w", crs=trio.crs.CRS.from_string(p), **profile) as f:
        f.write(np.zeros((1, 2, 2), np.uint8))
    with rasterio.open(path) as f:
        read = f.crs
    lons, lats = map(list, zip(*_points(name), strict=True))
    given = rasterio.crs.CRS.from_string(p)
    a = list(zip(*rasterio.warp.transform("EPSG:4326", given, lons, lats), strict=True))
    b = list(zip(*rasterio.warp.transform("EPSG:4326", read, lons, lats), strict=True))
    assert _dist(a, b) < 1e-3, read.to_wkt()
