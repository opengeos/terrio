"""A TIFF directory libtiff cannot read is refused at open by `terrio.rio`, as GDAL refuses it.

libtiff's `TIFFReadDirectory` reads the structural tags (ImageWidth, ImageLength, tile and strip
sizes, SamplesPerPixel, PlanarConfiguration, ExtraSamples) without recovery, and fails the
directory, so GDAL fails to open the file, when their type is not an integer type or a
single-valued one holds more than one value. terrio accepted such a file (raster fuzz case 268:
ImageWidth retyped UNDEFINED, SamplesPerPixel 32,767) and spent 60 to 75 s of CPU in the first read
before a catchable error. Skipped without rasterio.
"""

import struct
import time

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.errors import RasterioIOError as GdalIOError  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.errors import RasterioIOError  # noqa: E402


def _gdal_tiff(path):
    """The fuzz case's seed: a 192 x 192 Byte image in 64 x 64 Deflate tiles, as GDAL writes it."""
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=192,
        height=192,
        count=1,
        dtype="uint8",
        tiled=True,
        blockxsize=64,
        blockysize=64,
        compress="deflate",
        crs="EPSG:4326",
        transform=from_origin(0, 192, 1, 1),
    ) as dst:
        dst.write(np.arange(192 * 192, dtype=np.uint32).reshape(1, 192, 192).astype("uint8"))


def _patch(path, changes):
    """Rewrites first-IFD entries of a little-endian classic TIFF: `{tag: (type, count, value)}`,
    `None` keeping a field."""
    b = bytearray(open(path, "rb").read())
    assert b[:4] == b"II*\0"
    off = struct.unpack_from("<I", b, 4)[0]
    for i in range(struct.unpack_from("<H", b, off)[0]):
        at = off + 2 + 12 * i
        tag, typ, count = struct.unpack_from("<HHI", b, at)
        if tag in changes:
            new_type, new_count, value = changes[tag]
            struct.pack_into(
                "<HHI",
                b,
                at,
                tag,
                typ if new_type is None else new_type,
                count if new_count is None else new_count,
            )
            if value is not None:
                struct.pack_into("<H", b, at + 8, value)
    open(path, "wb").write(bytes(b))


@pytest.mark.parametrize(
    "changes",
    [
        {256: (7, None, None), 277: (None, None, 32767)},  # fuzz case 268, as found
        {256: (7, None, None)},  # ImageWidth UNDEFINED
        {257: (2, None, None)},  # ImageLength ASCII
        {322: (11, None, None)},  # TileWidth FLOAT
        {277: (None, 2, None)},  # SamplesPerPixel with two values
    ],
    ids=["case268", "width-undefined", "length-ascii", "tilewidth-float", "spp-count2"],
)
def test_an_unreadable_structural_tag_is_refused_at_open(tmp_path, changes):
    path = str(tmp_path / "bad.tif")
    _gdal_tiff(path)
    _patch(path, changes)
    with pytest.raises(GdalIOError):
        rasterio.open(path)
    t0 = time.perf_counter()
    with pytest.raises(RasterioIOError):
        with trio.open(path) as ds:
            ds.read()
    assert time.perf_counter() - t0 < 1.0


def test_integer_typed_structural_tags_are_read(tmp_path):
    """The control: the seed, and its ImageWidth stored as LONG, open and read on both."""
    for changes in ({}, {256: (4, None, None)}):
        path = str(tmp_path / f"ok{len(changes)}.tif")
        _gdal_tiff(path)
        _patch(path, changes)
        with rasterio.open(path) as g, trio.open(path) as t:
            np.testing.assert_array_equal(t.read(), g.read())
