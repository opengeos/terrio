"""`terrio.rio.shutil.copy` writes a COG from its source as it writes it (rasterio not needed).

A copy wrote its source's pixels into the dataset it created, which held them until it was closed
and its file written: the whole image, besides what the COG's writer holds. When the file is
written in rows (a COG without overviews of the source to copy or a compression written whole; a
GeoTIFF larger than the in-memory limit), the copy now reads the source as it writes, a row of a
COG's tiles at a time, and the rows of the source's dataset mask with its pixels'. These tests
check that the file is the one the copy holding the pixels and mask writes, byte for byte, that
the copy reads the source a row of tiles at a time and allocates no array of the whole image, and
that what is not written in rows is copied as before.
"""

import filecmp
import os
import re
import struct
import sys
import tracemalloc

import numpy as np
import pytest

import terrio
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
WINDOWS_SPOOL = re.compile(r"\.tmp[A-Za-z0-9]{6}")
PALETTE = {0: (0, 0, 0, 255), 1: (200, 30, 30, 255), 2: (30, 200, 30, 255), 3: (30, 30, 200, 255)}
# A dataset mask invalid in a corner and, across two rows of a COG's tiles, at every third pixel
# along diagonals, so that its overviews weigh valid and invalid pixels.
MASK = np.full((HEIGHT, WIDTH), 255, np.uint8)
MASK[:700, :300] = 0
MASK[1000:1100] = np.where(np.add.outer(np.arange(100), np.arange(WIDTH)) % 3, 255, 0)


def source(
    path,
    pixels=RGB,
    nodata=None,
    metadata=None,
    colormap=None,
    mask=None,
    overviews=None,
    **options,
):
    """A tiled GeoTIFF of `pixels` at `path`, with tags, a description, a unit, a scale and an
    offset on band 1 when `metadata`, a color table when `colormap`, a dataset mask when `mask`,
    and the overviews of the factors `overviews`."""
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
        if mask is not None:
            dst.write_mask(mask)
        if overviews:
            dst.build_overviews(overviews, Resampling.nearest)
    return str(path)


class Recording(DatasetReader):
    """A dataset that records the rows its pixels and its dataset mask are read in, and the files
    of `directory` while they are read."""

    def __init__(self, path, directory, **kwargs):
        super().__init__(path, **kwargs)
        self.directory = directory
        self.windows = []
        self.mask_windows = []
        self.seen = set()

    def read(self, *args, **kwargs):
        window = kwargs.get("window")
        self.windows.append(None if window is None else (int(window.row_off), int(window.height)))
        self.seen.update(p.name for p in self.directory.iterdir())
        return super().read(*args, **kwargs)

    def dataset_mask(self, *args, **kwargs):
        window = kwargs.get("window")
        self.mask_windows.append(
            None if window is None else (int(window.row_off), int(window.height))
        )
        self.seen.update(p.name for p in self.directory.iterdir())
        return super().dataset_mask(*args, **kwargs)


def traced_copy(src, out, **options):
    """`shutil.copy(src, out, **options)`, and the peak of the memory Python and NumPy allocate
    during it. Rust allocations are excluded; process RSS is measured separately."""
    tracemalloc.start()
    try:
        rio_shutil.copy(src, out, **options)
        return tracemalloc.get_traced_memory()[1]
    finally:
        tracemalloc.stop()


@pytest.mark.parametrize("how", ["path", "dataset", "memoryfile", "vrt", "masked"])
def test_a_cog_copy_allocates_no_array_of_the_whole_image(tmp_path, how):
    src = source(tmp_path / "src.tif", mask=MASK if how == "masked" else None)
    out = str(tmp_path / "out.tif")
    if how in ("path", "masked"):
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
        if how == "masked":
            assert np.array_equal(ds.dataset_mask(), MASK)


