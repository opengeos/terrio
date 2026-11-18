"""Generate crates/terrio-crs/tests/data/grads_and_prime_meridians.json: where PROJ 9.9.0 places
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
source by `Transformer.from_crs(wkt, source, always_xy=True)`. Coordinates of a CRS whose axes
are in radians, which pyproj gives in degrees, are written in radians. The source is EPSG:4275 (NTF
on Greenwich) or EPSG:4156 (S-JTSK) for a WKT that declares its codes, and a PROJ string on the same
ellipsoid for one without (PROJ's ballpark: no datum shift).

Prime meridians near Paris's (`pm_cases`): EPSG:4807 and PROJ strings with `+pm=` on both sides of
the 1e-10 radians within which PROJ transforms with its `+pm=paris`, each with the longitude of
(0, 0) in EPSG:4275 by `Transformer.from_crs(crs, "EPSG:4275", always_xy=True)`; also near
Copenhagen and Greenwich.

Exports (`exports`): the prime meridian PROJ writes for Paris in `to_proj4()`,
`to_wkt("WKT1_GDAL")` and `to_json()`.

Identified (`identified`): ESRI's WKT 1 of EPSG:4901 and 27500 as PROJ writes it, unstripped, and a
point in WGS 84 by `Transformer.from_crs(CRS.from_epsg(code), "EPSG:4326", always_xy=True)`.

Usage: PROJ_NETWORK=OFF python scripts/gen_grads_and_prime_meridians.py (needs pyproj 3.8.0)
"""

import json
import math
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


DATUM2 = (
    'DATUM["custom",ELLIPSOID["Clarke 1880 (IGN)",6378249.2,293.466021293627,'
    'LENGTHUNIT["metre",1]]]'
)


def geogcrs(primem, axis_unit="", cs_unit="", keyword="GEOGCRS"):
    """A WKT 2 geographic CRS on the Clarke 1880 (IGN) ellipsoid, its axes in `axis_unit` or the
    CRS in `cs_unit` (a unit node after its axes)."""
    axis = f",{axis_unit}" if axis_unit else ""
    cs = f",{cs_unit}" if cs_unit else ""
    return (
        f'{keyword}["custom",{DATUM2},{primem},CS[ellipsoidal,2],'
        f'AXIS["lon",east,ORDER[1]{axis}],AXIS["lat",north,ORDER[2]{axis}]{cs}]'
    )


GEOCENTRIC = (
    f'GEODCRS["custom",{DATUM2},PRIMEM["custom",2],CS[Cartesian,3],'
    'AXIS["X",geocentricX],AXIS["Y",geocentricY],AXIS["Z",geocentricZ],'
    'LENGTHUNIT["metre",1]]'
)


def lambert_ii(base, base_unit=""):
    """Lambert zone II's conversion (in grads) on a WKT 2 base CRS `base` (`BASEGEOGCRS` or
    `BASEGEODCRS`) with a prime meridian without unit, at 2, and `base_unit` after it."""
    unit = f",{base_unit}" if base_unit else ""
    return (
        f'PROJCRS["custom",{base}["custom",{DATUM2},PRIMEM["custom",2]{unit}],'
        'CONVERSION["custom",METHOD["Lambert Conic Conformal (1SP)"],'
        f'PARAMETER["Latitude of natural origin",52,{GRAD2}],'
        f'PARAMETER["Longitude of natural origin",0,{GRAD2}],'
        'PARAMETER["Scale factor at natural origin",0.99987742,SCALEUNIT["unity",1]],'
        'PARAMETER["False easting",600000,LENGTHUNIT["metre",1]],'
        'PARAMETER["False northing",2200000,LENGTHUNIT["metre",1]]],'
        'CS[Cartesian,2],AXIS["e",east],AXIS["n",north],LENGTHUNIT["metre",1]]'
    )


def strip2(wkt):
    """A WKT 2 without its IDs, usages and remarks, and its CRS and datum names replaced."""
    wkt = re.sub(r',ID\["EPSG",\d+\]', "", wkt)
    wkt = re.sub(r',USAGE\[SCOPE\["[^"]*"\],AREA\["[^"]*"\],BBOX\[[^\]]*\]\]', "", wkt)
    wkt = re.sub(r'(PROJCRS|BASEGEOGCRS|GEOGCRS|DATUM)\["[^"]*"', r'\1["custom"', wkt)
    assert ",ID[" not in wkt and "USAGE[" not in wkt, wkt
    return wkt


