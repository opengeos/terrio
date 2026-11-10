"""`terrio.rio.shutil.copy` writes a COG from its source as it writes it (rasterio not needed).

A copy wrote its source's pixels into the dataset it created, which held them until it was closed
and its file written: the whole image, besides what the COG's writer holds. When the file is
written in rows (a COG without a dataset mask, overviews of the source to copy, or a compression
written whole; a GeoTIFF larger than the in-memory limit), the copy now reads the source as it
writes, a row of a COG's tiles at a time. These tests check that the file is the one the copy
holding the pixels writes, byte for byte, that the copy reads the source a row of tiles at a time
and allocates no array of the whole image, and that what is not written in rows is copied as
before.
"""

import filecmp
import os
import re
import struct
import sys
import tracemalloc

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio import shutil as rio_shutil
from terrio.rio.enums import Resampling
from terrio.rio.io import DatasetReader, MemoryFile
from terrio.rio.transform import from_origin

T = from_origin(500000, 4100000, 30, 30)
# 3,000 rows: five rows of a COG's 512 x 512 tiles and a part of a sixth.
HEIGHT, WIDTH = 3000, 1024
RGB = (
    np.add.outer(np.arange(HEIGHT) * 7, np.arange(WIDTH) * 3)[None]
    + np.arange(3)[:, None, None] * 50
) % 251
RGB = RGB.astype("uint8")
PROFILE = dict(
    driver="GTiff", width=WIDTH, height=HEIGHT, crs="EPSG:32611", transform=T, tiled=True
)
SPOOL = re.compile(r"\.tmp[A-Za-z0-9_]{6}")
PALETTE = {0: (0, 0, 0, 255), 1: (200, 30, 30, 255), 2: (30, 200, 30, 255), 3: (30, 30, 200, 255)}


def source(path, pixels=RGB, nodata=None, metadata=None, colormap=None, **options):
    """A tiled GeoTIFF of `pixels` at `path`, with tags, a description, a unit, a scale and an
    offset on band 1 when `metadata`, and a color table when `colormap`."""
    profile = dict(
        PROFILE, count=pixels.shape[0], dtype=pixels.dtype.name, nodata=nodata, **options
    )
    with rio.open(path, "w", **profile) as dst:
        dst.write(pixels)
        if metadata:
            dst.update_tags(SOURCE="test")
            dst.update_tags(1, NAME="first")
            dst.set_band_description(1, "first band")
            dst.set_band_unit(1, "m")
            dst.scales = (0.5,) + (1.0,) * (pixels.shape[0] - 1)
            dst.offsets = (10.0,) + (0.0,) * (pixels.shape[0] - 1)
        if colormap:
            dst.write_colormap(1, colormap)
    return str(path)


class Recording(DatasetReader):
    """A dataset that records the rows its pixels are read in, and the files of `directory` while
    they are read."""

    def __init__(self, path, directory):
        super().__init__(path)
        self.directory = directory
        self.windows = []
        self.seen = set()

    def read(self, *args, **kwargs):
        window = kwargs.get("window")
        self.windows.append(None if window is None else (int(window.row_off), int(window.height)))
        self.seen.update(p.name for p in self.directory.iterdir())
        return super().read(*args, **kwargs)


def traced_copy(src, out, **options):
    """`shutil.copy(src, out, **options)`, and the peak of the memory Python and NumPy allocate
    during it. Rust allocations are excluded; process RSS is measured separately."""
    tracemalloc.start()
    try:
        rio_shutil.copy(src, out, **options)
        return tracemalloc.get_traced_memory()[1]
    finally:
        tracemalloc.stop()