@pytest.mark.parametrize(
    "driver, options, limit, rows, mask",
    [
        ("COG", dict(compress="deflate"), None, 512, None),
        ("COG", dict(compress="deflate", blocksize=256), None, 256, None),
        # A GeoTIFF over the in-memory limit is written in rows of about 16 MB, here the image.
        ("GTiff", dict(compress="lzw"), "1", HEIGHT, None),
        # A dataset mask is read in the rows of the pixels.
        ("COG", dict(compress="deflate"), None, 512, MASK),
        ("GTiff", dict(compress="lzw"), "1", HEIGHT, MASK),
    ],
    ids=["cog-512", "cog-256", "gtiff-over-the-limit", "masked-cog", "masked-gtiff-over-the-limit"],
)
def test_a_copy_written_in_rows_reads_its_source_a_row_at_a_time(
    tmp_path, monkeypatch, driver, options, limit, rows, mask
):
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    src = Recording(source(tmp_path / "src.tif", mask=mask), tmp_path)
    with src:
        rio_shutil.copy(src, str(tmp_path / "out.tif"), driver=driver, **options)
    assert src.windows == [(y, min(rows, HEIGHT - y)) for y in range(0, HEIGHT, rows)]
    assert src.mask_windows == ([] if mask is None else src.windows)
    # Only the source and the output being written exist, with no pixel or mask staging file.
    assert "src.tif" in src.seen
    outputs = src.seen - {"src.tif"}
    if sys.platform == "win32":
        # A COG's chunk spools are anonymous files on Unix, but Windows lists a file deleted on
        # close (`tempfile::tempfile_in`, named `.tmpXXXXXX`) in its directory until it closes.
        outputs = {name for name in outputs if not WINDOWS_SPOOL.fullmatch(name)}
    assert len(outputs) == 1
    assert next(iter(outputs)).startswith("out.tif.tmp-")
    assert {p.name for p in tmp_path.iterdir()} == {"src.tif", "out.tif"}


FLOAT = (RGB[:1].astype("float32") - 100.0) / 3.0
# An alpha band transparent where MASK is invalid.
RGBA = np.concatenate([RGB, MASK[None]])
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
    # An alpha band is no dataset mask: the color bands' mask is the alpha band's, the alpha
    # band's own all valid. The copy is written in rows without a mask, the alpha band weighing
    # the overviews' averages.
    "cog-rgba-alpha-average": (
        "COG",
        dict(pixels=RGBA, photometric="RGB", alpha="YES"),
        dict(compress="deflate", overview_resampling="average"),
        None,
    ),
    # A source with a dataset mask, read in the rows of its pixels: partly valid, all valid; tiled,
    # striped, or in blocks taller than a COG's tiles (read in slabs of two rows of tiles); of
    # several data types with nodata; with or without the COG's overviews, its source's ignored.
    "cog-masked-rgb-deflate": ("COG", dict(mask=MASK), dict(compress="deflate"), None),
    "cog-masked-all-valid": (
        "COG",
        dict(mask=np.full_like(MASK, 255)),
        dict(compress="deflate"),
        None,
    ),
    "cog-masked-striped-source": (
        "COG",
        dict(mask=MASK, tiled=False),
        dict(compress="deflate"),
        None,
    ),
    "cog-masked-tall-source-blocks": (
        "COG",
        dict(mask=MASK, blockxsize=512, blockysize=768),
        dict(compress="deflate"),
        None,
    ),
    "cog-masked-float32-nodata-metadata-zstd": (
        "COG",
        dict(pixels=FLOAT, nodata=-9999.0, metadata=True, mask=MASK),
        dict(compress="zstd", predictor=3),
        None,
    ),
    "cog-masked-uint16-nodata-band-interleaved-uncompressed": (
        "COG",
        dict(pixels=RGB.astype("uint16") * 200, nodata=0, interleave="band", mask=MASK),
        dict(),
        None,
    ),
    "cog-masked-paletted": (
        "COG",
        dict(pixels=RGB[:1] % 4, colormap=PALETTE, mask=MASK),
        dict(compress="deflate"),
        None,
    ),
    "cog-masked-without-overviews": (
        "COG",
        dict(mask=MASK),
        dict(compress="deflate", overviews="NONE"),
        None,
    ),
    "cog-masked-ignoring-its-source-s-overviews": (
        "COG",
        dict(mask=MASK, overviews=[2, 4]),
        dict(compress="deflate", overviews="IGNORE_EXISTING"),
        None,
    ),
    "gtiff-masked-over-the-in-memory-limit": (
        "GTiff",
        dict(metadata=True, mask=MASK),
        dict(compress="lzw", tiled=True),
        "1",
    ),
    # Not written in rows: the copy holds the pixels either way.
    "cog-jpeg-compressed": ("COG", dict(), dict(compress="jpeg"), None),
    "gtiff-below-the-in-memory-limit": ("GTiff", dict(), dict(compress="lzw", tiled=True), None),
    "cog-masked-copying-its-source-s-overviews": (
        "COG",
        dict(mask=MASK, overviews=[2, 4]),
        dict(compress="deflate"),
        None,
    ),
    "gtiff-masked-below-the-in-memory-limit": (
        "GTiff",
        dict(mask=MASK),
        dict(compress="lzw", tiled=True),
        None,
    ),
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
    # `shutil.copy` writes no mask to a dataset it gives its source to; a writer given one and
    # then a mask is no longer written in rows, so it reads its source whole when it is written.
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


