"""Generate crates/terrio-crs/tests/data/wkt1_axes.json: where PROJ 9.9.0 places a point in
projected CRSs given by WKT 1 whose `AXIS` nodes point in each direction and order.

Each case is a WKT 1 (Transverse Mercator, south-orientated Transverse Mercator, Lambert
Conformal Conic, Krovak, Mercator and Polar Stereographic, without `AXIS` and with each order of an east-west and a north-south
axis), its WKT 2 as PROJ writes it, a geographic point, the point's coordinates in both by
`Transformer.from_crs(geographic, wkt, always_xy=True)` (as GDAL 3.10's `gdaltransform`, in
traditional GIS order, gives them too), and the `+axis` of both WKTs' `to_proj4()`.

`other_axes`: the WKT 2 of Transverse Mercator with a CS whose axes are not one along east-west
and one along north-south (`south, up`, `northNorthEast`, one direction twice), which PROJ reads
east then north, and the point's coordinates in it.

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


OTHER_AXES = [
    ("south", "up"),
    ("up", "south"),
    ("west", "up"),
    ("south", "down"),
    ("west", "geocentricX"),
    ("northNorthEast", "eastSouthEast"),
    ("south", "south"),
    ("west", "west"),
]


def other_axes():
    """PROJCRS of Transverse Mercator whose CS has an axis along neither east-west nor
    north-south, or one direction twice, and where PROJ places a point in it."""
    base = CRS(METHODS["Transverse_Mercator"][1] + " +type=crs")
    tm = CRS(
        f'PROJCS["t",{WGS84},PROJECTION["Transverse_Mercator"],'
        f'{METHODS["Transverse_Mercator"][3]},{ORIGIN},UNIT["metre",1]]'
    ).to_wkt("WKT2_2019")
    cs = tm[tm.index("CS[Cartesian,2]") :]
    cases = []
    for a, b in OTHER_AXES:
        wkt = tm.replace(
            cs,
            f'CS[Cartesian,2],AXIS["a",{a},LENGTHUNIT["metre",1]],'
            f'AXIS["b",{b},LENGTHUNIT["metre",1]]]',
        )
        crs = CRS(wkt)
        point = METHODS["Transverse_Mercator"][2]
        x, y = Transformer.from_crs(base, crs, always_xy=True).transform(
            *point, errcheck=True
        )
        axis = next((t for t in crs.to_proj4().split() if t.startswith("+axis=")), None)
        cases.append({"wkt": wkt, "point": point, "proj": [x, y], "proj4_axis": axis})
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
                "other_axes": other_axes(),
                "geographic_exports": exports,
            },
            indent=1,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
