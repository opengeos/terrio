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


def test_missing_and_out_of_range_geojson_ids(tmp_path):
    source = tmp_path / "points.geojson"
    output = tmp_path / "points.gpkg"
    data = points([0, 205])
    del data["features"][0]["id"]
    source.write_text(json.dumps(data), encoding="utf-8")
    terrio.convert(str(source), str(output), preserve_fid=True)
    # The GeoJSON reader assigns a numeric ID before the GeoPackage writer sees the feature.
    assert sqlite_ids(output) == [0, 205]

    before = output.read_bytes()
    source.write_text(json.dumps(points([2**63, 205])), encoding="utf-8")
    with pytest.raises(ValueError, match="unrecognized number"):
        terrio.convert(str(source), str(output), preserve_fid=True)
    assert output.read_bytes() == before
