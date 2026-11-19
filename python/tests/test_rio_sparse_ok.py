"""`sparse_ok=True`: a GeoTIFF leaves out its blocks whose pixels are all nodata (or 0).

Such a block has an offset and a byte count of 0 (`BLOCK_OFFSET_x_y` and `BLOCK_SIZE_x_y` in the
TIFF metadata domain), and terrio reads its pixels as nodata (or 0), whoever wrote the file. A
block is left out only if it reads the same with the file's `.aux.xml` as without it. A COG
writes every tile, and warns that it does.
"""

import struct
import warnings

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


@pytest.mark.parametrize("limit", ["1000000000", "1000"], ids=["in-memory", "in-a-file"])
def test_a_complex_block_is_left_out_only_if_both_parts_are_0(tmp_path, monkeypatch, limit):
    """Catches a complex block left out on its real parts alone: one whose imaginary parts hold
    data is kept. Complex samples (`complex64`, GDAL's CFloat32) are left out when both their
    parts are 0, written whole or in rows."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    path = tmp_path / "out.tif"
    expected = np.zeros((1, 64, 192), "complex64")
    expected[0, 10, 70] = 1j
    expected[0, 20, 140] = 1
    profile = dict(driver="GTiff", width=192, height=64, count=1, dtype="complex64")
    with rio.open(
        path, "w", tiled=True, blockxsize=64, blockysize=64, sparse_ok=True, **profile
    ) as dst:
        dst.write(expected)
    with rio.open(path) as src:
        sizes = [src.get_tag_item(f"BLOCK_SIZE_{x}_0", "TIFF", bidx=1) for x in range(3)]
        np.testing.assert_array_equal(src.read(), expected)
    assert [size == "0" for size in sizes] == [True, False, False]
    np.testing.assert_array_equal(terrio.open_raster(str(path)).read(), expected)


def test_a_complex_band_nodata_fills_both_parts_of_a_block_left_out(tmp_path):
    """Catches a read by window filling the imaginary parts of a complex block left out with
    something else than the whole read does: a band's nodata from the `.aux.xml` (here added
    after writing, as another writer may) fills both parts of its samples, read by window
    (`terrio.rio`, `open_raster(lazy=True)`) as read whole (`open_raster`)."""
    path = tmp_path / "out.tif"
    values = np.zeros((1, 64, 128), "complex64")
    values[0, 10, 70] = 1 + 2j
    profile = dict(driver="GTiff", width=128, height=64, count=1, dtype="complex64")
    with rio.open(
        path, "w", tiled=True, blockxsize=64, blockysize=64, sparse_ok=True, **profile
    ) as dst:
        dst.write(values)
    _band_nodata_sidecar(path, [5])
    whole = terrio.open_raster(str(path)).read()
    assert whole[0, 0, 0] == 5 + 5j and whole[0, 10, 70] == 1 + 2j
    with rio.open(path) as src:
        np.testing.assert_array_equal(src.read(), whole)
        np.testing.assert_array_equal(src.read(window=Window(0, 0, 64, 64)), whole[:, :64, :64])
    np.testing.assert_array_equal(terrio.open_raster(str(path), lazy=True).read(), whole)


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


def _band_nodata_sidecar(path, values):
    """Writes the `.aux.xml` beside `path` that gives band `i + 1` the nodata `values[i]`."""
    bands = "".join(
        f'<PAMRasterBand band="{b}"><NoDataValue>{v}</NoDataValue></PAMRasterBand>'
        for b, v in enumerate(values, 1)
        if v is not None
    )
    with open(f"{path}.aux.xml", "w") as f:
        f.write(f"<PAMDataset>{bands}</PAMDataset>")


def _reads(path, url, monkeypatch):
    """`path` read with its `.aux.xml` (local reads), without it (sidecars disabled), and over
    HTTP (by range, or downloaded whole)."""
    reads = {}
    for sidecar in ("", "EMPTY_DIR"):
        with monkeypatch.context() as m:
            if sidecar:
                m.setenv("TERRIO_DISABLE_READDIR_ON_OPEN", sidecar)
            with rio.open(path) as src:
                reads[f"rio {sidecar}"] = src.read()
                reads[f"rio window {sidecar}"] = src.read(window=Window(5, 3, 290, 190))
            reads[f"eager {sidecar}"] = terrio.open_raster(str(path)).read()
            reads[f"lazy {sidecar}"] = terrio.open_raster(str(path), lazy=True).read()
    with rio.open(url) as src:
        reads["rio, HTTP"] = src.read()
    reads["lazy, HTTP"] = terrio.open_raster(url, lazy=True).read()
    reads["eager, HTTP"] = terrio.open_raster(url).read()
    return reads


@pytest.mark.parametrize(
    ("writer", "nodata", "sidecar", "left_out"),
    [
        ("dataset", 7, ("source", [7, 9]), False),
        ("dataset", None, ("source", [7, None]), False),
        ("rio", 7, ("output", [9, 9]), True),
        ("rio, rows", 7, ("output", [9, 9]), False),
        ("rio, rows", 7, ("output", [7, 7]), True),
        ("GeoTiffWriter", 7, ("output", [9, 9]), False),
    ],
    ids=[
        "bands-7-9",
        "bands-7-none",
        "stale-sidecar-rewritten",
        "stale-sidecar-kept-in-rows",
        "sidecar-kept-in-rows-agrees",
        "stale-sidecar-kept-by-GeoTiffWriter",
    ],
)
def test_a_block_is_left_out_only_if_it_reads_the_same_with_the_sidecar(
    tmp_path, monkeypatch, writer, nodata, sidecar, left_out
):
    """Catches a block left out that reads otherwise than the block written, on any read path.

    A band's nodata from the `.aux.xml` fills its blocks left out on local reads; reads without
    the sidecar fill the file's nodata tag (or 0). So `SPARSE_OK` writes every block, with a
    warning, when the two differ for a band: bands of different nodata written by
    `write_raster_dataset` (band 1 or band 2 of a left-out block reads 7, 9 or 0 for 0 or 7),
    or a sidecar left beside the path by an earlier file, which writers in rows keep (the whole
    writer rewrites its nodata). Blocks are still left out where the two agree."""
    # Block (0, 0) holds 7 in both bands, block (1, 0) holds 0, the others 1.
    values = np.ones((2, 200, 300), "uint16")
    values[:, :64, :64] = 7
    values[:, :64, 64:128] = 0
    profile = dict(driver="GTiff", width=300, height=200, count=2, dtype="uint16", nodata=nodata)
    blocks = dict(tiled=True, blockxsize=64, blockysize=64)
    co = {"TILED": "YES", "BLOCKXSIZE": "64", "BLOCKYSIZE": "64"}
    where, band_nodata = sidecar
    if where == "source":
        with rio.open(tmp_path / "src.tif", "w", **profile, **blocks) as dst:
            dst.write(values)
        _band_nodata_sidecar(tmp_path / "src.tif", band_nodata)
    for sparse in (False, True):
        path = tmp_path / ("sparse.tif" if sparse else "dense.tif")
        if where == "output":
            _band_nodata_sidecar(path, band_nodata)
        with warnings.catch_warnings(record=True) as record:
            warnings.simplefilter("always")
            if writer == "dataset":
                source = terrio.open_raster(str(tmp_path / "src.tif"))
                options = {**co, "SPARSE_OK": "YES"} if sparse else co
                source.write(str(path), driver="GTiff", options=options)
            elif writer == "GeoTiffWriter":
                w = terrio.GeoTiffWriter(
                    str(path),
                    300,
                    200,
                    2,
                    "uint16",
                    (0.0, 1.0, 0.0, 0.0, 0.0, -1.0),
                    nodata=nodata,
                    options={**co, "SPARSE_OK": "YES"} if sparse else co,
                )
                w.write(values)
                w.close()
            else:
                limit = "1000" if writer == "rio, rows" else "1000000000"
                monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
                with rio.open(path, "w", sparse_ok=sparse, **profile, **blocks) as dst:
                    dst.write(values)
        ignored = [w for w in record if "SPARSE_OK is ignored" in str(w.message)]
        assert len(ignored) == (1 if sparse and not left_out else 0)
    with rio.open(tmp_path / "sparse.tif") as src:
        sizes = {
            (x, y): src.get_tag_item(f"BLOCK_SIZE_{x}_{y}", "TIFF", bidx=1)
            for y in range(4)
            for x in range(5)
        }
    candidate = (0, 0) if nodata == 7 else (1, 0)
    assert {b for b, size in sizes.items() if size == "0"} == ({candidate} if left_out else set())
    terrio.clear_http_cache()
    server = Server(tmp_path)
    try:
        dense = _reads(tmp_path / "dense.tif", f"{server.url}/dense.tif", monkeypatch)
        sparse = _reads(tmp_path / "sparse.tif", f"{server.url}/sparse.tif", monkeypatch)
    finally:
        server.close()
        terrio.clear_http_cache()
    for read in dense:
        np.testing.assert_array_equal(sparse[read], dense[read], err_msg=read)