def in_radians(wkt):
    """Whether the axes of the CRS of `wkt` are in radians."""
    return all(a.unit_name == "radian" for a in CRS(wkt).axis_info)


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

    out, n = re.subn(rf'PARAMETER\["({names})",([-0-9.]+)\]', grads, out)
    # Every angle is rewritten: a value in another form (an exponent) would be skipped.
    assert (
        n
        == len(re.findall(rf'PARAMETER\["({names})",', out))
        == (5 if "XY_" in out else 4)
    )
    return out


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
    # Paris within 1e-8 of 2.33722917 in degrees is PROJ's 2.5969213 grads exactly.
    for near in ["2.337229179", "2.33722916"]:
        out.append(
            (
                f"Paris at {near}",
                gdal.replace(paris, f'PRIMEM["Paris",{near}]'),
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
        # Copenhagen is one of PROJ's `+pm=` names but not in this table: in the CRS's unit.
        (
            "Copenhagen in degrees, CRS in grads",
            "grad",
            'PRIMEM["Copenhagen",12.577875]',
        ),
        (
            "Copenhagen sexagesimal, CRS in degrees",
            "degree",
            'PRIMEM["Copenhagen",12.344035]',
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
        (
            "WKT 2, Paris at 2.337229179 as grads",
            geogcrs(f'PRIMEM["Paris",2.337229179,{GRAD2}]', GRAD2),
        ),
        (
            "WKT 2, Paris at 2.33722916 as grads",
            geogcrs(f'PRIMEM["Paris",2.33722916,{GRAD2}]', GRAD2),
        ),
        # No unit of its own: the CRS's, not its axes', else degrees.
        ("WKT 2, no unit, axes in grads", geogcrs('PRIMEM["custom",2.5]', GRAD2)),
        (
            "WKT 2, no unit, CRS in grads",
            geogcrs('PRIMEM["custom",2.5]', cs_unit=GRAD2),
        ),
        (
            "WKT 2, Paris without unit, CRS in grads",
            geogcrs('PRIMEM["Paris",2.33722917]', cs_unit=GRAD2),
        ),
        # A `UNIT` of the CRS is angular only named degree or grad, as PROJ reads it: else the
        # meridian is in degrees.
        (
            "WKT 2, no unit, CRS UNIT grad",
            geogcrs('PRIMEM["custom",2]', cs_unit='UNIT["grad",0.0157079632679489]'),
        ),
        (
            "WKT 2, no unit, GEODCRS UNIT grad",
            geogcrs(
                'PRIMEM["custom",2]',
                cs_unit='UNIT["grad",0.0157079632679489]',
                keyword="GEODCRS",
            ),
        ),
        (
            "WKT 2, no unit, GEODCRS UNIT radian",
            geogcrs(
                'PRIMEM["custom",2]', cs_unit='UNIT["radian",1]', keyword="GEODCRS"
            ),
        ),
        (
            "WKT 2, no unit, CRS UNIT radian",
            geogcrs('PRIMEM["custom",0.03]', cs_unit='UNIT["radian",1]'),
        ),
        (
            "WKT 2, no unit, CRS UNIT gon",
            geogcrs('PRIMEM["custom",2]', cs_unit='UNIT["gon",0.0157079632679489]'),
        ),
        (
            "WKT 2, no unit, CRS ANGLEUNIT radian",
            geogcrs('PRIMEM["custom",0.03]', cs_unit='ANGLEUNIT["radian",1]'),
        ),
        # Base CRSs: the base CRS's unit, else degrees.
        ("WKT 2, base, no unit", lambert_ii("BASEGEOGCRS")),
        ("WKT 2, base, ANGLEUNIT grad", lambert_ii("BASEGEOGCRS", GRAD2)),
        (
            "WKT 2, BASEGEODCRS, UNIT grad",
            lambert_ii("BASEGEODCRS", 'UNIT["grad",0.0157079632679489]'),
        ),
        (
            "WKT 2, BASEGEODCRS, UNIT radian",
            lambert_ii("BASEGEODCRS", 'UNIT["radian",1]'),
        ),
        # A geocentric CRS: its unit is a length, the meridian in degrees.
        (
            "WKT 2, geocentric",
            GEOCENTRIC,
        ),
    ]:
        out.append((name, wkt, CLARKE, FRANCE))
    return out


# Paris is `+pm=paris`, 2.337229166666667, within 1e-10 radians (5.73e-9 degrees): 2.3372291721 is
# 0.95e-10 above it, 2.3372291612 0.95e-10 below, 2.3372291730 and 2.3372291603 1.1e-10 off.
# 12.5778750001 is `+pm=copenhagen`, 12.577875. Greenwich is one of them: +-5e-9 (0.87e-10
# radians) is 0, +-6.5e-9 (1.13e-10 radians) is kept.
PM_CASES = ["EPSG:4807"] + [
    f"+proj=longlat +ellps=clrk80ign +pm={pm} +type=crs"
    for pm in [
        "2.33722917",
        "2.3372291721",
        "2.3372291612",
        "2.3372291730",
        "2.3372291603",
        "12.5778750001",
        "0.000000005",
        "-0.000000005",
        "0.0000000065",
        "-0.0000000065",
    ]
]


def main():
    assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str
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
                # pyproj gives (and takes) a CRS's coordinates in radians in degrees.
                "proj": [math.radians(v) for v in xy] if in_radians(wkt) else list(xy),
                "proj_back": list(back.transform(*xy, errcheck=True)),
            }
        )
    pm_rows = []
    for crs in PM_CASES:
        t = Transformer.from_crs(crs, "EPSG:4275", always_xy=True)
        pm_rows.append({"crs": crs, "lon": t.transform(0.0, 0.0, errcheck=True)[0]})
    # What PROJ writes for Paris: the `+pm=` of `to_proj4()`, the `PRIMEM` of
    # `to_wkt("WKT1_GDAL")`, and the prime meridian of `to_json()` in degrees.
    exports = []
    lambert = strip(CRS.from_epsg(27572).to_wkt("WKT1_GDAL"))
    lcc = (
        "+proj=lcc +lat_1=46.8 +lat_0=46.8 +lon_0=0 +k_0=0.99987742 +x_0=600000 "
        "+y_0=2200000 +ellps=clrk80ign +pm=2.33722917 +units=m +type=crs"
    )
    for crs in [lambert, lcc]:
        c = CRS(crs)
        pm = re.search(r"\+pm=(\S+)", c.to_proj4())[1]
        wkt1 = float(re.search(r'PRIMEM\["[^"]*",([^,\]]+)', c.to_wkt("WKT1_GDAL"))[1])
        json_pm = json.loads(c.to_json())["base_crs"]["datum"]["prime_meridian"]
        lon = json_pm["longitude"]
        if isinstance(lon, dict):
            lon = lon["value"] * math.degrees(lon["unit"]["conversion_factor"])
        exports.append(
            {"crs": crs, "proj4_pm": pm, "wkt1_pm": wkt1, "projjson_pm": lon}
        )
    # ESRI's WKT of ATF (Paris) and Nord de Guerre as PROJ writes it, unstripped: identified as
    # their codes, and placed in WGS 84 as PROJ places the codes. (PROJ's own transformation of
    # the bare ESRI WKT of EPSG:4901 goes through IGNF's chain with the prime meridian applied
    # twice, 178 km west; terrio does not follow it.)
    idents = []
    for code, point in [(4901, (0.5, 54.5)), (27500, (500000.0, 300000.0))]:
        crs = CRS.from_epsg(code)
        t = Transformer.from_crs(crs, "EPSG:4326", always_xy=True)
        idents.append(
            {
                "code": code,
                "wkt": crs.to_wkt("WKT1_ESRI"),
                "point": point,
                "proj": list(t.transform(*point, errcheck=True)),
            }
        )
    data = {
        "proj_version": pyproj.proj_version_str,
        "cases": rows,
        "pm_cases": pm_rows,
        "exports": exports,
        "identified": idents,
    }
    OUT.write_text(json.dumps(data, indent=1) + "\n")


if __name__ == "__main__":
    main()
