"""Coverage cleaning repairs shared nodes while retaining feature correspondence."""

import pytest

import terrio


def rectangle(x, y, w, h):
    return {
        "type": "Polygon",
        "coordinates": [[[x, y], [x + w, y], [x + w, y + h], [x, y + h], [x, y]]],
    }


def source(shapes):
    return {
        "type": "FeatureCollection",
        "features": [
            {"type": "Feature", "properties": {"source_id": i}, "geometry": shape}
            for i, shape in enumerate(shapes)
        ],
    }


@pytest.mark.parametrize("strategy", ["longest-border", "max-area", "min-area", "min-index"])
def test_clean_overlap(strategy):
    result = terrio.clean_coverage(
        source([rectangle(0, 0, 3, 2), rectangle(2, 0, 2, 2)]), merge_strategy=strategy
    )
    assert len(result) == 2
    assert [f["properties"]["source_id"] for f in result.to_geojson()["features"]] == [0, 1]
    assert len(terrio.check_coverage(result)) == 0


def test_containment_and_active_copy():
    polygons = terrio.Layer.from_geojson(
        source([rectangle(0, 0, 4, 4), rectangle(1, 1, 1, 1)]), name="polygons"
    )
    other = terrio.Layer.from_geojson(
        source([{"type": "Point", "coordinates": [20, 30, 40]}]), name="point"
    )
    ds = terrio.clean_coverage([polygons, other], active_layer="polygons")
    assert isinstance(ds, terrio.VectorDataset)
    assert ds.layers[1].to_geojson() == other.to_geojson()
    output = ds.layers[0].to_geojson()["features"]
    assert output[1]["geometry"]["type"] == "Polygon"
    assert not any(output[1]["geometry"]["coordinates"])
    with pytest.raises(ValueError, match="mutually exclusive"):
        terrio.clean_coverage([polygons, other], layer="polygons", active_layer="polygons")
    with pytest.raises(ValueError, match="not found"):
        terrio.clean_coverage([polygons, other], active_layer="missing")


@pytest.mark.parametrize(
    "kwargs",
    [{"maximum_gap_width": -1}, {"snapping_distance": float("nan")}, {"merge_strategy": "unknown"}],
)
def test_invalid_options(kwargs):
    with pytest.raises(ValueError):
        terrio.clean_coverage(source([]), **kwargs)


def test_active_layer_copy_retains_defined_measures_in_geopackage(tmp_path):
    import sqlite3
    import struct
    from contextlib import closing

    polygons = terrio.Layer.from_geojson(source([rectangle(0, 0, 4, 4)]), name="polygons")
    point = terrio.open_bytes(b'WKT\n"POINT ZM (1 2 3 4)"\n', "point.csv").layers[0]
    result = terrio.clean_coverage([polygons, point], active_layer="polygons")
    path = tmp_path / "clean.gpkg"
    result.write(str(path))
    with closing(sqlite3.connect(path)) as connection:
        assert connection.execute(
            "SELECT m FROM gpkg_geometry_columns WHERE table_name='point'"
        ).fetchone() == (1,)
        geometry = connection.execute("SELECT geom FROM point").fetchone()[0]
        assert struct.unpack("<dddd", geometry[-32:]) == (1, 2, 3, 4)


@pytest.mark.parametrize("geometry", [None, {"type": "Point", "coordinates": [1, 2]}])
def test_active_non_polygonal_features_are_rejected(geometry):
    with pytest.raises(ValueError, match="polygonal geometry"):
        terrio.clean_coverage(source([geometry]))
