"""Generate crates/terrio-crs/tests/data/wkt1_epsg_names.json: WKT 1 projected CRSs whose
parameters have EPSG's names (`Latitude of false origin`, `Longitude of projection centre`,
`Angle from Rectified to Skew Grid`, `Latitude of standard parallel`), which PROJ 9.9.0 reads as
GDAL's, GDAL's `Polar_Stereographic`, which PROJ reads only with `latitude_of_origin`, and a
`PROJCS` with two `UNIT` nodes, of which PROJ takes the first (opengeos/terrio#1012, items 25
and 31). Each case has PROJ's PROJ string and where it places a point from WGS 84 (`always_xy`),
or `null` where PROJ refuses the CRS.

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt1_epsg_names.py (needs pyproj 3.8.0)
"""

import json
import os
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/wkt1_epsg_names.json"
)
WGS84 = (
    'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433]]'
)
FALSE_ORIGIN = [
    ("Latitude of false origin", 40),
    ("Longitude of false origin", -96),
    ("Latitude of 1st standard parallel", 29.5),
    ("Latitude of 2nd standard parallel", 45.5),
    ("Easting at false origin", 1000),
    ("Northing at false origin", 2000),
]
CENTRE = [
    ("Latitude of projection centre", 40),
    ("Longitude of projection centre", 10),
    ("Azimuth of initial line", 30),
    ("Angle from Rectified to Skew Grid", 20),
    ("Scale factor on initial line", 1),
]
STANDARD_PARALLEL = [
    ("Latitude of standard parallel", -71),
    ("Longitude of origin", 0),
    ("False easting", 0),
    ("False northing", 0),
]
TM = [
    ("latitude_of_origin", 0),
    ("central_meridian", 9),
    ("scale_factor", 0.9996),
    ("false_easting", 500000),
    ("false_northing", 0),
]


def projcs(method, parameters, units='UNIT["metre",1]'):
    params = ",".join(f'PARAMETER["{k}",{v}]' for k, v in parameters)
    return f'PROJCS["x",{WGS84},PROJECTION["{method}"],{params},{units}]'


CASES = [
    (
        "Albers_Conic_Equal_Area",
        projcs("Albers_Conic_Equal_Area", FALSE_ORIGIN),
        (-90, 35),
    ),
    ("Albers Equal Area", projcs("Albers Equal Area", FALSE_ORIGIN), (-90, 35)),
    ("Equidistant_Conic", projcs("Equidistant_Conic", FALSE_ORIGIN), (-90, 35)),
    (
        "Lambert_Conformal_Conic_2SP",
        projcs("Lambert_Conformal_Conic_2SP", FALSE_ORIGIN),
        (-90, 35),
    ),
    (
        "Hotine_Oblique_Mercator_Azimuth_Center",
        projcs(
            "Hotine_Oblique_Mercator_Azimuth_Center",
            CENTRE
            + [
                ("Easting at projection centre", 1000),
                ("Northing at projection centre", 2000),
            ],
        ),
        (11, 41),
    ),
    (
        "Hotine_Oblique_Mercator",
        projcs(
            "Hotine_Oblique_Mercator",
            CENTRE + [("False easting", 0), ("False northing", 0)],
        ),
        (11, 41),
    ),
    (
        "Polar Stereographic (variant B)",
        projcs("Polar Stereographic (variant B)", STANDARD_PARALLEL),
        (10, -80),
    ),
    (
        "Polar_Stereographic with EPSG's standard parallel",
        projcs("Polar_Stereographic", STANDARD_PARALLEL),
        (10, -80),
    ),
    (
        "Polar_Stereographic with a standard parallel 1",
        projcs(
            "Polar_Stereographic",
            [("standard_parallel_1", -71)] + STANDARD_PARALLEL[1:],
        ),
        (10, -80),
    ),
    (
        "Polar_Stereographic with a latitude of natural origin",
        projcs(
            "Polar_Stereographic",
            [("latitude_of_natural_origin", -71)] + STANDARD_PARALLEL[1:],
        ),
        (10, -80),
    ),
    (
        "Polar_Stereographic with a latitude of origin",
        projcs(
            "Polar_Stereographic",
            [("latitude_of_origin", -71)] + STANDARD_PARALLEL[1:],
        ),
        (10, -80),
    ),
    (
        "Transverse_Mercator with two UNIT nodes",
        projcs(
            "Transverse_Mercator",
            TM,
            'UNIT["metre",1],UNIT["US survey foot",0.304800609601219]',
        ),
        (10, 45),
    ),
]


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.9.0", (
        pyproj.proj_version_str
    )
    assert os.environ.get("PROJ_NETWORK") == "OFF"
    cases = []
    for name, wkt, point in CASES:
        try:
            crs = CRS.from_wkt(wkt)
            proj4 = crs.to_proj4()
            x, y = Transformer.from_crs("EPSG:4326", crs, always_xy=True).transform(
                *point, errcheck=True
            )
            proj = [x, y]
        except pyproj.exceptions.ProjError:  # CRSError too, and refusals at transform time
            proj4, proj = None, None
        cases.append(
            {
                "name": name,
                "wkt": wkt,
                "point": list(point),
                "proj4": proj4,
                "proj": proj,
            }
        )
    OUT.write_text(
        json.dumps({"proj": pyproj.proj_version_str, "cases": cases}, indent=1) + "\n"
    )


if __name__ == "__main__":
    main()
