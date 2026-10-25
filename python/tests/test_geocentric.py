"""Public Python geocentric XYZ support against pinned GDAL/PROJ references."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2]
        / "crates/terrio-crs/tests/data/geocentric_reference.json"
    ).read_text()
)


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda c: c["name"])
def test_xyz_coordinate_oracles(case):
    points = np.array(case["points"])
    actual = np.column_stack(
        terrio.transform_points(
            points[:, 0],
            points[:, 1],
            case["src"],
            case["dst"],
            points[:, 2],
            s_coord_epoch=case.get("source_epoch"),
        )
    )
    assert np.all(np.abs(actual - np.array(case["expected"])) <= np.array(case["tolerance"]))


def test_geocentric_vector_retains_z():
    layer = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": {"type": "Point", "coordinates": [147, -42, 100]},
                    "properties": {"id": 7},
                }
            ],
        }
    )
    cartesian = layer.to_crs("EPSG:4978")
    coordinates = cartesian.to_geojson()["features"][0]["geometry"]["coordinates"]
    np.testing.assert_allclose(
        coordinates,
        [-3981230.6461390946, 2585441.4118733606, -4245670.74916175],
        atol=0.001,
        rtol=0,
    )
    assert cartesian.crs == "EPSG:4978"
