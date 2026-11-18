# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Freeze what EPSG and ESRI changed in GDAL's CRS list between PROJ 9.8.1 and PROJ 9.9.0.

terrio's catalog comes from PROJ 9.9.0 (EPSG v13.102), while CI's GDAL reference is GDAL 3.12.2 on
PROJ 9.8.1 (opengeos/terrio#1076). ``compare_crs_list_with_gdal.py`` compares ``terrio list-crs``
with GDAL's ``osr.GetCRSInfoListFromDatabase`` field by field; on PROJ 9.8.1 a field differs where
the registry changed it (a renamed or deprecated CRS, a new area of use). This script records each
such field with its PROJ 9.8.1 and 9.9.0 values, the CRSs PROJ 9.9.0 added with their entries,
and those it removed, so that the comparison can name those changes and still check terrio's
values against PROJ 9.9.0's.

Writes ``crates/terrio-crs/tests/data/crs_list_changes.json``.

Usage, with the dumps written by each environment::

    # GDAL on PROJ 9.8.1 (CI's reference: gdal=3.12.2 proj=9.8.1 on conda-forge)
    python scripts/gen_crs_list_changes.py --dump old.json
    # GDAL 3.13.3 on PROJ 9.9.0 (the PROJ terrio's tables come from)
    python scripts/gen_crs_list_changes.py --dump new.json
    python scripts/gen_crs_list_changes.py old.json new.json

The dumps are made with PROJ's network off; GDAL's CRS list does not depend on grids.
"""

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "crates" / "terrio-crs" / "tests" / "data" / "crs_list_changes.json"
AUTHORITIES = ("EPSG", "ESRI", "IAU_2015")


def dump(path):
    """Write GDAL's CRS list, as the comparison reads it, with its PROJ version."""
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from compare_crs_list_with_gdal import gdal_entries
    from osgeo import osr

    osr.SetPROJEnableNetwork(False)
    proj = f"{osr.GetPROJVersionMajor()}.{osr.GetPROJVersionMinor()}.{osr.GetPROJVersionMicro()}"
    entries = {a: {str(code): e for code, e in gdal_entries(a).items()} for a in AUTHORITIES}
    Path(path).write_text(json.dumps({"proj": proj, "entries": entries}), encoding="utf-8")
    print(f"PROJ {proj}: " + ", ".join(f"{len(entries[a])} {a}" for a in AUTHORITIES))


def combine(old_path, new_path):
    """Write the fields that differ between the two dumps, and the codes added and removed."""
    old, new = (json.loads(Path(p).read_text(encoding="utf-8")) for p in (old_path, new_path))
    if (old["proj"], new["proj"]) != ("9.8.1", "9.9.0"):
        raise ValueError(f"dumps of PROJ {old['proj']} and {new['proj']}, not PROJ 9.8.1 and 9.9.0")
    changes, added, removed = {}, {}, {}
    for authority in AUTHORITIES:
        a, b = old["entries"][authority], new["entries"][authority]
        changes[authority] = {
            code: {k: [a[code][k], b[code][k]] for k in b[code] if a[code][k] != b[code][k]}
            for code in sorted(set(a) & set(b), key=int)
            if a[code] != b[code]
        }
        # An added CRS with its PROJ 9.9.0 entry, which terrio's is compared with.
        added[authority] = {code: b[code] for code in sorted(set(b) - set(a), key=int)}
        removed[authority] = sorted(int(c) for c in set(a) - set(b))
    # One CRS a line, so that a regeneration's diff reads code by code.
    lines = [f'{{"from": "PROJ {old["proj"]}", "to": "PROJ {new["proj"]}",', f' "removed": {json.dumps(removed)},']
    for key, table in (("added", added), ("changes", changes)):
        lines.append(f' "{key}": {{')
        for i, authority in enumerate(AUTHORITIES):
            rows = [f"  {json.dumps(code)}: {json.dumps(value, ensure_ascii=False)}" for code, value in table[authority].items()]
            lines.append(f" {json.dumps(authority)}: {{")
            lines.append(",\n".join(rows))
            lines.append(" }" + ("," if i < len(AUTHORITIES) - 1 else ""))
        lines.append(" }" + ("," if key == "added" else ""))
    lines.append("}")
    OUT.write_text("\n".join(line for line in lines if line) + "\n", encoding="utf-8")
    json.loads(OUT.read_text(encoding="utf-8"))
    print(", ".join(f"{a}: {len(changes[a])} changed, {len(added[a])} added, {len(removed[a])} removed" for a in AUTHORITIES))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dump", metavar="PATH", help="write this environment's GDAL CRS list")
    parser.add_argument("dumps", nargs="*", metavar="DUMP", help="the PROJ 9.8.1 and PROJ 9.9.0 dumps")
    args = parser.parse_args()
    if args.dump:
        dump(args.dump)
    elif len(args.dumps) == 2:
        combine(*args.dumps)
    else:
        parser.error("give --dump PATH, or the two dumps")


if __name__ == "__main__":
    main()
