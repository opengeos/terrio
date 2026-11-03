"""Viewshed bindings, output persistence and invalid inputs."""

import numpy as np
import pytest

import terrio


def test_viewshed_ridge_and_height(tmp_path):
    src = terrio.Raster.from_array(
        np.array([[0, 0, 10, 0, 0]], dtype="float64"), (0, 10, 0, 10, 0, -10), crs="EPSG:32611"
    )
    path = str(tmp_path / "ground.tif")
    visible = terrio.viewshed(src, 5, 5, cc=0)
    np.testing.assert_array_equal(visible.read(1), [[255, 255, 255, 0, 0]])
    ground = terrio.viewshed(src, 5, 5, path, om="GROUND", cc=0)
    np.testing.assert_array_equal(ground.read(1), [[0, 0, 0, 14, 18]])
    np.testing.assert_array_equal(terrio.open(path).read(1), ground.read(1))
    np.testing.assert_array_equal(
        terrio.viewshed(src, 5, 5, cc=0, tz=14).read(1), [[255, 255, 255, 255, 0]]
    )
    assert terrio.viewshed(src, 5, 5, md=20).width == 3
    accum = terrio.viewshed(src, 0, 0, om="ACCUM", os=2, cc=0)
    np.testing.assert_array_equal(accum.read(1), [[170, 170, 255, 170, 170]])
    with pytest.raises(ValueError, match="spacing"):
        terrio.viewshed(src, 0, 0, om="ACCUM", os=0)
    assert terrio.viewshed(src, -15, 5).width == 5


@pytest.mark.parametrize("spacing", [1, 3, 20])
def test_cumulative_matches_gdal(tmp_path, spacing):
    """Compare the binding's Byte sums and metadata with GDAL 3.13.3."""
    import shutil
    import subprocess

    gdal = pytest.importorskip("osgeo.gdal")
    if shutil.which("gdal_viewshed") is None:
        pytest.skip("gdal_viewshed is required for the live comparison")

    rng = np.random.default_rng(812)
    z = rng.uniform(0, 100, (9, 11)).astype("float64")
    src = terrio.Raster.from_array(z, (400000, 30, 0, 4400000, 0, -30), crs="EPSG:32613")
    input_path = str(tmp_path / "dem.tif")
    output_path = str(tmp_path / "reference.tif")
    src.write(input_path)
    subprocess.run(
        ["gdal_viewshed", "-q", "-om", "ACCUM", "-os", str(spacing), input_path, output_path],
        check=True,
        capture_output=True,
    )
    expected = gdal.Open(output_path)
    actual = terrio.viewshed(src, 0, 0, om="ACCUM", os=spacing)
    np.testing.assert_array_equal(actual.read(1), expected.ReadAsArray())
    assert tuple(actual.transform) == expected.GetGeoTransform()
    assert actual.width == expected.RasterXSize
    assert actual.height == expected.RasterYSize
