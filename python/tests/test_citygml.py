"""CityGML is available through the shared Python vector path/bytes APIs."""

import json

import pytest

import terrio


@pytest.mark.parametrize("version", ["2.0", "3.0"])
def test_citygml_paths_bytes_xyz_and_metadata(tmp_path, version):
    gml = "http://www.opengis.net/gml" + ("/3.2" if version == "3.0" else "")
    data = f'''<c:CityModel xmlns:c="http://www.opengis.net/citygml/{version}"
    xmlns:g="{gml}" xmlns:b="http://www.opengis.net/citygml/building/{version}">
    <c:cityObjectMember><b:Building g:id="building"><g:name>Test house</g:name>
    <b:function codeSpace="codes">office</b:function>
    <b:function codeSpace="codes">retail</b:function>
    <b:measuredHeight uom="m">12.5</b:measuredHeight><b:lod2MultiSurface>
    <g:MultiSurface srsName="EPSG:32632" srsDimension="3"><g:surfaceMember>
    <g:Polygon><g:exterior><g:LinearRing><g:posList>10 20 30 10 21 30 10 21 31 10 20 30</g:posList>
    </g:LinearRing></g:exterior></g:Polygon></g:surfaceMember></g:MultiSurface>
    </b:lod2MultiSurface></b:Building></c:cityObjectMember></c:CityModel>'''.encode()
    path = tmp_path / "model.gml"
    path.write_bytes(data)
    for ds in [terrio.open_vector(str(path)), terrio.open_bytes(data, filename="model.gml")]:
        assert ds.driver == "CityGML"
        assert ds[0].crs == "EPSG:32632"
        feature = ds[0].to_geojson()["features"][0]
        assert feature["properties"]["city_id"] == "building"
        assert feature["properties"]["lod"] == "2"
        attrs = json.loads(feature["properties"]["attributes"])
        assert attrs["name"] == "Test house"
        assert attrs["function"] == [
            {"value": "office", "codeSpace": "codes"},
            {"value": "retail", "codeSpace": "codes"},
        ]
        assert attrs["measuredHeight"] == {"value": "12.5", "uom": "m"}
        assert feature["geometry"]["coordinates"][0][:3] == [
            [10, 20, 30],
            [10, 21, 30],
            [10, 21, 31],
        ]


@pytest.mark.parametrize("encoding", ["utf-8", "utf-16", "latin-1"])
def test_inferred_2d_position_crs_and_xml_encodings(encoding):
    xml = """<c:CityModel xmlns:c="http://www.opengis.net/citygml/3.0"
    xmlns:g="http://www.opengis.net/gml/3.2" xmlns:b="http://www.opengis.net/citygml/building/3.0">
    <c:cityObjectMember><b:Building g:id="house"><g:name>Montréal</g:name>
    <b:lod1Geometry><g:Point><g:pos srsName="EPSG:32632">10 20</g:pos></g:Point>
    </b:lod1Geometry></b:Building></c:cityObjectMember></c:CityModel>"""
    ds = terrio.open_bytes(xml.encode(encoding), filename="model.gml")
    assert ds.driver == "CityGML"
    feature = ds[0].to_geojson()["features"][0]
    assert feature["geometry"]["coordinates"] == [10, 20]
    assert json.loads(feature["properties"]["attributes"])["name"] == "Montréal"
