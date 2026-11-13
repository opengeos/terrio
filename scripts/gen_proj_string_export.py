"""Freeze PROJ's PROJ strings of CRSs for ``SpatialRef::to_proj4`` (``tests/proj_string_export.rs``).

PROJ 9.8.1 (pyproj 3.8.0) writes each CRS with ``CRS.to_proj4()`` (``proj_as_proj_string`` with
``PJ_PROJ_4``, as ``projinfo -o PROJ``), with the network off. The rows are every 20th EPSG and
ESRI code, the codes behind each rule of terrio's writer (named datums, prime meridians, axis
orders, vertical and compound CRSs, Web Mercator, CRSs PROJ cannot write), custom definitions
(PROJ strings and WKT, bound CRSs among them), compound references and every 20th IAU_2015 CRS.
``NONE`` marks a CRS PROJ has no PROJ string for.

Usage: python scripts/gen_proj_string_export.py (needs pyproj 3.8.0)

``--all PATH`` writes every EPSG, ESRI and IAU_2015 code instead, to PATH (not committed), for the
whole-table comparison: ``TERRIO_PROJ_STRINGS_ALL=PATH cargo test -p terrio-crs --test
proj_string_export -- --ignored``.
"""

import argparse
import re
import warnings
from pathlib import Path

import pyproj
from pyproj import CRS
from pyproj.database import query_crs_info

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/proj_string_export.tsv"
)

# The codes behind each rule of the writer.
RULES = [
    ("EPSG", c)
    for c in (
        4326,
        3857,
        3785,
        2056,
        27700,
        4269,
        4267,
        26917,
        2204,
        4978,
        9000,
        32632,
        2154,
        3035,
        2065,
        5514,
        2046,
        8044,
        4801,
        4807,
        4810,
        4901,
        2062,
        7405,
        5703,
        5754,
        5702,
        2985,
        2218,
    )
] + [("ESRI", c) for c in (102100, 54030, 53049, 54049, 102035, 54008, 53030, 53012, 53077, 54012, 54077)]

