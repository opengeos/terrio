"""CityJSON uses the shared driver through Python path and bytes entrypoints."""

import json

import pytest

import terrio


@pytest.mark.parametrize("crs", [None, 7415, {}, "unknown-unsupported-crs"])
def test_cityjson_declared_invalid_crs_is_rejected(crs):
    doc = {
        "type": "CityJSON",
        "version": "2.0",
        "vertices": [],
        "CityObjects": {},
        "transform": {"scale": [1, 1, 1], "translate": [0, 0, 0]},
        "metadata": {"referenceSystem": crs},
    }
    with pytest.raises(terrio.errors.InvalidDataError, match="referenceSystem"):
        terrio.open_bytes(json.dumps(doc).encode(), filename="crs.city.json")


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


def test_cityjson_degenerate_rings_keep_faces_and_semantic_alignment():
    doc = {
        "type": "CityJSON",
        "version": "2.0",
        "transform": {"scale": [1, 1, 1], "translate": [0, 0, 0]},
        "vertices": [[10, 0, 0], [10, 10, 0], [10, 10, 10], [10, 0, 10]],
        "CityObjects": {
            "house": {
                "type": "Building",
                "geometry": [
                    {
                        "type": "Solid",
                        "lod": "2.2",
                        "boundaries": [[[[0, 0, 0]], [[0, 1, 2, 3], [0, 0, 0]], [[0, 3, 2, 1]]]],
                        "semantics": {
                            "surfaces": [
                                {"type": "GroundSurface"},
                                {"type": "WallSurface"},
                                {"type": "RoofSurface"},
                            ],
                            "values": [[0, 1, 2]],
                        },
                    }
                ],
            }
        },
    }
    ds = terrio.open_bytes(json.dumps(doc).encode(), filename="tile.city.json")
    features = ds[0].to_geojson()["features"]
    assert len(features) == 2
    assert [json.loads(f["properties"]["surface_semantics"])["type"] for f in features] == [
        "WallSurface",
        "RoofSurface",
    ]
    assert all(f["properties"]["skipped_rings"] == 2 for f in features)
    assert all(f["properties"]["skipped_faces"] == 1 for f in features)
    assert len(features[0]["geometry"]["coordinates"]) == 1
    assert len(features[0]["geometry"]["coordinates"][0]) == 5


def test_cityjson_all_degenerate_faces_keep_object_diagnostics():
    doc = {
        "type": "CityJSON",
        "version": "2.0",
        "vertices": [[1, 2, 3]],
        "transform": {"scale": [1, 1, 1], "translate": [0, 0, 0]},
        "CityObjects": {
            "house": {
                "type": "Building",
                "geometry": [
                    {
                        "type": "MultiSurface",
                        "lod": "2.2",
                        "boundaries": [[[0, 0, 0]]],
                    }
                ],
            }
        },
    }
    ds = terrio.open_bytes(json.dumps(doc).encode(), filename="tile.city.json")
    feature = ds[0].to_geojson()["features"][0]
    assert feature["geometry"] is None
    assert feature["properties"]["city_id"] == "house"
    assert feature["properties"]["skipped_rings"] == 1
    assert feature["properties"]["skipped_faces"] == 1
    doc.pop("transform")
    with pytest.raises(terrio.errors.InvalidDataError, match="transform"):
        terrio.open_bytes(json.dumps(doc).encode(), filename="missing.city.json")
