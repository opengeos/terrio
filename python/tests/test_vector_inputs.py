"""Every operation taking vector data takes it in any form: paths, GeoPandas, Shapely, GeoJSON,
WKT, bounds or Arrow data."""

import json
import pathlib

import numpy as np
import pytest

import terrio

SQUARE = {"type": "Polygon", "coordinates": [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]]}
WKT = "POLYGON ((0 0, 2 0, 2 2, 0 2, 0 0))"


def _geometries(layer):
    return [f["geometry"] for f in layer.to_geojson()["features"]]


@pytest.fixture
def path(tmp_path):
    p = tmp_path / "square.geojson"
    terrio.Layer.from_geojson(SQUARE).write(str(p))
    return p


def test_vector_inputs_read_as_the_same_layer(path):
    expected = _geometries(terrio.convex_hull(SQUARE))
    inputs = {
        "str path": str(path),
        "pathlib.Path": path,
        "GeoJSON text": json.dumps({"type": "Feature", "properties": {}, "geometry": SQUARE}),
        "WKT": WKT,
        "EWKT": "SRID=4326;" + WKT,
        "bounds": (0, 0, 2, 2),
        "bounds as a NumPy array": np.array([0.0, 0.0, 2.0, 2.0]),
        "list of geometries": [SQUARE],
        "list of Features": [{"type": "Feature", "properties": {}, "geometry": SQUARE}],
    }
    for name, value in inputs.items():
        layer = terrio.convex_hull(value)
        assert _geometries(layer) == expected, name
        assert layer.crs == "EPSG:4326", name


def test_shapely_and_geopandas_inputs():
    shapely = pytest.importorskip("shapely")
    gpd = pytest.importorskip("geopandas")
    box = shapely.box(0, 0, 2, 2)
    expected = _geometries(terrio.convex_hull(box))
    assert _geometries(terrio.convex_hull([box, box])) == expected * 2
    gdf = gpd.GeoDataFrame(geometry=[box], crs="EPSG:3857")
    for value in (gdf, gdf.geometry):
        layer = terrio.convex_hull(value)
        assert _geometries(layer) == expected
        assert layer.crs == "EPSG:3857"
    # One clip geometry as a Shapely object: WKT, in the input's CRS.
    clipped = terrio.clip_vector(gdf, geometry=shapely.box(0, 0, 1, 1))
    assert clipped.bounds == (0.0, 0.0, 1.0, 1.0)


def test_paths_and_geometry_text_are_told_apart(tmp_path):
    with pytest.raises(Exception, match="point.gpkg"):
        terrio.convex_hull(str(tmp_path / "point.gpkg"))
    with pytest.raises(ValueError, match="xmin, ymin, xmax, ymax"):
        terrio.convex_hull((2, 0, 0, 2))


def test_zonal_stats_zones_as_bounds_and_wkt(tmp_path):
    raster = terrio.Raster.from_array(
        np.arange(16, dtype="float32").reshape(4, 4), transform=(0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    src = str(tmp_path / "r.tif")
    raster.write(src)
    by_layer = terrio.zonal_stats(terrio.Layer.from_geojson(SQUARE), src, stats=["sum"]).columns()
    for zones in ((0, 0, 2, 2), WKT, pathlib.Path(src).with_suffix(".geojson")):
        if isinstance(zones, pathlib.Path):
            terrio.Layer.from_geojson(SQUARE).write(str(zones))
        assert terrio.zonal_stats(zones, src, stats=["sum"]).columns() == by_layer, zones
