"""Native ISEA across the public Python surface."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2] / "crates/terrio-crs/tests/data/isea_reference.json"
    ).read_text()
)


def angular_delta(actual, expected):
    delta = np.asarray(actual) - np.asarray(expected)
    delta[:, 0] = ((delta[:, 0] + 180) % 360 - 180) * np.cos(np.deg2rad(np.asarray(expected)[:, 1]))
    return delta


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda case: case["name"])
def test_isea_oracle(case):
    samples = case["samples"]
    for target in [case["target"], case["wkt2"], case["wkt1"], case["wkt1_esri"]]:
        actual = np.column_stack(
            terrio.transform_points(
                [s[0] for s in samples], [s[1] for s in samples], case["source"], target
            )
        )
        assert np.all(np.abs(actual - [s[2] for s in samples]) * case["metres_per_unit"] <= 0.001)
    # PROJ's inverse, which is not the forward's on an ellipsoid and is refused outside its
    # default grid. As in crates/terrio-crs/tests/isea.rs.
    pairs = [(s[2], s[3]) for s in samples] + [tuple(s) for s in case["inverse_samples"]]
    accepted = [(p, e) for p, e in pairs if e is not None]
    if accepted:
        actual = np.column_stack(
            terrio.transform_points(
                [p[0] for p, _ in accepted],
                [p[1] for p, _ in accepted],
                case["target"],
                case["source"],
            )
        )
        assert np.all(np.abs(angular_delta(actual, [e for _, e in accepted])) <= 1e-8)
    for point in [p for p, e in pairs if e is None]:
        with pytest.raises(terrio.CRSError):
            terrio.transform_points([point[0]], [point[1]], case["target"], case["source"])
