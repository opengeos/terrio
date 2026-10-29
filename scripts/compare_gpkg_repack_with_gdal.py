#!/usr/bin/env python3
"""Compare pure Rust GeoPackage repacking with GDAL VACUUM on real-data derivatives."""

import argparse
from contextlib import closing
import json
import shutil
import sqlite3
import sys
import subprocess
import tempfile
import traceback
from pathlib import Path

from osgeo import gdal

gdal.UseExceptions()


def quote(name):
    return '"' + name.replace('"', '""') + '"'


def storage_snapshot(path):
    with closing(sqlite3.connect(path)) as database:
        schema = database.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name"
        ).fetchall()
        tables = {}
        for kind, name, _, _ in schema:
            if kind != "table":
                continue
            try:
                rows = database.execute("SELECT rowid,* FROM " + quote(name)).fetchall()
            except sqlite3.OperationalError:
                rows = database.execute("SELECT * FROM " + quote(name)).fetchall()
            tables[name] = sorted(rows, key=repr)
        pragmas = {
            name: database.execute("PRAGMA " + name).fetchone()[0]
            for name in ["page_size", "application_id", "user_version", "encoding", "auto_vacuum"]
        }
        assert database.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
        return schema, tables, pragmas


def vector_snapshot(path):
    dataset = gdal.OpenEx(str(path), gdal.OF_VECTOR)
    result = []
    with closing(sqlite3.connect(path)) as database:
        names = [
            row[0]
            for row in database.execute(
                "SELECT table_name FROM gpkg_contents WHERE data_type='features' ORDER BY table_name"
            )
        ]
    for name in names:
        layer = dataset.GetLayerByName(name)
        definition = layer.GetLayerDefn()
        crs = layer.GetSpatialRef()
        result.append(
            dict(
                name=layer.GetName(),
                count=layer.GetFeatureCount(),
                crs=crs.ExportToWkt() if crs else None,
                geometry_type=layer.GetGeomType(),
                fields=[
                    (
                        definition.GetFieldDefn(i).GetName(),
                        definition.GetFieldDefn(i).GetType(),
                        definition.GetFieldDefn(i).GetSubType(),
                    )
                    for i in range(definition.GetFieldCount())
                ],
                features=[feature.ExportToJson() for feature in layer],
            )
        )
    return result


def raster_snapshot(path):
    dataset = gdal.Open("GPKG:" + str(path) + ":elevation")
    return dict(
        size=(dataset.RasterXSize, dataset.RasterYSize),
        transform=dataset.GetGeoTransform(),
        crs=dataset.GetProjection(),
        metadata=dataset.GetMetadata(),
        bands=[
            (
                dataset.GetRasterBand(i).DataType,
                dataset.GetRasterBand(i).GetNoDataValue(),
                dataset.GetRasterBand(i).ReadRaster(),
                dataset.GetRasterBand(i).GetMaskBand().ReadRaster(),
                dataset.GetRasterBand(i).GetMetadata(),
            )
            for i in range(1, dataset.RasterCount + 1)
        ],
    )


def add_tables(path):
    with closing(sqlite3.connect(path)) as database, database:
        database.execute(
            "CREATE TABLE notes(id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT COLLATE NOCASE UNIQUE, payload BLOB)"
        )
        database.executemany(
            "INSERT INTO notes(label,payload) VALUES (?,?)",
            [(f"item-{i}", bytes([i % 251]) * 500) for i in range(5000)],
        )
        database.execute(
            "CREATE INDEX expression_notes ON notes(lower(label),id DESC) WHERE id%3=0"
        )
        database.execute(
            "CREATE TABLE compound(k TEXT COLLATE NOCASE,seq INTEGER,payload BLOB,PRIMARY KEY(k,seq)) WITHOUT ROWID"
        )
        database.executemany(
            "INSERT INTO compound VALUES (?,?,?)",
            [(f"key-{i}", i, b"x" * 1000) for i in range(1000)],
        )
        database.execute(
            "CREATE INDEX compound_expression ON compound(length(payload) DESC,lower(k))"
        )
        database.execute("CREATE VIEW named_notes AS SELECT id,label FROM notes WHERE id%2=1")
        database.execute("CREATE TABLE audit(action TEXT)")
        database.execute(
            "CREATE TRIGGER notes_updated AFTER UPDATE ON notes BEGIN INSERT INTO audit VALUES ('updated'); END"
        )
        database.execute("DELETE FROM notes WHERE id%3<>0")
        database.execute("DELETE FROM compound WHERE seq%3<>0")
        database.execute("DELETE FROM countries WHERE fid%2=0")
        database.execute("INSERT INTO notes(id,label,payload) VALUES(-7,'negative',X'0102')")
        database.execute(
            "INSERT INTO notes(id,label,payload) VALUES(9223372036854775807,'maximum',X'0304')"
        )