PROJ_STRINGS = [
    "+proj=utm +zone=32 +datum=WGS84",
    "+proj=utm +zone=32 +ellps=WGS84 +towgs84=0,0,0",
    (
        "+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 +ellps=airy "
        "+towgs84=446.448,-125.157,542.06,0.15,0.247,0.842,-20.489 +units=m"
    ),
    (
        "+proj=somerc +lat_0=46.9524055555556 +lon_0=7.43958333333333 +k_0=1 +x_0=2600000 +y_0=1200000 "
        "+ellps=bessel +towgs84=674.374,15.056,405.346,0,0,0,0 +units=m +no_defs"
    ),
    "+proj=longlat +datum=NAD27",
    "+proj=longlat +ellps=clrk66 +nadgrids=@conus,@alaska",
    "+proj=longlat +ellps=WGS84 +pm=paris",
    "+proj=longlat +a=6378137 +rf=298.257223563",
    "+proj=utm +zone=17 +datum=NAD83 +units=us-ft",
    (
        "+proj=lcc +lat_0=36.6666666666667 +lon_0=-105.5 +lat_1=38.4333333333333 +lat_2=37.2333333333333 "
        "+x_0=914401.828803658 +y_0=304800.609601219 +units=us-ft +a=6378137.0 +rf=298.257222101"
    ),
    (
        "+proj=merc +lon_0=3.19228055555556 +k=0.997 +x_0=3900000 +y_0=900000 +ellps=bessel +units=m "
        "+pm=106.807719444"
    ),
    "+proj=robin +lon_0=0 +x_0=0 +y_0=0 +R=6371000 +units=m",
    "+proj=aeqd +lat_0=47 +lon_0=8 +x_0=0 +y_0=0 +datum=WGS84 +units=m",
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=3500000 +y_0=0 +a=6377397.155 +b=6356078.962818189 +to_meter=1",
    # Ellipsoids: spheres as +R, named ones by name, others as given (+rf or +b), the shape
    # parameters in PROJ's order of precedence, +a changing a named ellipsoid.
    "+proj=sinu +lon_0=0 +x_0=0 +y_0=0 +a=6371007.181 +b=6371007.181 +units=m",
    "+proj=longlat +a=6371000",
    "+proj=longlat +R=6370997",
    "+proj=longlat +a=6378000 +b=6356000",
    "+proj=longlat +a=6378000 +rf=300",
    "+proj=longlat +a=6378137 +f=0.00335",
    "+proj=longlat +a=6378000 +es=0.006",
    "+proj=longlat +a=6378137 +e=0.08",
    "+proj=longlat +a=6378137 +f=0",
    "+proj=longlat +a=6378137 +b=6356000 +f=0.003",
    "+proj=longlat +a=6378137 +rf=298.257223563 +b=6356000",
    "+proj=longlat +a=6378206.4 +b=6356583.8",
    "+proj=longlat +ellps=clrk66 +a=6378000",
    "+proj=longlat +ellps=bessel +rf=300",
    "+proj=longlat +ellps=bessel +b=6356000",
    "+proj=longlat +a=6378000 +b=6378000 +ellps=WGS84",
    # Axis orders: only south-orientated Transverse Mercator's and Krovak's are written.
    "+proj=longlat +datum=WGS84 +axis=neu",
    "+proj=utm +zone=32 +datum=WGS84 +axis=neu",
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=0 +y_0=0 +ellps=bessel +axis=wsu +units=m",
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=0 +y_0=0 +ellps=bessel +axis=nwu +units=m",
    # Planetocentric latitudes: written on a sphere, not on an ellipsoid.
    "+proj=longlat +geoc +R=6371000",
    "+proj=longlat +geoc +datum=WGS84",
    # Web Mercator from GDAL's PROJ string or webmerc; other spherical Mercators as they are.
    # (These, like the others, give all their parameters: PROJ adds the defaults, terrio not.)
    "+proj=merc +a=6378137 +b=6378137 +lat_ts=0 +lon_0=0 +x_0=0 +y_0=0 +k=1 +units=m +nadgrids=@null +wktext +no_defs",
    "+proj=merc +a=6378137 +b=6378137 +lon_0=0 +x_0=0 +y_0=0 +k=1 +units=m +nadgrids=@null",
    "+proj=webmerc +datum=WGS84",
    "+proj=webmerc +datum=WGS84 +lon_0=10 +x_0=500 +units=ft",
    # A prime meridian by its longitude, as PROJ writes numbers; a quoted grid name.
    "+proj=longlat +ellps=WGS84 +pm=2.5000000000000000",
    "+proj=longlat +ellps=WGS84 +pm=2.33720833333333333",
    '+proj=longlat +datum=WGS84 +geoidgrids="@my grid.gtx"',
    # Angles in degrees, minutes and seconds, whose second marks are no quotes, in decimal degrees.
    "+proj=tmerc +lat_0=52d30'10\"N +lon_0=5d +k=1 +x_0=0 +y_0=0 +units=ft +ellps=GRS80",
    "+proj=longlat +ellps=GRS80 +pm=2d20'14.025\"E",
    # A hemisphere letter gives the sign, over a minus sign.
    "+proj=tmerc +lat_0=-52d30'10\"S +lon_0=-5d10'20\"W +k=1 +x_0=0 +y_0=0 +ellps=GRS80 +units=m",
    "+proj=tmerc +lat_0=-52d30'10\"N +lon_0=5 +k=1 +x_0=0 +y_0=0 +ellps=GRS80 +units=m",
    "+proj=tmerc +lat_0=-52.5S +lon_0=-5W +k=1 +x_0=0 +y_0=0 +ellps=GRS80 +units=m",
    "+proj=webmerc +datum=WGS84 +lon_0=10d",
    # Quoted values, unquoted as PROJ reads them; words that are no parameters dropped.
    '+proj=longlat +ellps=GRS80 +towgs84="1,2,3"',
    '+proj=tmerc +lat_0="10" +lon_0=5 +k=1 +x_0="500" +y_0=0 +ellps=GRS80 +units=m',
    '+proj=longlat +ellps=GRS80 +pm="2.337229166667"',
    '+proj=longlat +datum=WGS84 +geoidgrids="a ""b"" c"',
    "+proj=longlat +ellps=GRS80 foo",
    "+proj=merc +lon_0=10 +k=1 +x_0=5 +y_0=7 +a=6378137 +b=6378137 +nadgrids=@null +units=ft",
    "+proj=merc +lon_0=10 +k=1 +x_0=0 +y_0=0 +a=6378137 +b=6378137 +nadgrids=@null +wktext",
    "+proj=merc +lon_0=0 +k=1 +x_0=0 +y_0=0 +a=6378137 +b=6378137",
    # Grids: +nadgrids before the unit and in place of +towgs84, geoid grids with their CRS and
    # unit, a vertical unit only with a geoid model; numbers as C's %.15g.
    "+proj=tmerc +lat_0=0 +lon_0=0 +k=1 +x_0=0 +y_0=0 +ellps=clrk66 +nadgrids=@conus +units=m",
    "+proj=utm +zone=32 +ellps=GRS80 +towgs84=1,2,3 +nadgrids=@x +wktext",
    "+proj=longlat +datum=WGS84 +geoidgrids=@egm96_15.gtx",
    "+proj=longlat +datum=WGS84 +geoidgrids=@egm96_15.gtx +vunits=us-ft",
    "+proj=utm +zone=3 +datum=WGS84 +vunits=ft",
    "+proj=merc +lon_0=0 +k=1 +x_0=0 +y_0=0 +ellps=WGS84 +to_meter=0.00001",
    # The prime meridian before the shift.
    "+proj=longlat +ellps=bessel +towgs84=1,2,3 +pm=paris",
    # Datums: WGS84, NAD83 and NAD27 by name when nothing changes them, else (and the others)
    # their ellipsoid.
    "+proj=longlat +datum=NAD27 +rf=300",
    "+proj=longlat +datum=NAD27 +b=6356000",
    "+proj=longlat +datum=WGS84 +a=6378000",
    "+proj=longlat +datum=WGS84 +towgs84=1,2,3",
    "+proj=longlat +datum=WGS84 +nadgrids=@null",
    "+proj=longlat +datum=WGS84 +pm=paris",
    "+proj=longlat +datum=potsdam",
    "+proj=longlat +datum=OSGB36",
    "+proj=longlat +datum=ire65",
    "+proj=longlat +datum=carthage +towgs84=-263,6,431",
    # An ellipsoid PROJ defines by its semi-minor axis keeps its name but for +b.
    "+proj=longlat +ellps=clrk66 +rf=300",
    "+proj=longlat +ellps=clrk66 +f=0",
    "+proj=longlat +ellps=clrk66 +b=6356000",
    # A flattening as +f, +es or +e on a named ellipsoid or datum is dropped, but orthographic's
    # +f=0 (ESRI:102035's sphere).
    "+proj=ortho +f=0 +lat_0=90 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84 +units=m",
    "+proj=ortho +f=0.003 +lat_0=0 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84 +units=m",
    "+proj=longlat +f=0.001 +datum=WGS84",
    "+proj=longlat +ellps=GRS80 +es=0.006",
    "+proj=longlat +datum=WGS84 +rf=300",
    "+proj=longlat +a=6378137 +b=6356752 +datum=WGS84",
    # +proj written first, wherever it is given.
    "+lat_0=0 +proj=tmerc +lon_0=9 +k=0.9996 +x_0=500000 +y_0=0 +ellps=GRS80 +units=m",
    "+lat_0=10 +proj=tmerc +lon_0=9 +k=1 +x_0=0 +y_0=0 +ellps=GRS80 +units=m",
    # +ellps beside +datum is ignored (GDAL 2 and R wrote both).
    "+proj=longlat +ellps=WGS84 +datum=WGS84 +no_defs",
    "+proj=utm +zone=17 +datum=NAD27 +ellps=clrk66 +units=m",
    "+proj=longlat +ellps=intl +datum=WGS84 +pm=paris",
    "+proj=longlat +datum=WGS84 +ellps=WGS84 +towgs84=0,0,0",
    # Aliases of longlat.
    "+proj=latlong +datum=WGS84",
    "+proj=lonlat +ellps=intl",
    # Units by PROJ's names.
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=0 +y_0=0 +ellps=GRS80 +to_meter=1000",
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=0 +y_0=0 +ellps=GRS80 +to_meter=1.8288",
    "+proj=longlat +datum=WGS84 +geoidgrids=@egm96_15.gtx +vto_meter=1.8288",
    # UTM zones as +proj=utm, in metres only.
    "+proj=tmerc +lat_0=0 +lon_0=-93 +k=0.9996 +x_0=500000 +y_0=10000000 +ellps=GRS80 +units=m",
    "+proj=tmerc +lat_0=0 +lon_0=-93 +k=0.9996 +x_0=500000 +y_0=0 +datum=WGS84 +units=ft",
    "+proj=tmerc +lat_0=0 +lon_0=-93.5 +k=0.9996 +x_0=500000 +y_0=0 +datum=WGS84 +units=m",
]