def test_a_copy_s_writer_given_overviews_after_all_writes_its_source_s_mask_whole(tmp_path):
    # A writer given a source and its mask (`_copy_mask`), and then overviews, is no longer
    # written in rows: it reads its source's pixels and mask whole when it is written.
    src = source(tmp_path / "src.tif", mask=MASK)
    out = str(tmp_path / "out.tif")
    profile = dict(PROFILE, driver="COG", count=3, dtype="uint8", compress="deflate")
    with rio.open(src) as ds, rio.open(out, "w", **profile, _copy_of=ds, _copy_mask=True) as dst:
        assert dst._copy_of is ds
        dst.build_overviews([2], Resampling.nearest)
    with rio.open(out) as ds:
        assert np.array_equal(ds.dataset_mask(), MASK)
        assert np.array_equal(ds.read(), RGB)


def test_a_cog_writer_refuses_a_mask_of_its_rows_size_in_another_shape(tmp_path):
    """A mask is the `(rows, width)` of the pixels it masks: flattened, one of the same size in
    another shape (its rows and columns swapped, or one row) would mask other pixels."""
    from terrio import _terrio
    from terrio.errors import InvalidArgumentError

    rows, width = 2, 4
    pixels = np.arange(rows * width, dtype="uint8").reshape(rows, width)
    mask = np.array([[255, 0, 255, 255], [0, 255, 255, 0]], np.uint8)
    out = str(tmp_path / "masked.tif")
    writer = _terrio.CogWriter(out, width, 2 * rows, 1, "uint8", T.to_gdal(), mask=True)
    for other in (mask.T, mask.reshape(1, rows * width)):
        with pytest.raises(InvalidArgumentError, match="expected a mask of 2 rows of 4 columns"):
            writer.write(pixels, other)
    assert writer.rows_written == 0
    writer.write(pixels, mask)
    writer.write(pixels, mask[::-1])
    writer.close()
    with rio.open(out) as ds:
        assert np.array_equal(ds.dataset_mask(), np.vstack([mask, mask[::-1]]))


def test_a_cog_writer_refuses_a_mask_it_has_not_and_rows_without_the_one_it_has(tmp_path):
    """A mask given to a writer created without one, or none to a writer created with one, is
    refused before the writer takes the rows, each with a message saying what to give instead: the
    writers then take their rows as they should be given."""
    from terrio import _terrio
    from terrio.errors import InvalidArgumentError

    rows, width = 2, 4
    pixels = np.arange(rows * width, dtype="uint8").reshape(rows, width)
    mask = np.array([[255, 0, 255, 255], [0, 255, 255, 0]], np.uint8)
    out = str(tmp_path / "masked.tif")
    plain = _terrio.CogWriter(str(tmp_path / "plain.tif"), width, rows, 1, "uint8", T.to_gdal())
    masked = _terrio.CogWriter(out, width, rows, 1, "uint8", T.to_gdal(), mask=True)
    with pytest.raises(InvalidArgumentError, match="the COG has no mask: give its rows without"):
        plain.write(pixels, mask)
    with pytest.raises(InvalidArgumentError, match="the COG has a mask: give its rows"):
        masked.write(pixels)
    assert plain.rows_written == masked.rows_written == 0
    plain.write(pixels)
    masked.write(pixels, mask)
    plain.close()
    masked.close()
    with rio.open(out) as ds:
        assert np.array_equal(ds.dataset_mask(), mask)


@pytest.mark.parametrize("driver, limit", [("COG", None), ("GTiff", "1")])
def test_a_copy_whose_mask_goes_to_a_sidecar_holds_it(tmp_path, monkeypatch, driver, limit):
    """With `GDAL_TIFF_INTERNAL_MASK=NO`, a copy's mask goes to a `.msk` sidecar, written from the
    dataset's mask: the copy holds its source's pixels and mask, and writes the files it writes
    holding them. Read from the source as the file is written, the mask would go into it."""
    src = source(tmp_path / "src.tif", mask=MASK)
    monkeypatch.setenv("GDAL_TIFF_INTERNAL_MASK", "NO")
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    copied, held = str(tmp_path / "copied.tif"), str(tmp_path / "held.tif")
    rio_shutil.copy(src, copied, driver=driver, compress="deflate")
    with rio.open(src) as ds:
        rio_shutil._copy_dataset(ds, held, driver, dict(compress="deflate"), from_source=False)
    for suffix in ("", ".msk"):
        assert filecmp.cmp(copied + suffix, held + suffix, shallow=False), suffix
    with rio.open(copied) as ds:
        assert np.array_equal(ds.dataset_mask(), MASK)


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


def wide_pixels(rows=1030, cols=8200):
    """A UInt16 band of `rows` x `cols`, made when a test needs it."""
    return (np.add.outer(np.arange(rows) * 5, np.arange(cols)) % 4093).astype("uint16")


