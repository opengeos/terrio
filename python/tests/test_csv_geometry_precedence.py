"""CSV geometry survives coordinate-like attributes and explicit WKT selections."""

import csv
import json
import subprocess
import sys

import pytest

import terrio
import terrio.pyogrio as tpg

pytestmark = pytest.mark.filterwarnings("ignore:CSV does not store:UserWarning")

GEOMETRIES = [
    {"type": "Point", "coordinates": [10, 46]},
    {"type": "LineString", "coordinates": [[10, 46], [11, 47]]},
    {"type": "Polygon", "coordinates": [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]]},
]
PAIRS = [
    ("lon", "lat"),
    ("x", "y"),
    ("Longitude", "Latitude"),
    ("easting", "northing"),
    ("lng", "lat"),
    ("long", "latitude"),
    ("xcoord", "ycoord"),
    ("x_coord", "y_coord"),
    ("point_x", "point_y"),
]


def read_feature(path, surface):
    """Read one feature through a public read or conversion entry point.

    Args:
        path: CSV or TSV written by the shared encoder.
        surface: Whole read, stream, pyogrio dataframe, or conversion CLI.

    Returns:
        dict: Geometry and properties in GeoJSON representation.
    """
    if surface == "pyogrio":
        pytest.importorskip("geopandas")
        frame = tpg.read_dataframe(str(path))
        geometry = json.loads(json.dumps(frame.geometry.iloc[0].__geo_interface__))
        properties = frame.iloc[0].drop(labels=[frame.geometry.name]).to_dict()
        return {"geometry": geometry, "properties": properties}
    if surface == "convert":
        output = path.with_suffix(".geojson")
        result = subprocess.run(
            [sys.executable, "-m", "terrio", "convert", str(path), str(output)],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, result.stderr
        return json.loads(output.read_text())["features"][0]
    layer = (
        terrio.open_stream(str(path)).read_all()
        if surface == "stream"
        else terrio.open_vector(str(path))[0]
    )
    return layer.to_geojson()["features"][0]


@pytest.mark.parametrize("surface", ["path", "stream", "pyogrio", "convert"])
@pytest.mark.parametrize("extension", ["csv", "tsv"])
@pytest.mark.parametrize("pair", PAIRS)
@pytest.mark.parametrize("geometry", GEOMETRIES, ids=["point", "line", "polygon"])
def test_written_geometry_precedes_coordinate_named_attributes(
    tmp_path, surface, extension, pair, geometry
):
    """Keep the original geometry and all coordinate-like property values.

    Args:
        tmp_path: Temporary dataset directory.
        surface: Public read entry point.
        extension: Delimited-text output format.
        pair: Attribute names that resemble coordinate columns.
        geometry: Geometry whose coordinates differ from its attributes.
    """
    properties = {"tag": "original", pair[0]: 650000.5, pair[1]: 5100000.25}
    layer = terrio.Layer.from_geojson(
        {"type": "Feature", "properties": properties, "geometry": geometry},
        name="features",
        crs="EPSG:4326",
    )
    expected = layer.to_geojson()["features"][0]["geometry"]
    path = tmp_path / f"features.{extension}"
    layer.write(str(path))
    feature = read_feature(path, surface)
    assert feature["geometry"] == expected
    assert {key: feature["properties"][key] for key in properties} == properties


@pytest.mark.parametrize("surface", ["path", "stream", "pyogrio", "convert"])
def test_geometry_marker_avoids_existing_attribute_names(tmp_path, surface):
    """Allocate a distinct geometry marker without renaming marker-like attributes.

    Args:
        tmp_path: Temporary dataset directory.
        surface: Public read entry point.
    """
    properties = {"lon": 12.5, "lat": 48.5, "__terrio_wkt": "first", "__terrio_wkt_2": "second"}
    layer = terrio.Layer.from_geojson(
        {"type": "Feature", "properties": properties, "geometry": GEOMETRIES[1]}, crs="EPSG:4326"
    )
    path = tmp_path / "markers.csv"
    layer.write(str(path))
    assert next(csv.reader(path.read_text().splitlines()))[0] == "__terrio_wkt_3"
    feature = read_feature(path, surface)
    assert feature["geometry"] == GEOMETRIES[1]
    assert {key: feature["properties"][key] for key in properties} == properties


@pytest.mark.parametrize("extension,separator", [("csv", ","), ("tsv", "\t")])
@pytest.mark.parametrize("column,pattern", [("WKT", "WKT"), ("shape_text", "shape_*")])
@pytest.mark.parametrize("explicit", [False, True])
def test_explicit_wkt_names_precede_inferred_coordinates(
    tmp_path, extension, separator, column, pattern, explicit
):
    """An explicit WKT selection overrides automatic X/Y inference.

    Args:
        tmp_path: Temporary dataset directory.
        extension: Delimited-text format.
        separator: Field separator.
        column: Geometry field name.
        pattern: Explicit geometry-name pattern.
        explicit: Whether to request the WKT geometry instead of default X/Y inference.
    """
    path = tmp_path / f"selected.{extension}"
    with path.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.writer(stream, delimiter=separator)
        writer.writerow([column, "lon", "lat"])
        writer.writerow(["LINESTRING (10 46,11 47)", 650000.5, 5100000.25])
    options = {"GEOM_POSSIBLE_NAMES": pattern} if explicit else {}
    feature = terrio.open_vector(str(path), options=options)[0].to_geojson()["features"][0]
    expected = (
        GEOMETRIES[1] if explicit else {"type": "Point", "coordinates": [650000.5, 5100000.25]}
    )
    assert feature["geometry"] == expected
    assert feature["properties"]["lon"] == 650000.5
    assert feature["properties"]["lat"] == 5100000.25
