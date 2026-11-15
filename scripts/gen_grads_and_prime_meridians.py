"""Generate crates/terrio-crs/tests/data/grads_and_prime_meridians.json: where PROJ 9.8.1 places
points of CRSs whose angular unit is the grad, and of CRSs whose prime meridian PROJ reads otherwise
than as written.

WKT 1: NTF (Paris) (EPSG:4807) and its projected CRSs (EPSG:27561-27564, 27571-27574,
27581-27584, 27591-27594), ATF (Paris) (EPSG:27500), and Krovak (EPSG:5514) rewritten in grads.
Each WKT is PROJ's (`CRS.from_epsg(code).to_wkt("WKT1_GDAL")` and `"WKT1_ESRI"`), or that WKT with
its CRS and datum names replaced and its `AUTHORITY` nodes removed, so that it is read rather than
identified, or written by hand (a geographic CRS in grads on Greenwich, prime meridians PROJ knows
by name, Krovak with its angles in grads).

WKT 2: PROJ's `CRS.from_epsg(code).to_wkt("WKT2_2019")` of EPSG:4807, 27572 and 27500 without its
names and IDs, and geographic CRSs written by hand with prime meridians in their own unit, in the
CRS's, and in none.

Each of these cases is a point, its coordinates in the WKT's CRS by
`Transformer.from_crs(source, wkt, always_xy=True)`, and those coordinates taken back to the
source by `Transformer.from_crs(wkt, source, always_xy=True)`. The source is EPSG:4275 (NTF on
Greenwich) or EPSG:4156 (S-JTSK) for a WKT that declares its codes, and a PROJ string on the same
ellipsoid for one without (PROJ's ballpark: no datum shift).

Prime meridians near Paris's (`pm_cases`): EPSG:4807 and PROJ strings with `+pm=` on both sides of
the 1e-10 radians within which PROJ transforms with its `+pm=paris`, each with the longitude of
(0, 0) in EPSG:4275 by `Transformer.from_crs(crs, "EPSG:4275", always_xy=True)`.

Usage: PROJ_NETWORK=OFF python scripts/gen_grads_and_prime_meridians.py (needs pyproj 3.8.0)
"""

import json
import os
import re
from pathlib import Path

import pyproj
import pyproj.datadir
import pyproj.network
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/grads_and_prime_meridians.json"
)
CLARKE = "+proj=longlat +ellps=clrk80ign +type=crs"
PLESSIS = "+proj=longlat +a=6376523 +rf=308.64 +type=crs"
BESSEL = "+proj=longlat +ellps=bessel +type=crs"
# A point in France, and one in Corsica for the Corse zones.
FRANCE = (2.35, 48.85)
CORSICA = (9.0, 42.0)
CZECHIA = (14.4, 50.1)
GRAD = 'UNIT["grad",0.01570796326794897'


def strip(wkt):
    """`wkt` without its `AUTHORITY` nodes, and its CRS and datum names replaced."""
    wkt = re.sub(r',AUTHORITY\["EPSG","\d+"\]', "", wkt)
    wkt = re.sub(r'(PROJCS|GEOGCS|DATUM)\["[^"]*"', r'\1["custom"', wkt)
    return wkt


def hand(unit, size, primem='PRIMEM["Greenwich",0]'):
    """A geographic CRS on the Clarke 1880 (IGN) ellipsoid in `unit`, of `size` radians."""
    return (
        'GEOGCS["custom",DATUM["custom",SPHEROID["Clarke 1880 (IGN)",6378249.2,293.466021293627]],'
        f'{primem},UNIT["{unit}",{size}]]'
    )


DEG2 = 'ANGLEUNIT["degree",0.0174532925199433]'
GRAD2 = 'ANGLEUNIT["grad",0.0157079632679489]'


def geogcrs(primem, axis_unit="", cs_unit=""):
    """A WKT 2 geographic CRS on the Clarke 1880 (IGN) ellipsoid, its axes in `axis_unit` or its
    CS in `cs_unit`."""
    axis = f",{axis_unit}" if axis_unit else ""
    cs = f",{cs_unit}" if cs_unit else ""
    return (
        'GEOGCRS["custom",DATUM["custom",ELLIPSOID["Clarke 1880 (IGN)",6378249.2,'
        f'293.466021293627,LENGTHUNIT["metre",1]]],{primem},CS[ellipsoidal,2],'
        f'AXIS["lon",east,ORDER[1]{axis}],AXIS["lat",north,ORDER[2]{axis}]{cs}]'
    )


def strip2(wkt):
    """A WKT 2 without its IDs, usages and remarks, and its CRS and datum names replaced."""
    wkt = re.sub(r',ID\["EPSG",\d+\]', "", wkt)
    wkt = re.sub(r',USAGE\[SCOPE\["[^"]*"\],AREA\["[^"]*"\],BBOX\[[^\]]*\]\]', "", wkt)
    wkt = re.sub(r'(PROJCRS|BASEGEOGCRS|GEOGCRS|DATUM)\["[^"]*"', r'\1["custom"', wkt)
    assert ",ID[" not in wkt and "USAGE[" not in wkt, wkt
    return wkt


