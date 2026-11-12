"""JP2 channel roles describe stored components without changing their pixel order."""

import struct

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.enums import ColorInterp
from terrio.rio.errors import RasterioIOError
from terrio.rio.io import MemoryFile

ROLES = [
    ("Undefined",) * 3,
    ("Undefined",) * 4,
    ("Red", "Green", "Blue"),
    ("Blue", "Green", "Red"),
    ("Red", "Green", "Blue", "Alpha"),
    ("Alpha", "Red", "Green", "Blue"),
    ("Gray", "Alpha"),
]
GEOJP2_UUID = bytes.fromhex("b14bf8bd083d4b43a5ae8cd7d5a6ce03")


def _source(roles, dtype):
    y, x = np.mgrid[:48, :64]
    scale = 1 if dtype == "uint8" else 113
    return np.stack(
        [(11 + i * 31 + (x * 7 + y * 13) % 61) * scale for i in range(len(roles))]
    ).astype(dtype)


def _write(tmp_path, roles, dtype, memory, driver="JP2"):
    values = _source(roles, dtype)
    options = dict(driver=driver, width=64, height=48, count=len(roles), dtype=dtype)
    path = tmp_path / "roles.jp2"
    colors = [ColorInterp[name.lower()] for name in roles]
    if memory:
        with MemoryFile() as mem:
            with mem.open(**options) as dst:
                dst.colorinterp = colors
                dst.write(values)
            raw = mem.read()
        path.write_bytes(raw)
    else:
        with rio.open(path, "w", **options) as dst:
            dst.colorinterp = colors
            dst.write(values)
        raw = path.read_bytes()
    return values, path, raw


def _boxes(raw):
    offset = 0
    while offset < len(raw):
        length, kind = struct.unpack_from(">I4s", raw, offset)
        assert length >= 8 and offset + length <= len(raw)
        yield kind, raw[offset + 8 : offset + length]
        offset += length


@pytest.mark.parametrize("roles", ROLES, ids=lambda r: "-".join(r))
@pytest.mark.parametrize("dtype", ["uint8", "uint16"])
@pytest.mark.parametrize("memory", [False, True], ids=["path", "memory"])
def test_explicit_jp2_roles_preserve_values_and_unreferenced_storage(
    tmp_path, roles, dtype, memory
):
    values, path, raw = _write(tmp_path, roles, dtype, memory)
    for kind, payload in _boxes(raw):
        assert not (kind == b"uuid" and payload.startswith(GEOJP2_UUID))
    codestream = next(payload for kind, payload in _boxes(raw) if kind == b"jp2c")
    assert codestream[:2] == b"\xff\x4f"  # SOC
    cod = 2
    while codestream[cod : cod + 2] != b"\xff\x52":
        assert codestream[cod] == 0xFF
        length = struct.unpack_from(">H", codestream, cod + 2)[0]
        assert length >= 2 and cod + length + 2 <= len(codestream)
        cod += length + 2
    assert struct.unpack_from(">H", codestream, cod + 2)[0] >= 7
    assert codestream[cod + 8] == int(roles[:3] == ("Red", "Green", "Blue"))
    header = next(payload for kind, payload in _boxes(raw) if kind == b"jp2h")
    definitions = next(payload for kind, payload in _boxes(header) if kind == b"cdef")
    if not any(role in roles for role in ("Red", "Green", "Blue")):
        records = [struct.unpack_from(">HHH", definitions, 2 + 6 * i) for i in range(len(roles))]
        assert sum(kind == 0 and association == 1 for _, kind, association in records) == 1
    expected = tuple(ColorInterp[name.lower()] for name in roles)
    with rio.open(path) as src:
        assert src.dtypes == (dtype,) * len(roles)
        assert src.colorinterp == expected
        np.testing.assert_array_equal(src.read(), values)
        np.testing.assert_array_equal(
            src.read([len(roles), 1], window=((9, 22), (7, 18))),
            values[[len(roles) - 1, 0], 9:22, 7:18],
        )
    with MemoryFile(raw) as mem, mem.open() as src:
        assert src.colorinterp == expected
        np.testing.assert_array_equal(src.read(), values)
    assert not terrio.open_raster(str(path), lazy=True).has_transform


