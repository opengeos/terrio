"""Coverage simplification updates shared edges and preserves selected boundaries."""

import sqlite3

import pytest

import terrio


def source():
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"name": "west"},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [[0, 0], [2, 0], [1.8, 1], [2.2, 2], [1.8, 3], [2, 4], [0, 4], [0, 0]]
                    ],
                },
            },
            {
                "type": "Feature",
                "properties": {"name": "east"},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [[2, 0], [4, 0], [4, 4], [2, 4], [1.8, 3], [2.2, 2], [1.8, 1], [2, 0]]
                    ],
                },
            },
        ],
    }


def test_shared_edges_keep_a_valid_coverage_and_metadata():
    original = terrio.Layer.from_geojson(source(), name="coverage", crs="EPSG:3857")
    output = terrio.simplify_coverage(original, 1, preserve_boundary=True)
    assert len(output) == 2
    assert output.name == "coverage"
    assert output.crs == "EPSG:3857"
    assert output.fields == original.fields
    assert len(terrio.check_coverage(output)) == 0
    features = output.to_geojson()["features"]
    assert [f["properties"]["name"] for f in features] == ["west", "east"]
    for f in features:
        ring = f["geometry"]["coordinates"][0]
        assert len(ring) == 5
        assert all(x in (0, 2, 4) and y in (0, 4) for x, y in ring)


def test_isolated_ring_can_reach_a_triangle_like_gdal():
    polygon = {
        "type": "Polygon",
        "coordinates": [
            [[0, 0], [2, 0], [4, 0], [6, 0], [6, 2], [6, 4], [4, 4], [2, 4], [0, 4], [0, 2], [0, 0]]
        ],
    }
    output = terrio.simplify_coverage(polygon, 100).to_geojson()["features"]
    assert len(output[0]["geometry"]["coordinates"][0]) == 4
    fixed = terrio.simplify_coverage(polygon, 100, preserve_boundary=True).to_geojson()["features"]
    assert fixed[0]["geometry"] == polygon


@pytest.mark.parametrize("tolerance", [-1, float("nan"), float("inf")])
def test_invalid_tolerances_fail(tolerance):
    with pytest.raises(ValueError, match="tolerance"):
        terrio.simplify_coverage(source(), tolerance)


def test_single_layer_selection_and_nonpolygon_rejection():
    polygon = terrio.Layer.from_geojson(source(), name="coverage")
    point = terrio.Layer.from_geojson({"type": "Point", "coordinates": [1, 2]}, name="point")
    assert len(terrio.simplify_coverage([polygon, point], 1, layer="coverage")) == 2
    with pytest.raises(ValueError, match="choose one"):
        terrio.simplify_coverage([polygon, point], 1)
    with pytest.raises(ValueError, match="polygonal"):
        terrio.simplify_coverage(point, 1)


def test_geopackage_writers_can_retain_unknown_geometry_declarations(tmp_path):
    layer = terrio.Layer.from_geojson(source(), name="coverage")
    options = {"preserve_declared_geometry_type": True}
    path = tmp_path / "output.gpkg"
    layer.write(str(path), **options)
    with sqlite3.connect(path) as db:
        assert db.execute("SELECT geometry_type_name FROM gpkg_geometry_columns").fetchone() == (
            "GEOMETRY",
        )
    path.write_bytes(layer.to_bytes("out.gpkg", **options))
    with sqlite3.connect(path) as db:
        assert db.execute("SELECT geometry_type_name FROM gpkg_geometry_columns").fetchone() == (
            "GEOMETRY",
        )
    terrio.write_vector(layer, str(path), overwrite=True, **options)
    terrio.write_vector(layer, str(path), append=True, **options)
    with sqlite3.connect(path) as db:
        assert db.execute("SELECT geometry_type_name FROM gpkg_geometry_columns").fetchone() == (
            "GEOMETRY",
        )
        assert db.execute("SELECT count(*) FROM coverage").fetchone() == (4,)
    dataset = terrio.open_vector(str(path))
    dataset.write(str(tmp_path / "copy.gpkg"), **options)
    with sqlite3.connect(tmp_path / "copy.gpkg") as db:
        assert db.execute("SELECT geometry_type_name FROM gpkg_geometry_columns").fetchone() == (
            "GEOMETRY",
        )
    with pytest.raises(ValueError, match="GPKG"):
        layer.to_bytes("out.geojson", **options)
