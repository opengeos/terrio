# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
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


def write_sample_width_images(path, images, photometric=1):
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
            (262, 3, photometric),
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


def test_wider_cog_overview_copy_uses_the_widest_stored_width(tmp_path, gdal):
    driver = "COG"
    source = tmp_path / "external.tif"
    base = np.arange(16, dtype="uint8").reshape(4, 4)
    values = np.full((2, 2), 200, dtype="uint8")
    write_sample_width_images(source, [(base, 4)])
    write_sample_width_images(tmp_path / "external.tif.ovr", [(values, 8)])
    expected = gdal.Open(str(source))
    np.testing.assert_array_equal(expected.GetRasterBand(1).GetOverview(0).ReadAsArray(), values)
    target = tmp_path / "external-copy.tif"
    with pytest.raises(Exception, match="overview.*NBITS=4"):
        copy(source, target, driver=driver, nbits=4)
    assert not target.exists()
    copy(source, target, driver=driver)
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


@pytest.mark.parametrize(
    "surface,options",
    [
        ("rio", {"predictor": "NO"}),
        ("rio", {"predictor": "NONE"}),
        ("rio", {"predictor": None}),
        ("rio", {"predictor": False}),
        ("rio", {"dtype": np.uint16}),
        ("native", {"predictor": "NO"}),
        ("native", {"predictor": "NONE"}),
    ],
)
def test_copy_normalizes_options_before_inheriting_width(tmp_path, gdal, options, surface):
    import terrio

    source, target = tmp_path / "source.tif", tmp_path / "copy.tif"
    values = np.arange(16, dtype="uint16").reshape(4, 4)
    write_sample_width_images(source, [(values, 12)])
    if surface == "rio":
        copy(source, target, **options)
    else:
        terrio.open_raster(str(source)).write(str(target), options=options)
    result = gdal.Open(str(target))
    assert result.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "12"
    np.testing.assert_array_equal(result.ReadAsArray(), values)


def test_copy_accepts_compression_enum_without_losing_width(tmp_path, gdal):
    from terrio.rio.enums import Compression

    source, target = tmp_path / "source.tif", tmp_path / "copy.tif"
    values = np.arange(16, dtype="uint8").reshape(4, 4)
    write_sample_width_images(source, [(values, 4)])
    copy(source, target, compress=Compression.lzw)
    result = gdal.Open(str(target))
    assert result.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "4"
    np.testing.assert_array_equal(result.ReadAsArray(), values)


@pytest.mark.parametrize("surface", ["rio", "native"])
def test_implicit_width_does_not_make_stored_nodata_unrepresentable(tmp_path, gdal, surface):
    import terrio

    source, target = tmp_path / "source.tif", tmp_path / "copy.tif"
    original = gdal.GetDriverByName("GTiff").Create(
        str(source), 4, 4, 1, gdal.GDT_Byte, options=["NBITS=4"]
    )
    values = np.arange(16, dtype="uint8").reshape(4, 4)
    original.GetRasterBand(1).SetNoDataValue(255)
    original.WriteArray(values)
    original = None
    if surface == "rio":
        copy(source, target)
    else:
        terrio.open_raster(str(source)).write(str(target))
    result = gdal.Open(str(target))
    assert result.GetRasterBand(1).GetNoDataValue() == 255
    assert result.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") is None
    np.testing.assert_array_equal(result.ReadAsArray(), values)


def test_sample_width_rejects_an_invalid_overview_index(tmp_path):
    import terrio

    source = tmp_path / "source.tif"
    write_sample_width_images(source, [(np.zeros((4, 4), dtype="uint8"), 4)])
    reader = terrio.open_raster(str(source), lazy=True)
    assert reader._sample_bits() == 4
    with pytest.raises(terrio.InvalidArgumentError, match="no overview 0"):
        reader._sample_bits(0)


@pytest.mark.parametrize("external", [False, True])
def test_selected_overview_uses_its_own_photometric_tags(tmp_path, gdal, monkeypatch, external):
    from terrio.rio import _tiff

    tag_reads = []
    read_tags = _tiff._terrio.tiff_tags

    def tracked_tags(path):
        tag_reads.append(path)
        return read_tags(path)

    monkeypatch.setattr(_tiff._terrio, "tiff_tags", tracked_tags)
    source, target = tmp_path / "source.tif", tmp_path / "copy.tif"
    base = np.ones((4, 4), dtype="uint8")
    overview = np.full((2, 2), 200, dtype="uint8")
    if external:
        write_sample_width_images(source, [(base, 1)], photometric=0)
        write_sample_width_images(tmp_path / "source.tif.ovr", [(overview, 8)])
    else:
        write_sample_width_images(source, [(base, 1), (overview, 8)], photometric=0)
        raw = bytearray(source.read_bytes())
        # Change only the second image's PhotometricInterpretation to MINISBLACK.
        first = struct.unpack_from("<I", raw, 4)[0]
        count = struct.unpack_from("<H", raw, first)[0]
        second = struct.unpack_from("<I", raw, first + 2 + count * 12)[0]
        count = struct.unpack_from("<H", raw, second)[0]
        for i in range(count):
            offset = second + 2 + i * 12
            if struct.unpack_from("<H", raw, offset)[0] == 262:
                struct.pack_into("<H", raw, offset + 8, 1)
        source.write_bytes(raw)
    reference = gdal.Open(str(source))
    np.testing.assert_array_equal(reference.GetRasterBand(1).GetOverview(0).ReadAsArray(), overview)
    with rio.open(source, overview_level=0) as reader:
        assert reader.tags(ns="IMAGE_STRUCTURE").get("MINISWHITE") is None
        with pytest.raises(ValueError):
            reader.colormap(1)
        np.testing.assert_array_equal(reader.read(1), overview)
        assert tag_reads == [str(source)] + ([str(source) + ".ovr"] if external else [])
        copy(reader, target)
    result = gdal.Open(str(target))
    assert result.GetRasterBand(1).GetRasterColorTable() is None
    np.testing.assert_array_equal(result.ReadAsArray(), overview)