@pytest.mark.parametrize("roles", ROLES, ids=lambda r: "-".join(r))
@pytest.mark.parametrize("dtype", ["uint8", "uint16"])
@pytest.mark.parametrize("memory", [False, True], ids=["path", "memory"])
def test_gdal_reads_explicit_jp2_roles_and_exact_components(tmp_path, gdal, roles, dtype, memory):
    values, path, _ = _write(tmp_path, roles, dtype, memory)
    src = gdal.Open(str(path))
    actual = [
        gdal.GetColorInterpretationName(src.GetRasterBand(i + 1).GetColorInterpretation())
        for i in range(len(roles))
    ]
    assert actual == list(roles)
    np.testing.assert_array_equal(src.ReadAsArray(), values)
    assert (
        gdal.GetDataTypeName(src.GetRasterBand(1).DataType)
        == {"uint8": "Byte", "uint16": "UInt16"}[dtype]
    )
    expected_masks = np.stack(
        [src.GetRasterBand(i + 1).GetMaskBand().ReadAsArray() for i in range(len(roles))]
    )
    with rio.open(path) as native:
        np.testing.assert_array_equal(native.read_masks(), expected_masks)
    assert src.GetGeoTransform(can_return_null=True) is None


@pytest.mark.parametrize("roles", [("Blue", "Green", "Red"), ("Alpha", "Red", "Green", "Blue")])
def test_gdal_authored_channel_definitions_keep_stored_pixel_order(tmp_path, gdal, roles):
    values = _source(roles, "uint16")
    source = gdal.GetDriverByName("MEM").Create("", 64, 48, len(roles), gdal.GDT_UInt16)
    for i, name in enumerate(roles):
        band = source.GetRasterBand(i + 1)
        band.WriteArray(values[i])
        band.SetColorInterpretation(int(ColorInterp[name.lower()]))
    path = tmp_path / "gdal.jp2"
    output = gdal.Translate(
        str(path), source, format="JP2OpenJPEG", creationOptions=["REVERSIBLE=YES", "QUALITY=100"]
    )
    output.FlushCache()
    output = source = None
    with rio.open(path) as src:
        assert src.colorinterp == tuple(ColorInterp[name.lower()] for name in roles)
        np.testing.assert_array_equal(src.read(), values)


@pytest.mark.parametrize("driver", [None, "JP2OpenJPEG", "jp2"])
def test_inferred_and_alias_writers_keep_explicit_undefined_roles(tmp_path, driver):
    roles = ("Undefined",) * 3
    values, path, _ = _write(tmp_path, roles, "uint8", False, driver=driver)
    with rio.open(path) as src:
        assert src.colorinterp == (ColorInterp.undefined,) * 3
        np.testing.assert_array_equal(src.read(), values)


@pytest.mark.parametrize("roles", ROLES, ids=lambda r: "-".join(r))
def test_native_byte_export_retains_roles_and_absent_transform(tmp_path, gdal, roles):
    values, path, _ = _write(tmp_path, roles, "uint16", False)
    raw = terrio.open_raster(str(path), lazy=True).to_bytes("copy.jp2")
    for kind, payload in _boxes(raw):
        assert not (kind == b"uuid" and payload.startswith(GEOJP2_UUID))
    copy = tmp_path / "copy.jp2"
    copy.write_bytes(raw)
    with rio.open(copy) as src:
        assert src.colorinterp == tuple(ColorInterp[name.lower()] for name in roles)
        np.testing.assert_array_equal(src.read(), values)
    reference = gdal.Open(str(copy))
    assert reference.GetGeoTransform(can_return_null=True) is None


def test_partial_rgb_roles_are_rejected_without_an_incomplete_colour_space(tmp_path):
    with pytest.raises(RasterioIOError, match="requires red, green and blue"):
        _write(tmp_path, ("Red", "Undefined", "Undefined"), "uint8", False)


