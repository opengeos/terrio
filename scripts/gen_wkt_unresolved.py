# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/wkt_unresolved.json: where PROJ 9.9.0 places points of WKTs
it does not resolve to their EPSG CRS, and of WKTs it does, as a guard.

PROJ resolves a WKT without the CRS's ID by its name, and only when the WKT matches the CRS of
that name, axis order included: an ESRI WKT 1 (no ``AXIS``, so longitude first) and a renamed CRS
are not resolved. PROJ then also searches transformations through intermediate datums, and may
place points otherwise than the EPSG code.

The WKTs are variants of EPSG geographic 2D CRSs (``variants``): ESRI's WKT 1, the same with the
``GEOGCS`` renamed, WKT 2 without IDs, the same with the datum's ID, the same renamed (CRS and
datum), WKT 1 with the datum's ``AUTHORITY`` alone, a renamed WKT 2 on a datum ensemble with its ID
(WGS 84, ETRS89); for the CRSs PROJ places otherwise, also a renamed WKT 2 ``GEODCRS`` with an
ellipsoidal CS and an ESRI compound CRS with the ``GEOGCS`` renamed; WKT 2 named by an alias of the
CRS (``ALIASED``); and ESRI WKT 1 of some projected CRSs, also with the ``GEOGCS`` renamed. Each
case is a WKT, a point and the point in the other CRS by ``Transformer.from_crs(src, dst,
always_xy=True).transform``: to WGS 84, from WGS 84 (the point in WGS 84), and, for a sample, to
ETRS89, NAD83 and ETRS89 / UTM zone 32N. ``by_code`` is the same with the EPSG code in place of the
WKT.

