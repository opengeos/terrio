#!/usr/bin/env python3
"""Compare terrio's transformations to the CRSs of its ESRI table with PROJ's.

Transforms four points of each CRS's area of use from WGS 84 with terrio's Python module and
with pyproj (no datum shift grid on either side), and reports, per PROJ projection, how many
CRSs agree within a millimetre, how many differ (with the largest difference) and how many terrio
refuses: the codes whose projection terrio does not compute as PROJ does
(``ESRI_UNLIKE_PROJ`` in ``crates/terrio-crs/src/lib.rs``) or not at all.

Usage:
    python scripts/compare_esri_with_proj.py

terrio follows PROJ 9.8, whose equidistant cylindrical is ellipsoidal: with an older PROJ, the
ESRI CRSs in that projection differ.

Needs pyproj, terrio's Python module built from this tree (``maturin develop``) and the
``zstd`` command. Set ``TERRIO_GRID_CACHE`` to an empty directory when terrio's download cache
holds grids PROJ does not find.
"""

import collections
import os
import subprocess
import warnings

import pyproj
import terrio

warnings.simplefilter("ignore")
TABLE = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio-crs", "data", "esri.tsv.zst")


def points(crs):
    """Pick points in a CRS's area of use.

    Args:
        crs: The pyproj CRS.

    Returns:
        Four ``(lon, lat)`` points, latitudes within 80°.
    """
    aou = crs.area_of_use
    w, s, e, n = (aou.west, aou.south, aou.east, aou.north) if aou else (-180.0, -90.0, 180.0, 90.0)
    if w > e:
        e += 360.0
    s, n = max(s, -80.0), min(n, 80.0)
    pts = [(w + f * (e - w), s + g * (n - s)) for f, g in ((0.5, 0.5), (0.2, 0.3), (0.8, 0.75), (0.3, 0.9))]
    return [((x + 180.0) % 360.0 - 180.0, y) for x, y in pts]


def main():
    """Compare every CRS of the table and print the summary."""
    text = subprocess.run(["zstd", "-dc", TABLE], capture_output=True, text=True, check=True).stdout
    rows = [line.split("\t") for line in text.splitlines() if line.startswith("C\t")]
    stats = collections.defaultdict(collections.Counter)
    details = collections.defaultdict(list)
    for row in rows:
        code, method = int(row[1]), row[9].split()[0]
        try:
            crs = pyproj.CRS.from_authority("ESRI", code)
            t = pyproj.Transformer.from_crs("EPSG:4326", crs, always_xy=True)
        except (pyproj.exceptions.CRSError, pyproj.exceptions.ProjError):
            # A PROJ other than the one that generated the table may not know the code.
            stats["(unknown to this PROJ)"]["skipped"] += 1
            continue
        pts = [(p, t.transform(*p)) for p in points(crs)]
        pts = [(p, q) for p, q in pts if abs(q[0]) < 1e20 and abs(q[1]) < 1e20]
        if not pts:
            continue
        try:
            xs, ys = terrio.transform_points([p[0] for p, _ in pts], [p[1] for p, _ in pts], "EPSG:4326", f"ESRI:{code}")
        except Exception:
            stats[method]["refused"] += 1
            details[method].append(f"{code} refused")
            continue
        scale = 111000.0 if crs.is_geographic else 1.0
        d = max(max(abs(x - q[0]), abs(y - q[1])) * scale for x, y, (_, q) in zip(xs, ys, pts))
        if d < 1e-3:
            stats[method]["agree"] += 1
        else:
            stats[method]["differ"] += 1
            details[method].append(f"{code} differs by {d:.3f} m")
    for method in sorted(stats):
        print(method, dict(stats[method]), "; ".join(details[method]))


if __name__ == "__main__":
    main()