@pytest.mark.parametrize(
    "roles", [("Red",), ("Green",), ("Blue",), ("Red", "Green"), ("Red", "Alpha")]
)
def test_small_rgb_subsets_write_as_grayscale(tmp_path, gdal, roles):
    values, path, _ = _write(tmp_path, roles, "uint8", False)
    expected = tuple(ColorInterp.alpha if role == "Alpha" else ColorInterp.gray for role in roles)
    with rio.open(path) as src:
        assert src.colorinterp == expected
        np.testing.assert_array_equal(src.read(), values)
    reference = gdal.Open(str(path))
    assert [
        reference.GetRasterBand(i + 1).GetColorInterpretation() for i in range(len(roles))
    ] == list(expected)


def test_duplicate_rgb_roles_are_rejected(tmp_path):
    with pytest.raises(RasterioIOError, match="roles exactly once"):
        _write(tmp_path, ("Red", "Red", "Green", "Blue"), "uint8", False)


def test_gdal_mixed_precision_alpha_keeps_stored_band_order(tmp_path, gdal):
    # GDAL 3.13.3 only writes a 1-bit alpha when it is the last stored component.
    roles = ("Blue", "Green", "Red", "Alpha")
    values = _source(roles, "uint8")
    alpha = roles.index("Alpha")
    values[alpha] = np.where(np.indices((48, 64)).sum(axis=0) % 3 == 0, 0, 255)
    source = gdal.GetDriverByName("MEM").Create("", 64, 48, len(roles), gdal.GDT_Byte)
    for i, role in enumerate(roles):
        band = source.GetRasterBand(i + 1)
        band.WriteArray(values[i])
        band.SetColorInterpretation(int(ColorInterp[role.lower()]))
    path = tmp_path / "mixed-alpha.jp2"
    output = gdal.Translate(
        str(path),
        source,
        format="JP2OpenJPEG",
        creationOptions=["REVERSIBLE=YES", "QUALITY=100", "1BIT_ALPHA=YES"],
    )
    output.FlushCache()
    output = source = None
    with rio.open(path) as native:
        assert native.colorinterp == tuple(ColorInterp[name.lower()] for name in roles)
        np.testing.assert_array_equal(native.read(), values)
        np.testing.assert_array_equal(native.read(window=((9, 22), (7, 18))), values[:, 9:22, 7:18])
    reference = gdal.Open(str(path))
    header = next(payload for kind, payload in _boxes(path.read_bytes()) if kind == b"jp2h")
    depths = next(payload for kind, payload in _boxes(header) if kind == b"bpcc")
    assert [(depth & 0x7F) + 1 for depth in depths] == [1 if i == alpha else 8 for i in range(4)]
    np.testing.assert_array_equal(reference.ReadAsArray(), values)
    with rio.open(path) as native:
        expected_masks = np.stack(
            [reference.GetRasterBand(i + 1).GetMaskBand().ReadAsArray() for i in range(len(roles))]
        )
        np.testing.assert_array_equal(native.read_masks(), expected_masks)


@pytest.mark.parametrize("roles", [("Alpha",), ("Alpha", "Alpha")])
def test_all_alpha_jp2_requires_a_colour_channel(tmp_path, roles):
    with pytest.raises(RasterioIOError, match="requires a non-alpha channel"):
        _write(tmp_path, roles, "uint8", False)


def test_multiple_gray_components_normalize_to_one_y_channel(tmp_path, gdal):
    roles = ("Gray",) * 3
    values, path, raw = _write(tmp_path, roles, "uint8", False)
    header = next(payload for kind, payload in _boxes(raw) if kind == b"jp2h")
    definitions = next(payload for kind, payload in _boxes(header) if kind == b"cdef")
    records = [struct.unpack_from(">HHH", definitions, 2 + 6 * i) for i in range(3)]
    assert records == [(0, 0, 1), (1, 65535, 65535), (2, 65535, 65535)]
    with rio.open(path) as source:
        assert source.colorinterp == (ColorInterp.undefined,) * 3
        np.testing.assert_array_equal(source.read(), values)
    reference = gdal.Open(str(path))
    assert [reference.GetRasterBand(i).GetColorInterpretation() for i in range(1, 4)] == [
        gdal.GCI_Undefined
    ] * 3
    np.testing.assert_array_equal(reference.ReadAsArray(), values)


