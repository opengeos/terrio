# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate terrio's CRS catalog: the registry CRSs ``terrio_crs::list_crs`` lists.

Writes ``crates/terrio-crs/data/crs_catalog.tsv.zst``, a zstd-compressed text table with these
lines:

- ``#`` comments, the first giving the PROJ and GDAL versions, and ``#<TAB>authority<TAB>version<TAB>date``
  lines with the version of each registry in PROJ's database (``EPSG``, ``v12.029``,
  ``2025-10-02``).
- ``U<TAB>id<TAB>west,south,east,north<TAB>name``: an area of use, with its bounding box in
  degrees (empty when PROJ gives none).
- ``M<TAB>id<TAB>name``: a projection method (``Albers Equal Area``).
- ``B<TAB>id<TAB>name``: a celestial body (``Earth``).
- ``C<TAB>authority<TAB>code<TAB>kind<TAB>flags<TAB>area<TAB>method<TAB>body<TAB>name``: a CRS, of
  kind ``2`` (geographic 2D), ``3`` (geographic 3D), ``g`` (geocentric), ``p`` (projected), ``v``
  (vertical), ``k`` (compound) or ``o`` (other), with flags (``d``: deprecated) and the ids of
  its area of use, projection method and celestial body (empty when it has none).

Texts escape backslashes, tabs, line feeds and carriage returns (``\\\\``, ``\\t``, ``\\n``,
``\\r``).

The CRSs, with their names, kinds, deprecation, areas of use, projection methods and bodies, are
those ``OSRGetCRSInfoListFromDatabase`` (``proj_get_crs_info_list_from_database``) gives for the
EPSG, ESRI and IAU_2015 authorities, once each (with the area of use ``OSRGetAreaOfUse`` gives,
the first PROJ lists), without those terrio's own tables do not resolve: the EPSG
CRSs of ``epsg.tsv.zst`` (geographic, geocentric, projected, vertical and compound), the ESRI CRSs
of ``esri.tsv.zst`` and the IAU_2015 CRSs of ``iau.tsv.zst``.

Usage: python scripts/gen_crs_catalog.py

