"""Seven layer-algebra operations share XY topology and schema rules."""

import sqlite3
from contextlib import closing

import pytest

import terrio


def source(coordinates, value):
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"id": value},
                "geometry": {"type": "LineString", "coordinates": coordinates},
            }
        ],
    }


@pytest.mark.parametrize(
    "operation,count",
    [
        ("intersection", 1),
        ("union", 3),
        ("sym-difference", 2),
        ("identity", 2),
        ("update", 2),
        ("clip", 1),
        ("erase", 1),
    ],
)
def test_all_operations_and_input_immutability(operation, count, tmp_path):
    a = terrio.Layer.from_geojson(source([[0, 0], [3, 3]], 1), name="a")
    b = terrio.Layer.from_geojson(source([[0, 3], [3, 0]], 2), name="b")
    before = a.to_geojson()
    result = terrio.layer_algebra(a, b, operation)
    assert len(result) == count
    assert a.to_geojson() == before
    path = tmp_path / "result.gpkg"
    result.write(str(path))
    with closing(sqlite3.connect(path)) as connection:
        assert [r[0] for r in connection.execute("SELECT fid FROM a ORDER BY fid")] == list(
            range(1, count + 1)
        )
    if operation in {"clip", "erase", "update"}:
        assert list(result.fields) == ["id"]
    else:
        assert list(result.fields) == ["input_id", "method_id"]
    if operation == "update":
        assert result.to_geojson()["features"][-1]["properties"] == {"id": 2}


def test_dimension_policy_selectors_and_errors():
    a = terrio.Layer.from_geojson(source([[0, 0], [3, 3]], 1), name="a")
    b = terrio.Layer.from_geojson(source([[0, 3], [3, 0]], 2), name="b")
    assert (
        len(terrio.layer_algebra(a, b, "intersection", keep_lower_dimension_geometries=False)) == 0
    )
    assert len(terrio.layer_algebra(a, b, "clip", keep_lower_dimension_geometries=False)) == 1
    result = terrio.layer_algebra(
        [a, b],
        [a, b],
        "identity",
        layer="a",
        method_layer="b",
        input_prefix="a_",
        method_prefix="b_",
    )
    assert list(result.fields) == ["a_id", "b_id"]
    with pytest.raises(ValueError, match="choose one"):
        terrio.layer_algebra([a, b], b, "clip")
    with pytest.raises(ValueError, match="no layer"):
        terrio.layer_algebra(a, b, "clip", method_layer="missing")
    with pytest.raises(ValueError, match="operation must"):
        terrio.layer_algebra(a, b, "unsupported")


def test_overlapping_collection_union_is_used_for_point_queries():
    point = {"type": "Point", "coordinates": [1.5, 1.5]}
    collection = {
        "type": "GeometryCollection",
        "geometries": [
            {"type": "Polygon", "coordinates": [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]]},
            {"type": "Polygon", "coordinates": [[[1, 1], [3, 1], [3, 3], [1, 3], [1, 1]]]},
        ],
    }

    def layer(geometry):
        return terrio.Layer.from_geojson(
            {
                "type": "FeatureCollection",
                "features": [{"type": "Feature", "properties": {}, "geometry": geometry}],
            }
        )

    for a, b in [(point, collection), (collection, point)]:
        output = terrio.layer_algebra(layer(a), layer(b), "intersection").to_geojson()["features"]
        assert len(output) == 1
        assert output[0]["geometry"] == point
