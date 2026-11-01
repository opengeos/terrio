"""CityJSON versions share the writer across Python paths, bytes and convert."""

import json

import pytest

import terrio


def model():
    return {
        "type": "CityJSON",
        "version": "2.0",
        "transform": {"scale": [1, 1, 1], "translate": [0, 0, 0]},
        "metadata": {"referenceSystem": "https://www.opengis.net/def/crs/EPSG/0/7415"},
        "vertices": [
            [10, 0, 0],
            [10, 10, 0],
            [10, 10, 10],
            [10, 0, 10],
            [10, 2, 2],
            [10, 3, 2],
            [10, 3, 3],
        ],
        "CityObjects": {
            "house": {"type": "Building", "children": ["part"]},
            "part": {
                "type": "BuildingPart",
                "parents": ["house"],
                "attributes": {"name": "α & β", "height": 10},
                "geometry": [
                    {
                        "type": "MultiSurface",
                        "lod": "2.2",
                        "boundaries": [[[0, 1, 2, 3], [4, 5, 6]]],
                        "semantics": {
                            "surfaces": [{"type": "WallSurface", "finish": "brick"}],
                            "values": [0],
                        },
                    }
                ],
            },
        },
    }


@pytest.mark.parametrize("version", ["1.1", "2.0"])
def test_cityjson_writer_versions_across_public_entrypoints(tmp_path, version):
    doc = model()
    data = json.dumps(doc).encode()
    source_path = tmp_path / "source.city.json"
    source_path.write_bytes(data)
    source = terrio.open_bytes(data, filename="source.city.json")
    layer = source[0]
    outputs = [layer.to_bytes("model.city.json", cityjson_version=version)]
    for label, write in [
        ("layer", lambda p: layer.write(str(p), cityjson_version=version)),
        ("dataset", lambda p: source.write(str(p), cityjson_version=version)),
        ("function", lambda p: terrio.write_vector(source, str(p), cityjson_version=version)),
        ("convert", lambda p: terrio.convert(str(source_path), str(p), cityjson_version=version)),
    ]:
        path = tmp_path / f"{label}.city.json"
        write(path)
        outputs.append(path.read_bytes())
    for output in outputs:
        encoded = json.loads(output)
        assert encoded["version"] == version
        assert encoded["metadata"] == doc["metadata"]
        assert encoded["CityObjects"]["house"]["children"] == ["part"]
        part = encoded["CityObjects"]["part"]
        assert part["attributes"] == doc["CityObjects"]["part"]["attributes"]
        assert part["parents"] == ["house"]
        assert part["geometry"][0]["type"] == "MultiSurface"
        restored = terrio.open_bytes(output, filename="output.city.json")[0]
        assert restored.crs == "EPSG:7415"
        face = next(f for f in restored.to_geojson()["features"] if f["geometry"])
        assert len(face["geometry"]["coordinates"]) == 2
        assert face["geometry"]["coordinates"][0][:4] == doc["vertices"][:4]
        assert json.loads(face["properties"]["surface_semantics"]) == {
            "type": "WallSurface",
            "finish": "brick",
        }


def test_cityjson_version_is_not_silently_ignored_on_other_formats():
    layer = terrio.open_bytes(json.dumps(model()).encode(), filename="model.city.json")[0]
    with pytest.raises(terrio.errors.InvalidArgumentError, match="applies to CityJSON"):
        layer.to_bytes("wrong.geojson", cityjson_version="2.0")
    with pytest.raises(terrio.errors.InvalidArgumentError, match="version must be"):
        layer.to_bytes("wrong.city.json", cityjson_version="3.0")


@pytest.mark.parametrize("version", ["1.1", "2.0"])
def test_citygml_storey_alias_preserves_its_source_classification(version):
    xml = """<c:CityModel xmlns:c="http://www.opengis.net/citygml/3.0"
    xmlns:g="http://www.opengis.net/gml/3.2" xmlns:b="http://www.opengis.net/citygml/building/3.0">
    <c:cityObjectMember><b:Building g:id="house"><b:buildingSubdivision><b:Storey g:id="floor">
    <b:lod2MultiSurface><g:MultiSurface srsName="EPSG:32632" srsDimension="3"><g:surfaceMember>
    <g:Polygon><g:exterior><g:LinearRing><g:posList>0 0 1 0 1 1 1 1 1 0 0 1</g:posList>
    </g:LinearRing></g:exterior></g:Polygon></g:surfaceMember></g:MultiSurface></b:lod2MultiSurface>
    </b:Storey></b:buildingSubdivision></b:Building></c:cityObjectMember></c:CityModel>"""
    layer = terrio.open_bytes(xml.encode(), filename="model.gml")[0]
    output = json.loads(layer.to_bytes("model.city.json", cityjson_version=version))
    storey = output["CityObjects"]["floor"]
    assert storey["type"] == "BuildingStorey"
    assert storey["parents"] == ["house"]
    assert storey["attributes"]["terrio:sourceCityType"] == "Storey"


@pytest.mark.parametrize(
    "filename,format", [("out.geojson", None), ("misleading.city.json", "GTiff")]
)
def test_convert_rejects_cityjson_options_for_other_formats_before_input_io(
    tmp_path, filename, format
):
    with pytest.raises(terrio.errors.InvalidArgumentError, match="applies to CityJSON"):
        terrio.convert(
            str(tmp_path / "missing.tif"),
            str(tmp_path / filename),
            format=format,
            cityjson_version="1.1",
        )
    assert not (tmp_path / filename).exists()
