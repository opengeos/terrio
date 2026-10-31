"""Coverage simplification updates shared edges and preserves selected boundaries."""

import sqlite3
import struct
from contextlib import closing

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
    with closing(sqlite3.connect(path)) as db:
        assert db.execute("SELECT geometry_type_name FROM gpkg_geometry_columns").fetchone() == (
            "GEOMETRY",
        )
    path.write_bytes(layer.to_bytes("out.gpkg", **options))
    with closing(sqlite3.connect(path)) as db:
        assert db.execute("SELECT geometry_type_name FROM gpkg_geometry_columns").fetchone() == (
            "GEOMETRY",
        )
    terrio.write_vector(layer, str(path), overwrite=True, **options)
    terrio.write_vector(layer, str(path), append=True, **options)
    with closing(sqlite3.connect(path)) as db:
        assert db.execute("SELECT geometry_type_name FROM gpkg_geometry_columns").fetchone() == (
            "GEOMETRY",
        )
        assert db.execute("SELECT count(*) FROM coverage").fetchone() == (4,)
    dataset = terrio.open_vector(str(path))
    dataset.write(str(tmp_path / "copy.gpkg"), **options)
    with closing(sqlite3.connect(tmp_path / "copy.gpkg")) as db:
        assert db.execute("SELECT geometry_type_name FROM gpkg_geometry_columns").fetchone() == (
            "GEOMETRY",
        )
    with pytest.raises(ValueError, match="GPKG"):
        layer.to_bytes("out.geojson", **options)


def test_active_layer_copies_other_layers_and_preserves_measures(tmp_path):
    polygon = terrio.Layer.from_geojson(source(), name="coverage")
    point = terrio.open_bytes(b'WKT\n"POINT ZM (1 2 3 4)"\n', "point.csv").layers[0]
    result = terrio.simplify_coverage(
        [polygon, point], 1, preserve_boundary=True, active_layer="coverage"
    )
    assert isinstance(result, terrio.VectorDataset)
    assert result.layer_names == ["coverage", "point"]
    assert len(terrio.check_coverage(result.layers[0])) == 0
    assert result.layers[1].to_geojson() == point.to_geojson()
    path = tmp_path / "copied.gpkg"
    result.write(str(path), preserve_declared_geometry_type=True)
    with closing(sqlite3.connect(path)) as db:
        assert db.execute(
            "SELECT m FROM gpkg_geometry_columns WHERE table_name='point'"
        ).fetchone() == (1,)
        blob = db.execute("SELECT geom FROM point").fetchone()[0]
        assert struct.unpack("<dddd", blob[-32:]) == (1, 2, 3, 4)
    with pytest.raises(ValueError, match="no active layer"):
        terrio.simplify_coverage([polygon, point], 1, active_layer="missing")
    with pytest.raises(ValueError, match="mutually exclusive"):
        terrio.simplify_coverage([polygon, point], 1, layer="coverage", active_layer="coverage")


def test_appending_mixed_measure_layouts_promotes_the_declaration(tmp_path):
    measured = terrio.open_bytes(b'WKT\n"POINT M (1 2 3)"\n', "points.csv").layers[0]
    unmeasured = terrio.open_bytes(b'WKT\n"POINT (4 5)"\n', "points.csv").layers[0]
    for measured_first in [False, True]:
        first, second = (measured, unmeasured) if measured_first else (unmeasured, measured)
        path = tmp_path / f"append-{measured_first}.gpkg"
        first.write(str(path))
        terrio.write_vector(second, str(path), append=True)
        with closing(sqlite3.connect(path)) as db:
            assert db.execute("SELECT m FROM gpkg_geometry_columns").fetchone() == (2,)
            blobs = db.execute("SELECT geom FROM points ORDER BY fid").fetchall()
            measured_blob = blobs[0 if measured_first else 1][0]
            assert struct.unpack("<d", measured_blob[-8:]) == (3,)


def test_ring_direction_matches_gdal_when_feature_order_changes():
    """GDAL 3.13.3 / GEOS 3.14.1 also reverses the reused first shared span."""
    for reverse, expected in [(False, [8.0, 8.0]), (True, [8.0, -8.0])]:
        original = source()
        if reverse:
            original["features"].reverse()
        output = terrio.simplify_coverage(original, 1, preserve_boundary=True)
        signed = []
        for feature in output.to_geojson()["features"]:
            ring = feature["geometry"]["coordinates"][0]
            signed.append(
                sum(a[0] * b[1] - a[1] * b[0] for a, b in zip(ring, ring[1:], strict=False)) / 2
            )
        assert signed == expected
        assert len(terrio.check_coverage(output)) == 0


def test_shared_span_z_matches_the_reference_representative_edge():
    original = source()
    for index, feature in enumerate(original["features"]):
        ring = feature["geometry"]["coordinates"][0]
        for vertex, point in enumerate(ring):
            point.append((1 if index == 0 else 100) + vertex)
    output = terrio.simplify_coverage(original, 0, preserve_boundary=True)
    shared = []
    for feature in output.to_geojson()["features"]:
        shared.append(
            sorted(tuple(p) for p in feature["geometry"]["coordinates"][0] if p[0] in (1.8, 2.2))
        )
    assert shared == [[(1.8, 1, 3), (1.8, 3, 5), (2.2, 2, 4)]] * 2


def test_appending_mixed_height_layouts_promotes_the_declaration(tmp_path):
    elevated = terrio.open_bytes(b'WKT\n"POINT Z (1 2 3)"\n', "points.csv").layers[0]
    flat = terrio.open_bytes(b'WKT\n"POINT (4 5)"\n', "points.csv").layers[0]
    for elevated_first in [False, True]:
        first, second = (elevated, flat) if elevated_first else (flat, elevated)
        path = tmp_path / f"append-z-{elevated_first}.gpkg"
        first.write(str(path))
        terrio.write_vector(second, str(path), append=True)
        with closing(sqlite3.connect(path)) as db:
            assert db.execute("SELECT z FROM gpkg_geometry_columns").fetchone() == (2,)
