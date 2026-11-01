"""The CityGML source oracle must reject loss of faces, Z, holes and metadata."""
from copy import deepcopy
from pathlib import Path
import sys

import pytest

etree = pytest.importorskip("lxml.etree")
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_citygml import compare, source_faces


def example():
    doc = etree.ElementTree(etree.fromstring(b'''<CityModel xmlns="http://www.opengis.net/citygml/3.0" xmlns:g="http://www.opengis.net/gml/3.2" xmlns:b="http://www.opengis.net/citygml/building/3.0"><cityObjectMember><b:Building g:id="house"><g:name>house</g:name><b:boundary><b:WallSurface><b:lod2MultiSurface><g:MultiSurface><g:surfaceMember><g:Polygon><g:exterior><g:LinearRing><g:posList>10 0 0 10 10 0 10 10 10 10 0 0</g:posList></g:LinearRing></g:exterior><g:interior><g:LinearRing><g:posList>10 2 2 10 3 2 10 3 3 10 2 2</g:posList></g:LinearRing></g:interior></g:Polygon></g:surfaceMember></g:MultiSurface></b:lod2MultiSurface></b:WallSurface></b:boundary></b:Building></cityObjectMember></CityModel>'''))
    expected, owners = source_faces(doc)
    geometry = dict(type="Polygon", coordinates=[
        [[10,0,0],[10,10,0],[10,10,10],[10,0,0]],
        [[10,2,2],[10,3,2],[10,3,3],[10,2,2]],
    ])
    props = dict(city_id="house", city_type="Building", lod="2",
                 surface_semantics='{"type":"WallSurface"}', attributes='{"name":"house"}')
    return expected, owners, dict(type="FeatureCollection", features=[dict(type="Feature", properties=props, geometry=geometry)])


def test_accepts_correct_explicit_faces():
    compare(*example())


@pytest.mark.parametrize("change", ["z", "hole", "face", "lod", "semantic", "attribute"])
def test_rejects_corrupted_view(change):
    expected, owners, actual = example()
    actual = deepcopy(actual)
    feature = actual["features"][0]
    if change == "z": feature["geometry"]["coordinates"][0][1][2] += 0.001
    elif change == "hole": feature["geometry"]["coordinates"].pop()
    elif change == "face": actual["features"].clear()
    elif change == "lod": feature["properties"]["lod"] = "3"
    elif change == "semantic": feature["properties"]["surface_semantics"] = None
    elif change == "attribute": feature["properties"]["attributes"] = '{"name":"wrong"}'
    with pytest.raises(AssertionError): compare(expected, owners, actual)


def test_repeatable_properties_keep_all_source_occurrences():
    import json

    expected, owners, actual = example()
    owner = owners["house"]
    for value in ["office", "retail"]:
        child = etree.SubElement(owner, "{http://www.opengis.net/citygml/building/3.0}function", codeSpace="codes")
        child.text = value
    attrs = {"name": "house", "function": [{"value": "office", "codeSpace": "codes"}, {"value": "retail", "codeSpace": "codes"}]}
    actual["features"][0]["properties"]["attributes"] = json.dumps(attrs)
    compare(expected, owners, actual)
    attrs["function"] = attrs["function"][-1]
    actual["features"][0]["properties"]["attributes"] = json.dumps(attrs)
    with pytest.raises(AssertionError):
        compare(expected, owners, actual)


@pytest.mark.parametrize("value", [None, "", "namespace", "namespace https://schemas.opengis.net/a.xsd extra"])
def test_schema_locations_require_complete_pairs(value):
    from compare_citygml import schema_locations, XSI

    root = etree.Element("root")
    if value is not None:
        root.set(f"{{{XSI}}}schemaLocation", value)
    with pytest.raises(ValueError, match="namespace/URL pairs"):
        schema_locations(etree.ElementTree(root))


@pytest.mark.parametrize("owner", [False, True])
def test_polygon_without_owner_or_lod_has_clear_error(owner):
    root = etree.Element("{http://www.opengis.net/citygml/3.0}CityModel")
    parent = etree.SubElement(root, "{http://www.opengis.net/citygml/building/3.0}Building") if owner else root
    etree.SubElement(parent, "{http://www.opengis.net/gml/3.2}Polygon")
    with pytest.raises(ValueError, match="LoD property" if owner else "city object owner"):
        source_faces(etree.ElementTree(root))