@pytest.mark.parametrize("how", ["path", "dataset", "memoryfile", "vrt"])
def test_a_cog_copy_allocates_no_array_of_the_whole_image(tmp_path, how):
    src = source(tmp_path / "src.tif")
    out = str(tmp_path / "out.tif")
    if how == "path":
        peak = traced_copy(src, out, driver="COG", compress="deflate")
    elif how == "vrt":
        vrt = tmp_path / "src.vrt"
        bands = "".join(
            f'<VRTRasterBand dataType="Byte" band="{b}"><SimpleSource>'
            '<SourceFilename relativeToVRT="1">src.tif</SourceFilename>'
            f"<SourceBand>{b}</SourceBand></SimpleSource></VRTRasterBand>"
            for b in (1, 2, 3)
        )
        vrt.write_text(
            f'<VRTDataset rasterXSize="{WIDTH}" rasterYSize="{HEIGHT}">{bands}</VRTDataset>'
        )
        peak = traced_copy(str(vrt), out, driver="COG", compress="deflate")
    elif how == "dataset":
        with rio.open(src) as ds:
            peak = traced_copy(ds, out, driver="COG", compress="deflate")
    else:
        with open(src, "rb") as f, MemoryFile(f.read()) as memfile, memfile.open() as ds:
            peak = traced_copy(ds, out, driver="COG", compress="deflate")
    assert peak < RGB.nbytes / 2, f"{peak:,} bytes at the peak for an image of {RGB.nbytes:,}"
    with rio.open(out) as ds:
        assert np.array_equal(ds.read(), RGB)


@pytest.mark.parametrize(
    "driver, options, limit, rows",
    [
        ("COG", dict(compress="deflate"), None, 512),
        ("COG", dict(compress="deflate", blocksize=256), None, 256),
        # A GeoTIFF over the in-memory limit is written in rows of about 16 MB, here the image.
        ("GTiff", dict(compress="lzw"), "1", HEIGHT),
    ],
    ids=["cog-512", "cog-256", "gtiff-over-the-limit"],
)
def test_a_copy_written_in_rows_reads_its_source_a_row_at_a_time(
    tmp_path, monkeypatch, driver, options, limit, rows
):
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    src = Recording(source(tmp_path / "src.tif"), tmp_path)
    with src:
        rio_shutil.copy(src, str(tmp_path / "out.tif"), driver=driver, **options)
    assert src.windows == [(y, min(rows, HEIGHT - y)) for y in range(0, HEIGHT, rows)]
    # Only the source and the output being written exist, with no pixel staging file.
    assert "src.tif" in src.seen
    outputs = src.seen - {"src.tif"}
    if sys.platform == "win32":
        # A COG's overview spools are unlinked on creation on Unix, but on Windows they are named
        # `.tmpXXXXXX` files in the output's directory (as GDAL's) until they are closed.
        outputs = {name for name in outputs if not SPOOL.fullmatch(name)}
    assert len(outputs) == 1
    assert next(iter(outputs)).startswith("out.tif.tmp-")
    assert {p.name for p in tmp_path.iterdir()} == {"src.tif", "out.tif"}


FLOAT = (RGB[:1].astype("float32") - 100.0) / 3.0
COPIES = {
    "cog-byte-rgb-deflate": ("COG", dict(), dict(compress="deflate"), None),
    "cog-float32-nodata-metadata-zstd": (
        "COG",
        dict(pixels=FLOAT, nodata=-9999.0, metadata=True),
        dict(compress="zstd", predictor=3),
        None,
    ),
    "cog-uint16-band-interleaved-uncompressed": (
        "COG",
        dict(pixels=RGB.astype("uint16") * 200, interleave="band"),
        dict(),
        None,
    ),
    "cog-paletted": (
        "COG",
        dict(pixels=RGB[:1] % 4, colormap=PALETTE),
        dict(compress="deflate"),
        None,
    ),
    "gtiff-over-the-in-memory-limit": (
        "GTiff",
        dict(metadata=True),
        dict(compress="lzw", tiled=True),
        "1",
    ),
    # Not written in rows: the copy holds the pixels either way.
    "cog-jpeg-compressed": ("COG", dict(), dict(compress="jpeg"), None),
    "gtiff-below-the-in-memory-limit": ("GTiff", dict(), dict(compress="lzw", tiled=True), None),
}


@pytest.mark.parametrize("name", list(COPIES))
def test_a_copy_written_from_its_source_is_the_one_written_from_its_pixels(
    tmp_path, monkeypatch, name
):
    driver, source_options, options, limit = COPIES[name]
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    src = source(tmp_path / "src.tif", **source_options)
    streamed, held = str(tmp_path / "streamed.tif"), str(tmp_path / "held.tif")
    rio_shutil.copy(src, streamed, driver=driver, **options)
    with rio.open(src) as ds:
        rio_shutil._copy_dataset(ds, held, driver, options, from_source=False)
    assert filecmp.cmp(streamed, held, shallow=False)