@pytest.fixture(scope="module")
def wide_sources(tmp_path_factory):
    """`{name: (path, pixels)}`: images in rows of 1024 x 1024 tiles (terrio's JPEG 2000 tiles),
    each row of tiles holding less than the block cache's 64 MiB, made for the tests that use them:

    - "jp2": a UInt16 band (`wide_pixels`), a row of tiles and 6 rows of the next, whose row of
      tiles holds 16,793,600 bytes, more than a copy's 16 MiB slab budget;
    - "tif": the same band, a GeoTIFF of 512 x 1024 tiles;
    - "jp2-rgb": three Byte bands of 12,000 bytes a row, whose 16 MB of rows hold a row of tiles
      and a part of the next.
    """
    folder = tmp_path_factory.mktemp("wide")
    tiles = dict(tiled=True, blockxsize=512, blockysize=1024)
    band = wide_pixels()[None]
    rgb = (np.add.outer(np.arange(1400) * 3, np.arange(4000)) % 251).astype("uint8")
    rgb = np.stack([rgb, rgb[::-1], rgb[:, ::-1]])
    sources = {
        "jp2": (band, dict(driver="JP2OpenJPEG")),
        "tif": (band, dict(driver="GTiff", **tiles)),
        "jp2-rgb": (rgb, dict(driver="JP2OpenJPEG")),
    }
    out = {}
    for name, (pixels, options) in sources.items():
        path = str(folder / (name + (".tif" if name == "tif" else ".jp2")))
        count, height, width = pixels.shape
        profile = dict(width=width, height=height, count=count, dtype=pixels.dtype.name)
        with rio.open(path, "w", crs="EPSG:32611", transform=T, **profile, **options) as dst:
            dst.write(pixels)
        out[name] = (path, pixels)
    return out


@pytest.fixture
def block_cache_size():
    """Gives the block cache its size back after a test that changes it."""
    size = terrio.block_cache_max()
    yield
    terrio.set_block_cache_max(size)


@pytest.mark.parametrize(
    "name, driver, limit, cache, rows",
    [
        # A JPEG 2000 read decodes every tile its window crosses and keeps none: a whole row of
        # tiles at a time, each tile is decoded once.
        ("jp2", "COG", None, None, 1024),
        # A row of tiles holding more than the block cache is read within the budget, as before.
        ("jp2", "COG", None, 1 << 20, 512),
        # Over the in-memory limit, a GeoTIFF is read in rows of about 16 MB, 1,023 here: raised
        # to a row of tiles.
        ("jp2", "GTiff", "1", None, 1024),
        # Below it, a GeoTIFF is held, read in rows of 64 MB of 8-byte values, 1,023 here: raised
        # to a row of tiles.
        ("jp2", "GTiff", None, None, 1024),
        # Rows of about 16 MB holding more than a row of tiles, 1,398 here: rounded down to a
        # whole number of rows of tiles.
        ("jp2-rgb", "GTiff", "1", None, 1024),
        # A row of tiles counts every band: 12,288,000 bytes, more than this block cache.
        ("jp2-rgb", "GTiff", None, 8 << 20, 699),
    ],
    ids=[
        "jp2-cog",
        "jp2-over-the-cache",
        "jp2-gtiff-over-the-limit",
        "jp2-gtiff-held",
        "jp2-rows-of-16-mb",
        "jp2-bands-over-the-cache",
    ],
)
def test_a_copy_reads_a_jpeg_2000_a_row_of_its_tiles_at_a_time(
    tmp_path, monkeypatch, wide_sources, block_cache_size, name, driver, limit, cache, rows
):
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    if cache:
        terrio.set_block_cache_max(cache)
    path, pixels = wide_sources[name]
    height = pixels.shape[1]
    src = Recording(path, tmp_path)
    with src:
        rio_shutil.copy(src, str(tmp_path / "out.tif"), driver=driver, compress="deflate")
    assert src.windows == [(y, min(rows, height - y)) for y in range(0, height, rows)]
    with rio.open(tmp_path / "out.tif") as ds:
        assert np.array_equal(ds.read(), pixels)


def test_a_cog_copy_reads_a_geotiff_of_tall_tiles_within_the_slab_budget(tmp_path, wide_sources):
    """A GeoTIFF's decoded tiles stay in the block cache between reads: its copy reads rows of
    the 16 MiB budget, 512, though they cut its 1024-row tiles."""
    path, pixels = wide_sources["tif"]
    src = Recording(path, tmp_path)
    with src:
        rio_shutil.copy(src, str(tmp_path / "out.tif"), driver="COG", compress="deflate")
    assert src.windows == [(0, 512), (512, 512), (1024, 6)]
    with rio.open(tmp_path / "out.tif") as ds:
        assert np.array_equal(ds.read(), pixels)