@pytest.mark.parametrize("bits", [1, 4])
@pytest.mark.parametrize("surface", ["streamed-cog", "whole", "streamed-gtiff"])
def test_streamed_cog_reports_wider_clipping_but_keeps_bilevel_silent(
    tmp_path, gdal, monkeypatch, bits, surface
):
    """GDAL 3.13.3 silently clips bilevel values; wider domains report clipping."""
    import warnings

    if surface == "streamed-gtiff":
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1")
    path = tmp_path / f"clipped-{bits}.tif"
    with warnings.catch_warnings(record=True) as recorded:
        warnings.simplefilter("always")
        with rio.open(
            path,
            "w",
            driver="COG" if surface == "streamed-cog" else "GTiff",
            width=32,
            height=32,
            count=1,
            dtype="uint8",
            nbits=bits,
            blocksize=32,
        ) as writer:
            writer.write(np.full((1, 32, 32), 255, dtype="uint8"))
    clipping = [str(w.message) for w in recorded if "clipped to fit" in str(w.message)]
    assert len(clipping) == (1 if bits > 1 else 0)
    result = gdal.Open(str(path))
    np.testing.assert_array_equal(result.ReadAsArray(), np.full((32, 32), (1 << bits) - 1))


def test_copy_uses_full_width_for_a_requested_outside_domain_nodata(tmp_path, gdal):
    source, target = tmp_path / "source.tif", tmp_path / "copy.tif"
    values = np.arange(16, dtype="uint8").reshape(4, 4)
    write_sample_width_images(source, [(values, 4)])
    copy(source, target, nodata=255)
    result = gdal.Open(str(target))
    assert result.GetRasterBand(1).GetNoDataValue() == 255
    assert result.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") is None
    np.testing.assert_array_equal(result.ReadAsArray(), values)
    with pytest.raises(Exception, match="nodata.*NBITS=4"):
        copy(source, tmp_path / "explicit.tif", nodata=255, nbits=4)


@pytest.mark.parametrize("external", [False, True])
def test_cog_copy_of_an_overview_keeps_its_wider_descendant_levels(tmp_path, gdal, external):
    """GDAL 3.13.3 overview views expose only their selected pixels and later levels."""
    source, target = tmp_path / "levels.tif", tmp_path / "copied.tif"
    base = np.arange(16 * 16, dtype="uint16").reshape(16, 16)
    selected = np.full((8, 8), 300, dtype="uint16")
    descendant = np.full((4, 4), 2000, dtype="uint16")
    if external:
        write_sample_width_images(source, [(base, 9)])
        write_sample_width_images(tmp_path / "levels.tif.ovr", [(selected, 9), (descendant, 12)])
    else:
        write_sample_width_images(source, [(base, 9), (selected, 9), (descendant, 12)])
    gt = (500000, 30, 0, 4100000, 0, -30)
    original = gdal.Open(str(source), gdal.GA_Update)
    original.SetGeoTransform(gt)
    original.SetProjection("EPSG:32633")
    original = None
    reference = gdal.OpenEx(str(source), open_options=["OVERVIEW_LEVEL=0"])
    np.testing.assert_array_equal(reference.ReadAsArray(), selected)
    assert reference.GetRasterBand(1).GetOverviewCount() == 1
    np.testing.assert_array_equal(
        reference.GetRasterBand(1).GetOverview(0).ReadAsArray(), descendant
    )
    with rio.open(source, overview_level=0) as reader:
        copy(reader, target, driver="COG")
    result = gdal.Open(str(target))
    assert result.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "12"
    np.testing.assert_array_equal(result.ReadAsArray(), selected)
    assert result.GetRasterBand(1).GetOverviewCount() == 1
    np.testing.assert_array_equal(result.GetRasterBand(1).GetOverview(0).ReadAsArray(), descendant)
    assert (
        result.GetGeoTransform() == reference.GetGeoTransform() == (500000, 60, 0, 4100000, 0, -60)
    )
    assert result.GetSpatialRef().IsSame(reference.GetSpatialRef())
    assert result.GetRasterBand(1).GetNoDataValue() is None
    assert np.all(result.GetRasterBand(1).GetMaskBand().ReadAsArray() == 255)


