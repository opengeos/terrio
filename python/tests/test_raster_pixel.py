"""Pixel-wise raster operations: unscale, nodata_to_alpha, reclassify and as_features."""

import random

import numpy as np
import pytest

import terrio

GT = (100.0, 10.0, 0.0, 50.0, 0.0, -10.0)


def scaled(tmp_path):
    """A two-band Int16 GeoTIFF with scales, offsets and nodata, written and reopened.

    Args:
        tmp_path: The test's directory.

    Returns:
        str: The path.
    """
    data = np.array([[[1, 2], [-9999, 4]], [[10, 20], [30, -9999]]], dtype="int16")
    path = str(tmp_path / "scaled.tif")
    terrio.Raster.from_array(data, GT, crs="EPSG:32611", nodata=-9999).write(
        path,
        band_metadata=[{"scale": 0.5, "offset": 100.0}, {"scale": 2.0}],
    )
    return path


def test_unscale_applies_band_scales(tmp_path):
    path = scaled(tmp_path)
    out = terrio.unscale(path, str(tmp_path / "out.tif"))
    assert out.dtype == "float32"
    np.testing.assert_array_equal(out.read(1), [[100.5, 101.0], [-9999.0, 102.0]])
    np.testing.assert_array_equal(out.read(2), [[20.0, 40.0], [60.0, -9999.0]])
    np.testing.assert_array_equal(terrio.open(str(tmp_path / "out.tif")).read(1), out.read(1))
    byte = terrio.unscale(path, ot="Byte")
    np.testing.assert_array_equal(byte.read(1), [[101, 101], [0, 102]])
    with pytest.raises(ValueError, match="data type"):
        terrio.unscale(path, ot="Complex")


def test_nodata_to_alpha(tmp_path):
    src = terrio.Raster.from_array(
        np.array([[[1, 2, 3]], [[2, 6, 4]]], dtype="uint16"), GT, nodata=2
    )
    out = terrio.nodata_to_alpha(src, str(tmp_path / "alpha.tif"))
    assert out.count == 3 and out.dtype == "uint16" and out.nodata is None
    np.testing.assert_array_equal(out.read(3), [[255, 0, 255]])
    per_band = terrio.nodata_to_alpha(src, nodata=[1, 2])
    np.testing.assert_array_equal(per_band.read(3), [[0, 255, 255]])
    np.testing.assert_array_equal(terrio.nodata_to_alpha(src, nodata=3).read(3), [[255, 255, 0]])
    with pytest.raises(ValueError, match="one per band"):
        terrio.nodata_to_alpha(src, nodata=[1, 2, 3])


def test_reclassify_mapping_and_file(tmp_path):
    src = terrio.Raster.from_array(np.array([[1, 2, 255, 40]], dtype="uint8"), GT, nodata=255)
    out = terrio.reclassify(src, "[0,3)=1;[3,50]=2;NO_DATA=NO_DATA")
    np.testing.assert_array_equal(out.read(1), [[1, 1, 255, 2]])
    assert out.nodata == 255
    mapping = tmp_path / "map.txt"
    mapping.write_text("# classes\n[0,3)=1\n\nDEFAULT=PASS_THROUGH  # others\n")
    out = terrio.reclassify(src, f"@{mapping}", str(tmp_path / "r.tif"), ot="Int16")
    assert out.dtype == "int16"
    np.testing.assert_array_equal(out.read(1), [[1, 1, 255, 40]])
    with pytest.raises(ValueError, match="no specified mapping"):
        terrio.reclassify(src, "1=2")
    with pytest.raises(ValueError, match="overlaps"):
        terrio.reclassify(src, "[0,3)=1;[2,4)=2")


def test_as_features_fields_and_geometries(tmp_path):
    src = terrio.Raster.from_array(
        np.array([[[1, 0], [3, 4]], [[5, 6], [7, 8]]], dtype="uint8"),
        GT,
        crs="EPSG:32611",
        nodata=0,
    )
    path = str(tmp_path / "pixels.gpkg")
    layer = terrio.as_features(
        src, path, geometry_type="polygon", include_xy=True, include_row_col=True
    )
    assert len(layer) == 4
    reread = terrio.open_vector(path).layers[0]
    assert reread.name == "pixels" and len(reread) == 4
    skipped = terrio.as_features(src, skip_nodata=True, bands=[1], geometry_type="point")
    assert len(skipped) == 3
    with pytest.raises(ValueError, match="geometry type"):
        terrio.as_features(src, geometry_type="line")


@pytest.mark.parametrize("options", [{"format": "CSV"}, {"append": True}, {"overwrite": True}])
def test_as_features_output_options_require_destination(options):
    src = terrio.Raster.from_array(np.array([[1]], dtype="uint8"), GT)
    with pytest.raises(ValueError, match="require dst"):
        terrio.as_features(src, **options)


def test_float32_feature_nodata_and_duplicate_bands():
    near = np.nextafter(np.float32(0.1), np.float32(1))
    src = terrio.Raster.from_array(np.array([[0.1, near, 0.2]], dtype="float32"), GT, nodata=0.1)
    layer = terrio.as_features(src, skip_nodata=True, bands=[1, 1])
    features = layer.to_geojson()["features"]
    assert len(features) == 2
    assert list(features[0]["properties"]) == ["BAND_1"]
    assert features[0]["properties"]["BAND_1"] == float(near)


def test_mapping_file_limit_and_signed_nan(tmp_path):
    src = terrio.Raster.from_array(np.array([[1]], dtype="uint8"), GT)
    path = tmp_path / "huge-map.txt"
    with path.open("wb") as file:
        file.truncate(10_000_001)
    with pytest.raises(ValueError, match="too large"):
        terrio.reclassify(src, f"@{path}")
    for mapping in ("-nan=0", "+nan=0"):
        with pytest.raises(ValueError, match="NaN"):
            terrio.reclassify(src, mapping)


def test_many_intervals_with_nan_nodata_return_an_error_instead_of_panicking():
    src = terrio.Raster.from_array(
        np.array([[np.nan, 1.0]], dtype="float32"), GT, nodata=float("nan")
    )
    entries = [f"{i}={i}" for i in range(80)] + ["NO_DATA=0"]
    random.Random(0).shuffle(entries)
    with pytest.raises(terrio.errors.InvalidArgumentError, match="overlaps"):
        terrio.reclassify(src, ";".join(entries))
