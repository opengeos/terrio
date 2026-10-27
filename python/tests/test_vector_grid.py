"""Vector gridding (gdal_grid) and linear referencing (ogrlineref) bindings."""

import json

import numpy as np
import pytest

import terrio


def points_geojson(values):
    """A GeoJSON FeatureCollection of points with a `v` property.

    Args:
        values: ``(x, y, v)`` tuples.

    Returns:
        dict: The GeoJSON object.
    """
    features = [
        {
            "type": "Feature",
            "properties": {"v": v},
            "geometry": {"type": "Point", "coordinates": [x, y]},
        }
        for x, y, v in values
    ]
    return {"type": "FeatureCollection", "features": features}


def test_vector_grid_nearest_cell_centres(tmp_path):
    src = points_geojson([(0.5, 0.5, 1.0), (1.5, 0.5, 2.0), (0.5, 1.5, 3.0), (1.5, 1.5, 4.0)])
    path = str(tmp_path / "grid.tif")
    r = terrio.vector_grid(
        src, path, algorithm="nearest", zfield="v", extent=(0, 0, 2, 2), size=(2, 2)
    )
    np.testing.assert_array_equal(r.read(1), [[3.0, 4.0], [1.0, 2.0]])
    np.testing.assert_array_equal(terrio.open(path).read(1), r.read(1))
    # No nodata declared unless given, as gdal_grid.
    assert r.nodata is None
    assert (
        terrio.vector_grid(src, algorithm="nearest", zfield="v", size=(2, 2), nodata=-1).nodata
        == -1
    )


def test_vector_grid_algorithms_and_parameters():
    src = points_geojson([(1, 1, 1.0), (-1, 1, 2.0), (-1, -1, 3.0), (1, -1, 4.0), (2, 2, 10.0)])
    common = dict(zfield="v", extent=(-0.5, -0.5, 0.5, 0.5), size=(1, 1))
    assert terrio.vector_grid(src, algorithm="minimum", radius=5, **common).read(1)[0, 0] == 1.0
    assert terrio.vector_grid(src, algorithm="maximum:radius=5", **common).read(1)[0, 0] == 10.0
    assert terrio.vector_grid(src, algorithm="count", radius=5, **common).read(1)[0, 0] == 5.0
    avg = terrio.vector_grid(
        src, algorithm="average", radius=5, max_points_per_quadrant=1, **common
    )
    assert avg.read(1)[0, 0] == 2.5
    # An unrotated ellipse that only reaches along y misses every point.
    count = terrio.vector_grid(
        src, algorithm="count", radius1=0.1, radius2=5, angle=0, **common
    ).read(1)[0, 0]
    assert count == 0.0
    idw = terrio.vector_grid(src, power=1, **common).read(1)[0, 0]
    d = np.sqrt([2, 2, 2, 2, 8])
    v = np.array([1.0, 2, 3, 4, 10])
    assert idw == pytest.approx((v / d).sum() / (1 / d).sum(), rel=1e-12)
    kwargs = dict(min_points_per_quadrant=1, max_points=5)
    keyword = terrio.vector_grid(src, algorithm="invdist", **common, **kwargs)
    string = terrio.vector_grid(
        src, algorithm="invdist:min_points_per_quadrant=1:max_points=5", **common
    )
    np.testing.assert_array_equal(keyword.read(1), string.read(1))
    # The square's diagonal is a Delaunay tie: either triangle's plane.
    assert terrio.vector_grid(src, algorithm="linear", **common).read(1)[0, 0] in (2.0, 2.5, 3.0)
    with pytest.raises(ValueError):
        terrio.vector_grid(src, algorithm="kriging", **common)
    with pytest.raises(ValueError):
        terrio.vector_grid(src, algorithm="nearest", power=2, **common)
    with pytest.raises(ValueError):
        terrio.vector_grid(src, algorithm="invdist", max_points=3, **common)


