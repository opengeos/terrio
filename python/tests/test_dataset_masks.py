"""GDAL 3.13.2 per-dataset validity survives conversion and lazy warp inputs (#431)."""

import numpy as np
import pytest

import terrio


@pytest.mark.parametrize("external", [False, True])
def test_warp_honors_dataset_masks_for_paths_objects_and_mixed_inputs(gdal, tmp_path, external):
    path = tmp_path / "masked.tif"
    with gdal.config_option("GDAL_TIFF_INTERNAL_MASK", "NO" if external else "YES"):
        ds = gdal.GetDriverByName("GTiff").Create(
            str(path), 64, 64, 1, gdal.GDT_Byte, ["TILED=YES"]
        )
        ds.SetGeoTransform([0, 1, 0, 64, 0, -1])
        ds.GetRasterBand(1).Fill(100)
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        mask = np.full((64, 64), 255, "uint8")
        mask[:, :33] = 0
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        ds.BuildOverviews("NEAREST", [2, 4])
        ds = None
    lazy = terrio.open_raster(str(path), lazy=True)
    for overview in ["NONE", "AUTO", 0]:
        expected = gdal.Warp(
            "", str(path), format="MEM", width=32, height=32, overviewLevel=overview
        ).ReadAsArray()
        for source in [path, lazy, [lazy, lazy], [path, lazy]]:
            result = terrio.warp(source, ts=(32, 32), overview=overview)
            np.testing.assert_array_equal(result.read(1), expected)
    for driver in ["GTiff", "COG"]:
        out = tmp_path / f"{driver}.tif"
        lazy.write(str(out), driver=driver, tile_size=16)
        ds = gdal.Open(str(out))
        assert ds.GetRasterBand(1).GetMaskFlags() == gdal.GMF_PER_DATASET
        np.testing.assert_array_equal(ds.GetRasterBand(1).GetMaskBand().ReadAsArray(), mask)
