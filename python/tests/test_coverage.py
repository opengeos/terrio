"""Coverage validation uses GDAL output schema and reports original boundaries."""

import pytest

import terrio


def source(shift=0):
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"name": "west"},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[[0, 0], [1, 0], [1, 2], [0, 2], [0, 0]]],
                },
            },
            {
                "type": "Feature",
                "properties": {"name": "east"},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [
                            [1 + shift, 0],
                            [2 + shift, 0],
                            [2 + shift, 2],
                            [1 + shift, 2],
                            [1 + shift, 0],
                        ]
                    ],
                },
            },
        ],
    }


def test_valid_coverage_and_include_valid_correspondence():
    assert len(terrio.check_coverage(source())) == 0
    layer = terrio.check_coverage(source(), include_valid=True)
    assert layer.fields == {}
    output = layer.to_geojson()["features"]
    assert all(f["properties"] is None for f in output)
    assert all(f["geometry"]["coordinates"] == [] for f in output)


def test_overlap_and_narrow_gap_detection():
    assert len(terrio.check_coverage(source(-0.1))) == 2
    assert len(terrio.check_coverage(source(0.1))) == 0
    output = terrio.check_coverage(source(0.1), maximum_gap_width=0.2)
    assert len(output) == 2
    assert all(f["geometry"]["type"] == "MultiLineString" for f in output.to_geojson()["features"])


@pytest.mark.parametrize("gap", [-1, float("nan"), float("inf")])
def test_invalid_gap_widths_fail(gap):
    with pytest.raises(ValueError, match="maximum_gap_width"):
        terrio.check_coverage(source(), maximum_gap_width=gap)


def test_multilayer_input_requires_selection():
    valid = terrio.Layer.from_geojson(source(), name="valid")
    overlapping = terrio.Layer.from_geojson(source(-0.1), name="overlap")
    with pytest.raises(ValueError, match="choose one"):
        terrio.check_coverage([valid, overlapping])
    assert len(terrio.check_coverage([valid, overlapping], layer="overlap")) == 2
