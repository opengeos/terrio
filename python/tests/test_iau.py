"""IAU registry coordinates compared with the full frozen GDAL/PROJ oracle."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio
from terrio.rio.crs import CRS

ROOT = Path(__file__).resolve().parents[2]
REFERENCE = json.loads((ROOT / "crates/terrio-crs/tests/data/iau_reference.json").read_text())


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda c: str(c["code"]))
def test_iau_projected_coordinates(case):
    source, target = f"IAU_2015:{case['base']}", f"IAU_2015:{case['code']}"
    points = np.asarray(case["points"])
    result = np.column_stack(terrio.transform_points(points[:, 0], points[:, 1], source, target))
    assert np.all(np.abs(result - case["forward"]) <= 0.001)
    projected = np.asarray(case["forward"])
    for i, expected in enumerate(case["inverse"]):
        if expected is None:
            continue  # Domain refusals/recovery are covered by the native suite.
        actual = np.array(
            terrio.transform_points([projected[i, 0]], [projected[i, 1]], target, source)
        ).ravel()
        delta = actual - expected
        # GDAL and Terrio can choose opposite representations at the antimeridian.
        longitude_axis = case["longitude_axis"]
        delta[longitude_axis] = (delta[longitude_axis] + 180) % 360 - 180
        assert np.all(np.abs(delta) <= 1e-8)


def test_iau_authority_versions_and_body_checks():
    for definition in ["IAU_2015:49900", "IAU:2015:49900", "urn:ogc:def:crs:IAU:2015:49900"]:
        crs = CRS.from_string(definition)
        assert crs.to_authority() == ("IAU_2015", "49900")
        assert crs.to_epsg() is None
        assert CRS.from_wkt(crs.to_wkt()).to_authority() == ("IAU_2015", "49900")
    for definition in ["IAU_2000:49900", "IAU_2015:999999999"]:
        with pytest.raises(terrio.CRSError):
            terrio.transform_points([0], [0], definition, "IAU_2015:49910")
    with pytest.raises(terrio.CRSError, match="celestial body"):
        terrio.transform_points([0], [0], "IAU_2015:49900", "EPSG:4326")


@pytest.mark.parametrize("case", REFERENCE["extras"], ids=lambda c: c["name"])
def test_iau_units_and_offsets(case):
    points = np.asarray(case["points"])
    actual = np.column_stack(
        terrio.transform_points(points[:, 0], points[:, 1], case["source"], case["target"])
    )
    assert np.all(np.abs(actual - case["forward"]) * case["metres_per_unit"] <= 0.001)
    projected = np.asarray(case["forward"])
    actual = np.column_stack(
        terrio.transform_points(projected[:, 0], projected[:, 1], case["target"], case["source"])
    )
    assert np.all(np.abs(actual - case["inverse"]) <= 1e-8)


def test_iau_strict_datum_policy():
    with pytest.raises(terrio.CRSError, match="ALLOW_BALLPARK"):
        terrio.transform_points(
            [45], [-10], "IAU_2015:49900", "IAU_2015:49901", allow_ballpark=False
        )
    x, y = terrio.transform_points(
        [45], [-10], "IAU_2015:49901", "IAU_2015:49902", allow_ballpark=False
    )
    assert abs(x[0] - 10) < 1e-8
    assert abs(y[0] - 44.6617680466192) < 1e-8
