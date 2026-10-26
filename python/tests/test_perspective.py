"""Native perspective projections across the public Python surface."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2]
        / "crates/terrio-crs/tests/data/perspective_reference.json"
    ).read_text()
)


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda case: case["name"])
def test_perspective_oracle(case):
    visible = [sample for sample in case["samples"] if sample[2] is not None]
    actual = np.column_stack(
        terrio.transform_points(
            [s[0] for s in visible], [s[1] for s in visible], case["source"], case["target"]
        )
    )
    assert np.all(np.abs(actual - [s[2] for s in visible]) * case["metres_per_unit"] <= 0.001)
    reversible = [sample for sample in visible if sample[3] is not None]
    actual = np.column_stack(
        terrio.transform_points(
            [s[2][0] for s in reversible],
            [s[2][1] for s in reversible],
            case["target"],
            case["source"],
        )
    )
    expected = np.asarray([s[3] for s in reversible])
    delta = actual - expected
    delta[:, 0] = ((delta[:, 0] + 180) % 360 - 180) * np.cos(np.deg2rad(expected[:, 1]))
    assert np.all(np.abs(delta) <= 1e-8)
    invisible = next(s for s in case["samples"] if s[2] is None)
    with pytest.raises(terrio.CRSError):
        terrio.transform_points([invisible[0]], [invisible[1]], case["source"], case["target"])
    for point, expected in case["inverse_samples"]:
        if expected is None:
            with pytest.raises(terrio.CRSError):
                terrio.transform_points([point[0]], [point[1]], case["target"], case["source"])