@pytest.mark.parametrize(
    "driver, limit, dtype, cache, rows",
    [
        # 16,793,600 bytes of UInt16 fit a 32 MiB block cache: a whole row of tiles.
        ("COG", None, "uint16", 32 << 20, 1024),
        # 67,174,400 bytes of Float64 do not: the rows read before (512 for the COG's 16 MiB
        # budget, 1,023 held, 255 over the in-memory limit).
        ("COG", None, "float64", 32 << 20, 512),
        ("GTiff", None, "float64", 32 << 20, 1023),
        ("GTiff", "1", "float64", 32 << 20, 255),
        # A copy to a narrower type reads its rows in the source's: 16,793,600 bytes of UInt16,
        # more than this block cache, though 8,396,800 of Byte are less.
        ("GTiff", None, "uint8", 16 << 20, 1023),
    ],
    ids=[
        "cog-uint16",
        "cog-float64",
        "gtiff-held-float64",
        "gtiff-over-the-limit-float64",
        "gtiff-held-uint8",
    ],
)
def test_a_jpeg_2000_copy_counts_a_row_of_tiles_in_the_type_it_holds_it_in(
    tmp_path, monkeypatch, wide_sources, block_cache_size, driver, limit, dtype, cache, rows
):
    """A copy to another type holds its rows in both types: a row of tiles is read whole when it
    fits in the block cache in the wider of the two."""
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    terrio.set_block_cache_max(cache)
    path, pixels = wide_sources["jp2"]
    height = pixels.shape[1]
    src = Recording(path, tmp_path)
    with src:
        rio_shutil.copy(
            src, str(tmp_path / "out.tif"), driver=driver, compress="deflate", dtype=dtype
        )
    assert src.windows == [(y, min(rows, height - y)) for y in range(0, height, rows)]
    with rio.open(tmp_path / "out.tif") as ds:
        assert ds.dtypes[0] == dtype
        assert np.array_equal(ds.read(), pixels.astype(dtype))