@pytest.mark.parametrize("memory", [False, True])
def test_inherited_geotiff_roles_are_not_replaced_with_rgb(tmp_path, gdal, memory):
    values = _source(("Gray", "Undefined", "Undefined"), "uint8")
    source = tmp_path / "source.tif"
    reference = gdal.GetDriverByName("GTiff").Create(
        str(source), 64, 48, 3, gdal.GDT_Byte, options=["PHOTOMETRIC=MINISBLACK"]
    )
    for band in range(3):
        reference.GetRasterBand(band + 1).WriteArray(values[band])
    reference = None
    raster = terrio.open_raster(str(source), lazy=True)
    path = tmp_path / "inherited.jp2"
    if memory:
        path.write_bytes(raster.to_bytes("inherited.jp2"))
    else:
        raster.write(str(path))
    with rio.open(path) as result:
        assert result.colorinterp == (ColorInterp.undefined,) * 3
        np.testing.assert_array_equal(result.read(), values)
    reference = gdal.Open(str(path))
    assert [reference.GetRasterBand(i).GetColorInterpretation() for i in range(1, 4)] == [
        gdal.GCI_Undefined
    ] * 3
    np.testing.assert_array_equal(reference.ReadAsArray(), values)


@pytest.mark.parametrize(
    "roles", [("Gray", "Alpha", "Alpha"), ("Red", "Green", "Blue", "Alpha", "Alpha")]
)
def test_duplicate_alpha_roles_are_rejected(tmp_path, roles):
    with pytest.raises(RasterioIOError, match="at most one alpha"):
        _write(tmp_path, roles, "uint8", False)


@pytest.mark.parametrize("dtype", ["int8", "int16"])
def test_signed_alpha_roles_are_rejected(tmp_path, dtype):
    with pytest.raises(RasterioIOError, match="unsigned"):
        _write(tmp_path, ("Gray", "Alpha"), dtype, False)


@pytest.mark.parametrize(
    "surface", ["update", "path", "memory", "rio-copy", "rio-copy-alias", "update-roles"]
)
@pytest.mark.parametrize("y_band", [0, 1])
def test_gray_update_preserves_original_y_association(tmp_path, gdal, y_band, surface):
    values, path, raw = _write(tmp_path, ("Gray", "Gray"), "uint8", False)
    raw = bytearray(raw)
    offset = raw.index(b"cdef") + 6
    for band in range(2):
        struct.pack_into(
            ">HHH",
            raw,
            offset + 6 * band,
            band,
            0 if band == y_band else 65535,
            1 if band == y_band else 65535,
        )
    path.write_bytes(raw)
    reference = gdal.Open(str(path))
    expected = reference.ReadAsArray()
    reference = None
    if surface in ("update", "update-roles"):
        values[:, 3, 7] += 1
        expected[:, 3, 7] += 1
        with rio.open(path, "r+") as dst:
            assert dst.colorinterp == (ColorInterp.gray, ColorInterp.gray)
            if surface == "update-roles":
                dst.colorinterp = (ColorInterp.gray, ColorInterp.gray)
            dst.write(values)
    elif surface.startswith("rio-copy"):
        from terrio.rio.shutil import copy

        source = path
        path = tmp_path / "copied.jp2"
        copy(source, path, driver="JP2OpenJPEG" if surface.endswith("alias") else "JP2")
    else:
        raster = terrio.open_raster(str(path), lazy=True)
        assert raster.band_metadata[y_band]["color_association"] == 1
        path = tmp_path / "copied.jp2"
        if surface == "path":
            raster.write(str(path))
        else:
            path.write_bytes(raster.to_bytes(path.name))
    header = next(payload for kind, payload in _boxes(path.read_bytes()) if kind == b"jp2h")
    definitions = next(payload for kind, payload in _boxes(header) if kind == b"cdef")
    expected_y = 0 if surface == "update-roles" else y_band
    assert [struct.unpack_from(">HHH", definitions, 2 + 6 * band) for band in range(2)] == [
        (band, 0, 1) if band == expected_y else (band, 65535, 65535) for band in range(2)
    ]
    with rio.open(path) as result:
        assert result.colorinterp == (ColorInterp.gray, ColorInterp.gray)
        np.testing.assert_array_equal(result.read(), values)
    reference = gdal.Open(str(path))
    np.testing.assert_array_equal(reference.ReadAsArray(), expected)