def krovak_in_grads(wkt):
    """Krovak's WKT 1 with its geographic CRS and angles in grads."""
    out = re.sub(r'UNIT\["[Dd]egree",0.0174532925199433', GRAD, wkt, count=1)
    assert out != wkt
    names = (
        "latitude_of_center|longitude_of_center|azimuth|pseudo_standard_parallel_1|"
        "Latitude_Of_Center|Longitude_Of_Center|Azimuth|Pseudo_Standard_Parallel_1|"
        "XY_Plane_Rotation"
    )

    def grads(m):
        return f'PARAMETER["{m[1]}",{float(m[2]) / 0.9!r}]'

    return re.sub(rf'PARAMETER\["({names})",([-0-9.]+)\]', grads, out)


def cases():
    out = []
    geog = CRS.from_epsg(4807)
    for flavour in ["WKT1_GDAL", "WKT1_ESRI"]:
        wkt = geog.to_wkt(flavour)
        out.append((f"EPSG:4807 {flavour}", wkt, "EPSG:4275", FRANCE))
        out.append((f"EPSG:4807 {flavour}, read", strip(wkt), CLARKE, FRANCE))
    gdal = strip(geog.to_wkt("WKT1_GDAL"))
    paris = 'PRIMEM["Paris",2.33722917]'
    assert paris in gdal
    # Paris in grads, as WKT 1 defines the prime meridian (in the CRS's angular unit).
    out.append(
        (
            "Paris in grads",
            gdal.replace(paris, 'PRIMEM["Paris",2.5969213]'),
            CLARKE,
            FRANCE,
        )
    )
    # Another meridian at the same value is in grads.
    out.append(
        (
            "another meridian at 2.33722917",
            gdal.replace(paris, 'PRIMEM["custom",2.33722917]'),
            CLARKE,
            FRANCE,
        )
    )
    out.append(
        (
            "Greenwich",
            gdal.replace(paris, 'PRIMEM["Greenwich",0]'),
            CLARKE,
            FRANCE,
        )
    )
    # A geographic CRS in grads written by hand: ESRI's unit name and the exact size of a grad,
    # and the unit's other name.
    out.append(
        ("by hand, ESRI's Grad", hand("Grad", 0.01570796326794897), CLARKE, FRANCE)
    )
    out.append(("by hand, gon", hand("gon", 0.015707963267949), CLARKE, FRANCE))
    # PROJ's meridians by name and value in degrees, or in EPSG's sexagesimal degrees, whatever
    # the unit: Ferro in degrees beside grads, Ferro at -17.40 (17°40'W) beside degrees, and
    # Ferro at another value, in the CRS's unit.
    for name, unit, pm in [
        ("Ferro in degrees, CRS in grads", "grad", 'PRIMEM["Ferro",-17.6666666666667]'),
        ("Ferro sexagesimal, CRS in degrees", "degree", 'PRIMEM["Ferro",-17.4]'),
        ("Ferro in grads", "grad", 'PRIMEM["Ferro",-19.62962962962963]'),
        (
            "Lisbon in degrees, CRS in grads",
            "grad",
            'PRIMEM["Lisbon",-9.13190611111111]',
        ),
    ]:
        size = 0.0174532925199433 if unit == "degree" else 0.0157079632679489
        out.append((name, hand(unit, size, pm), CLARKE, FRANCE))
    projected = [27561, 27562, 27563, 27564, 27571, 27572, 27573, 27574]
    projected += [27581, 27582, 27583, 27584, 27591, 27592, 27593, 27594]
    for code in projected:
        crs = CRS.from_epsg(code)
        point = CORSICA if crs.name.endswith(("Corse", "IV")) else FRANCE
        for flavour in ["WKT1_GDAL", "WKT1_ESRI"]:
            wkt = crs.to_wkt(flavour)
            out.append((f"EPSG:{code} {flavour}", wkt, "EPSG:4275", point))
            out.append((f"EPSG:{code} {flavour}, read", strip(wkt), CLARKE, point))
    lambert2 = strip(CRS.from_epsg(27572).to_wkt("WKT1_GDAL"))
    out.append(
        (
            "EPSG:27572, Paris in grads",
            lambert2.replace(paris, 'PRIMEM["Paris",2.5969213]'),
            CLARKE,
            FRANCE,
        )
    )
    out.append(
        (
            "EPSG:27572, central meridian 10 grads",
            lambert2.replace(
                'PARAMETER["central_meridian",0]', 'PARAMETER["central_meridian",10]'
            ),
            CLARKE,
            (11.35, 48.85),
        )
    )
    # ATF (Paris) / Nord de Guerre: "Paris RGS", 2.33720833333333 in degrees beside grads.
    atf = CRS.from_epsg(27500)
    for flavour in ["WKT1_GDAL", "WKT1_ESRI"]:
        wkt = atf.to_wkt(flavour)
        out.append((f"EPSG:27500 {flavour}, read", strip(wkt), PLESSIS, (7.0, 49.0)))
    # Krovak East North with its angles in grads (its plane rotation of 90 degrees is 100 grads).
    krovak = CRS.from_epsg(5514)
    for flavour in ["WKT1_GDAL", "WKT1_ESRI"]:
        wkt = krovak_in_grads(krovak.to_wkt(flavour))
        out.append((f"EPSG:5514 {flavour} in grads", wkt, "EPSG:4156", CZECHIA))
        out.append((f"EPSG:5514 {flavour} in grads, read", strip(wkt), BESSEL, CZECHIA))
    for code in [4807, 27572, 27500]:
        crs = CRS.from_epsg(code)
        src = PLESSIS if code == 27500 else CLARKE
        point = (7.0, 49.0) if code == 27500 else FRANCE
        out.append(
            (f"EPSG:{code} WKT 2, read", strip2(crs.to_wkt("WKT2_2019")), src, point)
        )
    for name, wkt in [
        # Its own unit; EPSG's sexagesimal degrees in degrees.
        ("WKT 2, Ferro sexagesimal", geogcrs(f'PRIMEM["Ferro",-17.4,{DEG2}]', DEG2)),
        (
            "WKT 2, Lisbon sexagesimal",
            geogcrs(f'PRIMEM["Lisbon",-9.0754862,{DEG2}]', DEG2),
        ),
        ("WKT 2, ferro sexagesimal", geogcrs(f'PRIMEM["ferro",-17.4,{DEG2}]', DEG2)),
        (
            "WKT 2, Ferro in degrees as grads",
            geogcrs(f'PRIMEM["Ferro",-17.6666666666667,{GRAD2}]', GRAD2),
        ),
        (
            "WKT 2, Paris RGS in degrees as grads",
            geogcrs(f'PRIMEM["Paris RGS",2.33720833333333,{GRAD2}]', GRAD2),
        ),
        (
            "WKT 2, Paris in degrees as grads",
            geogcrs(f'PRIMEM["Paris",2.33722917,{GRAD2}]', GRAD2),
        ),
        (
            "WKT 2, Paris in degrees",
            geogcrs(f'PRIMEM["Paris",2.33722917,{DEG2}]', GRAD2),
        ),
        ("WKT 2, Paris in grads", geogcrs(f'PRIMEM["Paris",2.5969213,{GRAD2}]', GRAD2)),
        # No unit of its own: the CRS's, not its axes', else degrees.
        ("WKT 2, no unit, axes in grads", geogcrs('PRIMEM["custom",2.5]', GRAD2)),
        ("WKT 2, no unit, CS in grads", geogcrs('PRIMEM["custom",2.5]', cs_unit=GRAD2)),
        (
            "WKT 2, Paris without unit, CS in grads",
            geogcrs('PRIMEM["Paris",2.33722917]', cs_unit=GRAD2),
        ),
    ]:
        out.append((name, wkt, CLARKE, FRANCE))
    return out


