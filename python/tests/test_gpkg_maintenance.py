"""GeoPackage repacking preserves SQL objects, records and source files on refusal."""

import os
import sqlite3
import subprocess
import sys
from contextlib import closing

import pytest

import terrio


def fixture(tmp_path):
    path = tmp_path / "data.gpkg"
    layer = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {"name": "survey"},
                    "geometry": {"type": "Point", "coordinates": [12, 45]},
                }
            ],
        },
        name="sites",
        crs="EPSG:4326",
    )
    layer.write(str(path))
    with closing(sqlite3.connect(path)) as database, database:
        database.execute(
            "CREATE TABLE notes(id INTEGER PRIMARY KEY AUTOINCREMENT,"
            " label TEXT COLLATE NOCASE UNIQUE, payload BLOB)"
        )
        database.executemany(
            "INSERT INTO notes(label,payload) VALUES (?,?)",
            [(f"item-{i}", bytes([i % 251]) * 400) for i in range(1000)],
        )
        database.execute("CREATE INDEX lower_notes ON notes(lower(label),id DESC) WHERE id%3=0")
        database.execute(
            "CREATE TABLE compound(k TEXT COLLATE NOCASE PRIMARY KEY, payload BLOB) WITHOUT ROWID"
        )
        database.executemany(
            "INSERT INTO compound VALUES (?,?)", [(f"key-{i}", b"x" * 400) for i in range(500)]
        )
        database.execute("DELETE FROM notes WHERE id%3<>0")
        database.execute("DELETE FROM compound WHERE length(k)%2=0")
    return path


def snapshot(path):
    with closing(sqlite3.connect(path)) as database, database:
        return {
            "schema": database.execute(
                "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name"
            ).fetchall(),
            "notes": database.execute("SELECT * FROM notes ORDER BY id").fetchall(),
            "compound": database.execute("SELECT * FROM compound ORDER BY k").fetchall(),
            "sequence": database.execute("SELECT * FROM sqlite_sequence").fetchall(),
        }


@pytest.mark.parametrize("filename", ["data.gpkg", "snapshot.zip"])
def test_native_and_bytes_repack_preserve_all_values(tmp_path, filename):
    path = fixture(tmp_path)
    if path.name != filename:
        path = path.rename(tmp_path / filename)
    expected = snapshot(path)
    original = path.read_bytes()
    compact = terrio.gpkg_repack_bytes(original)
    assert path.read_bytes() == original
    copy = tmp_path / "compact.gpkg"
    copy.write_bytes(compact)
    assert snapshot(copy) == expected
    report = terrio.gpkg_repack(path)
    assert set(report) == {
        "bytes_before",
        "bytes_after",
        "page_size",
        "pages_before",
        "pages_after",
    }
    assert all(type(value) is int for value in report.values())
    assert report["bytes_before"] == len(original)
    assert report["bytes_after"] == path.stat().st_size < len(original)
    assert snapshot(path) == expected
    if path.suffix == ".zip":
        current = path.read_bytes()
        for archive in ["/vsizip/" + str(path), str(path) + "!member.gpkg"]:
            with pytest.raises((ValueError, OSError)):
                terrio.gpkg_repack(archive)
            assert path.read_bytes() == current
    for target in [path, copy]:
        with closing(sqlite3.connect(target)) as database, database:
            assert database.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
            assert database.execute("PRAGMA freelist_count").fetchone() == (0,)
    assert not list(tmp_path.glob("*-journal"))


@pytest.mark.parametrize(
    "name", ["/vsimem/gpkg-repack-python.gpkg", "/vsimem/gpkg-repack-python.zip"]
)
def test_memory_and_failed_repack_preserve_inputs(tmp_path, name):
    path = fixture(tmp_path)
    source = path.read_bytes()
    terrio.vsi_write(name, source)
    try:
        assert terrio.gpkg_repack(name)["bytes_before"] == len(source)
        compact = tmp_path / "memory.gpkg"
        compact.write_bytes(terrio.vsi_read(name))
        assert snapshot(compact) == snapshot(path)
        terrio.vsi_write(name, b"invalid")
        with pytest.raises(ValueError):
            terrio.gpkg_repack(name)
        assert terrio.vsi_read(name) == b"invalid"
    finally:
        terrio.vsi_delete(name)
    path.write_bytes(source[:100])
    with pytest.raises(ValueError):
        terrio.gpkg_repack(path)
    assert path.read_bytes() == source[:100]


def test_active_wal_refused_without_losing_committed_rows(tmp_path):
    path = fixture(tmp_path)
    connection = sqlite3.connect(path)
    try:
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute("INSERT INTO notes(label,payload) VALUES ('wal-kept', X'0102')")
        connection.commit()
        original = path.read_bytes()
        wal = path.with_name(path.name + "-wal").read_bytes()
        with pytest.raises((ValueError, OSError)):
            terrio.gpkg_repack(path)
        assert path.read_bytes() == original
        assert path.with_name(path.name + "-wal").read_bytes() == wal
        assert connection.execute(
            "SELECT payload FROM notes WHERE label='wal-kept'"
        ).fetchone() == (b"\x01\x02",)
    finally:
        connection.close()
    terrio.gpkg_repack(path)
    with closing(sqlite3.connect(path)) as database, database:
        assert database.execute("SELECT payload FROM notes WHERE label='wal-kept'").fetchone() == (
            b"\x01\x02",
        )


@pytest.mark.skipif(os.name != "posix", reason="non-UTF-8 and trailing-space filenames need POSIX")
@pytest.mark.parametrize(
    "name",
    [
        pytest.param(
            b"caf\xe9.gpkg",
            marks=pytest.mark.skipif(
                sys.platform == "darwin", reason="APFS rejects invalid UTF-8 filenames"
            ),
        ),
        "café.gpkg",
        " edge.gpkg ",
    ],
)
def test_native_non_utf8_path_preserves_database(tmp_path, name):
    name = os.fsdecode(name)
    source = fixture(tmp_path)
    expected = snapshot(source)
    path = tmp_path / name
    source.rename(path)
    neighbor = tmp_path / name.rstrip()
    if neighbor != path:
        neighbor.write_bytes(b"preserve unrelated file")
    before = path.stat().st_size
    report = terrio.gpkg_repack(path)
    assert report["bytes_before"] == before
    assert report["bytes_after"] == path.stat().st_size < before
    assert snapshot(path) == expected
    if neighbor != path:
        assert neighbor.read_bytes() == b"preserve unrelated file"
    with closing(sqlite3.connect(path)) as database, database:
        assert database.execute("PRAGMA integrity_check").fetchall() == [("ok",)]


def test_repack_refuses_database_locked_by_another_process(tmp_path):
    path = fixture(tmp_path)
    original = path.read_bytes()
    script = """
import sqlite3
import sys
connection = sqlite3.connect(sys.argv[1], timeout=0)
connection.execute("BEGIN EXCLUSIVE")
print("ready", flush=True)
sys.stdin.readline()
connection.rollback()
connection.close()
"""
    child = subprocess.Popen(
        [sys.executable, "-c", script, str(path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert child.stdout.readline() == "ready\n"
        with pytest.raises(terrio.TerrioIOError, match="locked by another connection"):
            terrio.gpkg_repack(path)
        assert path.read_bytes() == original
    finally:
        try:
            _, error = child.communicate("\n", timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
            child.communicate()
            raise
    assert child.returncode == 0, error
    terrio.gpkg_repack(path)
