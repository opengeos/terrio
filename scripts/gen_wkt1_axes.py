"""Generate crates/terrio-crs/tests/data/wkt1_axes.json: where PROJ 9.9.0 places a point in
projected CRSs given by WKT 1 whose `AXIS` nodes point in each direction and order.

Each case is a WKT 1 (Transverse Mercator, south-orientated Transverse Mercator, Lambert
Conformal Conic, Krovak, Mercator and Polar Stereographic, without `AXIS` and with each order of an east-west and a north-south
axis), its WKT 2 as PROJ writes it, a geographic point, the point's coordinates in both by
`Transformer.from_crs(geographic, wkt, always_xy=True)` (as GDAL 3.10's `gdaltransform`, in
traditional GIS order, gives them too), and the `+axis` of both WKTs' `to_proj4()`.

`krovak_dialects`: Krovak in WKT 1 with ESRI's parameters and names (`GCS_S_JTSK`, `D_S_JTSK`,
which make PROJ read it as ESRI's dialect, as does either alone, `gcs_` in lower case too) and with
GDAL's, without ESRI's scale and rotation of
the plane, with 1 and 0°, and with -1 and 90° (east-north), each without `AXIS` and with each
order of axes and with east and up, and the point's coordinates in it.

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt1_axes.py (needs pyproj 3.8.0)
"""

import json
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/wkt1_axes.json"
)
WGS84 = (
    'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]'
)
SJTSK = (
    'GEOGCS["S-JTSK",DATUM["System_Jednotne_Trigonometricke_Site_Katastralni",'
    'SPHEROID["Bessel 1841",6377397.155,299.1528128]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433]]'
)
ORIGIN = 'PARAMETER["false_easting",0],PARAMETER["false_northing",0]'
METHODS = {
    "Transverse_Mercator": (
        WGS84,
        "+proj=longlat +datum=WGS84",
        (18.5, -33.9),
        'PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",19],PARAMETER["scale_factor",1]',
    ),
    "Transverse_Mercator_South_Orientated": (
        WGS84,
        "+proj=longlat +datum=WGS84",
        (18.5, -33.9),
        'PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",19],PARAMETER["scale_factor",1]',
    ),
    "Lambert_Conformal_Conic_2SP": (
        WGS84,
        "+proj=longlat +datum=WGS84",
        (18.5, -33.9),
        (
            'PARAMETER["standard_parallel_1",-30],PARAMETER["standard_parallel_2",-36],'
            'PARAMETER["latitude_of_origin",-33],PARAMETER["central_meridian",19]'
        ),
    ),
    "Krovak": (
        SJTSK,
        "+proj=longlat +ellps=bessel",
        (14.4, 50.1),
        (
            'PARAMETER["latitude_of_center",49.5],PARAMETER["longitude_of_center",24.83333333333333],'
            'PARAMETER["azimuth",30.28813972222222],PARAMETER["pseudo_standard_parallel_1",78.5],'
            'PARAMETER["scale_factor",0.9999]'
        ),
    ),
    "Mercator_1SP": (
        WGS84,
        "+proj=longlat +datum=WGS84",
        (18.5, -33.9),
        'PARAMETER["central_meridian",19],PARAMETER["scale_factor",1]',
    ),
    "Polar_Stereographic": (
        WGS84,
        "+proj=longlat +datum=WGS84",
        (18.5, 80),
        'PARAMETER["latitude_of_origin",90],PARAMETER["central_meridian",19],PARAMETER["scale_factor",1]',
    ),
}
AXES = {
    "none": [],
    "east north": [("Easting", "EAST"), ("Northing", "NORTH")],
    "north east": [("Northing", "NORTH"), ("Easting", "EAST")],
    "west south": [("Y", "WEST"), ("X", "SOUTH")],
    "south west": [("X", "SOUTH"), ("Y", "WEST")],
    "west north": [("W", "WEST"), ("N", "NORTH")],
    "east south": [("E", "EAST"), ("S", "SOUTH")],
    "north west": [("N", "NORTH"), ("W", "WEST")],
    "south east": [("S", "SOUTH"), ("E", "EAST")],
}


ESRI_SJTSK = (
    'GEOGCS["GCS_S_JTSK",DATUM["D_S_JTSK",SPHEROID["Bessel_1841",6377397.155,299.1528128]],'
    'PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]'
)
ESRI_KROVAK = (
    'PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],'
    'PARAMETER["Pseudo_Standard_Parallel_1",78.5],PARAMETER["Scale_Factor",0.9999],'
    'PARAMETER["Azimuth",30.28813972222222],PARAMETER["Longitude_Of_Center",24.83333333333333],'
    'PARAMETER["Latitude_Of_Center",49.5]'
)
PLANE = {
    "no plane": "",
    "plane 1 0": ',PARAMETER["X_Scale",1.0],PARAMETER["Y_Scale",1.0],'
    'PARAMETER["XY_Plane_Rotation",0.0]',
    "plane -1 90": ',PARAMETER["X_Scale",-1.0],PARAMETER["Y_Scale",1.0],'
    'PARAMETER["XY_Plane_Rotation",90.0]',
}


