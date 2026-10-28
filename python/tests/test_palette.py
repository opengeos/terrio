"""Palette conversion across in-memory, serialized and guarded path APIs."""

import numpy as np
import pytest

import terrio


def test_palette_roundtrip_and_destination_guards(tmp_path):
    values = np.array([[[248, 0]], [[0, 248]], [[0, 0]]], dtype="uint8")
    source = terrio.Raster.from_array(values, (10, 1, 0, 20, 0, -1), crs="EPSG:4326")
    colors = [[248, 0, 0, 255], [0, 248, 0, 255]]
    path = str(tmp_path / "indexed.tif")
    indexed = terrio.rgb_to_palette(source, path, color_map=colors, dither=False, bit_depth=8)
    np.testing.assert_array_equal(indexed.read(1), [[0, 1]])
    expanded = terrio.palette_to_rgb(path)
    for b in range(1, 4):
        np.testing.assert_array_equal(expanded.read(b), values[b - 1])
    attached = terrio.attach_palette(indexed, [[1, 2, 3, 255], [4, 5, 6, 255]])
    np.testing.assert_array_equal(attached.read(1), indexed.read(1))
    assert terrio.read_color_table(attached)[0] == [1, 2, 3, 255]
    before = (tmp_path / "indexed.tif").read_bytes()
    opened = terrio.open_raster(path, lazy=True)
    with pytest.raises(ValueError, match="input"):
        terrio.palette_to_rgb(opened, path, overwrite=True)
    with pytest.raises(ValueError, match="input"):
        terrio.rgb_to_palette(source, path, color_map=opened, overwrite=True)
    with pytest.raises(ValueError, match="exists"):
        terrio.rgb_to_palette(source, path)
    with pytest.raises(ValueError, match="input"):
        terrio.palette_to_rgb(path, path, overwrite=True)
    alias = tmp_path / "alias.tif"
    alias.hardlink_to(tmp_path / "indexed.tif")
    with pytest.raises(ValueError, match="alias"):
        terrio.attach_palette(path, colors, str(alias), overwrite=True)
    assert (tmp_path / "indexed.tif").read_bytes() == before
    with pytest.raises(ValueError, match="dimensions"):
        terrio.rgb_to_palette(source, mask=np.ones((2, 1), dtype="uint8"))
    with pytest.raises(ValueError, match="bit depth"):
        terrio.rgb_to_palette(source, bit_depth=6)


def test_palette_mask_and_color_files():
    source = terrio.Raster.from_array(np.full((3, 2, 2), 248, dtype="uint8"), (0, 1, 0, 2, 0, -1))
    out = terrio.rgb_to_palette(source, mask=np.array([[0, 255], [255, 255]], dtype="uint8"))
    assert out.nodata == 0
    assert out.read(1)[0, 0] == 0
    rgba = terrio.palette_to_rgb(out, rgba=True)
    assert rgba.read(4)[0, 0] == 0
    assert rgba.read(4)[1, 1] == 255
    assert terrio.read_color_table(b"0 1 2 3 255\n2 red 128")[2] == [255, 0, 0, 128]


def test_palette_dataset_mask_survives_serialization(tmp_path):
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    path = str(tmp_path / "masked.tif")
    ds = gdal.GetDriverByName("GTiff").Create(path, 2, 2, 1, gdal.GDT_Byte)
    ds.SetGeoTransform((10, 1, 0, 20, 0, -1))
    table = gdal.ColorTable()
    table.SetColorEntry(0, (10, 20, 30, 255))
    table.SetColorEntry(1, (40, 50, 60, 255))
    ds.GetRasterBand(1).SetColorTable(table)
    ds.GetRasterBand(1).WriteArray(np.array([[0, 1], [1, 0]], dtype="uint8"))
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    mask = np.array([[0, 255], [255, 255]], dtype="uint8")
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
    ds = None
    for result in [terrio.palette_to_rgb(path), terrio.attach_palette(path, [[1, 2, 3, 255]])]:
        output = str(tmp_path / "output.tif")
        result.write(output)
        actual = gdal.Open(output)
        np.testing.assert_array_equal(actual.GetRasterBand(1).GetMaskBand().ReadAsArray(), mask)
        actual = None
        memory_path = "/vsimem/terrio-palette-test.tif"
        gdal.FileFromMemBuffer(memory_path, result.to_bytes("output.tif"))
        try:
            actual = gdal.Open(memory_path)
            np.testing.assert_array_equal(actual.GetRasterBand(1).GetMaskBand().ReadAsArray(), mask)
            actual = None
        finally:
            gdal.Unlink(memory_path)
