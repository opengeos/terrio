"""Check GeoPackage source IDs with independent SQLite queries and Terrio readback."""

import json
import sqlite3
from contextlib import closing

import pytest

import terrio


def points(ids, attributes=None):
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "id": fid,
                "properties": {} if attributes is None else attributes[index],
                "geometry": {"type": "Point", "coordinates": [index, index]},
            }
            for index, fid in enumerate(ids)
        ],
    }


def sqlite_ids(path):
    with closing(sqlite3.connect(path)) as conn:
        column = conn.execute("SELECT type, pk FROM pragma_table_info('points') WHERE name = 'fid'")
        assert column.fetchall() == [("INTEGER", 1)]
        assert conn.execute("PRAGMA integrity_check").fetchone() == ("ok",)
        assert conn.execute("SELECT rtreecheck('rtree_points_geom')").fetchone() == ("ok",)
        rows = conn.execute("SELECT rowid, fid FROM points ORDER BY fid").fetchall()
        assert all(rowid == fid for rowid, fid in rows)
        return [fid for _, fid in rows]


def test_issue_fixture_default_and_preserved_ids(tmp_path):
    source = tmp_path / "points.geojson"
    source.write_text(json.dumps(points([91, 205])), encoding="utf-8")
    output = tmp_path / "points.gpkg"
    data = terrio.open_vector(str(source))
    data.write(str(output))
    assert sqlite_ids(output) == [1, 2]

    terrio.convert(str(source), str(output), preserve_fid=True)
    assert sqlite_ids(output) == [91, 205]
    readback = terrio.sql(
        terrio.open_vector(str(output)), "SELECT FID AS source_id FROM points ORDER BY FID"
    )
    assert list(readback.columns()["source_id"]) == [91, 205]

    output.write_bytes(data.layers[0].to_bytes("points.gpkg", preserve_fid=True))
    assert sqlite_ids(output) == [91, 205]


@pytest.mark.parametrize(
    ("attributes", "expected", "error"),
    [
        ([{"fid": 701}, {"fid": 802}], [701, 802], "conflicts with feature ID 91"),
        ([{"fid": 701}, {"fid": 701}], [1, 2], "conflicts with feature ID 91"),
        ([{"fid": "91"}, {"fid": "205"}], [1, 2], "must be an integer field"),
        ([{"fid": 91.5}, {"fid": 205.5}], [1, 2], "must be an integer field"),
        ([{"fid": 91, "FID": 91}, {"fid": 205, "FID": 205}], [91, 205], "multiple attributes"),
    ],
)
def test_attribute_precedence_and_invalid_keys(tmp_path, attributes, expected, error):
    data = points([91, 205], attributes)
    output = tmp_path / "points.gpkg"
    terrio.write_vector(data, str(output))
    assert sqlite_ids(output) == expected
    before = output.read_bytes()
    with pytest.raises(ValueError, match=error):
        terrio.write_vector(data, str(output), preserve_fid=True)
    assert output.read_bytes() == before


def test_matching_case_insensitive_key_and_duplicates(tmp_path):
    data = points([91, 205], [{"FID": 91}, {"FID": None}])
    output = tmp_path / "points.gpkg"
    terrio.write_vector(data, str(output), preserve_fid=True)
    assert sqlite_ids(output) == [91, 205]
    with closing(sqlite3.connect(output)) as conn:
        assert conn.execute("SELECT name FROM pragma_table_info('points')").fetchall() == [
            ("fid",),
            ("geom",),
        ]

    before = output.read_bytes()
    duplicate = terrio.Layer.from_geojson(points([91, 91]))
    with pytest.raises(ValueError, match="duplicate feature ID 91"):
        duplicate.write(str(output), preserve_fid=True)
    assert output.read_bytes() == before


@pytest.mark.parametrize("first_id", [None, -10, 1.5, "named"])
def test_preservation_uses_the_geojson_readers_assigned_ids(tmp_path, first_id):
    """Preserve parsed IDs, without claiming original IDs for an unset feature.

    Args:
        tmp_path: Temporary directory for the source and GeoPackage.
        first_id: Missing, negative, fractional or string source ID.
    """
    source = tmp_path / "points.geojson"
    output = tmp_path / "points.gpkg"
    data = points([first_id, 205])
    if first_id is None:
        del data["features"][0]["id"]
    source.write_text(json.dumps(data), encoding="utf-8")
    parsed = terrio.sql(
        terrio.open_vector(str(source)), "SELECT FID AS source_id FROM points ORDER BY FID"
    )
    terrio.convert(str(source), str(output), preserve_fid=True)
    assert sqlite_ids(output) == list(parsed.columns()["source_id"])


def test_out_of_range_geojson_ids_leave_the_destination_intact(tmp_path):
    """Reject out-of-range IDs before replacing an existing destination.

    Args:
        tmp_path: Temporary directory for the source and GeoPackage.
    """
    source = tmp_path / "points.geojson"
    output = tmp_path / "points.gpkg"
    source.write_text(json.dumps(points([91, 205])), encoding="utf-8")
    terrio.convert(str(source), str(output), preserve_fid=True)
    before = output.read_bytes()
    source.write_text(json.dumps(points([2**63, 205])), encoding="utf-8")
    with pytest.raises(ValueError, match="unrecognized number"):
        terrio.convert(str(source), str(output), preserve_fid=True)
    assert output.read_bytes() == before


def test_negative_rowids_read_back_signed_through_pyogrio(tmp_path):
    """A GeoPackage rowid of -1 is the FID -1, as pyogrio gives it (issue #624).

    Args:
        tmp_path: The test's temporary directory.
    """
    path = str(tmp_path / "signed.gpkg")
    with closing(sqlite3.connect(path)) as db:
        db.executescript(
            """
            CREATE TABLE gpkg_contents (table_name TEXT, data_type TEXT, description TEXT);
            CREATE TABLE gpkg_geometry_columns
              (table_name TEXT, column_name TEXT, geometry_type_name TEXT,
               srs_id INTEGER, z INTEGER, m INTEGER);
            CREATE TABLE gpkg_spatial_ref_sys
              (srs_id INTEGER, organization TEXT,
               organization_coordsys_id INTEGER, definition TEXT);
            CREATE TABLE features (fid INTEGER PRIMARY KEY, name TEXT);
            INSERT INTO gpkg_contents VALUES ('features', 'attributes', '');
            INSERT INTO features VALUES (-1, 'negative'), (7, 'positive');
            """
        )
    pytest.importorskip("geopandas")
    from terrio import pyogrio

    frame = pyogrio.read_dataframe(path, read_geometry=False, fid_as_index=True)
    assert frame.index.dtype.name == "int64"
    assert frame.index.tolist() == [-1, 7]