def test_vector_grid_options(tmp_path):
    src = points_geojson([(0.5, 0.5, 1.0), (1.5, 1.5, 2.0), (9, 9, 100.0)])
    named = terrio.Layer.from_geojson(src, name="a")
    with pytest.raises(ValueError, match="missing"):
        terrio.vector_grid(named, algorithm="nearest", layers=["a", "missing"])
    path = str(tmp_path / "single.geojson")
    named.write(path)
    with pytest.raises(ValueError, match="missing"):
        terrio.vector_grid(path, algorithm="nearest", layers=["single", "missing"])
    assert terrio.vector_grid(path, algorithm="nearest", layers=["single"], zfield="v").count == 1
    r = terrio.vector_grid(
        src,
        algorithm="nearest",
        zfield="v",
        zoffset=1,
        zmultiply=10,
        bbox=(0, 0, 5, 5),
        extent=(0, 0, 2, 2),
        resolution=(1, 1),
        ot="Int16",
        crs="EPSG:3857",
    )
    assert r.read(1).dtype == np.int16
    # The north-west node is as near to both points: the later one, as GDAL.
    np.testing.assert_array_equal(r.read(1), [[30, 30], [20, 30]])
    assert "3857" in str(r.crs)
    w = terrio.vector_grid(
        src, algorithm="nearest", zfield="v", where="v < 50", extent=(0, 0, 2, 2), size=(2, 2)
    )
    assert w.read(1).max() == 2.0


def lineref_inputs():
    line = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {},
                "geometry": {
                    "type": "LineString",
                    "coordinates": [[0, 0], [1000, 0], [1000, 1000]],
                },
            }
        ],
    }
    stones = json.loads(
        json.dumps(
            points_geojson([(200, 0, 1200.0), (1000, 200, 2000.0), (1000, 700, 2500.0)])
        ).replace('"v"', '"pos"')
    )
    return (
        terrio.Layer.from_geojson(line, crs="EPSG:3857"),
        terrio.Layer.from_geojson(stones, crs="EPSG:3857"),
    )


def test_lineref(tmp_path):
    # The route and milestones are in EPSG:3857, with distances in metres.
    lines, stones = lineref_inputs()
    path = str(tmp_path / "parts.gpkg")
    parts = terrio.lineref_create(lines, stones, "pos", 500, path)
    assert len(parts) == 4
    rows = [
        tuple(f["properties"][k] for k in ("beg", "end", "scale"))
        for f in parts.to_geojson()["features"]
    ]
    assert rows == [
        (1000.0, 1500.0, 1.15),
        (1500.0, 2000.0, 1.25),
        (2000.0, 2500.0, 1.0),
        (2500.0, 2800.0, 1.0),
    ]
    assert terrio.lineref_position(path, 100, 0) == pytest.approx(1000 + 100 / 1.15)
    dataset = terrio.open_vector(path)
    assert terrio.lineref_position(parts, 100, 0) == pytest.approx(1000 + 100 / 1.15)
    assert terrio.lineref_position(dataset, 100, 0) == pytest.approx(1000 + 100 / 1.15)
    with pytest.raises(ValueError, match="no layer"):
        terrio.lineref_position(dataset, 100, 0, layer="missing")
    ((x, y, z),) = terrio.lineref_coordinates(path, 2250)
    assert (x, y, z) == pytest.approx((1000, 450, 0))
    sub = terrio.lineref_subline(parts, 1100, 2100)
    coords = sub.to_geojson()["features"][0]["geometry"]["coordinates"]
    assert coords[0] == pytest.approx([115, 0])
    assert coords[-1] == pytest.approx([1000, 300])
    with pytest.raises(ValueError):
        terrio.lineref_create(lines, stones, "missing", 500)


def test_explicit_empty_layer_selection_is_rejected(tmp_path):
    layer = terrio.Layer.from_geojson(points_geojson([(0, 0, 1), (1, 1, 2)]), name="points")
    path = str(tmp_path / "points.geojson")
    terrio.write_vector([layer], path)
    for source in [layer, path]:
        with pytest.raises(terrio.InvalidArgumentError, match="at least one layer name"):
            terrio.vector_grid(source, algorithm="nearest", layers=[])


def test_keyword_zero_radius_matches_packed_error():
    source = points_geojson([(0, 0, 1), (1, 1, 2)])
    for algorithm, options in [
        ("invdist:radius1=0:radius2=0:min_points_per_quadrant=1", {}),
        ("invdist", {"radius1": 0, "radius2": 0, "min_points_per_quadrant": 1}),
    ]:
        with pytest.raises(terrio.InvalidArgumentError, match="strictly positive"):
            terrio.vector_grid(source, algorithm=algorithm, **options)


def test_existing_sql_layer_selection_remains_permissive(tmp_path):
    """Strict selection belongs to the new gridding/lineref entry points."""
    layer = terrio.Layer.from_geojson(points_geojson([(1, 1, 2.0)]), name="points")
    path = str(tmp_path / "points.geojson")
    layer.write(path)
    for source in (layer, path):
        selected = terrio.sql(source, "SELECT * FROM points", layers=["points", "missing"])
        assert len(selected) == 1
