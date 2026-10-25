"""Compare terrio's transformations between vertical CRSs with PROJ's.

For each pair of ``crates/terrio-crs/data/epsg_vertical.tsv.zst`` and a point inside each of its
transformations' areas, transforms a height between the compound CRSs ``EPSG:4326+source`` and
``EPSG:4326+target`` with ``terrio crs`` and with pyproj (network access on, for the grids), and
reports the pairs whose heights differ by more than a micrometre.

Usage: python scripts/compare_vertical_with_proj.py [path/to/terrio]
"""

import os
import subprocess
import sys

import pyproj
from pyproj import Transformer

TABLE = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio-crs", "data", "epsg_vertical.tsv.zst")
TOLERANCE = 1e-6


def table():
    """Read the table.

    Returns:
        ``(ops, pairs)``: transformation bounds by id, and ``(source, target, ids)`` triples.
    """
    text = subprocess.run(["zstd", "-dc", TABLE], capture_output=True, text=True, check=True).stdout
    ops, pairs = {}, []
    for line in text.splitlines():
        f = line.split("\t")
        if f[0] == "O":
            ops[int(f[1])] = [float(v) for v in f[2].split(",")]
        elif f[0] == "W":
            ids = [int(i) for field in f[3:5] for i in field.split(",") if i]
            pairs.append((int(f[1]), int(f[2]), ids))
    return ops, pairs


def inside(b):
    """A point inside bounds, away from their edges.

    Args:
        b: ``(west, south, east, north)`` in degrees.

    Returns:
        ``(lon, lat)``.
    """
    west, south, east, north = b
    if east < west:
        east += 360.0
    lon = west + (east - west) * 0.37
    lon = lon - 360.0 if lon > 180.0 else lon
    return lon, south + (north - south) * 0.41


def main():
    """Run the comparison."""
    terrio = sys.argv[1] if len(sys.argv) > 1 else "terrio"
    pyproj.network.set_network_enabled(True)
    ops, pairs = table()
    checked, differ, failed = 0, [], []
    for a, b, ids in pairs:
        src, dst = f"EPSG:4326+{a}", f"EPSG:4326+{b}"
        try:
            t = Transformer.from_crs(src, dst, always_xy=True)
        except pyproj.exceptions.ProjError:
            continue
        points = sorted({inside(ops[i]) for i in ids})
        expected = [t.transform(x, y, 100.0) for x, y in points]
        lines = "".join(f"{x!r} {y!r} 100\n" for x, y in points)
        env = dict(os.environ, TERRIO_NETWORK="ON")
        out = subprocess.run([terrio, "crs", src, "--to", dst], input=lines, capture_output=True, text=True, env=env).stdout.splitlines()
        for (x, y), e, line in zip(points, expected, out + [""] * len(points)):
            checked += 1
            got = line.split()
            if len(got) != 3 or e[2] == float("inf"):
                failed.append((src, dst, x, y, line, e[2]))
                continue
            if abs(float(got[2]) - e[2]) > TOLERANCE:
                differ.append((src, dst, x, y, float(got[2]), e[2]))
    print(f"{checked} points, {len(pairs)} pairs: {len(differ)} differ, {len(failed)} failed")
    for d in differ[:40]:
        print("differs", *d)
    for f in failed[:20]:
        print("failed", *f)


if __name__ == "__main__":
    main()
