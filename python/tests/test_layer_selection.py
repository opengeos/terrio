"""Consistent vector layer selection through readers and operations (#529)."""

import json

import pytest

import terrio


def test_layer_alias_and_selection_errors(tmp_path):
    collection = {
        "type": "FeatureCollection",
        "name": "roads",
        "features": [
            {
                "type": "Feature",
                "properties": {"v": 7},
                "geometry": {"type": "Point", "coordinates": [1, 2]},
            }
        ],
    }
    path = tmp_path / "map.geojson"
    path.write_text(json.dumps(collection))
    for names in ([], ["map"], ["roads"], ["roads", "map", "map"]):
        ds = terrio.open_vector(str(path), layers=names)
        assert len(ds) == 1
        assert ds[0].name == "roads"
        assert terrio.render(path, layers=names).startswith(b"\x89PNG")
    dataset = terrio.open_vector(str(path))
    # Materialized datasets carry actual layer names, without a filename alias.
    assert terrio.render(dataset, layers=["roads"]).startswith(b"\x89PNG")
    with pytest.raises(ValueError) as error:
        terrio.render(dataset, layers=["map"])
    assert str(error.value) == "no layer named map (layers: roads)"
    with pytest.raises(ValueError) as error:
        terrio.vector_grid(dataset, layers=["map"])
    assert str(error.value) == "no layer named map (layers: roads)"
    for source in (str(path), dataset):
        with pytest.raises(ValueError) as error:
            terrio.render(source, layers=["roads", "missing"])
        assert str(error.value) == "no layer named missing (layers: roads)"
        with pytest.raises(ValueError) as error:
            terrio.vector_grid(source, layers=["roads", "missing"])
        assert str(error.value) == "no layer named missing (layers: roads)"
