"""`sparse_ok=True`: a GeoTIFF leaves out its blocks whose pixels are all nodata (or 0).

Such a block has an offset and a byte count of 0 (`BLOCK_OFFSET_x_y` and `BLOCK_SIZE_x_y` in the
TIFF metadata domain), and terrio reads its pixels as nodata (or 0), whoever wrote the file. A COG
writes every tile, and warns that it does.
"""

import struct

import numpy as np
import pytest
from test_remote_reads import Server

import terrio
import terrio.rio as rio
from terrio.rio.windows import Window


def _sparse(path, overviews=None):
    """Writes a 300 x 200 uint16 GeoTIFF of nodata 7 but for a window of 1s, leaving out the
    blocks never written, and returns its pixels."""
    expected = np.full((1, 200, 300), 7, "uint16")
    expected[:, 5:55, 10:110] = 1
    with rio.open(
        path,
        "w",
        driver="GTiff",
        width=300,
        height=200,
        count=1,
        dtype="uint16",
        nodata=7,
        tiled=True,
        blockxsize=64,
        blockysize=64,
        compress="deflate",
        sparse_ok=True,
    ) as dst:
        dst.write(expected[:, 5:55, 10:110], window=Window(10, 5, 100, 50))
        if overviews:
            dst.build_overviews(overviews)
    return expected


def _leave_out(path, ifd):
    """Sets the offsets and byte counts of every block of the `ifd`-th image of a classic
    little-endian TIFF to 0, as a writer leaving them all out does."""
    with open(path, "r+b") as f:
        data = f.read()
        assert data[:4] == b"II*\0"
        (at,) = struct.unpack_from("<I", data, 4)
        for _ in range(ifd):
            (n,) = struct.unpack_from("<H", data, at)
            (at,) = struct.unpack_from("<I", data, at + 2 + 12 * n)
        (n,) = struct.unpack_from("<H", data, at)
        for entry in range(at + 2, at + 2 + 12 * n, 12):
            tag, kind, count = struct.unpack_from("<HHI", data, entry)
            if tag in (273, 279, 324, 325):
                size = count * {3: 2, 4: 4}[kind]
                f.seek(entry + 8 if size <= 4 else struct.unpack_from("<I", data, entry + 8)[0])
                f.write(bytes(size))


@pytest.mark.parametrize("limit", ["1000000000", "1000"], ids=["in-memory", "in-a-file"])
@pytest.mark.parametrize("nodata", [None, 7], ids=["without-nodata", "nodata"])
@pytest.mark.parametrize(
    "layout",
    [dict(tiled=True, blockxsize=64, blockysize=32), dict(blockysize=8)],
    ids=["tiles", "strips"],
)
def test_blocks_never_written_are_left_out(tmp_path, monkeypatch, layout, nodata, limit):
    """Below and above the in-memory limit (written whole, or in rows); the file reads back
    whole and by window, and with `terrio.open_raster`, eagerly or lazily."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    path = tmp_path / "out.tif"
    profile = dict(driver="GTiff", width=300, height=200, count=2, dtype="uint16", nodata=nodata)
    expected = np.full((2, 200, 300), nodata or 0, "uint16")
    expected[:, 5:55, 10:110] = 1
    with rio.open(path, "w", compress="deflate", sparse_ok=True, **profile, **layout) as dst:
        dst.write(np.ones((2, 50, 100), "uint16"), window=Window(10, 5, 100, 50))
        bh, bw = dst.block_shapes[0]
    blocks = [(x, y) for y in range(-(-200 // bh)) for x in range(-(-300 // bw))]
    with rio.open(path) as src:
        offsets = {
            (x, y): src.get_tag_item(f"BLOCK_OFFSET_{x}_{y}", "TIFF", bidx=1) for x, y in blocks
        }
        sizes = {(x, y): src.get_tag_item(f"BLOCK_SIZE_{x}_{y}", "TIFF", bidx=1) for x, y in blocks}
        np.testing.assert_array_equal(src.read(), expected)
        np.testing.assert_array_equal(
            src.read(window=Window(5, 3, 290, 190)), expected[:, 3:193, 5:295]
        )
    touched = {(x, y) for y in range(5 // bh, -(-55 // bh)) for x in range(10 // bw, -(-110 // bw))}
    assert {b for b in blocks if sizes[b] != "0"} == touched
    assert {b for b in blocks if offsets[b] != "0"} == touched
    np.testing.assert_array_equal(terrio.open_raster(str(path)).read(), expected)
    np.testing.assert_array_equal(terrio.open_raster(str(path), lazy=True).read(), expected)


@pytest.mark.parametrize("masked", [False, True], ids=["in-rows", "whole"])
def test_a_cog_warns_once_that_it_writes_every_tile(tmp_path, masked):
    """Written in rows, or whole (with a mask)."""
    with pytest.warns(UserWarning, match="SPARSE_OK is not supported for COGs") as record:
        with rio.open(
            tmp_path / "out.tif",
            "w",
            driver="COG",
            width=64,
            height=64,
            count=1,
            dtype="uint8",
            sparse_ok=True,
        ) as dst:
            dst.write(np.ones((64, 64), "uint8"), 1)
            if masked:
                dst.write_mask(True)
    assert sum("SPARSE_OK" in str(w.message) for w in record) == 1


def test_blocks_left_out_read_as_nodata_over_http(tmp_path):
    """Read by range (`terrio.rio`, `open_raster(lazy=True)`), or downloaded whole and then read
    (`open_raster`)."""
    expected = _sparse(tmp_path / "out.tif")
    terrio.clear_http_cache()
    server = Server(tmp_path)
    try:
        url = f"{server.url}/out.tif"
        with rio.open(url) as src:
            np.testing.assert_array_equal(src.read(), expected)
            np.testing.assert_array_equal(
                src.read(window=Window(5, 3, 290, 190)), expected[:, 3:193, 5:295]
            )
        np.testing.assert_array_equal(terrio.open_raster(url, lazy=True).read(), expected)
        np.testing.assert_array_equal(terrio.open_raster(url).read(), expected)
    finally:
        server.close()
        terrio.clear_http_cache()


@pytest.mark.parametrize("limit", ["1000000000", "1000"], ids=["in-memory", "in-a-file"])
def test_an_update_reads_the_blocks_left_out_as_nodata(tmp_path, monkeypatch, limit):
    """An `r+` update, its pixels held in memory or in a file, which it reads whole or by slabs
    of rows."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    path = tmp_path / "out.tif"
    expected = _sparse(path)
    expected[:, 150:160, 250:260] = 3
    with rio.open(path, "r+") as dst:
        dst.write(expected[:, 150:160, 250:260], window=Window(250, 150, 10, 10))
    with rio.open(path) as src:
        np.testing.assert_array_equal(src.read(), expected)


def test_blocks_left_out_of_an_overview_read_as_nodata(tmp_path):
    """Read decimated, or at the overview's level. terrio writes overviews whole: every block of
    the overview is left out here afterwards, as another writer may leave them out."""
    path = tmp_path / "out.tif"
    expected = _sparse(path, overviews=[2])
    _leave_out(path, 1)
    nodata = np.full((1, 100, 150), 7, "uint16")
    with rio.open(path) as src:
        np.testing.assert_array_equal(src.read(), expected)
        np.testing.assert_array_equal(src.read(out_shape=(1, 100, 150)), nodata)
    with rio.open(path, overview_level=0) as src:
        np.testing.assert_array_equal(src.read(), nodata)
