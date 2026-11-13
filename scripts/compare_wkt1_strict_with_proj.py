#!/usr/bin/env python3
"""Read terrio's strict WKT 1 (`terrio crs CRS --wkt1-strict`) with live PROJ, as `.prj` readers do.

Every custom definition of ``crates/terrio-crs/tests/data/wkt1_strict_export.tsv`` (PROJ strings,
WKT, compound references and IAU_2015 CRSs; the registry codes are compared with PROJ's WKT 1
character for character by ``tests/wkt1_strict_export.rs``) is written by terrio. PROJ (which
GDAL reads WKT with) reads that WKT 1, and points around the CRS's origin must transform from the
original definition's geographic CRS as to the original, within 1 mm (1e-8 degrees). A definition
terrio writes no WKT 1 for is an expected rejection where PROJ's WKT 1 has none or needs an
extension, else a known difference (terrio stricter). Halley's and Eros's projected CRSs are not
compared: PROJ's own WKT 1 of them, read without their IAU code, gives another CRS too.

Usage: python scripts/compare_wkt1_strict_with_proj.py --terrio target/release/terrio [--json]
(needs pyproj).
"""

import argparse
import json
import math
import re
import subprocess
import warnings
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

DATA = Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/wkt1_strict_export.tsv"
# Their WKT 1 reads back as another CRS in PROJ too, without its IAU code.
OTHERWISE = ("IAU_2015:1000036", "IAU_2015:2000433")


def rows():
    """The custom rows of the frozen data: kind, definition, PROJ's WKT 1 (or None)."""
    for line in DATA.read_text().splitlines():
        if line.startswith("#"):
            continue
        kind, definition, wkt = line.split("\t", 2)
        if kind in ("proj", "wkt", "ref"):
            yield kind, definition, None if wkt == "NONE" else wkt


def strict(terrio, definition):
    """terrio's strict WKT 1 of a definition, None when it writes none. Any other failure (a
    crash, an unread definition) raises."""
    try:
        run = subprocess.run([terrio, "crs", definition, "--wkt1-strict"], capture_output=True, text=True, timeout=60)
    except subprocess.TimeoutExpired as err:
        raise RuntimeError("terrio did not finish within 60 s") from err
    if run.returncode == 0:
        return run.stdout.strip()
    if "has no WKT 1 without the PROJ4 extension" in run.stderr:
        return None
    raise RuntimeError(f"terrio failed (exit {run.returncode}): {run.stderr.strip()[-500:]}")


def needs_extension(wkt):
    """Whether PROJ's WKT 1 has an extension that no EPSG or ESRI root code identifies."""
    root = re.search(r',AUTHORITY\["(EPSG|ESRI)","\d+"\]\]$', wkt)
    return "EXTENSION[" in wkt and not root


def origin(crs):
    """The longitude and latitude of a projected CRS's origin, in degrees, from its conversion's
    parameters (a UTM zone's central meridian among them); (0, 0) for another CRS."""
    op = crs.coordinate_operation if crs.is_projected else None
    lon = lat = None
    for p in op.params if op else []:
        name = p.name.lower()
        if p.unit_category != "angular":
            continue
        degrees = math.degrees(p.value * p.unit_conversion_factor)
        if lon is None and "longitude" in name and ("origin" in name or "centre" in name or "center" in name):
            lon = degrees
        if lat is None and "latitude" in name and ("origin" in name or "centre" in name or "center" in name):
            lat = degrees
    return lon or 0.0, max(-80.0, min(80.0, lat or 0.0))


def points(crs):
    """Longitudes and latitudes around a CRS's origin, in its geographic CRS."""
    lon0, lat0 = origin(crs)
    return [(lon0 + dx, lat0 + dy) for dx in (-1.0, 0.5) for dy in (-1.0, 0.5)]


def compare(definition, ours):
    """The largest difference (metres or degrees) between the original and terrio's WKT 1, on
    points both transform; None when no point is compared."""
    original = CRS(definition)
    written = CRS(ours)
    base = original.geodetic_crs
    if base is None:
        return None
    try:
        to_original = Transformer.from_crs(base, original, always_xy=True)
    except pyproj.exceptions.ProjError:
        return None
    # A failure here is terrio's WKT 1's.
    to_written = Transformer.from_crs(base, written, always_xy=True)
    worst = None
    for lon, lat in points(original):
        a = to_original.transform(lon, lat)
        b = to_written.transform(lon, lat)
        if not all(math.isfinite(v) for v in (*a, *b)):
            continue
        d = max(abs(a[0] - b[0]), abs(a[1] - b[1]))
        worst = d if worst is None else max(worst, d)
    return worst


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--terrio", required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    warnings.simplefilter("ignore")
    pyproj.network.set_network_enabled(False)
    cases = []
    for kind, definition, proj_wkt in rows():
        name = definition if len(definition) <= 80 else definition[:77] + "..."
        group = {"proj": "PROJ strings", "wkt": "WKT", "ref": "references and IAU_2015"}[kind]
        try:
            ours = strict(args.terrio, definition)
        except RuntimeError as err:
            cases.append({"group": group, "name": name, "status": "differs", "detail": str(err)})
            continue
        if ours is None:
            if proj_wkt is None or needs_extension(proj_wkt):
                status, detail = "expected rejection", "no WKT 1 without extension, as PROJ"
            else:
                status, detail = "not compared", "terrio writes none, PROJ does (a known difference)"
        elif definition.startswith(OTHERWISE):
            status, detail = "not compared", "PROJ's own WKT 1 reads back as another CRS too"
        else:
            try:
                worst = compare(definition, ours)
            except (pyproj.exceptions.CRSError, pyproj.exceptions.ProjError) as err:
                status, detail = "differs", f"PROJ cannot read or transform to terrio's WKT 1: {err}"
            else:
                angular = CRS(definition).is_geographic
                tolerance = 1e-8 if angular else 1e-3
                if worst is None:
                    status, detail = "not compared", "no point the original CRS transforms"
                elif worst == 0:
                    status, detail = "identical", "read by PROJ, the same coordinates"
                elif worst <= tolerance:
                    status, detail = "within tolerance", f"largest difference {worst:.3g}"
                else:
                    status, detail = "differs", f"largest difference {worst:.6g}; terrio wrote {ours}"
        cases.append({"group": group, "name": name, "status": status, "detail": detail})
    if args.json:
        print(json.dumps(cases))
    else:
        for c in cases:
            print(f"{c['status']:18} {c['group']:24} {c['name']}  {c['detail']}")
    return 1 if any(c["status"] == "differs" for c in cases) else 0


if __name__ == "__main__":
    raise SystemExit(main())
