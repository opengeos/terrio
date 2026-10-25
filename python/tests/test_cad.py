"""DWG/DXF use the same vector APIs as other drivers, without GDAL."""

import pytest

import terrio


@pytest.mark.parametrize("ext", ["dxf", "dwg"])
def test_cad_roundtrip(tmp_path, ext):
    features = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"Layer": "survey", "Text": "日本語"},
                "geometry": {"type": "Point", "coordinates": [12, 34, 56]},
            },
            {
                "type": "Feature",
                "properties": {"Layer": "survey"},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]],
                        [[2, 2], [4, 2], [4, 4], [2, 4], [2, 2]],
                    ],
                },
            },
        ],
    }
    for properties in [
        {"Text": ""},
        {"Text": "annotation", "EntityType": "POINT"},
        {"Text": "multi\\Pline", "EntityType": "MTEXT"},
    ]:
        features["features"].append(
            {
                "type": "Feature",
                "properties": properties,
                "geometry": {"type": "Point", "coordinates": [1, 2, 3]},
            }
        )
    path = tmp_path / f"drawing.{ext}"
    terrio.write_vector(features, str(path))
    ds = terrio.open_vector(str(path))
    assert ds.driver == ext.upper()
    assert ds.layer_names == ["entities"]
    actual = ds[0].to_geojson()["features"]
    assert len(actual) == 5
    assert actual[0]["geometry"]["coordinates"] == [12, 34, 56]
    assert actual[0]["properties"]["Text"] == "日本語"
    assert actual[0]["properties"]["Layer"] == "survey"
    assert actual[1]["geometry"]["type"] == "Polygon"
    assert len(actual[1]["geometry"]["coordinates"]) == 2
    assert [f["properties"]["EntityType"] for f in actual[2:]] == ["POINT", "POINT", "MTEXT"]
    assert actual[4]["properties"]["Text"] == "multi\\Pline"
    target = tmp_path / "converted.geojson"
    terrio.convert(str(path), str(target))
    assert len(terrio.open_vector(str(target))[0]) == 5
