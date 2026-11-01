"""Open TIFF handles retain their original file identity, compared with GDAL 3.13.3."""

import os
import sys

import numpy as np
import pytest

import terrio


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="GDAL 3.13.3 does not open Windows files with FILE_SHARE_DELETE",
)
@pytest.mark.parametrize("compression", ["NONE", "DEFLATE"])
def test_replacement_keeps_original_pixels(tmp_path, compression):
    gdal = pytest.importorskip("osgeo.gdal")
    path, replacement = tmp_path / "open.tif", tmp_path / "replacement.tif"

    def write(path, value, tile_size):
        ds = gdal.GetDriverByName("GTiff").Create(
            str(path),
            256,
            256,
            1,
            gdal.GDT_Byte,
            options=[
                "TILED=YES",
                f"BLOCKXSIZE={tile_size}",
                f"BLOCKYSIZE={tile_size}",
                f"COMPRESS={compression}",
            ],
        )
        ds.SetGeoTransform((100, 10, 0, 50, 0, -10))
        ds.GetRasterBand(1).Fill(value)
        ds = None

    previous_cache = gdal.GetCacheMax()
    gdal.SetCacheMax(0)
    try:
        write(path, 10, 64)
        original, oracle = terrio.open(str(path)), gdal.Open(str(path))
        np.testing.assert_array_equal(original.read(1, window=(0, 0, 128, 128)), 10)
        np.testing.assert_array_equal(oracle.ReadAsArray(0, 0, 128, 128), 10)
        write(replacement, 200, 128)
        os.replace(replacement, path)
        for size in [32, 128, 256, 64, 256]:
            actual = original.read(1, window=(0, 0, size, size))
            expected = oracle.ReadAsArray(0, 0, size, size)
            np.testing.assert_array_equal(actual, expected)
            np.testing.assert_array_equal(actual, 10)
        current, current_oracle = terrio.open(str(path)), gdal.Open(str(path))
        np.testing.assert_array_equal(current.read(1), current_oracle.ReadAsArray())
        np.testing.assert_array_equal(current.read(1), 200)
    finally:
        gdal.SetCacheMax(previous_cache)
