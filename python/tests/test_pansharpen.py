"""Weighted Brovey bindings and persistence on unequal-resolution images."""

import numpy as np
import pytest

import terrio


def test_pansharpen_weights_bands_and_path(tmp_path):
    pan = terrio.Raster.from_array(
        np.full((4, 4), 80, dtype="uint16"), (0, 1, 0, 4, 0, -1), crs="EPSG:32611"
    )
    spectral = terrio.Raster.from_array(
        np.stack([np.full((2, 2), 20), np.full((2, 2), 40)]).astype("uint16"),
        (0, 2, 0, 4, 0, -2),
        crs="EPSG:32611",
    )
    path = str(tmp_path / "sharp.tif")
    out = terrio.pansharpen(pan, spectral, path, ot="Float64")
    np.testing.assert_array_equal(out.read(1), np.full((4, 4), 53))
    np.testing.assert_array_equal(out.read(2), np.full((4, 4), 107))
    np.testing.assert_array_equal(terrio.open(path).read(), out.read())
    selected = terrio.pansharpen(pan, [spectral], weights=[0.75, 0.25], bands=[2])
    np.testing.assert_array_equal(selected.read(1), np.full((4, 4), 128))
    with pytest.raises(ValueError, match="weight"):
        terrio.pansharpen(pan, spectral, weights=[1])


def test_pansharpen_virtual_path_and_reopen(tmp_path):
    pan = terrio.Raster.from_array(
        np.full((8, 8), 80, dtype="uint16"), (0, 1, 0, 8, 0, -1), crs="EPSG:32611"
    )
    spectral = terrio.Raster.from_array(
        np.stack([np.full((4, 4), 20), np.full((4, 4), 40)]).astype("uint16"),
        (0, 2, 0, 8, 0, -2),
        crs="EPSG:32611",
    )
    pan_path, spectral_path = tmp_path / "pan.tif", tmp_path / "spectral.tif"
    pan.write(str(pan_path))
    spectral.write(str(spectral_path))
    path = str(tmp_path / "sharp.vrt")
    out = terrio.pansharpen(pan_path, [spectral_path], path, bands=[2, 1, 2])
    assert "VRTPansharpenedDataset" in (tmp_path / "sharp.vrt").read_text()
    np.testing.assert_array_equal(out.read(1, window=(2, 3, 3, 2)), np.full((2, 3), 107))
    np.testing.assert_array_equal(terrio.open_raster(path, lazy=True).read(), out.read())
    with pytest.raises(ValueError, match="persistent"):
        terrio.pansharpen(pan, spectral, path)
