"""Check the public Python CRS API against the pinned projection oracle (#266)."""

import json
import math
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2] / "crates/terrio-crs/tests/data/crs_benchmark.json"
    ).read_text()
)


@pytest.mark.parametrize("case", REFERENCE["projections"], ids=lambda case: case["id"])
def test_projection_benchmark(case):
    """Forward and inverse array transforms retain the core's oracle tolerances."""
    definition = case["proj"]
    if case["id"] == "wintri":
        definition += f" +lat_1={math.degrees(math.acos(2 / math.pi))}"
    for i, point in enumerate(case["points"]):
        if case["fwd"][i] is None:
            with pytest.raises(ValueError):
                terrio.transform_points([point[0]], [point[1]], case["geographic"], definition)
        elif case["inv"][i] is None:
            try:
                lo, la = terrio.transform_points(
                    [case["fwd"][i][0]], [case["fwd"][i][1]], definition, case["geographic"]
                )
            except ValueError:
                continue
            dl = ((lo[0] - point[0] + 180) % 360 - 180) * math.cos(math.radians(point[1]))
            tolerance = 1e-6 if abs(point[1]) == 90 else 1e-7
            assert math.hypot(dl, la[0] - point[1]) < tolerance
    valid = [i for i, xy in enumerate(case["fwd"]) if xy is not None]
    points = np.array([case["points"][i] for i in valid])
    expected = np.array([case["fwd"][i] for i in valid])
    x, y = terrio.transform_points(points[:, 0], points[:, 1], case["geographic"], definition)
    assert np.all(np.hypot(x - expected[:, 0], y - expected[:, 1]) < 1e-3)

    valid = [i for i in valid if case["inv"][i] is not None]
    xy = np.array([case["fwd"][i] for i in valid])
    expected = np.array([case["inv"][i] for i in valid])
    lon, lat = terrio.transform_points(xy[:, 0], xy[:, 1], definition, case["geographic"])
    dlon = ((lon - expected[:, 0] + 180) % 360 - 180) * np.cos(np.radians(expected[:, 1]))
    # At a pole, one last bit before asin shifts the reference latitude by 8.5e-7°.
    tolerance = np.array([1e-6 if abs(case["points"][i][1]) == 90 else 1e-7 for i in valid])
    assert np.all(np.hypot(dlon, lat - expected[:, 1]) < tolerance)


@pytest.mark.parametrize(
    "reference",
    [
        "urn:ogc:def:crs:EPSG:6.6:4326",
        "urn:ogc:def:crs:EPSG:9.9:4326",
        "http://www.opengis.net/def/crs/EPSG/0/4326",
        "https://opengis.net/def/crs/EPSG/6.6/4326",
    ],
)
def test_versioned_authority_references(reference):
    """The version token is independent of the code, as in PROJ 9.8.1."""
    (x,), (y,) = terrio.transform_points([10.0], [40.0], reference, "EPSG:3857")
    assert abs(x - 1113194.9079327357) < 1e-7
    assert abs(y - 4865942.279503175) < 1e-7
