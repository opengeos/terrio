"""Keep the retile reference exceptions narrow and independently checked."""

import sys
from pathlib import Path

import numpy as np
import pytest
from osgeo import gdal, osr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_retile_with_gdal import compare_samples, verify_cog_reference_byte_bug


def write(folder, values, *, nodata=None, origin=0.0, mask=None):
    folder.mkdir(exist_ok=True)
    values = np.asarray([values], dtype=np.float32)
    ds = gdal.GetDriverByName("GTiff").Create(
        str(folder / "tile.tif"), values.shape[1], 1, 1, gdal.GDT_Float32
    )
    ds.SetGeoTransform([origin, 1, 0, 1, 0, -1])
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(4326)
    ds.SetProjection(crs.ExportToWkt())
    band = ds.GetRasterBand(1)
    band.WriteArray(values)
    if nodata is not None:
        band.SetNoDataValue(nodata)
    if mask is not None:
        band.CreateMaskBand(gdal.GMF_PER_DATASET)
        band.GetMaskBand().WriteArray(np.full(values.shape, mask, dtype=np.uint8))
    band = None
    ds.Close()
    return folder


def test_one_float32_ulp_requires_explicit_allowance(tmp_path):
    a = write(tmp_path / "a", [3000])
    b = write(tmp_path / "b", [np.nextafter(np.float32(3000), np.float32(np.inf))])
    with pytest.raises(AssertionError, match="different samples"):
        compare_samples(a, b)
    detail, rounded = compare_samples(a, b, allow_roundoff=True)
    assert rounded and "one Float32 ULP" in detail


@pytest.mark.parametrize("change", ["two-ulp", "mask", "origin", "infinity"])
def test_roundoff_cannot_hide_other_changes(tmp_path, change):
    value = np.float32(3000)
    step = np.nextafter(value, np.float32(np.inf))
    a = write(tmp_path / "a", [value], mask=0 if change == "mask" else None)
    b = write(
        tmp_path / "b",
        [
            np.inf
            if change == "infinity"
            else np.nextafter(step, np.float32(np.inf))
            if change == "two-ulp"
            else step
        ],
        mask=255 if change == "mask" else None,
        origin=0.01 if change == "origin" else 0,
    )
    with pytest.raises(AssertionError):
        compare_samples(a, b, allow_roundoff=True)


def test_cog_workaround_requires_exact_byte_conversion_defect(tmp_path):
    good = write(tmp_path / "good", [3000, 1.25, -2])
    bad = write(tmp_path / "bad", [255, 1, 0])
    verify_cog_reference_byte_bug(bad, good)
    wrong = write(tmp_path / "wrong", [254, 1, 0])
    with pytest.raises(AssertionError, match="known Byte conversion"):
        verify_cog_reference_byte_bug(wrong, good)
    with pytest.raises(AssertionError):
        verify_cog_reference_byte_bug(good, good)
