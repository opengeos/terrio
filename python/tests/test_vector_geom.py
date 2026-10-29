"""Tests of the vector geometry operations (GDAL's gdal vector simplify, segmentize,
convex-hull, concave-hull, swap-xy, set-geom-type, combine and sort)."""

import json

import pytest

import terrio

LINES = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"k": "a", "v": 1},
            "geometry": {"type": "LineString", "coordinates": [[0, 0], [5, 0.1], [10, 0]]},
        },
        {
            "type": "Feature",
            "properties": {"k": "b", "v": 2},
            "geometry": {"type": "Point", "coordinates": [1, 2]},
        },
        {"type": "Feature", "properties": {"k": "a", "v": 1}, "geometry": None},
    ],
}

POINTS = {
    "type": "Feature",
    "properties": {},
    "geometry": {
        "type": "MultiPoint",
        "coordinates": [[0, 0], [10, 1], [11, 10], [1, 12], [5, 5], [5, 9], [2, 3], [8, 3]],
    },
}


def features(layer):
    """The GeoJSON features of a layer.

    Args:
        layer: The terrio Layer.

    Returns:
        The list of GeoJSON feature dicts.
    """
    return layer.to_geojson()["features"]


def test_simplify_and_segmentize():
    simple = features(terrio.simplify(LINES, 1.0))
    assert simple[0]["geometry"]["coordinates"] == [[0, 0], [10, 0]]
    assert simple[2]["geometry"] is None
    assert simple[0]["properties"] == {"k": "a", "v": 1}
    dense = features(terrio.segmentize(LINES, 3.0))
    assert len(dense[0]["geometry"]["coordinates"]) == 5
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.simplify(LINES, -1.0)


def test_hulls():
    hull = features(terrio.convex_hull(LINES))
    assert hull[0]["geometry"]["type"] == "Polygon"
    assert hull[1]["geometry"]["type"] == "Point"
    concave = features(terrio.concave_hull(POINTS, 0.0))[0]["geometry"]
    assert concave["type"] == "Polygon"
    # GEOS 3.14 gives the same 8 vertices and an area of 61.
    assert len(concave["coordinates"][0]) == 9
    # GEOS observation: ratio 0.1 leaves these two triangles disconnected.
    triangles = {
        "type": "Feature",
        "properties": {},
        "geometry": {
            "type": "MultiPolygon",
            "coordinates": [
                [[[18, -5], [15, -6], [12, -2], [18, -5]]],
                [[[8, 3], [13, 3], [10, 4], [8, 3]]],
            ],
        },
    }
    result = features(terrio.concave_hull(triangles, 0.1))[0]["geometry"]
    assert result["type"] == "MultiPolygon"
    assert all(len(polygon) == 1 for polygon in result["coordinates"])
    assert {frozenset(map(tuple, p[0])) for p in result["coordinates"]} == {
        frozenset(map(tuple, p[0])) for p in triangles["geometry"]["coordinates"]
    }
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.concave_hull(POINTS, 2.0)


def test_swap_xy_and_set_geom_type():
    assert features(terrio.swap_xy(LINES))[1]["geometry"]["coordinates"] == [2, 1]
    multi = terrio.set_geom_type(LINES, multi=True)
    assert features(multi)[0]["geometry"]["type"] == "MultiLineString"
    assert len(features(terrio.set_geom_type(LINES, geometry_type="POINT", skip=True))) == 2
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.set_geom_type(LINES, geometry_type="POINT", multi=True)
    with pytest.raises(terrio.UnsupportedError):
        terrio.set_geom_type(LINES, geometry_type="CURVEPOLYGON")


def test_combine_and_sort():
    groups = features(terrio.combine(LINES, group_by=["k"], add_extra_fields="always-identical"))
    assert [f["properties"]["k"] for f in groups] == ["a", "b"]
    assert groups[0]["properties"]["v"] == 1
    assert groups[0]["geometry"]["type"] == "GeometryCollection"
    ordered = features(terrio.sort(LINES, method="strtree"))
    assert ordered[2]["geometry"] is None
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.sort(LINES, method="nope")


def test_convert_simplify(tmp_path):
    src = tmp_path / "lines.geojson"
    src.write_text(json.dumps(LINES))
    dst = tmp_path / "simple.geojson"
    terrio.convert(str(src), str(dst), simplify=1.0)
    out = json.loads(dst.read_text())["features"]
    assert out[0]["geometry"]["coordinates"] == [[0, 0], [10, 0]]
