"""Native two-point oblique Mercator across the public Python surface."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2]
        / "crates/terrio-crs/tests/data/omerc_two_point_reference.json"
    ).read_text()
)


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda case: case["name"])
def test_omerc_two_point_oracle(case):
    defined = [sample for sample in case["samples"] if sample[2] is not None]
    for target in [case["target"], case["wkt2"], case["wkt1"], case["wkt1_esri"]]:
        actual = np.column_stack(
            terrio.transform_points(
                [s[0] for s in defined], [s[1] for s in defined], case["source"], target
            )
        )
        assert np.all(np.abs(actual - [s[2] for s in defined]) * case["metres_per_unit"] <= 0.001)
    reversible = [sample for sample in defined if sample[3] is not None]
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
    for point, expected in case["inverse_samples"]:
        if expected is None:
            with pytest.raises(terrio.CRSError):
                terrio.transform_points([point[0]], [point[1]], case["target"], case["source"])


@pytest.mark.parametrize("case", REFERENCE["invalid"], ids=lambda case: case["definition"])
def test_omerc_two_point_refuses_what_proj_refuses(case):
    with pytest.raises(terrio.CRSError):
        terrio.transform_points(
            [0.0], [0.0], "+proj=longlat +datum=WGS84", case["definition"] + " +datum=WGS84"
        )
