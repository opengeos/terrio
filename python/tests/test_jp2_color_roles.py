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
    cod = codestream.index(b"\xff\x52")
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
