# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Compare ``terrio list-crs`` with GDAL's ``osr.GetCRSInfoListFromDatabase``.

For each authority (EPSG, ESRI, IAU_2015), every CRS terrio lists is compared field by field with
the one GDAL lists (the first of its rows, whose area of use is ``OSRGetAreaOfUse``'s): name, kind,
deprecation, area name, bounding box, projection method and celestial body. The CRSs GDAL lists
and terrio does not are reported by kind, as expected rejections: those terrio's registry does not
resolve (EPSG engineering CRSs, ESRI vertical CRSs, ESRI projections terrio does not compute).

Usage:
    python scripts/compare_crs_list_with_gdal.py [--terrio target/release/terrio] [--json]

Needs GDAL's Python bindings, with the PROJ version terrio's catalog was generated with
(``scripts/gen_crs_catalog.py``) for identical results.
"""

import argparse
import json
import subprocess
from collections import Counter
from pathlib import Path

from osgeo import osr

osr.UseExceptions()

ROOT = Path(__file__).resolve().parents[1]

# Pinned catalog counts for the CLI's wbprojection-fallback build (PROJ 9.8.1 snapshot).
# Missing registry entries must fail even if a newer GDAL database lists additional codes.
EXPECTED_COUNTS = {"EPSG": 7681, "ESRI": 2519, "IAU_2015": 2201}

KINDS = {
    osr.OSR_CRS_TYPE_GEOGRAPHIC_2D: "geographic_2d",
    osr.OSR_CRS_TYPE_GEOGRAPHIC_3D: "geographic_3d",
    osr.OSR_CRS_TYPE_GEOCENTRIC: "geocentric",
    osr.OSR_CRS_TYPE_PROJECTED: "projected",
    osr.OSR_CRS_TYPE_VERTICAL: "vertical",
    osr.OSR_CRS_TYPE_COMPOUND: "compound",
    osr.OSR_CRS_TYPE_OTHER: "other",
}


def gdal_entries(authority):
    """List GDAL's CRSs of an authority, once each, as ``terrio list-crs --json`` describes them.

    Args:
        authority: ``EPSG``, ``ESRI`` or ``IAU_2015``.

    Returns:
        A dict of entries by code.
    """
    entries = {}
    for i in osr.GetCRSInfoListFromDatabase(authority):
        code = int(i.code)
        if code in entries:
            continue
        bounds = [i.west_lon_degree, i.south_lat_degree, i.east_lon_degree, i.north_lat_degree] if i.bbox_valid else None
        entries[code] = {
            "authority": authority,
            "code": code,
            "name": i.name,
            "kind": KINDS[i.type],
            "deprecated": bool(i.deprecated),
            "area_name": i.area_name or None,
            "bounds": bounds,
            "projection_method": i.projection_method or None,
            "celestial_body": i.celestial_body_name or None,
        }
    return entries


def terrio_list(executable, *args):
    """Run ``terrio list-crs --include-deprecated --json`` with more arguments."""
    try:
        out = subprocess.run([executable, "list-crs", "--include-deprecated", "--json", *args], capture_output=True, text=True, check=True)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"terrio list-crs failed: {error.stderr.strip()}") from error
    return json.loads(out.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", nargs="?", help="unused: the conformance suite's data directory")
    parser.add_argument("--terrio", default=str(ROOT / "target" / "release" / "terrio"))
    parser.add_argument("--json", action="store_true", help="print the cases as JSON")
    args = parser.parse_args()
    osr.SetPROJEnableNetwork(False)
    listed = terrio_list(args.terrio)
    cases = []
    for authority in ("EPSG", "ESRI", "IAU_2015"):
        reference = gdal_entries(authority)
        ours = [e for e in listed if e["authority"] == authority]
        differs = []
        if len(ours) != EXPECTED_COUNTS[authority]:
            differs.append(f"catalog count {len(ours)} vs pinned {EXPECTED_COUNTS[authority]}")
        if len({e["code"] for e in ours}) != len(ours):
            differs.append("duplicate catalog codes")
        for e in ours:
            expected = reference.get(e["code"])
            if expected is None:
                differs.append(f"{authority}:{e['code']} is not in GDAL's list")
                continue
            fields = [k for k in expected if e.get(k) != expected[k]]
            if fields:
                differs.append(f"{authority}:{e['code']} {', '.join(f'{k}: {e.get(k)!r} vs {expected[k]!r}' for k in fields)}")
        name = f"{authority}: {len(ours)} listed CRSs, every field"
        if differs:
            cases.append({"name": name, "status": "differs", "detail": f"{len(differs)} differ: " + "; ".join(differs[:5])})
        else:
            cases.append({"name": name, "status": "identical", "detail": ""})
        codes = {e["code"] for e in ours}
        left_out = Counter(e["kind"] for c, e in reference.items() if c not in codes)
        if left_out:
            detail = ", ".join(f"{n} {k}" for k, n in sorted(left_out.items()))
            cases.append({"name": f"{authority}: {sum(left_out.values())} CRSs terrio does not resolve", "status": "expected rejection", "detail": detail})
    # The issue's acceptance: EPSG:3577 by a word of its name and by its code.
    for text in ("Albers", "3577"):
        found = [e for e in terrio_list(args.terrio, text) if (e["authority"], e["code"]) == ("EPSG", 3577)]
        expected = gdal_entries("EPSG")[3577]
        fields = [k for k in expected if found[0].get(k) != expected[k]] if found else []
        status = "identical" if found and not fields else "differs"
        detail = "" if found else "not found"
        if fields:
            detail = f"differs in {', '.join(fields)}"
        cases.append({"name": f"EPSG:3577 found by {text!r}", "status": status, "detail": detail})
    if args.json:
        print(json.dumps(cases, indent=1))
    else:
        for c in cases:
            print(f"{c['status']:>18}  {c['name']}  {c['detail']}")
    raise SystemExit(1 if any(c["status"] == "differs" for c in cases) else 0)


if __name__ == "__main__":
    main()
