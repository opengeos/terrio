"""Local vector documents under names that point to no driver (`data`, `data.txt`, a GML without
its extension), read by their content through `terrio.open_vector` and `terrio.pyogrio`, as a
remote response under such a name is, and only with their driver allowed."""

import json
import re

import pytest

import terrio
import terrio.pyogrio as tpg
import terrio.rio as rio
from terrio.pyogrio.errors import DataSourceError

GEOJSON = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"id": i},
            "geometry": {"type": "Point", "coordinates": [10 + i, 46]},
        }
        for i in range(3)
    ],
}
GML = (
    '<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0" '
    'xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:app="urn:test"><wfs:member>'
    '<app:road gml:id="road.1"><app:name>road</app:name><app:geom>'
    '<gml:LineString srsName="urn:ogc:def:crs:EPSG::25832">'
    '<gml:posList srsDimension="2">700000 5100000 700001 5100001</gml:posList>'
    "</gml:LineString></app:geom></app:road></wfs:member></wfs:FeatureCollection>"
)


@pytest.fixture
def documents(tmp_path):
    for name in ("data", "data.txt"):
        (tmp_path / name).write_text(json.dumps(GEOJSON))
    (tmp_path / "features").write_text(GML)
    (tmp_path / "notgeo").write_text(json.dumps({"name": "x", "values": [1, 2, 3]}))
    return tmp_path


@pytest.mark.parametrize(
    ("name", "driver", "count"),
    [("data", "GeoJSON", 3), ("data.txt", "GeoJSON", 3), ("features", "GML", 1)],
)
def test_a_document_under_a_name_of_no_driver_is_read_by_content(documents, name, driver, count):
    path = str(documents / name)
    for scope in ({}, {"TERRIO_ALLOWED_DRIVERS": driver}):
        with rio.Env(**scope):
            ds = terrio.open_vector(path)
            assert (ds.driver, sum(len(layer) for layer in ds.layers)) == (driver, count)
            info = tpg.read_info(path)
            assert (info["driver"], info["features"]) == (driver, count)


def test_content_the_allow_list_leaves_out_is_refused_as_before(documents):
    with rio.Env(TERRIO_ALLOWED_DRIVERS="CSV"):
        message = "data' holds GEOJSON data, and the GEOJSON driver is not allowed (allowed: CSV)"
        with pytest.raises(terrio.UnsupportedError, match=re.escape(message)):
            terrio.open_vector(str(documents / "data"))
        with pytest.raises(
            DataSourceError, match="features' holds GML data, and the GML driver is not allowed"
        ):
            tpg.read_info(str(documents / "features"))


def test_a_json_document_that_is_not_geojson_is_still_not_recognized(documents):
    with pytest.raises(terrio.UnsupportedError, match="format not recognized"):
        terrio.open_vector(str(documents / "notgeo"))
