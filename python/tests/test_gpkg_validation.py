"""GeoPackage verdicts are read-only and distinguish incomplete SQL checks."""

import os
import shutil
import sqlite3
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
