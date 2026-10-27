"""compare, pixel_info and xyz: GDAL-style reports, pixel queries and XYZ text."""

import json

import numpy as np
import pytest

import terrio


def _raster(values, nodata=None):
    return terrio.Raster.from_array(
        np.asarray(values, dtype="float32"),
        (100, 10, 0, 240, 0, -10),
        crs="EPSG:32611",
        nodata=nodata,
    )


def test_compare_reports_differences_as_gdal(tmp_path):
    a = np.arange(20, dtype="float32").reshape(4, 5)
    b = a.copy()
    b[1, 2] += 2.5
    ref, out = str(tmp_path / "a.tif"), str(tmp_path / "b.tif")
    terrio.write_raster(_raster(a), ref)
    terrio.write_raster(_raster(b), out)
    assert terrio.compare(ref, ref)["difference_count"] == 0
    c = terrio.compare(ref, out)
    messages = [d["message"] for d in c["differences"]]
    assert messages[0] == "Reference file and input file differ at the binary level."
    assert "1: pixels differing: 1" in messages
    assert "1: maximum pixel value difference: 2.500000" in messages
    assert c["difference_count"] == len(messages)
    assert c["bands"][0] == {
        "band": "1",
        "data_type": "Float32",
        "pixels": 20,
        "pixels_differing": 1,
        "max_abs_diff": 2.5,
    }
    # In-memory rasters: no binary comparison.
    assert terrio.compare(_raster(a), _raster(b))["difference_count"] == 2
    assert terrio.compare(ref, out, skip_all_optional=True)["difference_count"] == 2


def test_pixel_info_formats_and_interpolation():
    src = _raster(np.arange(20).reshape(4, 5), nodata=7)
    g = terrio.pixel_info(src, [(2.5, 1.5), (1.0, 1.0)], resampling="bilinear")
    assert g["crs"]["properties"]["name"] == "urn:ogc:def:crs:EPSG::32611"
    first = g["features"][0]["properties"]
    assert (first["column"], first["line"]) == (2.5, 1.5)
    # Pixel 7 is nodata: the bilinear kernel touching it has no value, nearest at it neither.
    assert "raw_value" not in first["bands"][0]
    assert g["features"][1]["properties"]["bands"][0]["raw_value"] == 3.0
    rows = terrio.pixel_info(src, [(125, 225, "site a")], position_crs="dataset", format="json")
    assert rows[0]["extra"] == "site a" and rows[0]["bands"][0]["value"] is None
    text = terrio.pixel_info(src, [(0.5, 0.5)], format="text")
    assert text == "Report:\n  Location: (0P,0L)\n  Band 1:\n    Value: 0\n"
    assert terrio.pixel_info(src, [(9, 9)], format="value") == "\n"
    csv = terrio.pixel_info(src, [(0.5, 0.5)], format="csv").splitlines()
    assert (
        csv[0] == "geom_x,geom_y,extra_content,column,line,band_1_raw_value,band_1_unscaled_value"
    )
    assert csv[1] == "0.5,0.5,,0.5,0.5,0,0"


def test_pixel_info_from_a_layer(tmp_path):
    src = _raster(np.arange(20).reshape(4, 5))
    path = tmp_path / "points.geojson"
    features = [
        {
            "type": "Feature",
            "properties": {"name": "p"},
            "geometry": {"type": "Point", "coordinates": [125.0, 225.0]},
        }
    ]
    path.write_text(json.dumps({"type": "FeatureCollection", "features": features}))
    g = terrio.pixel_info(src, position_dataset=str(path), position_crs="dataset")
    props = g["features"][0]["properties"]
    assert props["name"] == "p" and props["bands"][0]["raw_value"] == 7.0
    assert (
        "name"
        not in terrio.pixel_info(
            src, position_dataset=str(path), position_crs="dataset", include_fields=["NONE"]
        )["features"][0]["properties"]
    )


def test_xyz_text_and_file(tmp_path):
    src = _raster(np.arange(6).reshape(2, 3), nodata=5)
    assert terrio.xyz(src).splitlines()[0] == "105.000 235.000 0"
    assert len(terrio.xyz(src, skip_nodata=True).splitlines()) == 5
    assert terrio.xyz(src, skip=2, delimiter=",") == "105.000,235.000,0\n125.000,235.000,2\n"
    path = tmp_path / "out.xyz"
    assert terrio.xyz(src, str(path), srcwin=(1, 1, 2, 1), dst_nodata=-1) == 2
    assert path.read_text() == "115.000 225.000 4\n125.000 225.000 -1\n"
    with pytest.raises(ValueError):
        terrio.xyz(src, srcwin=(2, 0, 5, 1))


def test_same_object_compare_and_xyz_preserves_source(tmp_path):
    src = _raster(np.arange(6).reshape(2, 3))
    assert terrio.compare(src, src)["differences"] == []
    path = str(tmp_path / "source.tif")
    src.write(path)
    before = (tmp_path / "source.tif").read_bytes()
    with pytest.raises(ValueError, match="overwrite the input"):
        terrio.xyz(path, path)
    assert (tmp_path / "source.tif").read_bytes() == before


def test_pixel_info_file_output_and_field_separator(tmp_path):
    src = _raster(np.stack([np.arange(6).reshape(2, 3), np.arange(10, 16).reshape(2, 3)]))
    assert terrio.pixel_info(src, [(0, 0)], format="value", field_sep="|") == "0|10\n"
    path = str(tmp_path / "samples.gpkg")
    result = terrio.pixel_info(src, [(0, 0), (-1, -1)], dst=path)
    assert len(result["features"]) == 2
    assert len(terrio.open_vector(path)[0]) == 2


def test_pixel_info_refuses_two_position_sources():
    src = _raster([[1]])
    with pytest.raises(terrio.InvalidArgumentError, match="not both"):
        terrio.pixel_info(
            src, [(0, 0)], position_dataset={"type": "FeatureCollection", "features": []}
        )


def test_invalid_pixel_info_format_does_not_create_output(tmp_path):
    dst = tmp_path / "refused.gpkg"
    with pytest.raises(ValueError, match="unknown format"):
        terrio.pixel_info("missing.tif", [(0, 0)], dst=str(dst), format="xml")
    assert not dst.exists()


def test_xyz_empty_bands_keeps_first_band_default():
    src = _raster(np.array([[[1, 2]], [[10, 20]]]))
    assert terrio.xyz(src, bands=[]) == terrio.xyz(src, bands=[1])
    assert terrio.xyz(src, all_bands=True).splitlines()[0] == "105.000 235.000 1 10"
    assert terrio.xyz(src, all_bands=True) != terrio.xyz(src, bands=[])