def test_a_copy_keeps_its_source_s_dataset_mask(tmp_path):
    mask = np.full((HEIGHT, WIDTH), 255, np.uint8)
    mask[:700, :300] = 0
    src = str(tmp_path / "src.tif")
    with rio.open(src, "w", **dict(PROFILE, count=3, dtype="uint8")) as dst:
        dst.write(RGB)
        dst.write_mask(mask)
    out = str(tmp_path / "out.tif")
    rio_shutil.copy(src, out, driver="COG", compress="deflate")
    with rio.open(out) as ds:
        assert np.array_equal(ds.dataset_mask(), mask)


def test_a_copy_keeps_its_source_s_overviews(tmp_path):
    src = str(tmp_path / "src.tif")
    with rio.open(src, "w", **dict(PROFILE, count=3, dtype="uint8")) as dst:
        dst.write(RGB)
        dst.build_overviews([2, 4], Resampling.nearest)
    out = str(tmp_path / "out.tif")
    rio_shutil.copy(src, out, driver="COG", compress="deflate")
    with rio.open(out) as ds:
        assert ds.overviews(1) == [2, 4]
    for level in (0, 1):
        with rio.open(src, overview_level=level) as a, rio.open(out, overview_level=level) as b:
            assert np.array_equal(b.read(), a.read())


def test_a_copy_s_writer_given_a_mask_after_all_writes_its_source_whole(tmp_path):
    # `shutil.copy` gives no source to a copy with a mask; a writer given one and then a mask is
    # no longer written in rows, so it reads its source whole when it is written.
    src = source(tmp_path / "src.tif")
    mask = np.full((HEIGHT, WIDTH), 255, np.uint8)
    mask[:, :100] = 0
    out = str(tmp_path / "out.tif")
    profile = dict(PROFILE, driver="COG", count=3, dtype="uint8", compress="deflate")
    with rio.open(src) as ds, rio.open(out, "w", **profile, _copy_of=ds) as dst:
        assert dst._copy_of is ds
        dst.write_mask(mask)
    with rio.open(out) as ds:
        assert np.array_equal(ds.dataset_mask(), mask)
        assert np.array_equal(ds.read()[:, :, 100:], RGB[:, :, 100:])


@pytest.mark.parametrize("driver", ["COG", "GTiff"])
@pytest.mark.parametrize("link", ["symlink", "hardlink"])
@pytest.mark.parametrize("opened", [False, True])
def test_copy_refuses_source_alias_before_writing(tmp_path, monkeypatch, driver, link, opened):
    """A streamed copy must not truncate its source through a different local name."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1")
    src, alias = tmp_path / "source.tif", tmp_path / "alias.tif"
    source(src, compress="lzw")
    before = src.read_bytes()
    try:
        if link == "symlink":
            alias.symlink_to(src)
        else:
            os.link(src, alias)
    except (OSError, NotImplementedError) as error:
        pytest.skip(f"{link} unavailable: {error}")
    with rio.open(src) as ds:
        with pytest.raises(rio.errors.RasterioIOError, match="identify the same dataset"):
            rio_shutil.copy(
                ds if opened else src, alias, driver=driver, compress="deflate", tiled=True
            )
    assert src.read_bytes() == before
    assert alias.read_bytes() == before
    with rio.open(src) as ds:
        np.testing.assert_array_equal(ds.read(), RGB)


@pytest.mark.parametrize("driver", ["COG", "GTiff"])
@pytest.mark.parametrize("existing", [False, True])
def test_failed_streamed_copy_preserves_destination(tmp_path, monkeypatch, driver, existing):
    """Source failures must not publish partial output or replace an existing dataset."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1")
    src, dst = tmp_path / "source.tif", tmp_path / "output.tif"
    source(src)
    before = None
    if existing:
        source(dst, pixels=RGB ^ 255)
        before = dst.read_bytes()

    class FailingReader(DatasetReader):
        calls = 0

        def read(self, *args, **kwargs):
            self.calls += 1
            if self.calls > (1 if driver == "COG" else 0):
                raise OSError("source tile unavailable")
            return super().read(*args, **kwargs)

    with FailingReader(src) as ds:
        with pytest.raises(rio.errors.RasterioIOError, match="source tile unavailable"):
            rio_shutil.copy(ds, dst, driver=driver, compress="deflate", tiled=True)
    if existing:
        assert dst.read_bytes() == before
    else:
        assert not dst.exists()
    assert not list(tmp_path.glob("*.tmp-*"))


