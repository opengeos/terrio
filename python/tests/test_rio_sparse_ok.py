"""`sparse_ok=True`: a GeoTIFF leaves out its blocks whose pixels are all nodata (or 0).

Such a block has an offset and a byte count of 0 (`BLOCK_OFFSET_x_y` and `BLOCK_SIZE_x_y` in the
TIFF metadata domain), and terrio reads its pixels as nodata (or 0). A COG writes every tile, and
warns that it does.
"""

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.windows import Window


@pytest.mark.parametrize("limit", ["1000000000", "1000"], ids=["in-memory", "in-a-file"])
@pytest.mark.parametrize("nodata", [None, 7], ids=["without-nodata", "nodata"])
@pytest.mark.parametrize(
    "layout",
    [dict(tiled=True, blockxsize=64, blockysize=32), dict(blockysize=8)],
    ids=["tiles", "strips"],
)
def test_blocks_never_written_are_left_out(tmp_path, monkeypatch, layout, nodata, limit):
    """Below and above the in-memory limit (written whole, or in rows); the file reads back
    whole and by window, and with `terrio.open_raster(lazy=True)`."""
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
