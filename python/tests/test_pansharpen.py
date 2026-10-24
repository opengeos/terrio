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
