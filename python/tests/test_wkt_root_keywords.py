"""Engineering and long vertical WKT keywords against GDAL 3.13.3."""

import json
import subprocess
import sys
from pathlib import Path

import numpy as np
import pytest

import terrio
from terrio.rio.crs import CRS

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2]
        / "crates/terrio-crs/tests/data/wkt_keywords_reference.json"
    ).read_text()
)["cases"]


@pytest.mark.parametrize("case", REFERENCE, ids=lambda case: case["name"])
@pytest.mark.parametrize("entry", ["from_wkt", "from_user_input", "from_string"])
def test_wkt_root_preserves_crs_identity(case, entry, gdal):
    osr = pytest.importorskip("osgeo.osr")
    actual = getattr(CRS, entry)(case["wkt"])
    assert actual.to_epsg() == case["epsg"]
    reference, restored = osr.SpatialReference(), osr.SpatialReference()
    reference.ImportFromWkt(case["wkt"])
    restored.ImportFromWkt(actual.to_wkt())
    assert bool(restored.IsLocal()) == case["local"]
    assert bool(restored.IsVertical()) == case["vertical"]
    assert restored.IsSame(reference)


@pytest.mark.parametrize("case", REFERENCE, ids=lambda case: case["name"])
def test_wkt_keywords_work_for_raster_and_layer_construction(case, gdal):
    osr = pytest.importorskip("osgeo.osr")
    reference = osr.SpatialReference()
    reference.ImportFromWkt(case["wkt"])
    values = np.arange(6, dtype="uint8").reshape(2, 3)
    raster = terrio.Raster.from_array(values, (0, 1, 0, 2, 0, -1), crs=case["wkt"])
    assert CRS.from_user_input(raster.crs).to_epsg() == case["epsg"]
    np.testing.assert_array_equal(raster.read(1), values)
    points = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [1, 2]},
                "properties": {"id": 7},
            }
        ],
    }
    layer = terrio.Layer.from_geojson(points, crs=case["wkt"])
    assert CRS.from_user_input(layer.crs).to_epsg() == case["epsg"]
    for definition in (raster.crs, layer.crs):
        restored = osr.SpatialReference()
        restored.ImportFromWkt(CRS.from_user_input(definition).to_wkt())
        assert restored.IsSame(reference)
    assert layer.to_geojson()["features"][0]["geometry"]["coordinates"] == [1, 2]


@pytest.mark.parametrize("case", REFERENCE, ids=lambda case: case["name"])
def test_cli_reads_wkt_keyword_from_literal_and_file(case, tmp_path):
    path = tmp_path / "definition.wkt"
    path.write_text(case["wkt"])
    for source in [case["wkt"], str(path)]:
        result = subprocess.run(
            [sys.executable, "-m", "terrio", "crs", source, "--wkt"],
            capture_output=True,
            text=True,
            timeout=20,
            check=True,
        )
        assert case["wkt"] in result.stdout
        assert "Geocentric: true" not in result.stdout