WKTS = [
    # An ESRI .prj of CH1903+ / LV95, which terrio identifies as EPSG:2056.
    (
        'PROJCS["CH1903+_LV95",GEOGCS["GCS_CH1903+",DATUM["D_CH1903+",SPHEROID["Bessel_1841",6377397.155,'
        '299.1528128]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION['
        '"Hotine_Oblique_Mercator_Azimuth_Center"],PARAMETER["False_Easting",2600000.0],PARAMETER['
        '"False_Northing",1200000.0],PARAMETER["Scale_Factor",1.0],PARAMETER["Azimuth",90.0],PARAMETER['
        '"Longitude_Of_Center",7.439583333333333],PARAMETER["Latitude_Of_Center",46.95240555555556],UNIT['
        '"Meter",1.0]]'
    ),
    # Bound CRSs: TOWGS84 makes PROJ write the ellipsoid and +towgs84.
    (
        'GEOGCS["unnamed",DATUM["unnamed",SPHEROID["Bessel 1841",6377397.155,299.1528128],TOWGS84[598.1,'
        '73.7,418.2,0.202,0.045,-2.455,6.7]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]'
    ),
    (
        'PROJCS["unnamed",GEOGCS["unnamed",DATUM["unnamed",SPHEROID["Airy 1830",6377563.396,299.3249646],'
        'TOWGS84[446.448,-125.157,542.06,0.15,0.247,0.842,-20.489]],PRIMEM["Greenwich",0],UNIT["degree",'
        '0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",49],'
        'PARAMETER["central_meridian",-2],PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",'
        '400000],PARAMETER["false_northing",-100000],UNIT["metre",1]]'
    ),
    # A WKT on WGS 84 that terrio identifies with no code: +datum=WGS84 from its datum's name.
    (
        'PROJCS["custom",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,'
        '298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION['
        '"Lambert_Azimuthal_Equal_Area"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],'
        'PARAMETER["Central_Meridian",8.25],PARAMETER["Latitude_Of_Origin",46.75],UNIT["Meter",1.0]]'
    ),
    # An engineering CRS has no PROJ string.
    'LOCAL_CS["Engineering",LOCAL_DATUM["site",0],UNIT["metre",1],AXIS["X",EAST],AXIS["Y",NORTH]]',
    # A UTM zone without a code, and a CRS in Clarke's feet (numbers as PROJ writes them).
    (
        'PROJCS["custom",GEOGCS["GRS 80",DATUM["custom",SPHEROID["GRS 1980",6378137,298.257222101]],'
        'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
        'PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-93],PARAMETER["scale_factor",0.9996],'
        'PARAMETER["false_easting",500000],PARAMETER["false_northing",10000000],UNIT["metre",1]]'
    ),
    (
        'PROJCS["custom",GEOGCS["custom",DATUM["custom",SPHEROID["Clarke 1880",6378249.145,293.465]],'
        'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
        'PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-62],PARAMETER["scale_factor",0.9995],'
        'PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["Clarke\'s foot",0.3047972654]]'
    ),
    # Compound CRSs of WKT: a geoid model, a vertical unit in fathoms.
    (
        'COMPD_CS["WGS 84 + EGM96",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
        'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],VERT_CS["EGM96 height",VERT_DATUM["EGM96",'
        '2005,EXTENSION["PROJ4_GRIDS","egm96_15.gtx"]],UNIT["metre",1],AXIS["Up",UP]]]'
    ),
    (
        'COMPD_CS["WGS 84 + EGM96",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
        'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],VERT_CS["EGM96 height",VERT_DATUM["EGM96",'
        '2005,EXTENSION["PROJ4_GRIDS","egm96 15.gtx"]],UNIT["metre",1],AXIS["Up",UP]]]'
    ),
    (
        'COMPD_CS["WGS 84 + depth",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
        'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],VERT_CS["height",VERT_DATUM["local",2005],'
        'UNIT["fathom",1.8288],AXIS["Up",UP]]]'
    ),
    # A PROJ4 extension, which PROJ writes as it is.
    (
        'PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM['
        '"Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER['
        '"latitude_of_origin",0],PARAMETER["central_meridian",9],PARAMETER["scale_factor",0.9996],PARAMETER['
        '"false_easting",500000],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4",'
        '"+proj=tmerc +lon_0=9 +k=0.9996 +x_0=500000 +ellps=WGS84 +towgs84=0,0,0"]]'
    ),
    # GDAL's Web Mercator as an extension, which PROJ writes as its own; +type=crs moved last;
    # a compound CRS's vertical unit after a horizontal extension.
    (
        'PROJCS["Google Maps Global Mercator",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,'
        '298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Mercator_1SP"],'
        'PARAMETER["central_meridian",0],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER['
        '"false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=merc +a=6378137 +b=6378137 +lat_ts=0.0 '
        '+lon_0=0.0 +x_0=0.0 +y_0=0 +k=1.0 +units=m +nadgrids=@null +wktext +no_defs"]]'
    ),
    (
        'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
        'UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","+proj=longlat +datum=WGS84 +type=crs +no_defs"]]'
    ),
    (
        'COMPD_CS["c",PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
        'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER['
        '"latitude_of_origin",0],PARAMETER["central_meridian",9],PARAMETER["scale_factor",0.9996],PARAMETER['
        '"false_easting",500000],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4",'
        '"+proj=tmerc +lon_0=9 +k=0.9996 +x_0=500000 +ellps=WGS84 +no_defs"]],VERT_CS["h",VERT_DATUM["x",2005],'
        'UNIT["metre",1],AXIS["Up",UP]]]'
    ),
    # A datum PROJ names, on another ellipsoid: the ellipsoid, not the datum.
    (
        'GEOGCS["x",DATUM["WGS_1984",SPHEROID["Clarke 1866",6378206.4,294.978698213898]],PRIMEM["Greenwich",0],'
        'UNIT["degree",0.0174532925199433]]'
    ),
    # A geoid model's heights are of the horizontal CRS but on WGS 84, also after an extension.
    (
        'COMPD_CS["c",GEOGCS["x",DATUM["d",SPHEROID["GRS 1980",6378137,298.257222101]],PRIMEM["Greenwich",0],'
        'UNIT["degree",0.0174532925199433]],VERT_CS["v",VERT_DATUM["vd",2005,EXTENSION["PROJ4_GRIDS","g.gtx"]],'
        'UNIT["metre",1]]]'
    ),
    (
        'COMPD_CS["c",GEOGCS["x",DATUM["d",SPHEROID["GRS 1980",6378137,298.257222101]],PRIMEM["Greenwich",0],'
        'UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","+proj=longlat +datum=WGS84"]],VERT_CS["v",VERT_DATUM['
        '"vd",2005,EXTENSION["PROJ4_GRIDS","my grid.gtx"]],UNIT["metre",1]]]'
    ),
    (
        'COMPD_CS["c",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563],TOWGS84[0,0,0,0,0,0,'
        '0]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],VERT_CS["v",VERT_DATUM["vd",2005,EXTENSION['
        '"PROJ4_GRIDS","g.gtx"]],UNIT["metre",1]]]'
    ),
    (
        'COMPD_CS["c",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",'
        '0],UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","+proj=longlat +datum=WGS84 +no_defs"]],VERT_CS["v",'
        'VERT_DATUM["vd",2005,EXTENSION["PROJ4_GRIDS","g.gtx"]],UNIT["metre",1]]]'
    ),
    # A sphere in WKT 1: an inverse flattening of 0.
    (
        'GEOGCS["sphere",DATUM["sphere",SPHEROID["sphere",6371000,0]],PRIMEM["Greenwich",0],'
        'UNIT["degree",0.0174532925199433]]'
    ),
]