def test_a_copy_reads_an_overview_level_of_a_jpeg_2000_in_rows_of_its_tiles(
    tmp_path, monkeypatch, wide_sources
):
    """An overview level of a JPEG 2000 is read by window as the image is: copied to Float64 over
    the in-memory limit, its 515 rows are read at once, where rows of about 16 MB (511) ended
    inside its first row of tiles."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1")
    path = wide_sources["jp2"][0]
    with rio.open(path, overview_level=0) as ds:
        pixels = ds.read()
    src = Recording(path, tmp_path, overview_level=0)
    with src:
        assert (src.height, src.block_shapes) == (515, [(1024, 1024)])
        out = str(tmp_path / "out.tif")
        rio_shutil.copy(src, out, driver="GTiff", compress="deflate", dtype="float64")
    assert src.windows == [(0, 515)]
    with rio.open(out) as ds:
        assert np.array_equal(ds.read(), pixels.astype("float64"))


def test_a_cog_copy_reads_a_jpeg_2000_of_one_row_of_tiles_at_once(tmp_path, block_cache_size):
    """A row of tiles taller than the image was read in two parts, each decoding every tile. It
    is now read at once when the image's rows fit in the block cache (1,320,000 bytes here, in
    1.5 MiB), though the 1024 rows of its tiles would not."""
    terrio.set_block_cache_max(3 << 19)
    path = str(tmp_path / "one.jp2")
    pixels = wide_pixels(600, 1100)
    profile = dict(width=1100, height=600, count=1, dtype="uint16", crs="EPSG:32611")
    with rio.open(path, "w", driver="JP2OpenJPEG", transform=T, **profile) as dst:
        dst.write(pixels, 1)
    src = Recording(path, tmp_path)
    with src:
        assert src.block_shapes == [(1024, 1024)]
        rio_shutil.copy(src, str(tmp_path / "out.tif"), driver="COG", compress="deflate")
    assert src.windows == [(0, 600)]
    with rio.open(tmp_path / "out.tif") as ds:
        assert np.array_equal(ds.read(1), pixels)


def test_a_cog_copy_reads_384_row_jpeg_2000_tiles_a_row_of_tiles_at_a_time(tmp_path):
    """Rows of 384-row tiles, which the COG's 512-row tiles cut, are read one at a time, not in
    rows of both tile heights (1,536): the writer takes rows in any number, and the file is the
    one written whole from the pixels held. Read 512 rows at a time, two rows of tiles in three
    were decoded twice."""
    rasterio = pytest.importorskip("rasterio")
    path = str(tmp_path / "tiles.jp2")
    pixels = wide_pixels(1000, 1300)
    pixels = np.vstack([pixels, pixels])
    profile = dict(width=1300, height=2000, count=1, dtype="uint16", crs="EPSG:32611")
    options = dict(blockxsize=640, blockysize=384, quality=100, reversible="yes")
    with rasterio.open(path, "w", driver="JP2OpenJPEG", transform=T, **profile, **options) as dst:
        dst.write(pixels, 1)
    src = Recording(path, tmp_path)
    with src:
        assert src.block_shapes == [(384, 640)]
        rio_shutil.copy(src, str(tmp_path / "out.tif"), driver="COG", compress="deflate")
    assert src.windows == [(y, min(384, 2000 - y)) for y in range(0, 2000, 384)]
    held = str(tmp_path / "held.tif")
    with rio.open(path) as ds:
        rio_shutil._copy_dataset(ds, held, "COG", {"compress": "deflate"}, from_source=False)
    assert filecmp.cmp(str(tmp_path / "out.tif"), held, shallow=False)
    with rio.open(tmp_path / "out.tif") as ds:
        assert np.array_equal(ds.read(1), pixels)


@pytest.mark.parametrize("writer", ["terrio", "rasterio"])
def test_a_held_copy_reads_a_whole_jpeg_2000_in_one_read(tmp_path, writer):
    """A held copy reads a 2000 x 2000 image at once; rounding those rows down to whole rows of
    its 1024-row blocks read it in two parts. terrio writes the image in 1024 x 1024 tiles;
    rasterio here in one tile, which terrio reads in 1024-row blocks, so two reads decoded that
    tile in two passes."""
    pixels = np.vstack([wide_pixels(1000, 2000)] * 2)
    path = str(tmp_path / "src.jp2")
    profile = dict(width=2000, height=2000, count=1, dtype="uint16", crs="EPSG:32611", transform=T)
    if writer == "terrio":
        with rio.open(path, "w", driver="JP2OpenJPEG", **profile) as dst:
            dst.write(pixels, 1)
    else:
        rasterio = pytest.importorskip("rasterio")
        options = dict(blockxsize=2000, blockysize=2000, quality=100, reversible="yes")
        with rasterio.open(path, "w", driver="JP2OpenJPEG", **profile, **options) as dst:
            dst.write(pixels, 1)
    src = Recording(path, tmp_path)
    with src:
        assert src.block_shapes == [(1024, 1024)]
        rio_shutil.copy(src, str(tmp_path / "out.tif"), driver="GTiff", compress="deflate")
    assert src.windows == [(0, 2000)]
    with rio.open(tmp_path / "out.tif") as ds:
        assert np.array_equal(ds.read(1), pixels)


def test_a_jpeg_2000_copy_is_the_same_file_whatever_rows_it_reads_at_a_time(
    tmp_path, wide_sources, block_cache_size
):
    """The COG of a JPEG 2000 read a row of tiles at a time is the one read in half rows, and
    the one written whole from the pixels held."""
    path = wide_sources["jp2"][0]
    by_rows, by_half_rows, held = (str(tmp_path / f"{name}.tif") for name in "abc")
    rio_shutil.copy(path, by_rows, driver="COG", compress="deflate")
    with rio.open(path) as ds:
        rio_shutil._copy_dataset(ds, held, "COG", {"compress": "deflate"}, from_source=False)
    terrio.set_block_cache_max(1 << 20)
    rio_shutil.copy(path, by_half_rows, driver="COG", compress="deflate")
    assert filecmp.cmp(by_rows, by_half_rows, shallow=False)
    assert filecmp.cmp(by_rows, held, shallow=False)


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
    if sys.platform == "win32":
        # Windows refuses to replace a file the VRT still holds open (WinError 5): the copy must
        # fail cleanly and leave the destination as it was.
        before = dst.read_bytes()
        with pytest.raises(rio.errors.RasterioIOError):
            rio_shutil.copy(vrt, dst, driver=driver, compress="deflate", tiled=True)
        assert dst.read_bytes() == before
        assert not list(tmp_path.glob("*.tmp-*"))
        return
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


def corrupt_tile(path, index, subfile_type=0):
    """Overwrites the compressed bytes of a tile of the little-endian TIFF at `path`, in its first
    image of NewSubfileType `subfile_type` (0 the full-resolution image, 1 an overview, 4 a mask),
    so that reading the tile fails."""
    data = bytearray(path.read_bytes())
    (ifd,) = struct.unpack_from("<I", data, 4)
    while ifd:
        (n,) = struct.unpack_from("<H", data, ifd)
        found = {254: (0,)}
        for i in range(n):
            tag, kind, count, value = struct.unpack_from("<HHII", data, ifd + 2 + 12 * i)
            if tag == 254:  # NewSubfileType
                found[tag] = (value,)
            if tag in (324, 325):  # TileOffsets, TileByteCounts
                assert kind == 4 and count > 1, (tag, kind, count)
                found[tag] = struct.unpack_from(f"<{count}I", data, value)
        if found[254][0] == subfile_type:
            offset, size = found[324][index], found[325][index]
            data[offset : offset + size] = b"\xff" * size
            path.write_bytes(bytes(data))
            return
        (ifd,) = struct.unpack_from("<I", data, ifd + 2 + 12 * n)
    raise AssertionError(f"{path} has no image of NewSubfileType {subfile_type}")


# The copies that hold their source's pixels until they are written, and those written in rows
# from it (#797): the driver, the creation options, the in-memory limit, and what the source has
# besides its pixels.
COPIES_OF_A_FAILING_SOURCE = {
    "gtiff-below-the-in-memory-limit": ("GTiff", dict(compress="lzw", tiled=True), None, None),
    "cog-jpeg": ("COG", dict(compress="jpeg"), None, None),
    "cog-webp": ("COG", dict(compress="webp"), None, None),
    "cog-lerc": ("COG", dict(compress="lerc"), None, None),
    "cog-jxl": ("COG", dict(compress="jxl"), None, None),
    "cog-copying-overviews": ("COG", dict(compress="deflate"), None, "overviews"),
    # Written in rows.
    "cog": ("COG", dict(compress="deflate"), None, None),
    "gtiff-over-the-in-memory-limit": ("GTiff", dict(compress="lzw", tiled=True), "1", None),
    "cog-of-a-masked-source": ("COG", dict(compress="deflate"), None, "mask"),
    "gtiff-over-the-limit-of-a-masked-source": ("GTiff", dict(compress="lzw"), "1", "mask"),
    # Held, its mask read in rows into the dataset.
    "gtiff-below-the-limit-of-a-masked-source": ("GTiff", dict(compress="lzw"), None, "mask"),
}
# Copies of a source whose full-resolution image reads, but whose mask or first overview has a
# tile that cannot be read: the copy, the NewSubfileType of that image (4 a mask, 1 an
# overview), and the error the copy raises. The mask's read error is wrapped as a band's is
# ("Read failed"): read in the rows of the pixels as the file is written, in the write's error;
# read into a held copy, as it is. The overviews a COG copies are read when the dataset is
# written, as it closes.
COPIES_OF_A_SOURCE_WHOSE_MASK_OR_OVERVIEW_FAILS = {
    "cog-of-a-masked-source-whose-mask-fails": (
        "cog-of-a-masked-source",
        4,
        rio.errors.RasterioIOError,
        "Write failed: Read failed: corrupt deflate",
    ),
    "gtiff-over-the-limit-of-a-masked-source-whose-mask-fails": (
        "gtiff-over-the-limit-of-a-masked-source",
        4,
        rio.errors.RasterioIOError,
        "Write failed: Read failed: corrupt deflate",
    ),
    "gtiff-below-the-limit-of-a-masked-source-whose-mask-fails": (
        "gtiff-below-the-limit-of-a-masked-source",
        4,
        rio.errors.RasterioIOError,
        "^Read failed: corrupt deflate",
    ),
    "cog-copying-overviews-whose-overview-fails": (
        "cog-copying-overviews",
        1,
        rio.errors.RasterioIOError,
        "Write failed",
    ),
}


def failing_source(path, has=None, subfile_type=0):
    """`source(path)`, with a dataset mask or overviews, and one tile that cannot be read, in the
    image of NewSubfileType `subfile_type` (see `corrupt_tile`)."""
    with rio.open(path, "w", **dict(PROFILE, count=3, dtype="uint8", compress="deflate")) as dst:
        dst.write(RGB)
        if has == "mask":
            mask = np.full((HEIGHT, WIDTH), 255, np.uint8)
            mask[:700, :300] = 0
            dst.write_mask(mask)
        if has == "overviews":
            dst.build_overviews([2, 4], Resampling.nearest)
    corrupt_tile(path, 6 if subfile_type == 0 else 0, subfile_type)
    return str(path)


@pytest.mark.parametrize("onto", ["a-file", "a-symlink", "nothing"])
@pytest.mark.parametrize(
    "case",
    list(COPIES_OF_A_FAILING_SOURCE) + list(COPIES_OF_A_SOURCE_WHOSE_MASK_OR_OVERVIEW_FAILS),
)
def test_a_copy_whose_source_fails_leaves_its_destination_as_it_was(
    tmp_path, monkeypatch, case, onto
):
    """A source with a tile that cannot be read, in its full-resolution image, its mask or an
    overview the COG copies: the copy raises, an existing destination keeps its bytes (a symbolic
    link stays one, and its target keeps its bytes), and no file named after the destination (a
    temporary or spill file, a mask) is left. Files are checked by name, not counted: Windows
    lists a COG writer's spools in the directory while they are open."""
    name, subfile_type, raised, message = COPIES_OF_A_SOURCE_WHOSE_MASK_OR_OVERVIEW_FAILS.get(
        case, (case, 0, rio.errors.RasterioIOError, "Read failed")
    )
    driver, options, limit, has = COPIES_OF_A_FAILING_SOURCE[name]
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    src = failing_source(tmp_path / "src.tif", has, subfile_type)
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
    with pytest.raises(raised, match=message):
        rio_shutil.copy(src, str(out), driver=driver, **options)
    left = sorted(p.name for p in tmp_path.glob("out.tif*"))
    if existing is None:
        assert left == []
    else:
        assert left == ["out.tif"]
        assert existing.read_bytes() == before
    if onto == "a-symlink":
        assert out.is_symlink() and out.resolve() == target.resolve()


