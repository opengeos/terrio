# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate what GDAL adds to the PROJ string of an EPSG or ESRI CRS when it exports it.

GDAL's ``OSRExportToProj4`` writes PROJ's PROJ string of the CRS bound to WGS 84 when PROJ finds
a single transformation for it (``OSR_ADD_TOWGS84_ON_EXPORT_TO_PROJ4=YES``, its default, and PROJ's
``createBoundCRSToWGS84IfPossible``): the Helmert transformation as ``+towgs84``, and for a
compound CRS the geoid model as ``+geoidgrids`` and ``+geoid_crs``. terrio's own PROJ string
(``SpatialRef::to_proj4``, PROJ's ``proj_as_proj_string``) has neither; the rest of the two strings
is the same for every EPSG and ESRI CRS terrio carries.

Writes ``crates/terrio-crs/data/gdal_towgs84.tsv.zst``, a zstd-compressed text table with lines
``authority<TAB>code<TAB>towgs84<TAB>geoid``: the ``+towgs84`` values GDAL writes (empty for none)
and its ``+geoidgrids=... +geoid_crs=...`` tokens (empty for none), for each CRS GDAL writes either
for. Deprecated codes have their own definition, as in terrio and PROJ (``OSR_USE_NON_DEPRECATED``
is turned off).

Also writes ``crates/terrio-crs/tests/data/gdal_proj4_export.tsv``, ``authority:code<TAB>PROJ
string`` as GDAL exports it, for every fifth CRS of the table and every twentieth other one, and
the CRSs RustGIS named (opengeos/RustGIS#2335 follow-up).

Usage: python scripts/gen_gdal_towgs84.py

Needs GDAL 3.13.3 on PROJ 9.9.0 (conda-forge), its Python bindings and the ``zstd`` command.
"""

import os
import re
import subprocess

from osgeo import gdal, osr

ROOT = os.path.join(os.path.dirname(__file__), "..")
OUT = os.path.join(ROOT, "crates", "terrio-crs", "data", "gdal_towgs84.tsv.zst")
FIXTURE = os.path.join(ROOT, "crates", "terrio-crs", "tests", "data", "gdal_proj4_export.tsv")

KINDS = (
    osr.OSR_CRS_TYPE_GEOGRAPHIC_2D,
    osr.OSR_CRS_TYPE_GEOGRAPHIC_3D,
    osr.OSR_CRS_TYPE_GEOCENTRIC,
    osr.OSR_CRS_TYPE_PROJECTED,
    osr.OSR_CRS_TYPE_COMPOUND,
)

# Pinned in the test fixture whatever the sampling.
NAMED = [
    "EPSG:31467", "EPSG:2056", "EPSG:28992", "EPSG:3405", "EPSG:4258",
    "EPSG:27700", "EPSG:4269", "EPSG:4267", "ESRI:102033", "ESRI:102453",
    "EPSG:6871", "EPSG:3901",
]


def export(code):
    """GDAL's PROJ string of ``authority:code``, or None when GDAL refuses it."""
    srs = osr.SpatialReference()
    try:
        srs.SetFromUserInput(code)
        return srs.ExportToProj4()
    except RuntimeError:
        return None


def main():
    assert gdal.__version__ == "3.13.3", gdal.__version__
    assert (osr.GetPROJVersionMajor(), osr.GetPROJVersionMinor(), osr.GetPROJVersionMicro()) == (9, 9, 0)
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    gdal.SetConfigOption("OSR_USE_NON_DEPRECATED", "NO")
    rows, with_additions, others = [], [], []
    for authority in ("EPSG", "ESRI"):
        infos = [i for i in osr.GetCRSInfoListFromDatabase(authority) if i.type in KINDS]
        for info in sorted(infos, key=lambda i: int(i.code)):
            code = f"{authority}:{info.code}"
            proj = export(code)
            if proj is None:
                continue
            towgs84 = re.search(r" \+towgs84=(\S+)", proj)
            geoid = re.search(r" (\+geoidgrids=\S+ \+geoid_crs=\S+)", proj)
            if towgs84 or geoid:
                rows.append((authority, info.code, towgs84.group(1) if towgs84 else "", geoid.group(1) if geoid else ""))
                with_additions.append((code, proj))
            else:
                others.append((code, proj))
    text = "".join("\t".join(r) + "\n" for r in rows)
    subprocess.run(["zstd", "-19", "-q", "-f", "-o", OUT], input=text.encode(), check=True)
    fixture = dict(with_additions[::5] + others[::20])
    for code in NAMED:
        fixture[code] = export(code)
    with open(FIXTURE, "w") as f:
        for code, proj in sorted(fixture.items(), key=lambda c: (c[0].split(":")[0], int(c[0].split(":")[1]))):
            f.write(f"{code}\t{proj}\n")
    print(f"{len(rows)} CRSs with additions; {len(fixture)} in the fixture")


if __name__ == "__main__":
    main()
