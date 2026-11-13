"""Named JSON CRS metadata never silently relabels retained coordinates as WGS 84."""

import json
import subprocess
import sys

import pytest
from test_geojson_write_crs import CUSTOM, LOCAL, POINTS, PROJ

import terrio


@pytest.mark.parametrize(
    "definition", ["ESRI:102003", "EPSG:32615", "IAU_2015:49900", CUSTOM, PROJ, LOCAL]
)
@pytest.mark.parametrize("surface", ["layer", "bytes", "convert", "cli"])
def test_topojson_writer_names_or_refuses_crs(tmp_path, definition, surface):
    """Keep the source CRS and coordinates, or refuse before creating an output.

    Args:
        tmp_path: Temporary file directory.
        definition: Authority CRS or unnamed stored definition.
        surface: Public write entry point.
    """
    layer = terrio.Layer.from_geojson(POINTS, name="points", crs=definition)
    source = tmp_path / "source.gpkg"
    layer.write(str(source))
    output = tmp_path / "points.topojson"
    named = definition.startswith(("EPSG:", "ESRI:", "IAU_2015:"))

    def write():
        """Write the point fixture through the selected public entry point."""
        if surface == "layer":
            layer.write(str(output))
        elif surface == "bytes":
            output.write_bytes(layer.to_bytes(output.name))
        elif surface == "convert":
            terrio.convert(str(source), str(output))
        else:
            result = subprocess.run(
                [sys.executable, "-m", "terrio", "convert", str(source), str(output)],
                capture_output=True,
                text=True,
                timeout=30,
            )
            assert (result.returncode == 0) == named, result.stderr
            if not named:
                assert "TopoJSON cannot name" in result.stderr

    if named:
        write()
        authority, code = definition.split(":")
        assert (
            json.loads(output.read_text())["crs"]["properties"]["name"]
            == f"urn:ogc:def:crs:{authority}::{code}"
        )
        back = terrio.open_vector(str(output))[0]
        assert back.crs == definition
        assert back.to_geojson()["features"][0]["geometry"]["coordinates"] == [12000, 34000]
    else:
        if surface == "cli":
            write()
        else:
            with pytest.raises(terrio.UnsupportedError, match="TopoJSON cannot name"):
                write()
        assert not output.exists()


def document(extension, coordinate=5000):
    """Build a point document with an optional topology container.

    Args:
        extension: JSON format extension.
        coordinate: Point x and y in the unchanged source coordinate system.

    Returns:
        dict: A GeoJSON collection or TopoJSON topology.
    """
    point = {"type": "Point", "coordinates": [coordinate, coordinate]}
    if extension == "topojson":
        return {"type": "Topology", "arcs": [], "objects": {"points": point}}
    return {
        "type": "FeatureCollection",
        "features": [{"type": "Feature", "properties": {}, "geometry": point}],
    }


@pytest.mark.parametrize("extension", ["geojson", "topojson"])
@pytest.mark.parametrize(
    "name", ["urn:ogc:def:crs:EPSG::5800", "urn:ogc:def:crs:FOO::1", "not a crs"]
)
@pytest.mark.parametrize("surface", ["path", "bytes", "stream"])
def test_unknown_crs_member_is_refused(tmp_path, extension, name, surface):
    """Refuse unresolved metadata rather than applying the no-member CRS default.

    Args:
        tmp_path: Temporary file directory.
        extension: JSON format.
        name: Unresolvable authority or malformed CRS name.
        surface: Public vector-read entry point.
    """
    doc = document(extension)
    doc["crs"] = {"type": "name", "properties": {"name": name}}
    data = json.dumps(doc).encode()
    path = tmp_path / f"points.{extension}"
    path.write_bytes(data)
    with pytest.raises(terrio.InvalidDataError, match="cannot resolve crs member"):
        if surface == "path":
            terrio.open_vector(str(path))
        elif surface == "bytes":
            terrio.open_bytes(data, path.name)
        else:
            terrio.open_stream(str(path))


@pytest.mark.parametrize("extension", ["geojson", "topojson"])
def test_explicit_null_crs_stays_unknown(tmp_path, extension):
    """Preserve an explicitly unknown CRS even when coordinates resemble longitude/latitude.

    Args:
        tmp_path: Temporary file directory.
        extension: JSON format.
    """
    doc = document(extension, 10)
    doc["crs"] = None
    path = tmp_path / f"points.{extension}"
    path.write_text(json.dumps(doc))
    assert terrio.open_vector(str(path))[0].crs is None
    assert terrio.open_stream(str(path)).crs is None


@pytest.mark.parametrize("coordinate,expected", [(10, "EPSG:4326"), (5000, None)])
def test_topology_without_crs_uses_coordinate_domain(tmp_path, coordinate, expected):
    """Infer WGS 84 only when every topology coordinate fits longitude and latitude.

    Args:
        tmp_path: Temporary file directory.
        coordinate: Point coordinates in the topology.
        expected: CRS inferred from the coordinate domain.
    """
    path = tmp_path / "points.topojson"
    path.write_text(json.dumps(document("topojson", coordinate)))
    assert terrio.open_vector(str(path))[0].crs == expected
    assert terrio.open_stream(str(path)).crs == expected
