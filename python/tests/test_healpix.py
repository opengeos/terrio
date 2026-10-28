"""Native HEALPix and rHEALPix across the public Python surface."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2] / "crates/terrio-crs/tests/data/healpix_reference.json"
    ).read_text()
)


def angular_delta(actual, expected):
    delta = np.asarray(actual) - np.asarray(expected)
    delta[:, 0] = ((delta[:, 0] + 180) % 360 - 180) * np.cos(np.deg2rad(np.asarray(expected)[:, 1]))
    return delta


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda case: case["name"])
def test_healpix_oracle(case):
    # PROJ projects every point (the generator checks it).
    samples = case["samples"]
    for target in [case["target"], case["wkt2"], case["wkt1"], case["wkt1_esri"]]:
        actual = np.column_stack(
            terrio.transform_points(
                [s[0] for s in samples], [s[1] for s in samples], case["source"], target
            )
        )
        assert np.all(np.abs(actual - [s[2] for s in samples]) * case["metres_per_unit"] <= 0.001)
    # On the edge of the image the last bit moves the point in or out of it (the platform's
    # libm, and PROJ 9.8.1's US survey foot in these methods' own +units, 0.304800609601219,
    # where terrio has 1200/3937): there either is right, and a point terrio unprojects must be
    # the projected one. As in crates/terrio-crs/tests/healpix.rs.
    edge = set(case["edge_samples"])
    reversible = [s for i, s in enumerate(samples) if s[3] is not None and i not in edge]
    actual = np.column_stack(
        terrio.transform_points(
            [s[2][0] for s in reversible],
            [s[2][1] for s in reversible],
            case["target"],
            case["source"],
        )
    )
    assert np.all(np.abs(angular_delta(actual, [s[3] for s in reversible])) <= 1e-8)
    for i in edge:
        try:
            actual = terrio.transform_points(
                [samples[i][2][0]], [samples[i][2][1]], case["target"], case["source"]
            )
        except terrio.CRSError:
            continue
        assert np.all(np.abs(angular_delta(np.column_stack(actual), [samples[i][:2]])) <= 1e-8)
    accepted = [(p, e) for p, e in case["inverse_samples"] if e is not None]
    actual = np.column_stack(
        terrio.transform_points(
            [p[0] for p, _ in accepted], [p[1] for p, _ in accepted], case["target"], case["source"]
        )
    )
    assert np.all(np.abs(angular_delta(actual, [e for _, e in accepted])) <= 1e-8)
    refused = [s[2] for i, s in enumerate(samples) if s[3] is None and i not in edge] + [
        p for p, expected in case["inverse_samples"] if expected is None
    ]
    for point in refused:
        with pytest.raises(terrio.CRSError):
            terrio.transform_points([point[0]], [point[1]], case["target"], case["source"])
