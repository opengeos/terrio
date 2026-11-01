"""The CityJSON oracle must reject loss of Z, holes, attributes and primitives."""

from copy import deepcopy
from collections import Counter
from pathlib import Path
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_cityjson import compare, key


def fixture():
    props = dict(city_id="wall", city_type="Building", lod="2", parents=None,
                 attributes='{"name":"wall"}', geometry_type="MultiSurface", semantics=None)
    geometry = dict(type="Polygon", coordinates=[
        [[10, 20, 30], [10, 30, 30], [10, 30, 40]],
        [[10, 22, 32], [10, 23, 32], [10, 23, 33]],
    ])
    expected = Counter([key(props, geometry)])
    return expected, dict(type="FeatureCollection", features=[dict(type="Feature", properties=props, geometry=geometry)])


def test_accepts_same_numeric_values_with_explicit_ring_closure():
    expected, actual = fixture()
    for ring in actual["features"][0]["geometry"]["coordinates"]:
        ring.append(ring[0].copy())
        for coordinate in ring:
            coordinate[:] = [float(v) for v in coordinate]
    compare(expected, actual)


@pytest.mark.parametrize("change", ["z", "hole", "attribute", "missing", "duplicate"])
def test_rejects_corrupted_flat_view(change):
    expected, actual = fixture()
    actual = deepcopy(actual)
    feature = actual["features"][0]
    if change == "z":
        feature["geometry"]["coordinates"][0][1][2] += 0.001
    elif change == "hole":
        feature["geometry"]["coordinates"].pop()
    elif change == "attribute":
        feature["properties"]["city_id"] = "other"
    elif change == "missing":
        actual["features"].clear()
    elif change == "duplicate":
        actual["features"].append(deepcopy(feature))
    with pytest.raises(AssertionError):
        compare(expected, actual)
