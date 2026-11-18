# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate what GDAL adds to the PROJ string of an EPSG or ESRI CRS when it exports it.

GDAL's ``OSRExportToProj4`` writes PROJ's PROJ string of the CRS bound to WGS 84 when PROJ finds
a single transformation for it (``OSR_ADD_TOWGS84_ON_EXPORT_TO_PROJ4=YES``, its default, and PROJ's
``createBoundCRSToWGS84IfPossible``): the Helmert transformation as ``+towgs84``, and for a
compound CRS the geoid model as ``+geoidgrids`` and ``+geoid_crs``. terrio's own PROJ string
(``SpatialRef::to_proj4``, PROJ's ``proj_as_proj_string``) has neither; the test compares the rest
of the two strings.

Writes ``crates/terrio-crs/data/gdal_towgs84.tsv.zst``, a zstd-compressed text table with lines
``authority<TAB>code<TAB>towgs84<TAB>geoid``: the ``+towgs84`` values GDAL writes (empty for none)
and its ``+geoidgrids=... +geoid_crs=...`` tokens (empty for none), for each CRS GDAL writes either
for. Deprecated codes have their own definition, as in terrio and PROJ (``OSR_USE_NON_DEPRECATED``
is turned off).

Also writes ``crates/terrio-crs/tests/data/gdal_proj4_export.tsv``, ``authority:code<TAB>PROJ
string`` as GDAL exports it, for every fifth CRS of the table and every twentieth other one, and
the CRSs RustGIS named (opengeos/RustGIS#2335 follow-up).

And ``crates/terrio-crs/tests/data/gdal_proj4_export_inputs.tsv``, ``authority:code<TAB>kind<TAB>
input<TAB>PROJ string``: GDAL's string for the named CRSs given as other definitions, GDAL's WKT 1
and WKT 2 (with their root identifier, which GDAL binds the CRS by), the WKT 1 without its root
identifier, the WKT 1 with a datum shift of its own, ESRI's WKT and GDAL's PROJ string without
its additions.

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
INPUTS = os.path.join(ROOT, "crates", "terrio-crs", "tests", "data", "gdal_proj4_export_inputs.tsv")

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
    "EPSG:6871", "EPSG:3901", "ESRI:102043",
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
    gdal.SetConfigOption("OSR_ADD_TOWGS84_ON_EXPORT_TO_PROJ4", "YES")
    rows, with_additions, others = [], [], []
    for authority in ("EPSG", "ESRI"):
        # PROJ lists some codes twice (as more than one kind of CRS); each is exported once.
        codes = sorted({int(i.code) for i in osr.GetCRSInfoListFromDatabase(authority) if i.type in KINDS})
        for number in codes:
            code = f"{authority}:{number}"
            proj = export(code)
            if proj is None:
                continue
            towgs84 = re.search(r" \+towgs84=(\S+)", proj)
            geoid = re.search(r" (\+geoidgrids=\S+ \+geoid_crs=\S+)", proj)
            assert '"' not in proj, f"{code}: a quoted value: {proj}"
            assert bool(geoid) == ("+geoidgrids=" in proj), f"{code}: a geoid model without its CRS: {proj}"
            if towgs84 or geoid:
                rows.append((authority, str(number), towgs84.group(1) if towgs84 else "", geoid.group(1) if geoid else ""))
                with_additions.append((code, proj))
            else:
                others.append((code, proj))
    text = "".join("\t".join(r) + "\n" for r in rows)
    # Each file is written beside its place and moved there only once all three are complete.
    subprocess.run(["zstd", "-19", "-q", "-f", "-o", OUT + ".tmp"], input=text.encode(), check=True)
    fixture = dict(with_additions[::5] + others[::20])
    for code in NAMED:
        proj = export(code)
        assert proj is not None, f"GDAL refuses {code}"
        fixture[code] = proj
    with open(FIXTURE + ".tmp", "w") as f:
        for code, proj in sorted(fixture.items(), key=lambda c: (c[0].split(":")[0], int(c[0].split(":")[1]))):
            f.write(f"{code}\t{proj}\n")
    lines = 0
    with open(INPUTS + ".tmp", "w") as f:
        for code in NAMED:
            srs = osr.SpatialReference()
            srs.SetFromUserInput(code)
            # WKT 1 whatever OSR_WKT_FORMAT says, without the TOWGS84 GDAL may add to it.
            wkt1 = srs.ExportToWkt(["FORMAT=WKT1_GDAL", "ADD_TOWGS84_ON_EXPORT_TO_WKT1=NO"])
            esri = srs.Clone()
            esri.MorphToESRI()
            root = re.search(r',AUTHORITY\["[A-Z]+","\d+"\]\]$', wkt1)
            plain = re.sub(r" \+towgs84=\S+| \+geoidgrids=\S+ \+geoid_crs=\S+", "", srs.ExportToProj4())
            inputs = {
                "wkt1": wkt1,
                # One line: the newlines of EPSG's area descriptions become spaces.
                "wkt2": srs.ExportToWkt(["FORMAT=WKT2_2019", "MULTILINE=NO"]).replace("\n", " "),
                "wkt1-rootless": wkt1[: root.start()] + "]" if root else None,
                "esri": esri.ExportToWkt(),
                # The WKT 1 with a datum shift of its own, which GDAL keeps.
                "wkt1-towgs84": re.sub(r"(SPHEROID\[[^\[\]]*(?:\[[^\[\]]*\])?\])", r"\1,TOWGS84[1,2,3,0,0,0,0]", wkt1, count=1)
                if "TOWGS84" not in wkt1 and not wkt1.startswith("COMPD_CS")
                else None,
                "proj": plain,
            }
            for kind, text in inputs.items():
                if text is None:
                    continue
                proj = export(text)
                assert proj is not None and "\t" not in text and "\n" not in text, (code, kind)
                f.write(f"{code}\t{kind}\t{text}\t{proj}\n")
                lines += 1
    for path in (OUT, FIXTURE, INPUTS):
        os.replace(path + ".tmp", path)
    print(f"{len(rows)} CRSs with additions; {len(fixture)} in the fixture, {lines} inputs")


if __name__ == "__main__":
    main()
