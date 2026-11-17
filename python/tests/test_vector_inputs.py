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


def test_paths_and_geometry_text_are_told_apart(tmp_path, monkeypatch):
    with pytest.raises(Exception, match="point.gpkg"):
        terrio.convex_hull(str(tmp_path / "point.gpkg"))
    # Text shaped as geometries that is no file and does not parse names both readings.
    for text in ("POLYGON ((0 0, 1 0", "[draft].geojson"):
        with pytest.raises(ValueError, match="neither an existing file nor valid geometries"):
            terrio.convex_hull(text)
    # An existing file is a path, whatever its name.
    monkeypatch.chdir(tmp_path)
    terrio.Layer.from_geojson(SQUARE).write("[draft].geojson")
    assert _geometries(terrio.convex_hull("[draft].geojson")) == _geometries(
        terrio.convex_hull(SQUARE)
    )


def test_bounds_out_of_order_are_put_in_order_with_a_warning():
    with pytest.warns(UserWarning, match="antimeridian"):
        across = terrio.Layer.from_geojson((170, 0, -170, 2))
    assert across.to_geojson()["features"][0]["geometry"]["type"] == "MultiPolygon"
    assert across.bounds == (-180.0, 0.0, 180.0, 2.0)
    with pytest.warns(UserWarning, match="ymin > ymax"):
        swapped = terrio.convex_hull((0, 2, 2, 0))
    assert _geometries(swapped) == _geometries(terrio.convex_hull(SQUARE))
    # Not longitudes: projected corners swapped.
    with pytest.warns(UserWarning, match="xmin > xmax, read with the two swapped"):
        projected = terrio.Layer.from_geojson((500002, 0, 500000, 2))
    assert projected.bounds == (500000.0, 0.0, 500002.0, 2.0)


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


def test_lists_take_wkt_strings():
    layer = terrio.Layer.from_geojson([WKT, SQUARE])
    assert _geometries(layer) == [SQUARE, SQUARE]


def test_bounds_as_a_clip_geometry():
    clipped = terrio.clip_vector(terrio.Layer.from_geojson(SQUARE), geometry=(0, 0, 1, 1))
    assert clipped.bounds == (0.0, 0.0, 1.0, 1.0)


def test_warp_wkt_cutline_is_in_the_sources_crs(tmp_path):
    """As the CLI's --cutline: WKT has no CRS of its own, so it is in the sources' CRS."""
    src = str(tmp_path / "utm.tif")
    terrio.Raster.from_array(
        np.ones((4, 4), dtype="float32"),
        transform=(500000, 10, 0, 4600040, 0, -10),
        crs="EPSG:32631",
    ).write(src)
    cutline = (
        "POLYGON ((500000 4600000, 500020 4600000, 500020 4600020, 500000 4600020, 500000 4600000))"
    )
    out = terrio.warp(src, cutline=cutline, crop_to_cutline=True, dstnodata=0)
    assert (out.width, out.height) == (2, 2)
    assert out.read(1).tolist() == [[1.0, 1.0], [1.0, 1.0]]


def test_geodataframe_without_crs_is_in_the_arrays_crs():
    gpd = pytest.importorskip("geopandas")
    shapely = pytest.importorskip("shapely")
    pytest.importorskip("xarray")
    from terrio.rio.crs import CRS
    from terrio.xarray import _ops

    gdf = gpd.GeoDataFrame(geometry=[shapely.box(500000, 4600000, 500020, 4600020)])  # no CRS
    layer = _ops.in_crs(gdf, CRS.from_user_input("EPSG:32631"))
    assert isinstance(layer, terrio.Layer) and layer.crs == "EPSG:32631"


def test_clip_bounds_across_the_antimeridian_read_as_everywhere_else():
    """clip_vector's geometry=bounds follows the same rule as the other inputs."""
    points = terrio.Layer.from_geojson(
        [{"type": "Point", "coordinates": [0, 1]}, {"type": "Point", "coordinates": [175, 1]}]
    )
    with pytest.warns(UserWarning, match="antimeridian"):
        clipped = terrio.clip_vector(points, geometry=(170, 0, -170, 2))
    assert _geometries(clipped) == [{"type": "Point", "coordinates": [175.0, 1.0]}]