# GDAL's WKT 1 of registry CRSs with a TOWGS84 shift (as GDAL 2 wrote them), which terrio
# identifies by their code: PROJ writes them as bound CRSs, with their ellipsoid and +towgs84.
SHIFTED = [
    (31467, "598.1,73.7,418.2,0.202,0.045,-2.455,6.7"),
    (23032, "-87,-98,-121,0,0,0,0"),
    (4326, "0,0,0,0,0,0,0"),
    (32632, "0,0,0,0,0,0,0"),
    (2056, "674.374,15.056,405.346,0,0,0,0"),
    (4314, "598.1,73.7,418.2"),
    (27700, "446.448,-125.157,542.06,0.15,0.247,0.842,-20.489"),
    # The prime meridian before the shift; a registry compound CRS's horizontal shift.
    (27572, "-168,-60,320,0,0,0,0"),
    (7405, "446.448,-125.157,542.06,0.15,0.247,0.842,-20.489"),
]

# CRSs given by reference: compound CRSs of a horizontal and a vertical code, and every 20th
# IAU_2015 CRS.
REFERENCES = ["EPSG:4326+5773", "EPSG:2056+5728", "EPSG:32632+5773", "EPSG:4269+5703"]


# GDAL's WKT 1 of registry CRSs without their codes and names, which terrio reads rather than
# identifies: Polar Stereographic (variant C), which PROJ cannot write.
UNIDENTIFIED = [2985]