@pytest.mark.parametrize("resampled", [False, True], ids=["plain", "resampled"])
def test_a_dataset_mask_that_fails_to_read_raises_rasterioioerror(tmp_path, resampled):
    """A mask tile that cannot be read raises `RasterioIOError` ("Read failed"), as a pixel tile
    does, whether the mask is read as it is stored or resampled."""
    src = failing_source(tmp_path / "src.tif", "mask", 4)
    options = dict(out_shape=(HEIGHT // 2, WIDTH // 2), resampling=Resampling.average)
    with rio.open(src) as ds, pytest.raises(rio.errors.RasterioIOError, match="^Read failed"):
        ds.dataset_mask(**(options if resampled else {}))


class MaskReadFails(DatasetReader):
    """A dataset whose dataset mask fails to read with the core's I/O error."""

    def dataset_mask(self, *args, **kwargs):
        raise terrio.errors.TerrioIOError("the mask cannot be read")


@pytest.mark.parametrize("route", ["cog-rows", "gtiff-rows", "gtiff-held", "writer-held"])
def test_a_copy_s_mask_that_fails_to_read_raises_rasterioioerror(tmp_path, monkeypatch, route):
    """Wherever a copy reads its source's mask, in the rows of the pixels it writes (a COG, a
    GeoTIFF over the in-memory limit), in rows into the dataset that holds them (a GeoTIFF below
    the limit), or whole when a writer given its source no longer writes in rows, a mask that fails
    to read raises `RasterioIOError` ("Read failed"), as pixels that fail to read do."""
    src, out = source(tmp_path / "src.tif", mask=MASK), str(tmp_path / "out.tif")
    if route == "gtiff-rows":
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1")
    with MaskReadFails(src) as ds, pytest.raises(rio.errors.RasterioIOError) as raised:
        if route == "writer-held":
            profile = dict(PROFILE, driver="COG", count=3, dtype="uint8", compress="deflate")
            with rio.open(out, "w", **profile, _copy_of=ds, _copy_mask=True) as dst:
                dst.build_overviews([2], Resampling.nearest)
        else:
            driver = "COG" if route == "cog-rows" else "GTiff"
            rio_shutil.copy(ds, out, driver=driver, compress="deflate", tiled=True)
    assert "Read failed: the mask cannot be read" in str(raised.value)


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


@pytest.mark.parametrize(
    "mask, read",
    [(None, "pixels"), (MASK, "pixels"), (MASK, "mask")],
    ids=["pixels-of-an-unmasked-source", "pixels-of-a-masked-source", "mask"],
)
def test_an_interrupted_copy_written_in_rows_leaves_no_temporary_file(tmp_path, mask, read):
    """A copy written in rows, interrupted while it reads its source's pixels or mask for a later
    row of tiles, raises the interruption as it is, and leaves its destination as it was without
    the temporary file it was writing."""
    src, out = source(tmp_path / "src.tif", mask=mask), tmp_path / "out.tif"
    with rio.open(out, "w", driver="GTiff", width=10, height=10, count=1, dtype="uint8") as ds:
        ds.write(np.full((1, 10, 10), 7, np.uint8))
    before = out.read_bytes()

    class Interrupted(DatasetReader):
        def read(self, *args, **kwargs):
            window = kwargs.get("window")
            if read == "pixels" and window is not None and window.row_off > 0:
                raise KeyboardInterrupt
            return super().read(*args, **kwargs)

        def dataset_mask(self, *args, **kwargs):
            window = kwargs.get("window")
            if read == "mask" and window is not None and window.row_off > 0:
                raise KeyboardInterrupt
            return super().dataset_mask(*args, **kwargs)

    with Interrupted(src) as ds, pytest.raises(KeyboardInterrupt):
        rio_shutil.copy(ds, str(out), driver="COG", compress="deflate")
    assert out.read_bytes() == before
    assert sorted(p.name for p in tmp_path.glob("out.tif*")) == ["out.tif"]