@pytest.mark.parametrize("surface", ["rio", "native"])
def test_uncompressed_packed_copy_ignores_an_ineffective_predictor(tmp_path, gdal, surface):
    import terrio

    source, target = tmp_path / "source.tif", tmp_path / "copy.tif"
    values = np.arange(16, dtype="uint8").reshape(4, 4)
    write_sample_width_images(source, [(values, 4)])
    if surface == "rio":
        copy(source, target, compress="NONE", predictor=2)
    else:
        terrio.open_raster(str(source)).write(
            str(target), compress="NONE", options={"PREDICTOR": "2"}
        )
    result = gdal.Open(str(target))
    assert result.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "4"
    assert result.GetMetadataItem("PREDICTOR", "IMAGE_STRUCTURE") is None
    np.testing.assert_array_equal(result.ReadAsArray(), values)
    explicit = tmp_path / "explicit.tif"
    with rio.open(
        explicit,
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="uint8",
        nbits=4,
        compress="NONE",
        predictor=2,
    ) as writer:
        writer.write(values, 1)
    np.testing.assert_array_equal(gdal.Open(str(explicit)).ReadAsArray(), values)


@pytest.mark.parametrize("surface", ["rio", "native"])
def test_default_cog_compression_does_not_inherit_an_incompatible_width(tmp_path, gdal, surface):
    import terrio

    source, target = tmp_path / "source.tif", tmp_path / "copy.tif"
    values = np.arange(16, dtype="uint8").reshape(4, 4)
    write_sample_width_images(source, [(values, 4)])
    if surface == "rio":
        copy(source, target, driver="COG", predictor=2)
    else:
        terrio.open_raster(str(source)).write(str(target), driver="COG", options={"PREDICTOR": "2"})
    result = gdal.Open(str(target))
    assert result.GetMetadataItem("COMPRESSION", "IMAGE_STRUCTURE") == "LZW"
    assert result.GetMetadataItem("PREDICTOR", "IMAGE_STRUCTURE") == "2"
    assert result.GetRasterBand(1).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") is None
    np.testing.assert_array_equal(result.ReadAsArray(), values)


def test_cog_copy_of_an_overview_keeps_its_own_and_descendant_masks(tmp_path, gdal):
    source, target = tmp_path / "masked.tif", tmp_path / "copy.tif"
    values = np.arange(32 * 32, dtype="uint16").reshape(32, 32) % 512
    mask = np.where(np.indices((32, 32)).sum(axis=0) % 3, 255, 0).astype("uint8")
    previous = gdal.GetConfigOption("GDAL_TIFF_INTERNAL_MASK")
    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "YES")
    try:
        dataset = gdal.GetDriverByName("GTiff").Create(
            str(source), 32, 32, 1, gdal.GDT_UInt16, options=["NBITS=9"]
        )
        dataset.SetGeoTransform((500000, 10, 0, 4100000, 0, -10))
        dataset.WriteArray(values)
        dataset.CreateMaskBand(gdal.GMF_PER_DATASET)
        dataset.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        dataset.BuildOverviews("NEAREST", [2, 4])
        dataset = None
    finally:
        gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", previous)
    reference = gdal.OpenEx(str(source), open_options=["OVERVIEW_LEVEL=0"])
    with rio.open(source, overview_level=0) as reader:
        copy(reader, target, driver="COG")
    result = gdal.Open(str(target))
    expected = reference.GetRasterBand(1)
    actual = result.GetRasterBand(1)
    assert (result.RasterXSize, result.RasterYSize, actual.GetOverviewCount()) == (16, 16, 1)
    assert actual.GetMetadataItem("NBITS", "IMAGE_STRUCTURE") == "9"
    assert result.GetGeoTransform() == reference.GetGeoTransform()
    for left, right in [(actual, expected), (actual.GetOverview(0), expected.GetOverview(0))]:
        np.testing.assert_array_equal(left.ReadAsArray(), right.ReadAsArray())
        np.testing.assert_array_equal(
            left.GetMaskBand().ReadAsArray(), right.GetMaskBand().ReadAsArray()
        )


@pytest.mark.parametrize("failure", [PermissionError, ValueError])
def test_selected_overview_tag_errors_are_not_hidden(tmp_path, monkeypatch, failure):
    from terrio.rio import _tiff

    source = tmp_path / "source.tif"
    write_sample_width_images(source, [(np.ones((4, 4), dtype="uint8"), 1)], photometric=0)
    write_sample_width_images(
        tmp_path / "source.tif.ovr", [(np.full((2, 2), 200, dtype="uint8"), 8)]
    )
    with rio.open(source, overview_level=0) as reader:
        read_tags = _tiff._terrio.tiff_tags

        def failing_tags(path):
            if path.endswith(".ovr"):
                raise failure("overview tags unavailable")
            return read_tags(path)

        monkeypatch.setattr(_tiff._terrio, "tiff_tags", failing_tags)
        with pytest.raises(failure, match="overview tags unavailable"):
            _tiff.tiff_structure(str(source), reader._raster)
