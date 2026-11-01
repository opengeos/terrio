"""CityJSON uses the shared driver through Python path and bytes entrypoints."""

import json

import terrio


def test_cityjson_late_type_and_unknown_crs(tmp_path):
    doc = {
        "vertices": [[10, 20, 30], [10, 21, 30], [10, 21, 31]],
        "CityObjects": {
            "house": {
                "type": "Building",
                "attributes": {"note": "a" * 20000},
                "geometry": [{"type": "MultiSurface", "lod": "2", "boundaries": [[[0, 1, 2]]]}],
            }
        },
        "type": "CityJSON",
        "version": "2.0",
        "transform": {"scale": [1, 1, 1], "translate": [0, 0, 0]},
    }
    data = json.dumps(doc).encode()
    path = tmp_path / "late.json"
    path.write_bytes(data)
    for ds in [terrio.open_vector(str(path)), terrio.open_bytes(data, filename="late.json")]:
        assert ds.driver == "CityJSON"
        assert ds[0].crs is None
        features = ds[0].to_geojson()["features"]
        assert len(features) == 1
        assert features[0]["properties"]["city_id"] == "house"
        assert features[0]["geometry"]["coordinates"][0][:3] == doc["vertices"]
