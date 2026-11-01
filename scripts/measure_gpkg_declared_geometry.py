"""Measure bounded GeoPackage replacement in an isolated Linux process.

Run with an installed Terrio wheel:
    python scripts/measure_gpkg_declared_geometry.py --payload-mib 8
"""

import argparse
import json
import sqlite3
import tempfile
from contextlib import closing
from pathlib import Path

import terrio


def read_chars():
    for line in Path("/proc/self/io").read_text().splitlines():
        if line.startswith("rchar:"):
            return int(line.split()[1])
    raise RuntimeError("/proc/self/io has no rchar counter")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--payload-mib", type=int, default=8)
    args = parser.parse_args()
    if args.payload_mib < 1 or not Path("/proc/self/io").exists():
        parser.error("requires Linux /proc and a positive payload size")
    payload = "x" * (args.payload_mib * 1024 * 1024)
    unknown = terrio.Layer.from_geojson(
        {"type": "Point", "coordinates": [1, 2]}, name="unknown", crs="EPSG:4326"
    )
    kept = terrio.Layer.from_geojson(
        {
            "type": "Feature",
            "properties": {"payload": payload},
            "geometry": {"type": "Point", "coordinates": [3, 4]},
        },
        name="kept",
        crs="EPSG:4326",
    )
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "layers.gpkg"
        terrio.write_vector([unknown, kept], str(path))
        before_stat = path.stat()
        before = read_chars()
        terrio.write_vector(
            unknown, str(path), overwrite=True, preserve_declared_geometry_type=True
        )
        delta = read_chars() - before
        after_stat = path.stat()
        same_inode = (before_stat.st_dev, before_stat.st_ino) == (
            after_stat.st_dev,
            after_stat.st_ino,
        )
        result = {
            "file_bytes": before_stat.st_size,
            "payload_bytes": len(payload),
            "process_read_chars": delta,
            "read_bound": before_stat.st_size // 2,
            "inode_preserved": same_inode,
        }
        print(json.dumps(result, indent=2))
        assert same_inode, "replacement rewrote the file"
        assert 0 <= delta < result["read_bound"], (
            "replacement read too much of the file"
        )
        with closing(sqlite3.connect(path)) as db:
            assert db.execute("SELECT payload FROM kept").fetchone() == (payload,)
            assert db.execute(
                "SELECT geometry_type_name FROM gpkg_geometry_columns WHERE table_name='unknown'"
            ).fetchone() == ("GEOMETRY",)


if __name__ == "__main__":
    main()
