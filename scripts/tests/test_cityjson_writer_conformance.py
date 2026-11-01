"""The writer oracle rejects topology, metadata and XYZ corruption."""
from copy import deepcopy
from pathlib import Path
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_cityjson_writer import compare


def model():
    return dict(type="CityJSON", version="2.0", transform=dict(scale=[1,1,1], translate=[0,0,0]),
                vertices=[[10,0,0],[10,10,0],[10,10,10]], CityObjects={"house": dict(type="Building",
                    attributes=dict(name="house"), geometry=[dict(type="MultiSurface", lod="2", boundaries=[[[0,1,2]]],
                        semantics=dict(surfaces=[dict(type="WallSurface")], values=[0]))])})


def test_accepts_flat_output_within_quantization_error():
    source = model()
    output = deepcopy(source)
    assert compare(source, output) == (1, 0.0)
    output["transform"]["translate"][2] = 0.25
    assert compare(source, output) == (1, 0.25)


@pytest.mark.parametrize("change", ["z", "face", "attribute", "semantics", "lod", "identity", "unsafe", "constant", "crs", "dimension"])
def test_rejects_corrupted_output(change):
    source, output = model(), model()
    obj = output["CityObjects"]["house"]
    if change == "z": output["vertices"][0][2] += 10
    elif change == "face":
        obj["geometry"][0]["boundaries"].append([[0,1,2]])
        obj["geometry"][0]["semantics"]["values"].append(0)
    elif change == "attribute": obj["attributes"]["name"] = "lost"
    elif change == "semantics": obj["geometry"][0]["semantics"]["surfaces"][0]["type"] = "RoofSurface"
    elif change == "lod": obj["geometry"][0]["lod"] = "3"
    elif change == "identity": output["CityObjects"]["other"] = output["CityObjects"].pop("house")
    elif change == "unsafe": output["vertices"][0][2] = 2**54
    elif change == "constant": output["transform"]["translate"][0] = 0.25
    elif change == "dimension": output["vertices"][0].pop()
    elif change == "crs":
        source["metadata"] = {"referenceSystem":"https://www.opengis.net/def/crs/EPSG/0/7415"}
        output["metadata"] = {"referenceSystem":"https://www.opengis.net/def/crs/OTHER/0/7415"}
    with pytest.raises((AssertionError, KeyError)):
        compare(source, output)


def test_compatible_subset_preserves_classifications_without_mutating_source():
    from compare_cityjson_writer import compatible_source

    source = model()
    source["CityObjects"]["generic"] = {"type": "GenericCityObject"}
    subset, removed = compatible_source(source, "1.1")
    assert removed == {"generic"}
    assert subset["CityObjects"] == {"house": source["CityObjects"]["house"]}
    assert "generic" in source["CityObjects"]
    assert compatible_source(source, "2.0") == (source, set())


@pytest.mark.parametrize("direction", ["parents", "children"])
def test_subset_cannot_silently_remove_object_links(direction):
    from compare_cityjson_writer import compatible_source

    source = model()
    source["CityObjects"]["generic"] = {"type": "GenericCityObject", direction: ["house"]}
    with pytest.raises(ValueError, match="object links"):
        compatible_source(source, "1.1")


@pytest.mark.parametrize("kind", ["missing", "ignores_version", "supported"])
def test_optional_node_writer_probe_requires_method_and_version_option(tmp_path, kind):
    import shutil
    from conformance import node_cityjson_writer_available

    if not shutil.which("node"):
        pytest.skip("Node is not installed")
    module = tmp_path / "terrio.js"
    module.with_name("terrio_bg.wasm").write_bytes(b"presence marker")
    method = "" if kind == "missing" else (
        "toBytes(name,options){return Buffer.from(JSON.stringify({version:"
        + ("'2.0'" if kind == "ignores_version" else "options.cityjsonVersion")
        + "}));},")
    module.write_text("module.exports={open(){return {" + method + "free(){}};}};")
    assert node_cityjson_writer_available(module) == (kind == "supported")


@pytest.mark.parametrize("schema", [{}, {"definitions": {"Building": {}}}])
def test_core_sample_cannot_pass_with_missing_or_restructured_definitions(tmp_path, schema):
    import json
    from compare_cityjson_writer import core_type_source

    (tmp_path / "cityobjects.schema.json").write_text(json.dumps(schema))
    with pytest.raises(ValueError, match="expected 33 core classes"):
        core_type_source(tmp_path)


def test_expected_rejection_is_a_separate_successful_conformance_outcome(monkeypatch, tmp_path):
    import conformance

    case = conformance.Case("versions", "unsupported 1.1", conformance.EXPECTED_REJECTION, "rejected without output")
    assert conformance.tally([case]) == (1, 0, 0, 1, 0, 0)
    monkeypatch.setattr(conformance, "SUITES", {"cityjson-write": lambda ctx: conformance.Suite(
        "cityjson-write", "writer", "schema", "", cases=[case])})
    monkeypatch.setattr(conformance, "missing_modules", lambda *names: [])
    monkeypatch.setattr(conformance, "format_names", lambda path: {})
    monkeypatch.setattr(conformance, "versions", lambda ctx: {})
    monkeypatch.setattr(conformance, "markdown", lambda *args: "")
    monkeypatch.setattr(sys, "argv", ["conformance.py", str(tmp_path), "--terrio", sys.executable,
                                     "--only", "cityjson-write", "--strict"])
    assert conformance.main() == 0


def test_expected_rejections_are_separate_in_markdown_summary():
    from types import SimpleNamespace
    import conformance

    suite = conformance.Suite("writer", "Writer", "schema", "", cases=[
        conformance.Case("versions", "unsupported", conformance.EXPECTED_REJECTION, "rejected")])
    report = conformance.markdown([suite], SimpleNamespace(max_details=10), [])
    assert "| 1 | 0 | 0 | 1 | 0 | 0 |" in report
    assert "Expected rejection" in report
    rows = report.split("## Summary", 1)[1].split("## Writer", 1)[0].splitlines()
    assert {row.count("|") for row in rows if row.startswith("|")} == {9}
