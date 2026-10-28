"""Native quadrilateralized spherical cube across the public Python surface."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2] / "crates/terrio-crs/tests/data/qsc_reference.json"
    ).read_text()
)


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda case: case["name"])
def test_qsc_oracle(case):
    # PROJ refuses no qsc point (the generator checks it), so every sample has its values.
    samples = case["samples"]
    for target in [case["target"], case["wkt2"], case["wkt1"], case["wkt1_esri"]]:
        actual = np.column_stack(
            terrio.transform_points(
                [s[0] for s in samples], [s[1] for s in samples], case["source"], target
            )
        )
        assert np.all(np.abs(actual - [s[2] for s in samples]) * case["metres_per_unit"] <= 0.001)
    actual = np.column_stack(
        terrio.transform_points(
            [s[2][0] for s in samples], [s[2][1] for s in samples], case["target"], case["source"]
        )
    )
    expected = np.asarray([s[3] for s in samples])
    delta = actual - expected
    delta[:, 0] = ((delta[:, 0] + 180) % 360 - 180) * np.cos(np.deg2rad(expected[:, 1]))
    # At the antipode of the face centre the inverse goes through acos near 180 degrees and moves
    # by about 1e-6 degrees with the last bit of the northing: it is checked by its projection.
    antipodal = case["antipodal_samples"]
    tolerance = np.full(len(samples), 1e-8)
    tolerance[antipodal] = 1e-5
    assert np.all(np.abs(delta) <= tolerance[:, None])
    again = np.column_stack(
        terrio.transform_points(
            actual[antipodal, 0], actual[antipodal, 1], case["source"], case["target"]
        )
    )
    projected = np.asarray([samples[i][2] for i in antipodal])
    assert np.all(np.abs(again - projected) * case["metres_per_unit"] <= 0.001)
    points = [p for p, _ in case["inverse_samples"]]
    actual = np.column_stack(
        terrio.transform_points(
            [p[0] for p in points], [p[1] for p in points], case["target"], case["source"]
        )
    )
    expected = np.asarray([e for _, e in case["inverse_samples"]])
    delta = actual - expected
    delta[:, 0] = ((delta[:, 0] + 180) % 360 - 180) * np.cos(np.deg2rad(expected[:, 1]))
    assert np.all(np.abs(delta) <= 1e-8)
