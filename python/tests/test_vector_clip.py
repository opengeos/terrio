"""Shared vector clipping across in-memory data, templates and file inputs."""

import json

import numpy as np
import pytest

import terrio


def feature(kind, coordinates, name):
    return {
        "type": "Feature",
        "properties": {"name": name},
        "geometry": {"type": kind, "coordinates": coordinates},
    }


def source():
    return {
        "type": "FeatureCollection",
        "features": [
            feature("LineString", [[-1, 1, 0], [3, 1, 40]], "road"),
            feature("Point", [0, 2, 7], "boundary"),
            feature("Point", [5, 5], "outside"),
        ],
    }


def rows(layer):
    return layer.to_geojson()["features"]


@pytest.mark.parametrize(
    "options",
    [
        {"bbox": [0, 0, 2, 2]},
        {"geometry": "POLYGON ((0 0,2 0,2 2,0 2,0 0))"},
        {
            "geometry": json.dumps(
                {"type": "Polygon", "coordinates": [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]]}
            )
        },
    ],
)
def test_bbox_and_geometry_keep_attributes_boundaries_and_interpolated_z(options):
    output = rows(terrio.clip_vector(source(), **options))
    assert [f["properties"]["name"] for f in output] == ["road", "boundary"]
    assert output[0]["geometry"] == {"type": "LineString", "coordinates": [[0, 1, 10], [2, 1, 30]]}
    assert output[1]["geometry"] == {"type": "Point", "coordinates": [0, 2, 7]}


def test_vector_template_filter_and_paths(tmp_path):
    mask = {
        "type": "FeatureCollection",
        "features": [
            feature("Polygon", [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]], "keep"),
            feature("Polygon", [[[4, 4], [6, 4], [6, 6], [4, 6], [4, 4]]], "drop"),
        ],
    }
    expected = rows(terrio.clip_vector(source(), bbox=[0, 0, 2, 2]))
    assert rows(terrio.clip_vector(source(), like=mask, like_where="name='keep'")) == expected
    assert len(rows(terrio.clip_vector(source(), like=mask))) == 3
    assert (
        rows(
            terrio.clip_vector(
                json.dumps(source()), like=json.dumps(mask), like_where="name='keep'"
            )
        )
        == expected
    )
    path = tmp_path / "source.geojson"
    path.write_text(json.dumps(source()))
    template = tmp_path / "mask.geojson"
    template.write_text(json.dumps(mask))
    assert rows(terrio.clip_vector(path, like=template, like_where="name='keep'")) == expected


def test_raster_template_and_lazy_path(tmp_path):
    raster = terrio.Raster.from_array(
        np.zeros((2, 2), dtype="uint8"), [0, 1, 0, 2, 0, -1], crs="EPSG:4326"
    )
    expected = rows(terrio.clip_vector(source(), bbox=[0, 0, 2, 2]))
    assert rows(terrio.clip_vector(source(), like=raster)) == expected
    path = tmp_path / "bounds.tif"
    raster.write(str(path))
    lazy = terrio.open_raster(str(path), lazy=True)
    assert rows(terrio.clip_vector(source(), like=lazy)) == expected
    assert rows(terrio.clip_vector(source(), like=path)) == expected


def test_region_reprojection_preserves_input_coordinates():
    # Transform arithmetic can move the exact upper boundary by a few ULPs.
    # Test coordinate preservation with an interior point; the boundary ambiguity
    # has its own independently checked GDAL conformance case.
    src = source()
    src["features"][1]["geometry"]["coordinates"] = [0, 1.5, 7]
    expected = rows(terrio.clip_vector(src, bbox=[0, 0, 2, 2]))
    output = rows(
        terrio.clip_vector(
            src, bbox=[0, 0, 222638.98158654713, 222684.20850554405], crs="EPSG:3857"
        )
    )
    assert [f["properties"] for f in output] == [f["properties"] for f in expected]
    np.testing.assert_allclose(
        output[0]["geometry"]["coordinates"], expected[0]["geometry"]["coordinates"], atol=1e-10
    )
    assert output[1]["geometry"] == expected[1]["geometry"]


@pytest.mark.parametrize(
    "options",
    [
        {},
        {"bbox": [0, 0, 1, 1], "geometry": "POINT (0 0)"},
        {"bbox": [0, 0, 0, 1]},
        {"geometry": "POINT (0 0)"},
        {"bbox": [0, 0, 1, 1], "like_where": "x=1"},
    ],
)
def test_invalid_options_fail(options):
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.clip_vector(source(), **options)


def test_geopackage_raster_template(tmp_path, gdal):
    from osgeo import gdal as api
    from osgeo import osr

    dataset = api.GetDriverByName("MEM").Create("", 64, 64, 1, api.GDT_Byte)
    dataset.SetGeoTransform([0, 2 / 64, 0, 2, 0, -2 / 64])
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(4326)
    dataset.SetProjection(crs.ExportToWkt())
    path = tmp_path / "bounds.gpkg"
    api.Translate(str(path), dataset, format="GPKG").Close()
    expected = rows(terrio.clip_vector(source(), bbox=[0, 0, 2, 2]))
    assert rows(terrio.clip_vector(source(), like=path)) == expected
