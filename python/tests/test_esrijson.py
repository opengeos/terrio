"""ESRIJSON reaches Python through the shared driver, from paths and from bytes."""

import json

import terrio

PAGE = {
    "objectIdFieldName": "OBJECTID",
    "geometryType": "esriGeometryPolygon",
    "spatialReference": {"wkid": 4326},
    "fields": [
        {"name": "OBJECTID", "type": "esriFieldTypeOID"},
        {"name": "NAME", "type": "esriFieldTypeString", "length": 20},
    ],
    "features": [
        {
            "attributes": {"OBJECTID": 7, "NAME": "Lake"},
            "geometry": {
                "rings": [
                    [[0, 0], [0, 4], [4, 4], [4, 0], [0, 0]],
                    [[1, 1], [2, 1], [2, 2], [1, 2], [1, 1]],
                ]
            },
        }
    ],
}


def test_esrijson_from_a_path_and_from_bytes(tmp_path):
    data = json.dumps(PAGE).encode()
    path = tmp_path / "lakes.json"
    path.write_bytes(data)
    for ds in [terrio.open_vector(str(path)), terrio.open_bytes(data, filename="lakes.json")]:
        assert ds.driver == "ESRIJSON"
        assert ds[0].crs == "EPSG:4326"
        feature = ds[0].to_geojson()["features"][0]
        assert feature["properties"]["NAME"] == "Lake"
        assert len(feature["geometry"]["coordinates"]) == 2


def test_a_geojson_json_file_stays_geojson(tmp_path):
    path = tmp_path / "plain.json"
    path.write_text(json.dumps({"type": "FeatureCollection", "features": []}))
    assert terrio.open_vector(str(path)).driver == "GeoJSON"
