"""The native CityGML writer is shared by every Python output entrypoint."""

import json
from xml.etree import ElementTree

import pytest

import terrio


def model():
    return dict(
        type="CityJSON",
        version="2.0",
        transform=dict(scale=[1, 1, 1], translate=[0, 0, 0]),
        metadata=dict(referenceSystem="https://www.opengis.net/def/crs/EPSG/0/7415"),
        vertices=[
            [10, 0, 0],
            [10, 10, 0],
            [10, 10, 10],
            [10, 0, 10],
            [10, 2, 2],
            [10, 3, 2],
            [10, 3, 3],
        ],
        CityObjects={
            "house": dict(type="Building", children=["1 bad"]),
            "1 bad": dict(
                type="BuildingPart",
                parents=["house"],
                attributes=dict(name="A & B", nested=[1, True, None]),
                geometry=[
                    dict(
                        type="MultiSurface",
                        lod="2.2",
                        boundaries=[[[0, 1, 2, 3], [4, 5, 6]]],
                        semantics=dict(
                            surfaces=[dict(type="WallSurface", finish="brick")], values=[0]
                        ),
                    )
                ],
            ),
        },
    )


@pytest.mark.parametrize("version", ["2.0", "3.0"])
def test_citygml_versions_across_python_entrypoints(tmp_path, version):
    doc = model()
    path = tmp_path / "input.city.json"
    path.write_text(json.dumps(doc))
    source = terrio.open_vector(str(path))
    layer = source[0]
    outputs = [layer.to_bytes("output.citygml", citygml_version=version)]
    for label, write in [
        ("layer", lambda p: layer.write(str(p), citygml_version=version)),
        ("dataset", lambda p: source.write(str(p), citygml_version=version)),
        ("function", lambda p: terrio.write_vector(source, str(p), citygml_version=version)),
        ("convert", lambda p: terrio.convert(str(path), str(p), citygml_version=version)),
    ]:
        target = tmp_path / f"{label}.citygml"
        write(target)
        outputs.append(target.read_bytes())
    for output in outputs:
        assert (
            ElementTree.fromstring(output).tag
            == f"{{http://www.opengis.net/citygml/{version}}}CityModel"
        )
        restored = terrio.open_bytes(output, filename="result.citygml")[0]
        assert restored.crs == "EPSG:7415"
        features = restored.to_geojson()["features"]
        face = next(feature for feature in features if feature["geometry"])
        assert face["properties"]["city_id"] == "1 bad"
        assert face["properties"]["lod"] == "2.2"
        assert json.loads(face["properties"]["parents"]) == ["house"]
        assert (
            json.loads(face["properties"]["attributes"])
            == doc["CityObjects"]["1 bad"]["attributes"]
        )
        assert json.loads(face["properties"]["surface_semantics"]) == {
            "type": "WallSurface",
            "finish": "brick",
        }
        assert face["geometry"]["coordinates"][0][:4] == doc["vertices"][:4]
        assert len(face["geometry"]["coordinates"]) == 2


def test_version_options_reject_other_formats_before_io(tmp_path):
    layer = terrio.open_bytes(json.dumps(model()).encode(), filename="input.city.json")[0]
    assert "citygml/3.0" in layer.to_bytes("default.citygml").decode()
    with pytest.raises(terrio.InvalidArgumentError, match="applies to CityGML"):
        layer.to_bytes("wrong.geojson", citygml_version="2.0")
    with pytest.raises(terrio.InvalidArgumentError, match="version must be"):
        layer.to_bytes("wrong.citygml", citygml_version="1.0")
    for filename, driver in [("out.geojson", None), ("misleading.citygml", "GTiff")]:
        target = tmp_path / filename
        with pytest.raises(terrio.InvalidArgumentError, match="applies to CityGML"):
            terrio.convert("missing.tif", str(target), format=driver, citygml_version="2.0")
        assert not target.exists()


@pytest.mark.parametrize("mode", ["append", "overwrite"])
def test_existing_models_reject_layer_modes_without_mutation(tmp_path, mode):
    source = terrio.open_bytes(json.dumps(model()).encode(), filename="input.city.json")
    path = tmp_path / "existing.citygml"
    source.write(str(path))
    before = path.read_bytes()
    with pytest.raises(terrio.UnsupportedError, match="no persistent layer names"):
        terrio.write_vector(source, str(path), **{mode: True})
    assert path.read_bytes() == before


@pytest.mark.parametrize(
    "choice, native", [("footprint", "lod0FootPrint"), ("roof-edge", "lod0RoofEdge")]
)
def test_building_lod0_choice_across_python_entrypoints(tmp_path, choice, native):
    doc = model()
    doc["vertices"] = [
        [0, 0, 2],
        [10, 0, 2],
        [10, 10, 2],
        [0, 10, 2],
        [2, 2, 2],
        [3, 2, 2],
        [3, 3, 2],
    ]
    obj = doc["CityObjects"]["1 bad"]
    obj["geometry"][0]["lod"] = "0"
    obj["geometry"][0].pop("semantics")
    path = tmp_path / "lod0.city.json"
    path.write_text(json.dumps(doc))
    source = terrio.open_vector(str(path))
    layer = source[0]
    with pytest.raises(terrio.InvalidArgumentError, match="ambiguous building LoD 0"):
        layer.to_bytes("out.citygml", citygml_version="2.0")
    options = dict(citygml_version="2.0", citygml_lod0_surface=choice)
    outputs = [layer.to_bytes("out.citygml", **options)]
    for label, write in [
        ("layer", lambda p: layer.write(str(p), **options)),
        ("dataset", lambda p: source.write(str(p), **options)),
        ("function", lambda p: terrio.write_vector(source, str(p), **options)),
        ("convert", lambda p: terrio.convert(str(path), str(p), **options)),
    ]:
        target = tmp_path / f"{label}.citygml"
        write(target)
        outputs.append(target.read_bytes())
    for output in outputs:
        assert f"<bldg:{native}>".encode() in output
        restored = terrio.open_bytes(output, filename="native.citygml")[0]
        assert f"<bldg:{native}>".encode() in restored.to_bytes(
            "again.citygml", citygml_version="2.0"
        )
    with pytest.raises(terrio.InvalidArgumentError, match="applies to CityGML"):
        terrio.convert("missing.tif", str(tmp_path / "wrong.tif"), citygml_lod0_surface=choice)


@pytest.mark.parametrize(
    "kind, native", [("GroundSurface", "lod0FootPrint"), ("RoofSurface", "lod0RoofEdge")]
)
def test_lod0_classification_selects_property_and_rejects_conflicting_source_hint(kind, native):
    doc = model()
    geometry = doc["CityObjects"]["1 bad"]["geometry"][0]
    geometry["lod"] = "0"
    geometry["semantics"]["surfaces"][0]["type"] = kind
    source = terrio.open_bytes(json.dumps(doc).encode(), filename="source.city.json")[0]
    output = source.to_bytes("out.citygml", citygml_version="2.0")
    assert f"<bldg:{native}>".encode() in output
    other_kind = "RoofSurface" if kind == "GroundSurface" else "GroundSurface"
    corrupted = output.replace(kind.encode(), other_kind.encode())
    conflicting = terrio.open_bytes(corrupted, filename="conflict.citygml")[0]
    with pytest.raises(
        terrio.InvalidArgumentError, match="conflicts with its surface classification"
    ):
        conflicting.to_bytes("out.citygml", citygml_version="2.0")
