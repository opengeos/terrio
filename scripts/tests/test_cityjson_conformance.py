"""The CityJSON oracle must reject loss of Z, holes, attributes and primitives."""

from copy import deepcopy
from collections import Counter
from pathlib import Path
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_cityjson import compare, convert_cli, key, reference, schema_version


def fixture():
    props = dict(city_id="wall", city_type="Building", lod="2", parents=None,
                 attributes='{"name":"wall"}', geometry_type="MultiSurface", semantics=None,
                 skipped_rings=0, skipped_faces=0)
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


@pytest.mark.parametrize("change", ["z", "hole", "attribute", "missing", "duplicate", "diagnostic"])
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
    elif change == "diagnostic":
        feature["properties"]["skipped_rings"] = 1
    with pytest.raises(AssertionError):
        compare(expected, actual)


def test_template_oracle_applies_matrix_before_world_anchor():
    doc = dict(vertices=[[100, 200, 300]], CityObjects={"tree": dict(type="SolitaryVegetationObject",
        geometry=[dict(type="GeometryInstance", template=0, boundaries=[0],
            transformationMatrix=[2,0,0,10,0,3,0,20,0,0,4,30,0,0,0,1])])})
    doc["geometry-templates"] = {"vertices-templates": [[1,2,3]],
        "templates": [dict(type="MultiPoint", lod="2", boundaries=[0])]}
    rows = reference(doc)
    assert len(rows) == 1
    assert rows[0][0]["lod"] == "2"
    assert rows[0][1] == dict(type="Point", coordinates=[112,226,342])


def test_degenerate_oracle_preserves_semantic_indices():
    doc = dict(vertices=[[10,0,0],[10,1,0],[10,1,1]], CityObjects={"house": dict(type="Building",
        geometry=[dict(type="MultiSurface", lod="2.2", boundaries=[[[0,0,0]],[[0,1,2]]],
            semantics=dict(surfaces=[dict(type="GroundSurface"),dict(type="WallSurface")],values=[0,1]))])})
    rows = reference(doc)
    assert len(rows) == 1
    assert rows[0][0]["skipped_rings"] == 1
    assert rows[0][0]["skipped_faces"] == 1
    assert rows[0][0]["surface_semantics"] == {"type":"WallSurface"}


@pytest.mark.parametrize("version", ["1.0", "2.0.1", "3.0", None])
def test_unsupported_schema_version_fails_explicitly(version):
    with pytest.raises(ValueError, match="no published schema"):
        schema_version(version)


def test_cli_failure_keeps_stderr_in_the_report(monkeypatch):
    import subprocess

    def fail(*args, **kwargs):
        raise subprocess.CalledProcessError(2, args[0], stderr="CityJSON: invalid vertex index")

    monkeypatch.setattr(subprocess, "run", fail)
    with pytest.raises(RuntimeError, match="invalid vertex index"):
        convert_cli("terrio", "input.city.json", "output.geojson")