def change_encoding(source_path, target_path, encoding):
    # Recreate a real-data derivative through SQLite. Virtual tables recreate their own
    # shadow tables; reinsert their logical rows instead of copying module-owned node blobs.
    with closing(sqlite3.connect(source_path)) as source, closing(sqlite3.connect(target_path)) as target, source, target:
        target.execute("PRAGMA encoding='" + encoding + "'")
        schema = source.execute("SELECT type,name,sql FROM sqlite_schema ORDER BY rowid").fetchall()
        types = {row[1]: row[2] for row in source.execute("PRAGMA table_list")}
        tables = [
            (name, sql)
            for kind, name, sql in schema
            if kind == "table" and name != "sqlite_sequence" and types.get(name) != "shadow"
        ]
        for _, sql in tables:
            target.execute(sql)
        sequence = [(name, sql) for _, name, sql in schema if name == "sqlite_sequence"]
        for name, _ in tables + sequence:
            rows = source.execute("SELECT * FROM " + quote(name)).fetchall()
            if name == "sqlite_sequence":
                target.execute("DELETE FROM sqlite_sequence")
            if rows:
                target.executemany(
                    "INSERT INTO "
                    + quote(name)
                    + " VALUES ("
                    + ",".join("?" for _ in rows[0])
                    + ")",
                    rows,
                )
        assert source.execute("SELECT * FROM sqlite_sequence ORDER BY name").fetchall() == target.execute("SELECT * FROM sqlite_sequence ORDER BY name").fetchall()
        for kind, _, sql in schema:
            if kind in ("index", "trigger", "view") and sql:
                target.execute(sql)
        for pragma in ("application_id", "user_version"):
            target.execute(
                "PRAGMA " + pragma + "=" + str(source.execute("PRAGMA " + pragma).fetchone()[0])
            )
        target.execute("DELETE FROM notes WHERE id%7<>0")
        target.execute("DELETE FROM compound WHERE seq%7<>0")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []

    def repack(path, success=True):
        process = subprocess.run(
            [args.terrio, "gpkg-repack", str(path), "--json"], capture_output=True, text=True
        )
        assert (process.returncode == 0) == success, process.stderr
        return json.loads(process.stdout) if success else process.stderr

    with tempfile.TemporaryDirectory(prefix="terrio-gpkg-repack-") as temporary:
        root = Path(temporary)
        base = root / "base.gpkg"
        output = gdal.VectorTranslate(
            str(base),
            str(args.data / "countries.gpkg"),
            format="GPKG",
            layerName="countries",
            geometryType="PROMOTE_TO_MULTI",
        )
        output = None
        add_tables(base)
        cases = [("fragmented vector and arbitrary SQL objects", base, False)]
        for name, mode, size in [
            ("auto-vacuum full", 1, 4096),
            ("incremental-vacuum 512-byte pages", 2, 512),
            ("65536-byte pages", 0, 65536),
        ]:
            path = root / (name.replace(" ", "-") + ".gpkg")
            shutil.copyfile(base, path)
            with closing(sqlite3.connect(path)) as database, database:
                database.execute("PRAGMA auto_vacuum=" + str(mode))
                database.execute("PRAGMA page_size=" + str(size))
                database.execute("VACUUM")
                database.execute("DELETE FROM notes WHERE id%7<>0")
                database.execute("DELETE FROM compound WHERE seq%7<>0")
            cases.append((name, path, False))
        for encoding in ("UTF-16le", "UTF-16be"):
            path = root / (encoding + ".gpkg")
            change_encoding(base, path, encoding)
            cases.append((encoding + " encoded records", path, False))
        mixed_path = root / "mixed.gpkg"
        shutil.copyfile(base, mixed_path)
        raster = gdal.Translate(
            str(mixed_path),
            str(args.data / "dem.tif"),
            format="GPKG",
            srcWin=[0, 0, 64, 64],
            outputType=gdal.GDT_Float32,
            noData=-9999,
            creationOptions=["APPEND_SUBDATASET=YES", "RASTER_TABLE=elevation", "TILE_FORMAT=TIFF"],
        )
        raster = None
        cases.append(("mixed vector and gridded coverage", mixed_path, True))
        for name, path, mixed in cases:
            try:
                ours = root / (path.stem + "-terrio.gpkg")
                reference = root / (path.stem + "-gdal.gpkg")
                shutil.copyfile(path, ours)
                shutil.copyfile(path, reference)
                expected = storage_snapshot(path)
                vectors = vector_snapshot(path)
                pixels = raster_snapshot(path) if mixed else None
                report = repack(ours)
                baseline = subprocess.run(
                    ["gdal", "driver", "gpkg", "repack", str(reference)],
                    capture_output=True,
                    text=True,
                )
                assert baseline.returncode == 0, baseline.stderr
                assert storage_snapshot(ours) == storage_snapshot(reference) == expected
                assert vector_snapshot(ours) == vector_snapshot(reference) == vectors
                if mixed:
                    assert raster_snapshot(ours) == raster_snapshot(reference) == pixels
                assert report["bytes_after"] == ours.stat().st_size < report["bytes_before"]
                for target in (ours, reference):
                    with closing(sqlite3.connect(target)) as database, database:
                        assert database.execute("PRAGMA freelist_count").fetchone() == (0,)
                        database.execute("UPDATE notes SET label='changed' WHERE id=21")
                        assert database.execute("SELECT * FROM audit").fetchall() == [("updated",)]
                        database.execute(
                            "DELETE FROM countries WHERE fid=(SELECT MIN(fid) FROM countries)"
                        )
                        assert database.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
                assert storage_snapshot(ours) == storage_snapshot(reference)
                results.append(
                    dict(
                        name=name,
                        status="identical",
                        detail=f"all SQL objects/records/rowids, encoding, page size, auto-vacuum mode, GDAL vectors/CRS and raster pixels/masks exact; integrity and trigger/index maintenance pass; {report['bytes_before']} -> {report['bytes_after']} bytes (GDAL {reference.stat().st_size}); physical layouts may differ",
                    )
                )
            except Exception as error:
                traceback.print_exc()
                results.append(dict(name=name, status="differs", detail=str(error)))
        try:
            wal = root / "wal.gpkg"
            shutil.copyfile(base, wal)
            with closing(sqlite3.connect(wal)) as database, database:
                database.execute("PRAGMA journal_mode=WAL")
                database.execute("INSERT INTO audit VALUES ('wal-kept')")
                database.commit()
                before, sidecar = wal.read_bytes(), wal.with_name(wal.name + "-wal").read_bytes()
                repack(wal, success=False)
                assert wal.read_bytes() == before
                assert wal.with_name(wal.name + "-wal").read_bytes() == sidecar
            expected = storage_snapshot(wal)
            repack(wal)
            assert storage_snapshot(wal) == expected
            invalid = root / "invalid.gpkg"
            invalid.write_bytes(base.read_bytes()[:100])
            repack(invalid, success=False)
            assert invalid.read_bytes() == base.read_bytes()[:100]
            results.append(
                dict(
                    name="active WAL and invalid-input guards",
                    status="identical",
                    detail="live SQLite connection/WAL refused without modifying main or WAL bytes; committed WAL rows preserved after close/checkpoint and repack; truncated input unchanged",
                )
            )
        except Exception as error:
            traceback.print_exc()
            results.append(
                dict(
                    name="active WAL and invalid-input guards", status="differs", detail=str(error)
                )
            )
        try:
            orphan = root / "orphan-wal.gpkg"
            shutil.copyfile(base, orphan)
            subprocess.run(
                [
                    sys.executable,
                    "-c",
                    "import sqlite3,sys,os; db=sqlite3.connect(sys.argv[1]); db.execute('PRAGMA journal_mode=WAL'); db.execute(\"INSERT INTO audit VALUES ('crash-committed')\"); db.commit(); os._exit(0)",
                    str(orphan),
                ],
                check=True,
            )
            before = orphan.read_bytes()
            wal_path = orphan.with_name(orphan.name + "-wal")
            pending = wal_path.read_bytes()
            error = repack(orphan, success=False)
            assert "write-ahead log" in error, error
            assert orphan.read_bytes() == before and wal_path.read_bytes() == pending
            with closing(sqlite3.connect(orphan)) as database:
                assert database.execute("SELECT * FROM audit").fetchall() == [("crash-committed",)]
            expected = storage_snapshot(orphan)
            repack(orphan)
            assert storage_snapshot(orphan) == expected
            results.append(
                dict(
                    name="orphan WAL preservation and SQLite recovery",
                    status="identical",
                    detail="committed WAL from a stopped process refused without changing main/WAL bytes; SQLite recovers and checkpoints it, then repacking preserves every committed record",
                )
            )
        except Exception as error:
            traceback.print_exc()
            results.append(
                dict(
                    name="orphan WAL preservation and SQLite recovery",
                    status="differs",
                    detail=str(error),
                )
            )
        try:
            source = root / "points.geojson"
            destination = root / "points.gpkg"
            source.write_text(
                json.dumps(
                    {
                        "type": "FeatureCollection",
                        "features": [
                            {
                                "type": "Feature",
                                "properties": {"name": "kept"},
                                "geometry": {"type": "Point", "coordinates": [1, 2]},
                            }
                        ],
                    }
                )
            )
            subprocess.run(
                [args.terrio, "convert", str(source), str(destination)],
                check=True,
                capture_output=True,
                text=True,
            )
            subprocess.run(
                [
                    sys.executable,
                    "-c",
                    "from osgeo import gdal; import os,sys; gdal.UseExceptions(); "
                    "db=gdal.OpenEx(sys.argv[1],gdal.OF_VECTOR|gdal.OF_UPDATE); "
                    "result=db.ExecuteSQL('PRAGMA journal_mode=WAL'); "
                    "db.ReleaseResultSet(result) if result else None; "
                    "db.ExecuteSQL(\"UPDATE points SET name='wal-value'\"); os._exit(0)",
                    str(destination),
                ],
                check=True,
            )
            with closing(sqlite3.connect(destination.as_uri() + "?mode=ro", uri=True)) as db:
                assert db.execute("SELECT name FROM points").fetchall() == [("wal-value",)]
            before = destination.read_bytes()
            sidecar = destination.with_name(destination.name + "-wal")
            pending = sidecar.read_bytes()
            command = [args.terrio, "convert", str(source), str(destination), "--append"]
            refused = subprocess.run(command, capture_output=True, text=True)
            assert refused.returncode != 0 and "write-ahead log" in refused.stderr, refused.stderr
            assert destination.read_bytes() == before and sidecar.read_bytes() == pending
            with closing(sqlite3.connect(destination)) as db:
                db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
                assert db.execute("SELECT name FROM points").fetchall() == [("wal-value",)]
            appended = subprocess.run(command, capture_output=True, text=True)
            assert appended.returncode == 0, appended.stderr
            with closing(sqlite3.connect(destination)) as db:
                assert db.execute("SELECT name FROM points ORDER BY fid").fetchall() == [
                    ("wal-value",),
                    ("kept",),
                ]
                assert db.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
            results.append(
                dict(
                    name="append rewrite preserves orphan WAL commits",
                    status="identical",
                    detail="actual GDAL feature update committed to orphan WAL: append refuses with main/WAL bytes unchanged; after SQLite checkpoint, append preserves both committed and new values",
                )
            )
        except Exception as error:
            traceback.print_exc()
            results.append(
                dict(
                    name="append rewrite preserves orphan WAL commits",
                    status="differs",
                    detail=str(error),
                )
            )
    if args.json:
        print(json.dumps(results))
    else:
        for result in results:
            print(result["status"], result["name"], result["detail"])
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
