"""GeoPackage verdicts are read-only and distinguish incomplete SQL checks."""

import os
import shutil
import sqlite3
import struct
from contextlib import closing
from pathlib import Path

import pytest

import terrio


@pytest.fixture
def geopackage(tmp_path):
    data = os.environ.get("TERRIO_TEST_DATA")
    if not data or not (Path(data) / "countries.gpkg").exists():
        pytest.skip("requires real GeoPackage comparison data")
    path = tmp_path / "countries.gpkg"
    shutil.copyfile(Path(data) / "countries.gpkg", path)
    return path


def test_malformed_bytes_return_a_verdict():
    report = terrio.validate_gpkg(b"not sqlite")
    assert not report["valid"] and report["complete"]
    assert report["errors"][0]["requirement"] == 1


def test_path_bytes_and_memory_agree_without_changes(geopackage):
    before = geopackage.read_bytes()
    report = terrio.validate_gpkg(geopackage)
    assert report["valid"] and report["complete"], report
    assert terrio.validate_gpkg(before) == report
    memory = "/vsimem/python-gpkg-validation.gpkg"
    terrio.vsi_write(memory, before)
    try:
        assert terrio.validate_gpkg(memory) == report
        assert terrio.vsi_read(memory) == before
    finally:
        terrio.vsi_delete(memory)
    assert geopackage.read_bytes() == before


def test_extra_checks_strict_and_unsupported_views(geopackage):
    with closing(sqlite3.connect(geopackage)) as db:
        db.executescript(
            "CREATE TABLE attributes(id INTEGER PRIMARY KEY,v TINYINT);"
            "INSERT INTO attributes VALUES(1,300);"
            "INSERT INTO gpkg_contents(table_name,data_type,identifier) "
            "VALUES('attributes','attributes','attributes')"
        )
        db.commit()
    assert terrio.validate_gpkg(geopackage)["valid"]
    full = terrio.validate_gpkg(geopackage, full_check=True)
    assert full["valid"] and full["warnings"], full
    strict = terrio.validate_gpkg(geopackage, full_check=True, strict=True)
    assert not strict["valid"] and strict["complete"] and not strict["errors"]
    with closing(sqlite3.connect(geopackage)) as db:
        db.executescript(
            "CREATE VIEW unsupported AS SELECT count(*) AS id FROM attributes;"
            "INSERT INTO gpkg_contents(table_name,data_type,identifier) "
            "VALUES('unsupported','attributes','unsupported')"
        )
        db.commit()
    report = terrio.validate_gpkg(geopackage)
    assert not report["valid"] and not report["complete"]
    assert report["unsupported_checks"], report


def test_case_options_and_sidecar_refusal(geopackage):
    with closing(sqlite3.connect(geopackage)) as db:
        db.executescript(
            "CREATE TABLE checked(v TEXT CHECK(lower(v)='é'));"
            "PRAGMA ignore_check_constraints=ON;INSERT INTO checked VALUES('É')"
        )
        db.commit()
    assert not terrio.validate_gpkg(geopackage)["valid"]
    assert terrio.validate_gpkg(geopackage, unicode_case=True)["valid"]
    before = geopackage.read_bytes()
    for suffix in ["-wal", "-journal"]:
        sidecar = Path(str(geopackage) + suffix)
        sidecar.write_bytes(b"uncheckpointed")
        try:
            with pytest.raises(terrio.TerrioError, match="checkpointed"):
                terrio.validate_gpkg(geopackage)
            assert geopackage.read_bytes() == before
            assert sidecar.read_bytes() == b"uncheckpointed"
        finally:
            sidecar.unlink()


def test_unsupported_generated_expression_is_incomplete(geopackage):
    if sqlite3.sqlite_version_info < (3, 38, 0):
        pytest.skip("requires SQLite 3.38 JSON extraction operators")
    with closing(sqlite3.connect(geopackage)) as db:
        db.executescript(
            "CREATE TABLE derived(a TEXT,b TEXT AS(a ->> '$.x'));"
            'INSERT INTO derived(a) VALUES(\'{"x":"ok"}\')'
        )
        assert db.execute("SELECT b FROM derived").fetchone()[0] == "ok"
        db.commit()
    before = geopackage.read_bytes()
    report = terrio.validate_gpkg(geopackage)
    assert not report["valid"] and not report["complete"], report
    assert not report["errors"], report
    assert any("row expressions" in message for message in report["unsupported_checks"])
    assert geopackage.read_bytes() == before


@pytest.mark.parametrize("depth", [129, 1000, 1001])
def test_json_check_uses_sqlite_nesting_limit(geopackage, depth):
    value = "[" * depth + "0" + "]" * depth
    with closing(sqlite3.connect(geopackage)) as db:
        try:
            expected = bool(db.execute("SELECT json_valid(?)", (value,)).fetchone()[0])
        except sqlite3.OperationalError:
            pytest.skip("SQLite JSON functions are unavailable")
        db.executescript(
            "CREATE TABLE deep_json(value TEXT CHECK(json_valid(value)));"
            "PRAGMA ignore_check_constraints=ON;"
        )
        db.execute("INSERT INTO deep_json VALUES(?)", (value,))
        db.commit()
    report = terrio.validate_gpkg(geopackage)
    assert report["complete"], report
    assert report["valid"] == expected, report


def test_stored_nan_is_sql_null_in_registered_views(geopackage):
    marker = 1.2345678987654321
    with closing(sqlite3.connect(geopackage)) as db:
        db.executescript(
            "CREATE TABLE nan_values(id INTEGER PRIMARY KEY,v REAL);"
            "CREATE VIEW nan_view AS SELECT id,CAST(min(v,0) AS REAL) AS low,"
            "CAST(max(v,0) AS REAL) AS high FROM nan_values;"
            "INSERT INTO gpkg_contents(table_name,data_type,identifier) "
            "VALUES('nan_view','attributes','nan_view');"
        )
        db.execute("INSERT INTO nan_values VALUES(1,?)", (marker,))
        db.commit()
    before = geopackage.read_bytes()
    encoded = struct.pack(">d", marker)
    assert before.count(encoded) == 1
    geopackage.write_bytes(before.replace(encoded, struct.pack(">d", float("nan"))))
    with closing(sqlite3.connect(geopackage)) as db:
        assert db.execute("SELECT typeof(v),v,min(v,0),max(v,0) FROM nan_values").fetchone() == (
            "null",
            None,
            None,
            None,
        )
        assert db.execute("SELECT count(*) FROM nan_values WHERE v").fetchone() == (0,)
    report = terrio.validate_gpkg(geopackage)
    assert report["valid"] and report["complete"], report


def test_expensive_pattern_returns_incomplete_verdict(geopackage):
    pattern = "%a" * 20
    with closing(sqlite3.connect(geopackage)) as db:
        db.executescript(
            "CREATE TABLE text_values(id INTEGER PRIMARY KEY,v TEXT);"
            f"CREATE VIEW pattern_view AS SELECT id FROM text_values WHERE v LIKE '{pattern}';"
            "INSERT INTO gpkg_contents(table_name,data_type,identifier) "
            "VALUES('pattern_view','attributes','pattern_view');"
        )
        db.execute("INSERT INTO text_values VALUES(1,?)", ("a" * 100_000,))
        assert db.execute("SELECT * FROM pattern_view").fetchall() == [(1,)]
        db.commit()
    report = terrio.validate_gpkg(geopackage)
    assert not report["valid"] and not report["complete"], report
    assert not report["errors"], report
    assert any("LIKE/GLOB" in message for message in report["unsupported_checks"])