``pending`` cases are those PROJ places by a step terrio does not follow yet: ONGD14 renamed with
an ID of an authority PROJ does not know (``AUTHORITY["LOCAL","1"]``), from which PROJ's datum
pivot takes only the geocentric CRS's transformation. EPSG:4901 (ATF (Paris)) is left out: PROJ's
placement of its ESRI WKT applies the Paris prime meridian twice, which terrio does not copy
(``tests/grads_and_prime_meridians.rs`` checks it is placed as PROJ places EPSG:4901).

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt_unresolved.py (needs pyproj 3.8.0, no grids)
"""

import json
import math
import os
import re
import sys
import warnings
from pathlib import Path

import pyproj
import pyproj.datadir
import pyproj.network
from pyproj import CRS, Transformer
from pyproj.database import query_crs_info

# The sibling script's grid check, importable under `python -I` too.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_wkt_datum_shift import installed_grids

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/wkt_unresolved.json"
)

# Codes whose variants PROJ places otherwise than the code, and a sample of others.
SEARCHED = [
    4182,
    4204,
    4221,
    4283,
    4314,
    4623,
    4626,
    4632,
    4642,
    4662,
    4689,
    4802,
    4805,
    4806,
    4816,
    5132,
]
PENDING_CODES = [7373, 9309, 10831]
# Searched variants also placed to and from other targets than WGS 84.
SEARCHED_TO_TARGETS = {
    (code, kind) for code in (4806, 5132, 4314) for kind in ("esri", "esri gcs renamed")
}
# CRSs on datum ensembles.
ENSEMBLES = [4326, 4258]
# WKTs named by an alias of their CRS (which PROJ finds by it): lon-first, and on a renamed datum
# (NAD83(2011), which terrio then reads as no code).
ALIASED = {4806: ("Rome 1940 (Rome)", "lon first"), 6318: ("415", "renamed datum")}
SAMPLE_EVERY = 25
TARGETS = [4258, 4269, 25832]
TARGET_VARIANTS = [
    "esri",
    "esri gcs renamed",
    "wkt2 no ids",
    "wkt2 ensemble id, renamed",
]
PROJECTED = [3003, 31467, 3006, 2193, 27700, 2056, 5514, 32633]
# Geographic CRSs a custom Transverse Mercator without code is put on, as WKT 1 (a `GEOGCS`
# without `AXIS`, longitude first) and WKT 2 (a `BASEGEOGCRS` without `CS`, latitude first).
CUSTOM_BASES = [4269, 4230, 4314, 4277, 4806]
# A projected CRS PROJ finds by its name, with another false easting.
CHANGED = 31467
DEG = 'ANGLEUNIT["degree",0.0174532925199433]'
# What terrio does not follow yet (see the module's doc comment).
PENDING = {"7373 esri gcs renamed, local id"}


# A vertical CRS PROJ has no transformation for, which leaves the horizontal one alone.
VERT_CS = (
    'VERT_CS["Local height",VERT_DATUM["Local",2005],UNIT["metre",1],AXIS["Up",UP]]'
)


def strip_ids(w):
    """A WKT without its EPSG and ESRI IDs."""
    return re.sub(r',(AUTHORITY|ID)\["(EPSG|ESRI)","?\d+"?\]', "", w)


def variants(c, more):
    """The WKT variants of a geographic CRS, by name.

    Args:
        c: The CRS.
        more: Whether to add its variants as a geodetic and as a compound CRS.

    Returns:
        A dict from variant name to WKT.
    """
    v = {}
    esri = c.to_wkt("WKT1_ESRI")
    if esri:
        v["esri"] = esri
        v["esri gcs renamed"] = re.sub(r'GEOGCS\["[^"]*"', 'GEOGCS["GCS_Local"', esri)
    w2 = c.to_wkt("WKT2_2019")
    w2 = strip_ids(
        re.sub(r",USAGE\[SCOPE\[[^\]]*\],AREA\[[^\]]*\],BBOX\[[^\]]*\]\]", "", w2)
    )
    v["wkt2 no ids"] = w2
    if more:
        # A WKT 2 geodetic CRS with an ellipsoidal CS, which PROJ reads as a geographic CRS.
        v["wkt2 geodcrs renamed"] = re.sub(r'^GEOGCRS\["[^"]*"', 'GEODCRS["Local"', w2)
    if esri and more:
        # The same with an ID of an authority PROJ does not know: it skips it, but searches
        # only among that authority's CRSs, so through no other datum.
        v["esri gcs renamed, local id"] = (
            v["esri gcs renamed"][:-1] + ',AUTHORITY["LOCAL","1"]]'
        )

        # A compound CRS whose horizontal CRS is ESRI's, renamed.
        v["esri compound renamed"] = (
            f'COMPD_CS["Local + height",{v["esri gcs renamed"]},{VERT_CS}]'
        )
    # A datum ensemble with its ID (WGS 84, ETRS89), the CRS renamed.
    if c.datum.to_json_dict().get("type") == "DatumEnsemble":
        ensemble = c.datum.to_json_dict()["id"]["code"]
        with_id = re.sub(
            r"(ENSEMBLEACCURACY\[[^\]]*\])\]",
            rf'\1,ID["EPSG",{ensemble}]]',
            w2,
            count=1,
        )
        v["wkt2 ensemble id, renamed"] = re.sub(
            r'^GEOGCRS\["[^"]*"', 'GEOGCRS["Local"', with_id
        )
    datum = (c.datum.to_json_dict().get("id") or {}).get("code")
    if datum:
        with_id = datum_id(w2, datum)
        v["wkt2 datum id"] = with_id
        renamed = re.sub(
            r'(GEOGCRS|BASEGEOGCRS|PROJCRS)\["[^"]*"', r'\1["Local"', with_id
        )
        if more:
            # Under the CRS's own name: PROJ compares the datum's name too.
            v["wkt2 datum id, datum renamed"] = re.sub(
                r'DATUM\["[^"]*"', 'DATUM["Local datum"', with_id
            )
        v["wkt2 datum id, renamed"] = re.sub(
            r'DATUM\["[^"]*"', 'DATUM["Local datum"', renamed
        )
    w1 = c.to_wkt("WKT1_GDAL")
    m = re.search(r'AUTHORITY\["EPSG","(\d+)"\]\],PRIMEM', w1)
    if m:
        v["wkt1 datum authority"] = re.sub(
            r"\]\],PRIMEM",
            f'],AUTHORITY["EPSG","{m.group(1)}"]],PRIMEM',
            strip_ids(w1),
            count=1,
        )
    return v


def aliased(c, code):
    """The WKT of a CRS named by an alias of it (``ALIASED``), by variant name."""
    if code not in ALIASED:
        return {}
    alias, how = ALIASED[code]
    w2 = c.to_wkt("WKT2_2019")
    w2 = strip_ids(
        re.sub(r",USAGE\[SCOPE\[[^\]]*\],AREA\[[^\]]*\],BBOX\[[^\]]*\]\]", "", w2)
    )
    datum = c.datum.to_json_dict()["id"]["code"]
    w2 = datum_id(w2, datum)
    w2 = re.sub(r'^GEOGCRS\["[^"]*"', f'GEOGCRS["{alias}"', w2)
    if how == "lon first":
        axes = re.findall(
            r'AXIS\["[^"]*",[a-z]+,ORDER\[\d\],ANGLEUNIT\["degree",0\.0174532925199433\]\]',
            w2,
        )
        assert len(axes) == 2, w2
        swapped = (
            axes[1].replace("ORDER[2]", "ORDER[1]")
            + ","
            + axes[0].replace("ORDER[1]", "ORDER[2]")
        )
        w2 = w2.replace(axes[0] + "," + axes[1], swapped)
    else:
        w2 = re.sub(r'DATUM\["[^"]*"', 'DATUM["Local datum"', w2)
    return {f"wkt2 alias name, {how}": w2}


def custom_tm(
    base, lon0, wkt1, false_easting=500000, name="Custom TM", axes=None, scale=0.9996
):
    """A Transverse Mercator WKT on a geographic CRS's WKT, without code."""
    if wkt1:
        axes = axes or 'AXIS["Easting",EAST],AXIS["Northing",NORTH]'
        return (
            f'PROJCS["{name}",{base},PROJECTION["Transverse_Mercator"],'
            f'PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",{lon0}],'
            f'PARAMETER["scale_factor",{scale}],'
            f'PARAMETER["false_easting",{false_easting}],PARAMETER["false_northing",0],'
            f'UNIT["metre",1],{axes}]'
        )
    return (
        f'PROJCRS["{name}",{base},CONVERSION["custom",METHOD["Transverse Mercator"],'
        f'PARAMETER["Latitude of natural origin",0,{DEG}],'
        f'PARAMETER["Longitude of natural origin",{lon0},{DEG}],'
        f'PARAMETER["Scale factor at natural origin",{scale},SCALEUNIT["unity",1]],'
        f'PARAMETER["False easting",{false_easting},LENGTHUNIT["metre",1]],'
        'PARAMETER["False northing",0,LENGTHUNIT["metre",1]]],CS[Cartesian,2],'
        'AXIS["(E)",east,ORDER[1],LENGTHUNIT["metre",1]],'
        'AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1]]]'
    )