@pytest.mark.parametrize("block_rows, slab_rows", [(768, 1536), (1024, 1024)])
def test_cog_copy_reads_tall_source_blocks_in_bounded_slabs(tmp_path, block_rows, slab_rows):
    src = Recording(source(tmp_path / "src.tif", blockxsize=512, blockysize=block_rows), tmp_path)
    with src:
        rio_shutil.copy(src, tmp_path / "out.tif", driver="COG", compress="deflate")
    assert src.windows == [(y, min(slab_rows, HEIGHT - y)) for y in range(0, HEIGHT, slab_rows)]
    with rio.open(tmp_path / "out.tif") as ds:
        np.testing.assert_array_equal(ds.read(), RGB)


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="Windows refuses to replace a file the open VRT still reads (WinError 5)",
)
@pytest.mark.parametrize("driver", ["COG", "GTiff"])
def test_streamed_copy_reads_a_vrt_before_replacing_its_input(tmp_path, monkeypatch, driver):
    """A VRT may read the output's old contents throughout a streamed replacement."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1")
    dst, vrt = tmp_path / "source.tif", tmp_path / "source.vrt"
    source(dst)
    bands = "".join(
        f'<VRTRasterBand dataType="Byte" band="{b}"><SimpleSource>'
        '<SourceFilename relativeToVRT="1">source.tif</SourceFilename>'
        f"<SourceBand>{b}</SourceBand></SimpleSource></VRTRasterBand>"
        for b in (1, 2, 3)
    )
    vrt.write_text(f'<VRTDataset rasterXSize="{WIDTH}" rasterYSize="{HEIGHT}">{bands}</VRTDataset>')
    rio_shutil.copy(vrt, dst, driver=driver, compress="deflate", tiled=True)
    with rio.open(dst) as ds:
        np.testing.assert_array_equal(ds.read(), RGB)
    assert not list(tmp_path.glob("*.tmp-*"))


@pytest.mark.parametrize("driver", ["COG", "GTiff"])
def test_streamed_copy_replaces_destination_symlink_without_writing_its_target(
    tmp_path, monkeypatch, driver
):
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1")
    src, target, dst = tmp_path / "source.tif", tmp_path / "target.tif", tmp_path / "link.tif"
    source(src)
    source(target, pixels=RGB ^ 255)
    before = target.read_bytes()
    try:
        dst.symlink_to(target)
    except (OSError, NotImplementedError) as error:
        pytest.skip(f"symlink unavailable: {error}")
    rio_shutil.copy(src, dst, driver=driver, compress="deflate", tiled=True)
    assert not dst.is_symlink()
    assert target.read_bytes() == before
    with rio.open(dst) as ds:
        np.testing.assert_array_equal(ds.read(), RGB)


def corrupt_tile(path, index):
    """Overwrites the compressed bytes of a tile of the little-endian TIFF at `path` (its first
    image), so that reading the tile fails."""
    data = bytearray(path.read_bytes())
    (ifd,) = struct.unpack_from("<I", data, 4)
    (n,) = struct.unpack_from("<H", data, ifd)
    found = {}
    for i in range(n):
        tag, kind, count, value = struct.unpack_from("<HHII", data, ifd + 2 + 12 * i)
        if tag in (324, 325):  # TileOffsets, TileByteCounts
            assert kind == 4 and count > 1, (tag, kind, count)
            found[tag] = struct.unpack_from(f"<{count}I", data, value)
    offset, size = found[324][index], found[325][index]
    data[offset : offset + size] = b"\xff" * size
    path.write_bytes(bytes(data))


# The copies that hold their source's pixels until they are written, and those written in rows
# from it (#797): the driver, the creation options, the in-memory limit, and what the source has
# besides its pixels.
COPIES_OF_A_FAILING_SOURCE = {
    "gtiff-below-the-in-memory-limit": ("GTiff", dict(compress="lzw", tiled=True), None, None),
    "cog-jpeg": ("COG", dict(compress="jpeg"), None, None),
    "cog-webp": ("COG", dict(compress="webp"), None, None),
    "cog-lerc": ("COG", dict(compress="lerc"), None, None),
    "cog-jxl": ("COG", dict(compress="jxl"), None, None),
    "cog-of-a-masked-source": ("COG", dict(compress="deflate"), None, "mask"),
    "gtiff-over-the-limit-of-a-masked-source": ("GTiff", dict(compress="lzw"), "1", "mask"),
    "cog-copying-overviews": ("COG", dict(compress="deflate"), None, "overviews"),
    # Written in rows.
    "cog": ("COG", dict(compress="deflate"), None, None),
    "gtiff-over-the-in-memory-limit": ("GTiff", dict(compress="lzw", tiled=True), "1", None),
}


def failing_source(path, has=None):
    """`source(path)`, with a dataset mask or overviews, and one tile that cannot be read."""
    with rio.open(path, "w", **dict(PROFILE, count=3, dtype="uint8", compress="deflate")) as dst:
        dst.write(RGB)
        if has == "mask":
            mask = np.full((HEIGHT, WIDTH), 255, np.uint8)
            mask[:700, :300] = 0
            dst.write_mask(mask)
        if has == "overviews":
            dst.build_overviews([2, 4], Resampling.nearest)
    corrupt_tile(path, 6)
    return str(path)


@pytest.mark.parametrize("onto", ["a-file", "a-symlink", "nothing"])
@pytest.mark.parametrize("name", list(COPIES_OF_A_FAILING_SOURCE))
def test_a_copy_whose_source_fails_leaves_its_destination_as_it_was(
    tmp_path, monkeypatch, name, onto
):
    """A source with a tile that cannot be read: the copy raises, an existing destination keeps
    its bytes (a symbolic link stays one, and its target keeps its bytes), and no file named after
    the destination (a temporary or spill file, a mask) is left. Files are checked by name, not
    counted: Windows lists a COG writer's spools in the directory while they are open."""
    driver, options, limit, has = COPIES_OF_A_FAILING_SOURCE[name]
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    src = failing_source(tmp_path / "src.tif", has)
    out, target = tmp_path / "out.tif", tmp_path / "target.tif"
    existing = {"a-file": out, "a-symlink": target}.get(onto)
    if existing is not None:
        with rio.open(
            existing, "w", driver="GTiff", width=10, height=10, count=1, dtype="uint8"
        ) as ds:
            ds.write(np.full((1, 10, 10), 7, np.uint8))
    if onto == "a-symlink":
        try:
            out.symlink_to(target)
        except (OSError, NotImplementedError) as error:
            pytest.skip(f"symlink unavailable: {error}")
    before = existing.read_bytes() if existing is not None else None
    with pytest.raises(rio.errors.RasterioIOError, match="Read failed"):
        rio_shutil.copy(src, str(out), driver=driver, **options)
    left = sorted(p.name for p in tmp_path.glob("out.tif*"))
    if existing is None:
        assert left == []
    else:
        assert left == ["out.tif"]
        assert existing.read_bytes() == before
    if onto == "a-symlink":
        assert out.is_symlink() and out.resolve() == target.resolve()


def test_an_interrupted_held_copy_writes_nothing(tmp_path):
    """A copy held until it is written, interrupted while it reads its source, leaves its
    destination as it was."""
    src, out = source(tmp_path / "src.tif"), tmp_path / "out.tif"
    with rio.open(out, "w", driver="GTiff", width=10, height=10, count=1, dtype="uint8") as ds:
        ds.write(np.full((1, 10, 10), 7, np.uint8))
    before = out.read_bytes()

    class Interrupted(DatasetReader):
        def read(self, *args, **kwargs):
            raise KeyboardInterrupt

    with Interrupted(src) as ds, pytest.raises(KeyboardInterrupt):
        rio_shutil.copy(ds, str(out), driver="GTiff", compress="lzw", tiled=True)
    assert out.read_bytes() == before
    assert sorted(p.name for p in tmp_path.glob("out.tif*")) == ["out.tif"]