def unidentified_wkt(code):
    """GDAL's WKT 1 of an EPSG code without its AUTHORITY nodes and with its names replaced."""
    wkt = re.sub(r',AUTHORITY\["EPSG","\d+"\]', "", CRS.from_epsg(code).to_wkt("WKT1_GDAL"))
    return re.sub(r'(PROJCS|GEOGCS|DATUM)\["[^"]*"', r'\1["custom"', wkt)


def shifted_wkt(code, towgs84):
    """GDAL's WKT 1 of an EPSG code with a TOWGS84 node in its datum."""
    wkt = CRS.from_epsg(code).to_wkt("WKT1_GDAL")
    wkt, n = re.subn(r"(SPHEROID\[[^\]]*\]\])", rf"\1,TOWGS84[{towgs84}]", wkt, count=1)
    assert n == 1, f"no SPHEROID node in EPSG:{code}'s WKT"
    return wkt


def proj4(crs):
    """The PROJ string PROJ writes for a CRS, ``NONE`` when it has none."""
    try:
        return crs.to_proj4() or "NONE"
    except pyproj.exceptions.CRSError:
        return "NONE"


def write_all(path):
    """Every EPSG, ESRI and IAU_2015 code's PROJ string, in the frozen data's format."""
    lines = [f"# Every code's PROJ string as PROJ {pyproj.proj_version_str} writes it (pyproj {pyproj.__version__})."]
    for auth in ("EPSG", "ESRI"):
        for info in query_crs_info(auth_name=auth):
            lines.append(f"{auth}\t{info.code}\t{proj4(CRS.from_authority(auth, info.code))}")
    for info in query_crs_info(auth_name="IAU_2015"):
        lines.append(f"ref\tIAU_2015:{info.code}\t{proj4(CRS(f'IAU_2015:{info.code}'))}")
    Path(path).write_text("\n".join(lines) + "\n")
    print(f"{len(lines) - 1} rows written to {path}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--all", metavar="PATH", help="write every code to PATH instead")
    args = parser.parse_args()
    warnings.simplefilter("ignore")
    pyproj.network.set_network_enabled(False)
    assert pyproj.proj_version_str == "9.8.1", pyproj.proj_version_str
    if args.all:
        write_all(args.all)
        return
    rows = []
    for auth in ("EPSG", "ESRI"):
        for i, info in enumerate(query_crs_info(auth_name=auth)):
            if i % 20 == 0:
                rows.append((auth, int(info.code)))
    rows = sorted(set(rows) | set(RULES))
    lines = [
        (
            f"# PROJ strings of CRSs as PROJ {pyproj.proj_version_str} writes them (pyproj {pyproj.__version__}"
            " CRS.to_proj4(), network off),"
        ),
        "# from scripts/gen_proj_string_export.py. Columns: kind (EPSG, ESRI, proj, wkt, ref), code or",
        "# definition, PROJ string (NONE: PROJ has none).",
    ]
    for auth, code in rows:
        lines.append(f"{auth}\t{code}\t{proj4(CRS.from_authority(auth, code))}")
    for p in PROJ_STRINGS:
        lines.append(f"proj\t{p}\t{proj4(CRS(p))}")
    for w in WKTS + [shifted_wkt(c, t) for c, t in SHIFTED] + [unidentified_wkt(c) for c in UNIDENTIFIED]:
        lines.append(f"wkt\t{w}\t{proj4(CRS(w))}")
    iau = [f"IAU_2015:{i.code}" for i in query_crs_info(auth_name="IAU_2015")][::20]
    for r in REFERENCES + iau:
        lines.append(f"ref\t{r}\t{proj4(CRS(r))}")
    OUT.write_text("\n".join(lines) + "\n")
    print(f"{len(lines) - 3} rows written to {OUT}")


if __name__ == "__main__":
    main()