def bases(code):
    """A geographic CRS's WKT 1 ``GEOGCS`` without IDs or ``AXIS``, its WKT 2 ``BASEGEOGCRS``
    without IDs or ``CS``, and its WKT 1 ``GEOGCS`` with IDs."""
    c = CRS.from_epsg(code)
    with_ids = c.to_wkt("WKT1_GDAL")
    w1 = re.sub(r',AXIS\["[^"]*",[A-Z]+\]', "", strip_ids(with_ids))
    w2 = strip_ids(
        re.sub(
            r",USAGE\[SCOPE\[[^\]]*\],AREA\[[^\]]*\],BBOX\[[^\]]*\]\]",
            "",
            c.to_wkt("WKT2_2019"),
        )
    )
    w2 = re.sub(
        r",CS\[ellipsoidal,2\].*\]$", "]", w2.replace("GEOGCRS[", "BASEGEOGCRS[", 1)
    )
    return w1, w2, with_ids


def datum_id(w2, datum):
    """A WKT 2 with its datum's (or datum ensemble's) EPSG ID, at the end of the ``DATUM`` or
    ``ENSEMBLE`` node, before ``PRIMEM`` (after an ``ANCHOREPOCH`` or ``ENSEMBLEACCURACY`` too)."""
    out, n = re.subn(r"\],PRIMEM\[", f',ID["EPSG",{datum}]],PRIMEM[', w2, count=1)
    assert n == 1, w2
    return out


