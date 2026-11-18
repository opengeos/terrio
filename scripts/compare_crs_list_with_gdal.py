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
(``scripts/gen_crs_catalog.py``, PROJ 9.9.0) for identical results. With GDAL on PROJ 9.8.1 (CI's
reference, GDAL 3.12.2), the fields EPSG and ESRI changed between the two
(``crates/terrio-crs/tests/data/crs_list_changes.json``, ``scripts/gen_crs_list_changes.py``) are
reported as such and compared with PROJ 9.9.0's values: GDAL must have each changed field's PROJ
9.8.1 value and terrio its PROJ 9.9.0 value, and a CRS added since is compared with PROJ 9.9.0's
entry. Any other difference is reported as one.
"""

import argparse
import json
import subprocess
from collections import Counter
from pathlib import Path

from osgeo import osr

osr.UseExceptions()

ROOT = Path(__file__).resolve().parents[1]


class _Missing:
    """A field missing from terrio's entry, unlike any value of GDAL's (None included)."""

    def __repr__(self):
        return "missing"


MISSING = _Missing()
CHANGES = ROOT / "crates" / "terrio-crs" / "tests" / "data" / "crs_list_changes.json"

# Pinned catalog counts for the CLI's wbprojection-fallback build (PROJ 9.9.0 snapshot, EPSG
# v13.102). Missing registry entries must fail even if a newer GDAL database lists additional
# codes.
EXPECTED_COUNTS = {"EPSG": 7961, "ESRI": 2582, "IAU_2015": 2201}

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


def registry_changes():
    """What EPSG and ESRI changed since GDAL's PROJ, when it is the one the changes start from."""
    proj = f"PROJ {osr.GetPROJVersionMajor()}.{osr.GetPROJVersionMinor()}.{osr.GetPROJVersionMicro()}"
    changes = json.loads(CHANGES.read_text(encoding="utf-8"))
    return changes if changes["from"] == proj else None


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
    changes = registry_changes()
    cases = []
    for authority in ("EPSG", "ESRI", "IAU_2015"):
        reference = gdal_entries(authority)
        ours = [e for e in listed if e["authority"] == authority]
        differs, changed = [], []
        # The CRSs the registry removed since GDAL's PROJ: no rejection of terrio's, but terrio
        # must not list them.
        removed = set(changes["removed"][authority]) if changes else set()
        # The CRSs the registry added since GDAL's PROJ, which GDAL must not list.
        added_codes = {int(c) for c in changes["added"][authority]} if changes else set()
        differs.extend(
            f"{authority}:{c} is in GDAL's list, though {changes['from']} does not have it" for c in sorted(added_codes & set(reference))
        )
        if len(ours) != EXPECTED_COUNTS[authority]:
            differs.append(f"catalog count {len(ours)} vs pinned {EXPECTED_COUNTS[authority]}")
        if len({e["code"] for e in ours}) != len(ours):
            differs.append("duplicate catalog codes")
        for e in ours:
            expected = reference.get(e["code"])
            added = changes["added"][authority].get(str(e["code"])) if changes else None
            registry = changes["changes"][authority].get(str(e["code"])) if changes else None
            stale = []
            if added is not None:
                # A CRS the registry added since GDAL's PROJ: compared with the newer PROJ's entry,
                # and not counted as checked when GDAL lists it (a difference reported before the loop).
                if expected is not None:
                    stale = ["listed"]
                expected = added
            elif e["code"] in removed:
                # A CRS the registry removed since GDAL's PROJ that terrio still lists.
                differs.append(f"{authority}:{e['code']} is listed, though {changes['to']} removed it")
                continue
            elif expected is None:
                differs.append(f"{authority}:{e['code']} is not in GDAL's list")
                continue
            elif registry is not None:
                # A CRS the registry changed: GDAL must have each changed field's older value, and
                # terrio the newer one; its other fields are compared with GDAL's.
                stale = [k for k in registry if expected[k] != registry[k][0]]
                if stale:
                    differs.append(f"{authority}:{e['code']} GDAL's {', '.join(stale)} is not {changes['from']}'s")
                expected = {k: registry[k][1] if k in registry else v for k, v in expected.items()}
            # A field terrio leaves out differs, also from an explicit null.
            fields = [k for k in expected if e.get(k, MISSING) != expected[k]]
            if fields:
                differs.append(f"{authority}:{e['code']} {', '.join(f'{k}: {e.get(k, MISSING)!r} vs {expected[k]!r}' for k in fields)}")
            elif (added is not None or registry is not None) and not stale:
                # Only a CRS that passed every check is counted as the registry's change.
                changed.append(e["code"])
        name = f"{authority}: {len(ours)} listed CRSs, every field"
        if changed:
            name += f" (those the registry added or changed since {changes['from']} as {changes['to']} has them)"
        if differs:
            cases.append({"name": name, "status": "differs", "detail": f"{len(differs)} differ: " + "; ".join(differs[:5])})
        else:
            cases.append({"name": name, "status": "identical", "detail": ""})
        if changed:
            detail = (f"compared with {changes['to']}'s values (crs_list_changes.json): GDAL's {changes['from']} has them "
                      f"before {authority} added or changed them, e.g. {authority}:{changed[0]}")
            cases.append({"name": f"{authority}: {len(changed)} CRSs added or changed after {changes['from']}",
                          "status": "not compared", "detail": detail})
        codes = {e["code"] for e in ours}
        left_out = Counter(e["kind"] for c, e in reference.items() if c not in codes and c not in removed and c not in added_codes)
        if removed:
            cases.append({"name": f"{authority}: {len(removed)} CRSs removed after {changes['from']}", "status": "not compared",
                          "detail": f"GDAL's {changes['from']} lists them, {changes['to']} does not (crs_list_changes.json)"})
        # The CRSs the registry added that terrio does not list are counted with the others it does
        # not resolve, as against the newer PROJ (the pinned count catches a missing one).
        left_out.update(e["kind"] for c, e in (changes["added"][authority] if changes else {}).items() if int(c) not in codes)
        if left_out:
            detail = ", ".join(f"{n} {k}" for k, n in sorted(left_out.items()))
            cases.append({"name": f"{authority}: {sum(left_out.values())} CRSs terrio does not resolve", "status": "expected rejection", "detail": detail})
    # The issue's acceptance: EPSG:3577 by a word of its name and by its code.
    for text in ("Albers", "3577"):
        found = [e for e in terrio_list(args.terrio, text) if (e["authority"], e["code"]) == ("EPSG", 3577)]
        expected = gdal_entries("EPSG")[3577]
        fields = [k for k in expected if found[0].get(k, MISSING) != expected[k]] if found else []
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
