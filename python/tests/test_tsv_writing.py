"""TSV filename defaults preserve fields and geometries on every public writer."""

import json
import subprocess
import sys

import pytest

import terrio


@pytest.mark.parametrize("polygon", [False, True])
@pytest.mark.parametrize("surface", ["layer", "dataset", "bytes", "convert", "cli"])
def test_tsv_roundtrip_uses_tabs(tmp_path, polygon, surface):
    """Round-trip a filename-selected TSV through a public writing surface.

    Args:
        tmp_path: Directory for the input and output datasets.
        polygon: Whether the fixture geometry has commas in its WKT.
        surface: The public writing surface being exercised.
    """
    geometry = (
        {"type": "Polygon", "coordinates": [[[0, 0], [2, 0], [2, 2], [0, 0]]]}
        if polygon
        else {"type": "Point", "coordinates": [10, 46]}
    )
    document = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"id": 1, "name": 'a, tab\tand "quote"\nnext line'},
                "geometry": geometry,
            }
        ],
    }
    source = tmp_path / "source.geojson"
    source.write_text(json.dumps(document))
    dataset = terrio.open_vector(str(source))
    layer = dataset[0]
    output = tmp_path / "shapes.tsv"
    if surface == "layer":
        layer.write(str(output))
    elif surface == "dataset":
        dataset.write(str(output))
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
        assert result.returncode == 0, result.stderr
    assert output.read_bytes().startswith(b"WKT\tid\tname\n")
    layer.write(str(tmp_path / "control.csv"))
    reference = terrio.open_vector(str(tmp_path / "control.csv"))[0]
    actual = terrio.open_vector(str(output))[0]
    assert actual.fields == reference.fields
    assert actual.to_geojson()["features"] == reference.to_geojson()["features"]
    assert actual.to_geojson()["features"][0]["geometry"] == geometry