# Paris is `+pm=paris`, 2.337229166666667, within 1e-10 radians (5.73e-9 degrees): 2.3372291721 is
# 0.95e-10 above it, 2.3372291612 0.95e-10 below, 2.3372291730 and 2.3372291603 1.1e-10 off.
PM_CASES = ["EPSG:4807"] + [
    f"+proj=longlat +ellps=clrk80ign +pm={pm} +type=crs"
    for pm in [
        "2.33722917",
        "2.3372291721",
        "2.3372291612",
        "2.3372291730",
        "2.3372291603",
    ]
]


def main():
    assert pyproj.proj_version_str == "9.8.1", pyproj.proj_version_str
    pyproj.network.set_network_enabled(False)
    # No grids, so the values don't depend on what is installed.
    for d in pyproj.datadir.get_data_dir().split(os.pathsep) + [
        pyproj.datadir.get_user_data_dir()
    ]:
        grids = [
            f
            for f in (os.listdir(d) if os.path.isdir(d) else [])
            if f.endswith((".tif", ".gsb", ".gtx"))
        ]
        assert not grids, (d, grids)
    rows = []
    for name, wkt, src, point in cases():
        to = Transformer.from_crs(src, CRS(wkt), always_xy=True)
        xy = to.transform(*point, errcheck=True)
        back = Transformer.from_crs(CRS(wkt), src, always_xy=True)
        rows.append(
            {
                "name": name,
                "wkt": wkt,
                "src": src,
                "point": point,
                "proj": list(xy),
                "proj_back": list(back.transform(*xy, errcheck=True)),
            }
        )
    pm_rows = []
    for crs in PM_CASES:
        t = Transformer.from_crs(crs, "EPSG:4275", always_xy=True)
        pm_rows.append({"crs": crs, "lon": t.transform(0.0, 0.0, errcheck=True)[0]})
    data = {"proj_version": pyproj.proj_version_str, "cases": rows, "pm_cases": pm_rows}
    OUT.write_text(json.dumps(data, indent=1) + "\n")


if __name__ == "__main__":
    main()
