"""Compound authority references preserve horizontal and vertical CRS (GDAL 3.13.3)."""

import json

import pytest

import terrio
from terrio.rio.crs import CRS

CASES = [
    (2154, 5720, (652000, 6862000), (652000, 6862000)),
    (4269, 5713, (52, 4), (4, 52)),
    (4326, 5773, (52, 4), (4, 52)),
]


def document(name, position, city=False):
    """Build a point document with an explicit compound CRS.

    Args:
        name: CRS authority reference.
        position: Source X and Y ordinates.
        city: Whether to wrap the point in CityGML instead of GML.

    Returns:
        Encoded XML document containing one point at height 35.
    """
    point = (
        f'<g:Point srsName="{name}" srsDimension="3"><g:pos>'
        f"{position[0]} {position[1]} 35</g:pos></g:Point>"
    )
    if city:
        return f"""<c:CityModel xmlns:c="http://www.opengis.net/citygml/3.0"
        xmlns:b="http://www.opengis.net/citygml/building/3.0" xmlns:g="http://www.opengis.net/gml/3.2">
        <c:cityObjectMember><b:Building g:id="house"><b:lod1Geometry>{point}</b:lod1Geometry>
        </b:Building></c:cityObjectMember></c:CityModel>""".encode()
    return f"""<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0"
    xmlns:g="http://www.opengis.net/gml/3.2" xmlns:ex="urn:example">
    <wfs:member><ex:p g:id="p.1"><ex:geom>{point}</ex:geom></ex:p></wfs:member>
    </wfs:FeatureCollection>""".encode()


@pytest.mark.parametrize("horizontal,vertical,position,xy", CASES)
def test_compound_crs_and_layer_argument_keep_components(horizontal, vertical, position, xy):
    """Check compound references passed through CRS and layer APIs.

    Args:
        horizontal: Horizontal EPSG code.
        vertical: Vertical EPSG code.
        position: Source ordinates for this shared parameter set.
        xy: Expected GIS-order ordinates.
    """
    name = f"urn:ogc:def:crs,crs:EPSG::{horizontal},crs:EPSG::{vertical}"
    expected = CRS.from_user_input(f"EPSG:{horizontal}+{vertical}")
    actual = CRS.from_user_input(name)
    assert actual.to_epsg() is None
    assert actual.to_wkt() == expected.to_wkt()
    assert actual.to_dict(projjson=True)["type"] == "CompoundCRS"
    layer = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {},
                    "geometry": {"type": "Point", "coordinates": [*xy, 35]},
                }
            ],
        },
        crs=name,
    )
    assert CRS.from_user_input(layer.crs).to_wkt() == expected.to_wkt()
    assert layer.to_geojson()["features"][0]["geometry"]["coordinates"] == [*xy, 35]


@pytest.mark.parametrize("horizontal,vertical,position,xy", CASES)
@pytest.mark.parametrize("city", [False, True])
def test_gml_path_and_bytes_keep_compound_axes(tmp_path, horizontal, vertical, position, xy, city):
    """Check compound axes and heights in file and byte readers.

    Args:
        tmp_path: Temporary directory fixture.
        horizontal: Horizontal EPSG code.
        vertical: Vertical EPSG code.
        position: Source ordinates in the declared axis order.
        xy: Expected GIS-order ordinates.
        city: Whether to use CityGML.
    """
    name = f"urn:ogc:def:crs,crs:EPSG::{horizontal},crs:EPSG::{vertical}"
    data = document(name, position, city)
    path = tmp_path / "point.gml"
    path.write_bytes(data)
    for dataset in [terrio.open_vector(str(path)), terrio.open_bytes(data, filename="point.gml")]:
        layer = dataset[0]
        assert (
            CRS.from_user_input(layer.crs).to_wkt()
            == CRS.from_user_input(f"EPSG:{horizontal}+{vertical}").to_wkt()
        )
        assert layer.to_geojson()["features"][0]["geometry"]["coordinates"] == [*xy, 35]


@pytest.mark.parametrize(
    "text",
    [
        "not EPSG:4326",
        "EPSG:4326.0:3857",
        "urn:ogc:def:crs:EPSG:4326:3857junk",
        "https://www.opengis.net/def/crs/EPSG/4326/3857junk",
        "urn:ogc:def:crs,crs:IGNF::LAMB93,crs:EPSG::5720",
        "urn:ogc:def:crs,crs:EPSG::2154,crs:EPSG::999999999",
        "urn:ogc:def:crs,crs:EPSG::2154",
        "EPSG:4979+5720",
        "EPSG:5720+5713",
    ],
)
def test_partial_and_unrelated_references_are_refused(text):
    """Check that unsupported authority references fail explicitly.

    Args:
        text: Invalid or unsupported CRS reference.
    """
    with pytest.raises(ValueError):
        CRS.from_user_input(text)


@pytest.mark.parametrize("city", [False, True])
def test_unsupported_component_is_not_silently_dropped(city):
    """Check that readers reject an unsupported compound component.

    Args:
        city: Whether to use CityGML.
    """
    name = "urn:ogc:def:crs,crs:IGNF::LAMB93,crs:EPSG::5720"
    with pytest.raises(ValueError):
        terrio.open_bytes(document(name, (1, 2), city), filename="point.gml")


@pytest.mark.parametrize("horizontal,vertical,position,xy", CASES)
def test_compound_components_match_gdal(horizontal, vertical, position, xy):
    """Compare component identities and axes with GDAL.

    Args:
        horizontal: Horizontal EPSG code.
        vertical: Vertical EPSG code.
        position: Source ordinates for this shared parameter set.
        xy: GIS-order ordinates for this shared parameter set.
    """
    osr = pytest.importorskip("osgeo.osr")
    osr.UseExceptions()
    name = f"urn:ogc:def:crs,crs:EPSG::{horizontal},crs:EPSG::{vertical}"
    reference = osr.SpatialReference()
    assert reference.SetFromUserInput(name) == 0
    assert reference.IsCompound()
    assert reference.GetAxesCount() == 3
    actual = CRS.from_user_input(name).to_dict(projjson=True)
    expected = json.loads(reference.ExportToPROJJSON())
    assert actual["type"] == expected["type"] == "CompoundCRS"
    assert [part["id"]["code"] for part in actual["components"]] == [horizontal, vertical]
    assert [part["id"]["code"] for part in expected["components"]] == [horizontal, vertical]
    assert [len(part["coordinate_system"]["axis"]) for part in actual["components"]] == [2, 1]
