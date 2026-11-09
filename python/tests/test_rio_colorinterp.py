"""The color interpretation `terrio.rio` writes to a GeoTIFF or a COG (rasterio not needed).

A dataset's color interpretation, set with `colorinterp`, copied by `shutil.copy` or kept by an
update, reads back as it was set. Given no `PHOTOMETRIC`, which a COG never takes and an update
passes only for an RGB file, the GeoTIFF writer gives three Byte bands, or four compressed with
WebP, the red, green and blue interpretation unless told otherwise, as it does with
`PHOTOMETRIC=YCBCR`; `terrio.rio` sends it each band whose interpretation differs from that.

Such bands came back with red, green or blue on every band whose interpretation was not sent:
`undefined` x 3 as `undefined, green, blue`, `gray, undefined, undefined` as RGB.
"""

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio import shutil as rio_shutil
from terrio.rio.enums import ColorInterp
from terrio.rio.transform import from_origin

U, GRAY, ALPHA = ColorInterp.undefined, ColorInterp.gray, ColorInterp.alpha
R, G, B = ColorInterp.red, ColorInterp.green, ColorInterp.blue
T = from_origin(500000, 4100000, 30, 30)
MASK = np.full((48, 64), 255, np.uint8)
MASK[:8] = 0
# Interpretations of three bands that are not red, green and blue, and two that are colors.
NOT_RGB = [(U, U, U), (GRAY, U, U), (U, G, B), (GRAY, GRAY, GRAY)]
COLORS = [(R, G, B), (B, G, R)]


def _write(
    path, driver, colorinterp=None, count=3, dtype="uint8", mask=False, exact=True, **options
):
    """Writes a 64 x 48 raster, with `colorinterp` set unless it is None, and a mask if asked.
    Its pixels are checked when read back unless the compression loses them (`exact=False`).

    Returns:
        tuple: `dst.colorinterp` before the file is written.
    """
    pixels = (np.arange(count * 48 * 64) % 251).astype(dtype).reshape(count, 48, 64)
    with rio.open(
        path,
        "w",
        driver=driver,
        width=64,
        height=48,
        count=count,
        dtype=dtype,
        crs="EPSG:32611",
        transform=T,
        **options,
    ) as dst:
        if colorinterp is not None:
            dst.colorinterp = colorinterp
        dst.write(pixels)
        if mask:
            dst.write_mask(MASK)
        before = dst.colorinterp
    if exact:
        with rio.open(path) as src:
            assert np.array_equal(src.read(), pixels)
    return before


def _read_back(path):
    """The file's color interpretation, as read."""
    with rio.open(path) as src:
        return src.colorinterp


# Each way the writer writes a file: whole (`Raster.write`: a GeoTIFF that fits in memory, a COG
# with a mask) or in rows (a GeoTIFF above `TERRIO_RIO_IN_MEMORY_LIMIT`, a COG without a mask).
WAYS = pytest.mark.parametrize(
    "driver, mask, limit",
    [("GTiff", False, None), ("GTiff", False, "1000"), ("COG", True, None), ("COG", False, None)],
    ids=["gtiff-whole", "gtiff-rows", "cog-whole", "cog-rows"],
)


@WAYS
@pytest.mark.parametrize("colorinterp", NOT_RGB + COLORS, ids=lambda c: "-".join(i.name for i in c))
def test_three_byte_bands_read_back_the_color_interpretation_they_were_given(
    tmp_path, monkeypatch, driver, mask, limit, colorinterp
):
    """Three Byte bands keep the interpretation set on the dataset, in a GeoTIFF and in a COG."""
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    path = tmp_path / "a.tif"
    _write(path, driver, colorinterp, mask=mask)
    assert _read_back(path) == colorinterp


@WAYS
def test_three_byte_bands_are_red_green_and_blue_by_default(
    tmp_path, monkeypatch, driver, mask, limit
):
    """Without an interpretation set, three Byte bands are written red, green and blue, and the
    dataset says so before it is written: a COG writer said `gray, undefined, undefined`."""
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    path = tmp_path / "a.tif"
    assert _write(path, driver, mask=mask) == (R, G, B)
    assert _read_back(path) == (R, G, B)


@pytest.mark.parametrize(
    "colorinterp",
    [None, (GRAY, U, U, U), (U, U, U, U), (R, G, B, ALPHA), (R, G, B, U)],
    ids=lambda c: "default" if c is None else "-".join(i.name for i in c),
)
def test_a_cog_of_four_byte_bands_compressed_with_webp_reads_back_its_color_interpretation(
    tmp_path, colorinterp
):
    """Four Byte bands compressed with WebP are written red, green, blue and undefined by default,
    and keep any other interpretation set: `gray, undefined, undefined, undefined` came back as
    the default."""
    path = tmp_path / "a.tif"
    before = _write(path, "COG", colorinterp, count=4, compress="webp", webp_lossless=True)
    expected = (R, G, B, U) if colorinterp is None else colorinterp
    assert before == expected
    assert _read_back(path) == expected