Needs GDAL's Python bindings (with the PROJ version the EPSG table was generated with) and the
``zstd`` command.
"""

import os
import sqlite3
import subprocess

from osgeo import gdal, osr

osr.UseExceptions()

DATA = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio-crs", "data")
OUT = os.path.join(DATA, "crs_catalog.tsv.zst")

# OSRCRSType to the catalog's kinds.
KINDS = {
    osr.OSR_CRS_TYPE_GEOGRAPHIC_2D: "2",
    osr.OSR_CRS_TYPE_GEOGRAPHIC_3D: "3",
    osr.OSR_CRS_TYPE_GEOCENTRIC: "g",
    osr.OSR_CRS_TYPE_PROJECTED: "p",
    osr.OSR_CRS_TYPE_VERTICAL: "v",
    osr.OSR_CRS_TYPE_COMPOUND: "k",
    osr.OSR_CRS_TYPE_OTHER: "o",
}


def table_codes(name, kinds, column=1):
    """List the codes of a terrio table's lines of some kinds.

    Args:
        name: The table's file name in ``crates/terrio-crs/data``.
        kinds: The line kinds (first fields) to keep, or ``None`` for every line.
        column: The field holding the code.

    Returns:
        The set of codes, as strings.
    """
    text = subprocess.run(["zstd", "-dc", os.path.join(DATA, name)], capture_output=True, check=True).stdout.decode()
    codes = set()
    for line in text.splitlines():
        if line.startswith("#"):
            continue
        f = line.split("\t")
        if kinds is None or f[0] in kinds:
            codes.add(f[column])
    return codes


def registry_versions():
    """Read the versions of the registries in PROJ's database.

    Returns:
        ``[(authority, version, date)]`` for EPSG, ESRI and IAU_2015.
    """
    path = next(p for p in (os.path.join(d, "proj.db") for d in osr.GetPROJSearchPaths()) if os.path.exists(p))
    db = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    meta = dict(db.execute("SELECT key, value FROM metadata"))
    return [
        ("EPSG", meta["EPSG.VERSION"], meta["EPSG.DATE"]),
        ("ESRI", meta["ESRI.VERSION"], meta["ESRI.DATE"]),
        # PROJ's IAU_2015 CRSs are generated from the 2015 report of the IAU WGCCRE.
        ("IAU_2015", "2015", ""),
    ]


def clean(text):
    """Escape a database text for a TSV field: backslashes, tabs, line feeds and carriage returns
    as ``\\\\``, ``\\t``, ``\\n`` and ``\\r``, the rest unchanged (two EPSG area names end with a line
    feed, EPSG:10551's name with a space)."""
    escapes = {"\\": "\\\\", "\t": "\\t", "\n": "\\n", "\r": "\\r"}
    return "".join(escapes.get(c, c) for c in text or "")


def rows():
    """List the catalog's lines.

    Returns:
        The ``U``, ``M``, ``B`` and ``C`` rows, as tuples.
    """
    resolved = {
        "EPSG": table_codes("epsg.tsv.zst", {"C", "V", "K"}),
        "ESRI": table_codes("esri.tsv.zst", {"C"}),
        "IAU_2015": table_codes("iau.tsv.zst", None, column=0),
    }
    areas, methods, bodies = {}, {}, {}

    def ident(index, key):
        return index.setdefault(key, len(index))

    crs = []
    for auth in ("EPSG", "ESRI", "IAU_2015"):
        infos = osr.GetCRSInfoListFromDatabase(auth)
        listed = {i.code for i in infos}
        missing = sorted(resolved[auth] - listed, key=int)
        if missing:
            print(f"{auth}: {len(missing)} codes of terrio's table not in PROJ's database: {missing[:10]}")
        seen = set()
        # PROJ lists a CRS once per usage (EPSG:25832 has two areas of use), first that of
        # `OSRGetAreaOfUse`: kept alone. The sort is stable.
        for i in sorted(infos, key=lambda i: int(i.code)):
            if i.code not in resolved[auth] or i.code in seen:
                continue
            seen.add(i.code)
            bounds = (
                ",".join(repr(v) for v in (i.west_lon_degree, i.south_lat_degree, i.east_lon_degree, i.north_lat_degree))
                if i.bbox_valid
                else ""
            )
            area = ident(areas, (bounds, clean(i.area_name))) if bounds or i.area_name else ""
            method = ident(methods, clean(i.projection_method)) if i.projection_method else ""
            body = ident(bodies, clean(i.celestial_body_name)) if i.celestial_body_name else ""
            flags = "d" if i.deprecated else ""
            crs.append(("C", auth, i.code, KINDS[i.type], flags, area, method, body, clean(i.name)))
    out = [("U", n, bounds, name) for (bounds, name), n in areas.items()]
    out += [("M", n, name) for name, n in methods.items()]
    out += [("B", n, name) for name, n in bodies.items()]
    return out + crs


def main():
    """Write the compressed catalog."""
    osr.SetPROJEnableNetwork(False)
    lines = ["\t".join(map(str, r)) + "\n" for r in rows()]
    proj = ".".join(str(v) for v in (osr.GetPROJVersionMajor(), osr.GetPROJVersionMinor(), osr.GetPROJVersionMicro()))
    header = f"# CRSs of PROJ {proj}'s database, via GDAL {gdal.__version__}. Generated by scripts/gen_crs_catalog.py.\n"
    header += "".join(f"#\t{a}\t{v}\t{d}\n" for a, v, d in registry_versions())
    data = (header + "".join(lines)).encode()
    out = subprocess.run(["zstd", "-19", "-q", "-c"], input=data, capture_output=True, check=True).stdout
    with open(OUT, "wb") as f:
        f.write(out)
    kinds = {}
    for line in lines:
        key = line[0] if line[0] != "C" else "C " + line.split("\t")[1]
        kinds[key] = kinds.get(key, 0) + 1
    print(f"{kinds}, {len(data)} bytes, {len(out)} compressed")


if __name__ == "__main__":
    main()
