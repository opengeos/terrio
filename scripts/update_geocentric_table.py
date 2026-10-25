#!/usr/bin/env python3
"""Append/refresh geocentric definitions without regenerating unrelated EPSG operations.

Uses gen_epsg_table.geocentric_rows, also called by the full table generator.
Requires matching GDAL/pyproj PROJ databases, and zstd.
"""
import subprocess
from pathlib import Path

import pyproj
from osgeo import gdal
from gen_epsg_table import geocentric_rows


def main():
    path = Path(__file__).resolve().parents[1] / "crates/terrio-crs/data/epsg.tsv.zst"
    text = subprocess.check_output(["zstd", "-d", "-q", "-c", str(path)]).decode()
    kept = [line for line in text.splitlines() if not (line.startswith("C\t") and "+proj=geocent" in line) and not line.startswith("# Geocentric endpoint")]
    base = {int(row[1]): row for line in kept if (row := line.split("\t"))[0] == "C"}
    rows = ["\t".join(map(str, row)) for row in geocentric_rows(base)]
    note = f"# Geocentric endpoint definitions: PROJ {pyproj.proj_version_str}, GDAL {gdal.VersionInfo('RELEASE_NAME')}; scripts/update_geocentric_table.py."
    content = "\n".join(kept + [note] + rows) + "\n"
    packed = subprocess.check_output(["zstd", "-19", "-q", "-c"], input=content.encode())
    path.write_bytes(packed)
    print(f"{len(rows)} geocentric definitions; compressed table {len(packed)} bytes")


if __name__ == "__main__":
    main()