@pytest.mark.parametrize("colorinterp", NOT_RGB + COLORS, ids=lambda c: "-".join(i.name for i in c))
def test_shutil_copy_to_a_cog_keeps_the_source_color_interpretation(tmp_path, colorinterp):
    """`shutil.copy` to a COG keeps its GeoTIFF source's interpretation of three Byte bands."""
    src = tmp_path / "src.tif"
    _write(src, "GTiff", colorinterp, photometric="minisblack")
    assert _read_back(src) == colorinterp
    rio_shutil.copy(str(src), str(tmp_path / "copy.tif"), driver="COG")
    assert _read_back(tmp_path / "copy.tif") == colorinterp


@pytest.mark.parametrize(
    "count, dtype, colorinterp, expected",
    [
        (4, "uint8", None, (GRAY, U, U, U)),
        (4, "uint8", (GRAY, U, U, U), (GRAY, U, U, U)),
        (4, "uint8", (R, G, B, ALPHA), (R, G, B, ALPHA)),
        (3, "uint16", None, (GRAY, U, U)),
        (3, "uint16", (U, U, U), (U, U, U)),
        (3, "uint16", (R, G, B), (R, G, B)),
        (5, "uint8", (R, G, B, U, U), (R, G, B, U, U)),
    ],
    ids=[
        "4-byte",
        "4-byte-gray",
        "4-byte-rgba",
        "3-uint16",
        "3-uint16-undefined",
        "3-uint16-rgb",
        "5-byte-rgb",
    ],
)
def test_other_cogs_keep_their_default_and_their_color_interpretation(
    tmp_path, count, dtype, colorinterp, expected
):
    """Other band counts and types (four Byte bands without WebP, three UInt16 bands, five Byte
    bands) are written gray, then undefined, by default, and keep the interpretation set."""
    path = tmp_path / "a.tif"
    assert _write(path, "COG", colorinterp, count=count, dtype=dtype) == expected
    assert _read_back(path) == expected


# Files of three Byte bands in each layout an update reads: a GeoTIFF RGB by default, a
# MINISBLACK one, one above `TERRIO_RIO_IN_MEMORY_LIMIT` (updated in rows) and a COG.
@pytest.mark.parametrize(
    "driver, options, limit",
    [
        ("GTiff", {}, None),
        ("GTiff", {"photometric": "minisblack"}, None),
        ("GTiff", {}, "1000"),
        ("COG", {}, None),
    ],
    ids=["gtiff", "gtiff-minisblack", "gtiff-rows", "cog"],
)
@pytest.mark.parametrize("colorinterp", NOT_RGB + COLORS, ids=lambda c: "-".join(i.name for i in c))
def test_an_update_keeps_the_color_interpretation_of_three_byte_bands(
    tmp_path, monkeypatch, driver, options, limit, colorinterp
):
    """An update (`r+`) writes three Byte bands back with the interpretation they had. It
    passes the writer `PHOTOMETRIC` only for an RGB file, and without it the writer makes
    them red, green and blue: `undefined` x 3 came back as `undefined, green, blue`, and
    `gray, undefined, undefined` as RGB."""
    path = tmp_path / "a.tif"
    _write(path, driver, colorinterp, **options)
    assert _read_back(path) == colorinterp
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    with rio.open(path, "r+") as dst:
        dst.update_tags(note="updated")
    with rio.open(path) as src:
        assert src.tags()["note"] == "updated"
        assert src.colorinterp == colorinterp


@pytest.mark.parametrize(
    "colorinterp",
    [None, (U, U, U), (GRAY, U, U), (B, G, R)],
    ids=lambda c: "default" if c is None else "-".join(i.name for i in c),
)
def test_jpeg_ycbcr_bands_read_back_their_color_interpretation(tmp_path, colorinterp):
    """A GeoTIFF of three Byte bands compressed with JPEG in YCbCr (`photometric="ycbcr"`) is
    red, green and blue by default, as the dataset says before it is written, and keeps any
    other interpretation set."""
    path = tmp_path / "a.tif"
    before = _write(path, "GTiff", colorinterp, exact=False, compress="jpeg", photometric="ycbcr")
    expected = (R, G, B) if colorinterp is None else colorinterp
    assert before == expected
    assert _read_back(path) == expected