def krovak_dialects():
    """Krovak in ESRI's and GDAL's WKT 1, with each scale and rotation of the plane and each
    order of axes, and where PROJ places a point in it."""
    source = CRS(METHODS["Krovak"][1] + " +type=crs")
    point = METHODS["Krovak"][2]
    dialects = {
        "ESRI": (ESRI_SJTSK, ESRI_KROVAK, "Meter"),
        "GDAL": (SJTSK, METHODS["Krovak"][3] + "," + ORIGIN, "metre"),
        # ESRI's dialect by the datum's name alone, and by a lower-case `gcs_`.
        "ESRI datum": (
            ESRI_SJTSK.replace("GCS_S_JTSK", "S_JTSK"),
            ESRI_KROVAK,
            "Meter",
        ),
        "ESRI lower-case": (
            ESRI_SJTSK.replace("GCS_", "gcs_").replace("D_S_JTSK", "S_JTSK"),
            ESRI_KROVAK,
            "Meter",
        ),
    }
    axes = dict(AXES, **{"east up": [("E", "EAST"), ("U", "UP")]})
    cases = []
    for dialect, (geogcs, params, unit) in dialects.items():
        for plane, extra in PLANE.items():
            for name, nodes in axes.items():
                nodes = "".join(f',AXIS["{n}",{d}]' for n, d in nodes)
                wkt = (
                    f'PROJCS["Krovak {dialect} {plane} {name}",{geogcs},'
                    f'PROJECTION["Krovak"],{params}{extra},UNIT["{unit}",1.0]{nodes}]'
                )
                x, y = Transformer.from_crs(source, CRS(wkt), always_xy=True).transform(
                    *point, errcheck=True
                )
                cases.append({"wkt": wkt, "point": point, "proj": [x, y]})
    return cases


def main():
    """Write coordinate and export-axis references from the pinned PROJ version."""
    assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str
    cases = []
    for method, (geogcs, geographic, point, params) in METHODS.items():
        for name, axes in AXES.items():
            nodes = "".join(f',AXIS["{n}",{d}]' for n, d in axes)
            wkt = f'PROJCS["{method} {name}",{geogcs},PROJECTION["{method}"],{params},{ORIGIN},UNIT["metre",1]{nodes}]'
            crs = CRS(wkt)
            wkt2 = crs.to_wkt("WKT2_2019")
            source = CRS(geographic + " +type=crs")
            x, y = Transformer.from_crs(source, crs, always_xy=True).transform(
                *point, errcheck=True
            )
            x2, y2 = Transformer.from_crs(source, CRS(wkt2), always_xy=True).transform(
                *point, errcheck=True
            )
            axis = next(
                (t for t in crs.to_proj4().split() if t.startswith("+axis=")), None
            )
            axis2 = next(
                (t for t in CRS(wkt2).to_proj4().split() if t.startswith("+axis=")),
                None,
            )
            cases.append(
                {
                    "wkt": wkt,
                    "wkt2": wkt2,
                    "geographic": geographic,
                    "point": point,
                    "proj": [x, y],
                    "proj_wkt2": [x2, y2],
                    "proj4_axis": axis,
                    "proj4_axis_wkt2": axis2,
                }
            )
    exports = []
    for axes in AXES.values():
        axis = (
            "".join(direction[0].lower() for _, direction in axes) + "u"
            if axes
            else "enu"
        )
        geographic = CRS(f"+proj=longlat +ellps=GRS80 +axis={axis} +type=crs").to_wkt()
        bound = (
            f"BOUNDCRS[SOURCECRS[{geographic}],TARGETCRS[{CRS(4326).to_wkt()}],"
            'ABRIDGEDTRANSFORMATION["shift",METHOD["Geocentric translations (geog2D domain)",ID["EPSG",9603]],'
            'PARAMETER["X-axis translation",1,ID["EPSG",8605]],'
            'PARAMETER["Y-axis translation",2,ID["EPSG",8606]],'
            'PARAMETER["Z-axis translation",3,ID["EPSG",8607]]]]'
        )
        for wkt in (geographic, bound):
            exports.append({"wkt": wkt, "proj4": CRS(wkt).to_proj4()})
    OUT.write_text(
        json.dumps(
            {
                "proj_version": pyproj.proj_version_str,
                "cases": cases,
                "krovak_dialects": krovak_dialects(),
                "geographic_exports": exports,
            },
            indent=1,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
