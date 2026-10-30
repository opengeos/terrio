#!/usr/bin/env python3
"""GeoPackage specification checks against GDAL on real-data derivatives."""

import argparse
from contextlib import closing
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile

from osgeo import gdal

gdal.UseExceptions()


def sql(path, statement):
    with closing(sqlite3.connect(path)) as database:
        database.executescript(statement)
        database.commit()


def fixtures(data, root):
    base = root / "vector.gpkg"
    output = gdal.VectorTranslate(
        str(base), str(data / "countries.gpkg"), format="GPKG", layerName="countries"
    )
    output = None
    yield "Natural Earth vector", base, True
    empty = root / "empty.gpkg"
    output = gdal.VectorTranslate(str(empty), str(base), format="GPKG", where="1=0")
    output = None
    yield "empty feature table", empty, True
    raster = root / "raster.gpkg"
    output = gdal.Translate(
        str(raster),
        str(data / "dem.tif"),
        format="GPKG",
        srcWin=[0, 0, 64, 64],
        outputType=gdal.GDT_Byte,
        noData=255,
        scaleParams=[[0, 4000, 0, 255]],
        creationOptions=["TILE_FORMAT=PNG"],
    )
    output = None
    yield "USGS raster tiles", raster, True
    changes = [
        ("wrong application ID", "PRAGMA application_id=0"),
        ("wrong version", "PRAGMA user_version=10000"),
        ("missing spatial reference table", "DROP TABLE gpkg_spatial_ref_sys"),
        ("missing contents table", "DROP TABLE gpkg_contents"),
        ("missing undefined Cartesian SRS", "DELETE FROM gpkg_spatial_ref_sys WHERE srs_id=-1"),
        ("missing undefined geographic SRS", "DELETE FROM gpkg_spatial_ref_sys WHERE srs_id=0"),
        ("missing WGS84 SRS", "DELETE FROM gpkg_spatial_ref_sys WHERE srs_id=4326"),
        ("unknown contents type", "UPDATE gpkg_contents SET data_type='invalid'"),
        ("dangling contents table", "UPDATE gpkg_contents SET table_name='missing'"),
        ("invalid change timestamp", "UPDATE gpkg_contents SET last_change='yesterday'"),
        ("dangling contents SRS", "UPDATE gpkg_contents SET srs_id=999999"),
        ("missing geometry registration", "DROP TABLE gpkg_geometry_columns"),
        ("missing geometry column", "UPDATE gpkg_geometry_columns SET column_name='missing'"),
        ("invalid geometry type", "UPDATE gpkg_geometry_columns SET geometry_type_name='INVALID'"),
        ("invalid Z policy", "UPDATE gpkg_geometry_columns SET z=3"),
        ("invalid M policy", "UPDATE gpkg_geometry_columns SET m=3"),
        ("dangling geometry SRS", "UPDATE gpkg_geometry_columns SET srs_id=999999"),
    ]
    for number, (name, statement) in enumerate(changes):
        path = root / f"invalid-vector-{number}.gpkg"
        shutil.copyfile(base, path)
        sql(path, statement)
        yield name, path, False
    for number, (name, statement) in enumerate(
        [
            ("missing tile matrix set", "DROP TABLE gpkg_tile_matrix_set"),
            ("missing tile matrix", "DROP TABLE gpkg_tile_matrix"),
            ("dangling tile matrix set SRS", "UPDATE gpkg_tile_matrix_set SET srs_id=999999"),
            ("reversed tile extent", "UPDATE gpkg_tile_matrix_set SET min_x=max_x+1"),
            ("missing tile zoom registration", "DELETE FROM gpkg_tile_matrix"),
        ]
    ):
        path = root / f"invalid-raster-{number}.gpkg"
        shutil.copyfile(raster, path)
        sql(path, statement)
        yield name, path, False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--reference-only", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-gpkg-validation-") as temporary:
        for name, path, valid in fixtures(args.data, Path(temporary)):
            for full in [False, True]:
                command = ["gdal", "driver", "gpkg", "validate", str(path)]
                if full:
                    command.append("--full-check")
                reference = subprocess.run(command, capture_output=True, text=True)
                detail = reference.stdout + reference.stderr
                status = "identical" if (reference.returncode == 0) == valid else "differs"
                if not args.reference_only:
                    command = [args.terrio, "gpkg-validate", str(path), "--json"]
                    if full:
                        command.append("--full-check")
                    ours = subprocess.run(command, capture_output=True, text=True)
                    try:
                        report = json.loads(ours.stdout)
                        assert report["valid"] == valid
                        assert report["complete"]
                        assert (ours.returncode == 0) == valid
                    except (ValueError, KeyError, AssertionError):
                        status = "differs"
                        detail += "\nTerrio: " + ours.stdout + ours.stderr
                results.append(
                    dict(
                        name=name + (" (full)" if full else ""),
                        status=status,
                        expected_valid=valid,
                        reference=detail.strip(),
                    )
                )
    if args.json:
        print(json.dumps(results))
    else:
        for result in results:
            print(result["status"], result["name"], result["reference"])
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
