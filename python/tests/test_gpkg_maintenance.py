"""GeoPackage repacking preserves SQL objects, records and source files on refusal."""

import sqlite3

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
    with sqlite3.connect(path) as database:
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
    with sqlite3.connect(path) as database:
        return {
            "schema": database.execute(
                "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name"
            ).fetchall(),
            "notes": database.execute("SELECT * FROM notes ORDER BY id").fetchall(),
            "compound": database.execute("SELECT * FROM compound ORDER BY k").fetchall(),
            "sequence": database.execute("SELECT * FROM sqlite_sequence").fetchall(),
        }


def test_native_and_bytes_repack_preserve_all_values(tmp_path):
    path = fixture(tmp_path)
    expected = snapshot(path)
    original = path.read_bytes()
    compact = terrio.gpkg_repack_bytes(original)
    assert path.read_bytes() == original
    copy = tmp_path / "compact.gpkg"
    copy.write_bytes(compact)
    assert snapshot(copy) == expected
    report = terrio.gpkg_repack(path)
    assert report["bytes_before"] == len(original)
    assert report["bytes_after"] == path.stat().st_size < len(original)
    assert snapshot(path) == expected
    for target in [path, copy]:
        with sqlite3.connect(target) as database:
            assert database.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
            assert database.execute("PRAGMA freelist_count").fetchone() == (0,)
    assert not list(tmp_path.glob("*-journal"))


def test_memory_and_failed_repack_preserve_inputs(tmp_path):
    path = fixture(tmp_path)
    source = path.read_bytes()
    name = "/vsimem/gpkg-repack-python.gpkg"
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
    with sqlite3.connect(path) as database:
        assert database.execute("SELECT payload FROM notes WHERE label='wal-kept'").fetchone() == (
            b"\x01\x02",
        )
