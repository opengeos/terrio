"""Raster/vector analysis is available without going through the CLI."""

import numpy as np
import pytest

import terrio


def features(layer):
    return layer.to_geojson()["features"]


@pytest.mark.parametrize("kind", ["loaded", "lazy", "path"])
def test_contour_band_options_and_3d_output(tmp_path, kind):
    values = np.array([[[0, 10], [0, 10]], [[0, 20], [0, 20]]], dtype="float32")
    source = terrio.Raster.from_array(values, (0, 1, 0, 2, 0, -1), crs="EPSG:4326")
    if kind != "loaded":
        path = tmp_path / "source.tif"
        source.write(str(path))
        source = path if kind == "path" else terrio.open_raster(str(path), lazy=True)
    lines = terrio.contour(source, band=2, levels=[10], with_z=True, field_name="height")
    result = features(lines)
    assert len(result) == 1
    assert result[0]["properties"]["height"] == 10
    coords = np.asarray(result[0]["geometry"]["coordinates"])
    np.testing.assert_allclose(coords[:, 0], 1, atol=1e-6)
    np.testing.assert_array_equal(coords[:, 2], 10)
    assert (coords[:, 1].min(), coords[:, 1].max()) == (0, 2)
    with pytest.raises(ValueError, match="interval or"):
        terrio.contour(source)
    with pytest.raises(ValueError, match="cannot be used together"):
        terrio.contour(source, interval=10, nodata=-9999, ignore_nodata=True)


def test_footprint_band_union_intersection_mask_and_attributes():
    source = terrio.Raster.from_array(
        np.array([[[1, 0], [0, 0]], [[0, 1], [0, 0]]], dtype="uint8"),
        (0, 1, 0, 2, 0, -1),
        crs="EPSG:4326",
        nodata=0,
    )
    both = features(terrio.footprint(source, singlepart=True, attributes={"source": "test"}))
    assert len(both) == 1 and both[0]["properties"] == {"source": "test"}
    assert features(terrio.footprint(source, intersection=True)) == []
    mask = terrio.Raster.from_array(
        np.array([[[1, 0], [0, 0]]], dtype="uint8"), (0, 1, 0, 2, 0, -1), crs="EPSG:4326"
    )
    selected = features(terrio.footprint(source, mask=mask, bands=[1]))
    ring = selected[0]["geometry"]["coordinates"][0][0]
    assert {tuple(p) for p in ring} == {(0, 1), (1, 1), (1, 2), (0, 2)}


def polygon(coordinates):
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"group": "a"},
                "geometry": {"type": "Polygon", "coordinates": [coordinates]},
            }
        ],
    }


def test_geometry_diagnostics_and_repairs_preserve_attributes():
    source = polygon([[0, 0], [2, 2], [0, 2], [2, 0], [0, 0]])
    before = features(terrio.check_geometry(source))[0]
    assert before["properties"]["is_valid"] is False
    assert before["properties"]["valid_reason"] != "Valid Geometry"
    repaired = terrio.make_valid(source)
    after = features(terrio.check_geometry(repaired))[0]
    assert after["properties"]["is_valid"] is True
    assert after["properties"]["group"] == "a"
    assert before["geometry"] == source["features"][0]["geometry"]


def test_buffer_and_grouped_dissolve():
    points = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"group": group},
                "geometry": {"type": "Point", "coordinates": [x, 0]},
            }
            for x, group in [(0, "a"), (1, "a"), (5, "b")]
        ],
    }
    buffered = terrio.buffer(points, 1, quadrant_segments=4)
    assert len(features(buffered)) == 3
    assert len(features(terrio.dissolve(buffered, fields=["group"]))) == 2
    assert all(f["properties"]["is_valid"] for f in features(terrio.check_geometry(buffered)))
    with pytest.raises(ValueError):
        terrio.buffer(points, 1, quadrant_segments=0)
