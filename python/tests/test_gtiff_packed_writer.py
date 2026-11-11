"""Packed unsigned TIFF writes and copies, compared with GDAL 3.13.3."""

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.shutil import copy
from terrio.rio.transform import from_origin


@pytest.mark.parametrize(
    "bits,dtype",
    [
        (1, "uint8"),
        (2, "uint8"),
        (4, "uint8"),
        (7, "uint8"),
        (9, "uint16"),
        (12, "uint16"),
        (15, "uint16"),
        (17, "uint32"),
        (24, "uint32"),
        (31, "uint32"),
    ],
)
@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_packed_unsigned_write_retains_samples_and_bit_width(tmp_path, gdal, bits, dtype, driver):
    y, x = np.mgrid[:19, :37]
    values = ((x * 104729 + y * 15485863) % (1 << bits)).astype(dtype)
    path = tmp_path / "packed.tif"
    transform = from_origin(500000, 5800000, 10, 10)
    with rio.open(
        path,
        "w",
        driver=driver,
        width=37,
        height=19,
        count=1,
        dtype=dtype,
        nbits=bits,
        crs="EPSG:32633",
        transform=transform,
        compress="DEFLATE",
        blocksize=16,
        tiled=True,
        blockxsize=16,
        blockysize=16,
    ) as dst:
        dst.write(values, 1)
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1)
    assert band.GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == str(bits)
    np.testing.assert_array_equal(band.ReadAsArray(), values)
    assert ds.GetGeoTransform() == transform.to_gdal()
    with rio.open(path) as src:
        np.testing.assert_array_equal(src.read(1), values)
        assert src.tags(1, ns="IMAGE_STRUCTURE")["NBITS"] == str(bits)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_copy_retains_bilevel_palette(tmp_path, gdal, driver):
    values = (np.indices((19, 37)).sum(axis=0) % 3 == 0).astype("uint8")
    source = tmp_path / "source.tif"
    ds = gdal.GetDriverByName("GTiff").Create(
        str(source), 37, 19, 1, gdal.GDT_Byte, options=["NBITS=1"]
    )
    ds.GetRasterBand(1).WriteArray(values)
    palette = gdal.ColorTable()
    palette.SetColorEntry(0, (0, 0, 0, 255))
    palette.SetColorEntry(1, (255, 255, 255, 255))
    ds.GetRasterBand(1).SetRasterColorTable(palette)
    ds = None
    path = tmp_path / "copy.tif"
    copy(source, path, driver=driver, compress="DEFLATE")
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1)
    assert band.GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "1"
    assert band.GetRasterColorTable().GetCount() == 2
    np.testing.assert_array_equal(band.ReadAsArray(), values)


@pytest.mark.parametrize(
    "bits,dtype,extra",
    [
        (0, "uint8", {}),
        (8, "uint16", {}),
        (16, "uint32", {}),
        (9, "uint8", {}),
        (17, "uint16", {}),
        (33, "uint32", {}),
        ("invalid", "uint8", {}),
        (4, "int16", {}),
        (4, "float32", {}),
        (4, "uint8", {"predictor": 2}),
    ],
)
def test_invalid_packed_options_are_refused(tmp_path, bits, dtype, extra):
    with pytest.raises(Exception, match="NBITS|packed"):
        with rio.open(
            tmp_path / "invalid.tif",
            "w",
            driver="GTiff",
            width=9,
            height=1,
            count=1,
            dtype=dtype,
            nbits=bits,
            compress="DEFLATE",
            **extra,
        ) as dst:
            dst.write(np.zeros((1, 9), dtype=dtype), 1)


def test_explicit_bit_width_overrides_the_source(tmp_path, gdal):
    source = tmp_path / "source.tif"
    ds = gdal.GetDriverByName("GTiff").Create(
        str(source), 9, 1, 1, gdal.GDT_Byte, options=["NBITS=1"]
    )
    ds.GetRasterBand(1).WriteArray(np.array([[0, 1, 0, 1, 0, 1, 0, 1, 1]], dtype="uint8"))
    ds = None
    path = tmp_path / "wider.tif"
    copy(source, path, nbits=4)
    ds = gdal.Open(str(path))
    assert ds.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "4"
