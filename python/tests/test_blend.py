"""Raster blend API, serialization, and validation."""

import numpy as np
import pytest

import terrio


def test_blend_alpha_and_output(tmp_path):
    base = terrio.Raster.from_array(
        np.array([[[30]], [[80]], [[170]], [[255]]], dtype="uint8"),
        (10, 1, 0, 20, 0, -1),
        crs="EPSG:32611",
    )
    overlay = terrio.Raster.from_array(
        np.array([[[250]], [[20]], [[0]], [[0]]], dtype="uint8"), (10, 1, 0, 20, 0, -1)
    )
    out = terrio.blend(base, overlay, str(tmp_path / "out.tif"))
    for band in range(1, 5):
        np.testing.assert_array_equal(out.read(band), base.read(band))
        np.testing.assert_array_equal(
            terrio.open(str(tmp_path / "out.tif")).read(band), base.read(band)
        )
    with pytest.raises(ValueError, match="operator"):
        terrio.blend(base, overlay, operator="bad")
    with pytest.raises(ValueError, match="opacity"):
        terrio.blend(base, overlay, opacity=101)
    assert out.nodata is None