def centre(c):
    """The centre of a CRS's area of use, in WGS 84 longitude and latitude."""
    w, s, e, n = c.area_of_use.bounds
    if e < w:
        e += 360
    lon, lat = (w + e) / 2, (s + n) / 2
    return (lon - 360 if lon > 180 else lon), lat


def metres(a, b, geographic):
    """The distance between two points in metres, near enough for longitudes and latitudes."""
    if geographic:
        return (
            math.hypot((a[0] - b[0]) * math.cos(math.radians(b[1])), a[1] - b[1])
            * 111195
        )
    return math.hypot(a[0] - b[0], a[1] - b[1])


def place(src, dst, point):
    """PROJ's coordinates of a point of ``src`` in ``dst``, None when not finite."""
    r = Transformer.from_crs(src, dst, always_xy=True).transform(*point)
    return [r[0], r[1]] if all(map(math.isfinite, r)) else None


def main():
    """Write the cases."""
    assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str
    pyproj.network.set_network_enabled(False)
    assert os.environ.get("PROJ_NETWORK") == "OFF"
    # No grids, so the values don't depend on what is installed: no file PROJ's database names
    # as a grid, nor of a grid format (`gen_wkt_datum_shift.installed_grids`).
    found = installed_grids()
    assert not found, found
    warnings.filterwarnings("ignore")
    wgs84 = CRS(4326)
    infos = [
        i
        for i in query_crs_info("EPSG", pyproj.enums.PJType.GEOGRAPHIC_2D_CRS)
        if not i.deprecated
    ]
    codes = sorted(
        {int(i.code) for i in infos[::SAMPLE_EVERY]}
        | set(SEARCHED)
        | set(PENDING_CODES)
        | set(ENSEMBLES)
        | set(ALIASED)
    )
    wkts, cases = {}, []

    def add(name, key, code_crs, wkt_crs, other, point, reverse, geographic):
        # A point PROJ could not place (not finite) gives no case.
        if point is None:
            return
        src, dst = (other, wkt_crs) if reverse else (wkt_crs, other)
        proj = place(src, dst, point)
        by_code = place(*((other, code_crs) if reverse else (code_crs, other)), point)
        if proj and by_code:
            # Pending: PROJ places it otherwise than the code, by a step terrio does not follow.
            pending = key in PENDING and metres(proj, by_code, geographic) > 0.1
            cases.append(
                {
                    "name": name,
                    "wkt": key,
                    "other": other.to_epsg(),
                    "reverse": reverse,
                    "point": list(point),
                    "proj": proj,
                    "by_code": by_code,
                    "geographic": geographic,
                    "pending": pending,
                }
            )

    for code in codes:
        if code == 4901:
            continue
        c = CRS.from_epsg(code)
        if c.datum is None or c.area_of_use is None:
            continue
        lonlat = centre(c)
        point = place(wgs84, c, lonlat)
        if point is None:
            continue
        for kind, wkt in (
            variants(c, code in SEARCHED or code in PENDING_CODES) | aliased(c, code)
        ).items():
            key = f"{code} {kind}"
            w = CRS.from_wkt(wkt)
            wkts[key] = wkt
            add(f"{key} -> 4326", key, c, w, wgs84, point, False, True)
            add(f"{key} <- 4326", key, c, w, wgs84, place(c, wgs84, point), True, True)
            if kind in TARGET_VARIANTS and code not in SEARCHED:
                for t in TARGETS:
                    tc = CRS(t)
                    add(f"{key} -> {t}", key, c, w, tc, point, False, tc.is_geographic)
            # Some searched CRSs to other targets, both ways.
            if (code, kind) in SEARCHED_TO_TARGETS:
                for t in TARGETS:
                    tc = CRS(t)
                    add(f"{key} -> {t}", key, c, w, tc, point, False, tc.is_geographic)
                    if tc.is_geographic:
                        back = place(c, tc, point)
                        add(f"{key} <- {t}", key, c, w, tc, back, True, True)
    for code in PROJECTED:
        c = CRS.from_epsg(code)
        lonlat = centre(c)
        point = place(wgs84, c, lonlat)
        esri = c.to_wkt("WKT1_ESRI")
        for kind, wkt in [
            ("esri", esri),
            (
                "esri gcs renamed",
                re.sub(r'GEOGCS\["[^"]*"', 'GEOGCS["GCS_Local"', esri),
            ),
        ]:
            key = f"{code} projected {kind}"
            w = CRS.from_wkt(wkt)
            wkts[key] = wkt
            add(f"{key} -> 4326", key, c, w, wgs84, point, False, True)
            add(f"{key} <- 4326", key, c, w, wgs84, lonlat, True, False)
            add(f"{key} -> 4258", key, c, w, CRS(4258), point, False, True)
    # Custom projected CRSs without code: PROJ resolves a WKT 2 base by its name, not a WKT 1 one
    # (longitude first).
    for code in CUSTOM_BASES:
        g = CRS.from_epsg(code)
        lon, lat = centre(g)
        w1, w2, with_ids = bases(code)
        ref = CRS.from_wkt(custom_tm(with_ids, lon, True))
        point = place(wgs84, ref, (lon, lat))
        for kind, wkt in [
            ("wkt1", custom_tm(w1, lon, True)),
            ("wkt2", custom_tm(w2, lon, False)),
        ]:
            key = f"{code} custom tm, {kind} base without ids"
            w = CRS.from_wkt(wkt)
            wkts[key] = wkt
            add(f"{key} -> 4326", key, ref, w, wgs84, point, False, True)
            add(f"{key} <- 4326", key, ref, w, wgs84, (lon, lat), True, False)
            add(f"{key} -> 4258", key, ref, w, CRS(4258), point, False, True)
    # A projected CRS PROJ finds by its name, with another conversion: not resolved, but its
    # geographic CRS is when given as WKT 2.
    c = CRS.from_epsg(CHANGED)
    lon, lat = centre(c)
    w1, w2, with_ids = bases(c.geodetic_crs.to_epsg())
    lon0 = 9
    axes = 'AXIS["Northing",NORTH],AXIS["Easting",EAST]'
    ref = CRS.from_wkt(custom_tm(with_ids, lon0, True, 3500001, c.name, axes, 1))
    point = place(wgs84, ref, (lon, lat))
    changed = [
        ("wkt1", custom_tm(w1, lon0, True, 3500001, c.name, axes, 1)),
        ("wkt2", custom_tm(w2, lon0, False, 3500001, c.name, scale=1)),
    ]
    for kind, wkt in changed:
        key = f"{CHANGED} other false easting, {kind}"
        w = CRS.from_wkt(wkt)
        wkts[key] = wkt
        add(f"{key} -> 4326", key, ref, w, wgs84, point, False, True)
        add(f"{key} <- 4326", key, ref, w, wgs84, (lon, lat), True, False)
    pending = {c["wkt"] for c in cases if c["pending"]}
    assert pending == PENDING, (pending - PENDING, PENDING - pending)
    OUT.write_text(json.dumps({"wkts": wkts, "cases": cases}, indent=1) + "\n")
    searched = sum(
        1 for c in cases if metres(c["proj"], c["by_code"], c["geographic"]) > 0.01
    )
    print(
        f"{len(cases)} cases, {searched} placed otherwise than by the code, "
        f"{OUT.stat().st_size} bytes"
    )


if __name__ == "__main__":
    main()
