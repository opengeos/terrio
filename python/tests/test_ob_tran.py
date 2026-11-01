"""Native general oblique transformation (ob_tran) across the public Python surface."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2] / "crates/terrio-crs/tests/data/ob_tran_reference.json"
    ).read_text()
)


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda case: case["name"])
def test_ob_tran_oracle(case):
    samples = [s for s in case["samples"] if s[2] is not None]
    for target in [case["target"], case["wkt2"], case["wkt1"]]:
        actual = np.column_stack(
            terrio.transform_points(
                [s[0] for s in samples], [s[1] for s in samples], case["source"], target
            )
        )
        assert np.all(np.abs(actual - [s[2] for s in samples]) * case["metres_per_unit"] <= 0.001)
    for s in case["samples"]:
        if s[2] is None:
            with pytest.raises(terrio.CRSError):
                terrio.transform_points([s[0]], [s[1]], case["source"], case["target"])
    # Known domain-policy difference: Mollweide/Wagner antimeridians now invert.
    samples = [
        [*s[:3], s[:2]]
        if s[3] is None
        and any(f"+o_proj={p}" in case["target"].split() for p in ["moll", "wag4", "wag5"])
        else s
        for s in samples
    ]
    reversible = [s for s in samples if s[3] is not None]
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

    # PROJ's spherical sinusoidal inverse returns a latitude past a pole, which the rotation turns
    # into a point; terrio's sinusoidal refuses it (as crates/terrio-crs/tests/ob_tran.rs checks).
    def past_pole(point):
        northing = (point[1] * case["metres_per_unit"] - case["false_northing_metres"]) / case[
            "radius_metres"
        ]
        return case["refuses_past_pole"] and abs(northing) > np.pi / 2

    # One batched call, which reuses a single transformer for every point.
    probes = [(p, e) for p, e in case["inverse_samples"] if e is not None and not past_pole(p)]
    actual = np.column_stack(
        terrio.transform_points(
            [p[0] for p, _ in probes], [p[1] for p, _ in probes], case["target"], case["source"]
        )
    )
    expected = np.asarray([e for _, e in probes])
    delta = actual - expected
    delta[:, 0] = ((delta[:, 0] + 180) % 360 - 180) * np.cos(np.deg2rad(expected[:, 1]))
    assert np.all(np.abs(delta) <= 1e-8)
    refused = [s[2] for s in samples if s[3] is None] + [
        p for p, expected in case["inverse_samples"] if expected is None or past_pole(p)
    ]
    for point in refused:
        with pytest.raises(terrio.CRSError):
            terrio.transform_points([point[0]], [point[1]], case["target"], case["source"])


@pytest.mark.parametrize("projection", ["moll", "wag4", "wag5"])
def test_oblique_mollweide_and_wagner_antimeridians(projection):
    source = "+proj=longlat +R=6371000"
    target = f"+proj=ob_tran +o_proj={projection} +o_lon_p=0 +o_lat_p=45 +lon_0=147 +R=6371000"
    lon, lat = [-33.0, -33.0, 147.0], [-75.0, -30.0, -60.0]
    x, y = terrio.transform_points(lon, lat, source, target)
    back_lon, back_lat = terrio.transform_points(x, y, target, source)
    np.testing.assert_allclose((np.asarray(back_lon) - lon + 180) % 360 - 180, 0, atol=1e-8)
    np.testing.assert_allclose(back_lat, lat, rtol=0, atol=1e-8)
