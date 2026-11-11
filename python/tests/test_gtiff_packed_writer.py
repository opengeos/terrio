"""Packed unsigned TIFF writes and copies, compared with GDAL 3.13.3."""

import struct

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


def write_sample_width_images(path, images):
    """Write independent TIFF 6.0 strips with per-image sample widths for GDAL 3.13.3 checks."""
    out = bytearray(b"II*\0" + struct.pack("<I", 8))
    for index, (values, bits) in enumerate(images):
        height, width = values.shape
        samples = np.asarray(values, dtype="uint32")
        binary = ((samples[..., None] >> np.arange(bits - 1, -1, -1, dtype="uint32")) & 1).astype(
            "uint8"
        )
        payload = np.packbits(binary.reshape(height, -1), axis=1).tobytes()
        count = 12
        offset = len(out) + 2 + count * 12 + 4
        tags = [
            (254, 4, int(index > 0)),
            (256, 4, width),
            (257, 4, height),
            (258, 3, bits),
            (259, 3, 1),
            (262, 3, 1),
            (273, 4, offset),
            (277, 3, 1),
            (278, 4, height),
            (279, 4, len(payload)),
            (284, 3, 1),
            (339, 3, 1),
        ]
        out.extend(struct.pack("<H", count))
        for tag, kind, value in tags:
            out.extend(struct.pack("<HHI", tag, kind, 1))
            out.extend(struct.pack("<H2x" if kind == 3 else "<I", value))
        following = offset + len(payload) if index + 1 < len(images) else 0
        out.extend(struct.pack("<I", following))
        out.extend(payload)
    path.write_bytes(out)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_internal_overview_copies_use_the_overviews_sample_width(tmp_path, gdal, driver):
    base = np.arange(16, dtype="uint16").reshape(4, 4)
    values = np.array([[1000, 2000], [3000, 4000]], dtype="uint16")
    source = tmp_path / "internal.tif"
    write_sample_width_images(source, [(base, 9), (values, 12)])
    reference = gdal.Open(str(source))
    overview = reference.GetRasterBand(1).GetOverview(0)
    assert overview.GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "12"
    np.testing.assert_array_equal(overview.ReadAsArray(), values)
    target = tmp_path / "internal-copy.tif"
    with rio.open(source, overview_level=0) as reader:
        assert reader.tags(1, ns="IMAGE_STRUCTURE")["NBITS"] == "12"
        copy(reader, target, driver=driver)
    actual = gdal.Open(str(target))
    assert actual.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "12"
    np.testing.assert_array_equal(actual.ReadAsArray(), values)


def test_wider_cog_overview_copy_is_refused_instead_of_clipped(tmp_path, gdal):
    driver = "COG"
    source = tmp_path / "external.tif"
    base = np.arange(16, dtype="uint8").reshape(4, 4)
    values = np.full((2, 2), 200, dtype="uint8")
    write_sample_width_images(source, [(base, 4)])
    write_sample_width_images(tmp_path / "external.tif.ovr", [(values, 8)])
    expected = gdal.Open(str(source))
    np.testing.assert_array_equal(expected.GetRasterBand(1).GetOverview(0).ReadAsArray(), values)
    target = tmp_path / "external-copy.tif"
    options = {}
    with pytest.raises(Exception, match="overview.*NBITS=4"):
        copy(source, target, driver=driver, **options)
    assert not target.exists()
    copy(source, target, driver=driver, nbits=8, **options)
    actual = gdal.Open(str(target))
    np.testing.assert_array_equal(actual.ReadAsArray(), base)
    np.testing.assert_array_equal(actual.GetRasterBand(1).GetOverview(0).ReadAsArray(), values)


@pytest.mark.parametrize("overview_kind", ["internal", "external"])
def test_auto_translation_does_not_label_wider_overview_values_as_the_base(
    tmp_path, gdal, overview_kind
):
    import terrio

    source = tmp_path / "auto.tif"
    if overview_kind == "internal":
        base = np.arange(16, dtype="uint16").reshape(4, 4)
        values, base_bits, overview_bits = np.full((2, 2), 2000, dtype="uint16"), 9, 12
        write_sample_width_images(source, [(base, base_bits), (values, overview_bits)])
    else:
        base = np.arange(16, dtype="uint8").reshape(4, 4)
        values, base_bits, overview_bits = np.full((2, 2), 200, dtype="uint8"), 4, 8
        write_sample_width_images(source, [(base, base_bits)])
        write_sample_width_images(tmp_path / "auto.tif.ovr", [(values, overview_bits)])
    expected = gdal.Open(str(source))
    np.testing.assert_array_equal(expected.GetRasterBand(1).GetOverview(0).ReadAsArray(), values)
    target = tmp_path / "auto-copy.tif"
    terrio.translate(str(source), str(target), outsize=(2, 2), overview="AUTO")
    actual = gdal.Open(str(target))
    np.testing.assert_array_equal(actual.ReadAsArray(), values)
    assert actual.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == (
        str(overview_bits) if overview_bits != values.dtype.itemsize * 8 else None
    )


@pytest.mark.parametrize("options", [{"compress": "JPEG"}, {"compress": "DEFLATE", "predictor": 2}])
def test_incompatible_destination_options_do_not_inherit_packed_width(tmp_path, gdal, options):
    source = tmp_path / "narrow.tif"
    values = np.zeros((16, 16), dtype="uint8")
    write_sample_width_images(source, [(values, 4)])
    target = tmp_path / "compatible.tif"
    copy(source, target, driver="GTiff", **options)
    actual = gdal.Open(str(target))
    assert actual.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") is None
    np.testing.assert_array_equal(actual.ReadAsArray(), values)
    with pytest.raises(Exception, match="packed NBITS"):
        copy(source, tmp_path / "explicit.tif", driver="GTiff", nbits=4, **options)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_unrepresentable_packed_nodata_is_refused_before_it_becomes_valid(tmp_path, driver):
    target = tmp_path / "nodata.tif"
    with pytest.raises(Exception, match="nodata.*NBITS=4"):
        with rio.open(
            target,
            "w",
            driver=driver,
            dtype="uint8",
            width=2,
            height=1,
            count=1,
            nodata=255,
            nbits=4,
        ) as writer:
            writer.write(np.array([[1, 255]], dtype="uint8"), 1)
    assert not target.exists()
