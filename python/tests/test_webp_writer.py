"""WEBP files written through `terrio.rio` as a tile server writes its tiles, against GDAL.

A tile server encodes its tiles with rasterio's writers, `MemoryFile().open(driver="WEBP", ...)`
with the creation options `quality` and `lossless`. terrio had no WEBP writer, so every WEBP tile
failed. Its writer is lossless only (pure Rust): it writes a lossless file whatever `LOSSLESS`,
`QUALITY` and `EXACT` say, so its pixels are compared with those of GDAL's `LOSSLESS=YES` file.
GDAL (rasterio 1.5.0, GDAL 3.12.1) is the oracle. Skipped without rasterio.
"""

import re

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.io import MemoryFile as GdalMemoryFile  # noqa: E402

import terrio.rio  # noqa: E402
from terrio.rio.errors import RasterioIOError  # noqa: E402
from terrio.rio.io import MemoryFile  # noqa: E402

pytestmark = pytest.mark.filterwarnings("ignore:Dataset has no geotransform")

# GDAL's messages (`WEBPDataset::CreateCopy`).
_BANDS = "WEBP driver doesn't support {} bands. Must be 3 (RGB) or 4 (RGBA) bands."
_TYPE = "WEBP driver doesn't support data type UInt16. Only eight bit byte bands supported."


def _encode(memfile, driver, data, **options):
    """The bytes of a tile written as a tile server writes it, through a memory file."""
    count, height, width = data.shape
    with memfile() as m:
        with m.open(
            driver=driver, width=width, height=height, count=count, dtype=data.dtype, **options
        ) as dst:
            dst.write(data)
        return bytes(m.read())


def _gdal_read(b):
    with GdalMemoryFile(b) as m, m.open() as ds:
        return ds.read()


def _arrays():
    """A smooth RGB tile, a noisy one, and an alpha band: opaque, transparent and half."""
    y, x = np.mgrid[0:64, 0:64]
    smooth = ((x + y) * 2).astype("uint8")
    rgb = np.stack([smooth, smooth[::-1], 255 - smooth])
    noise = np.random.default_rng(3).integers(0, 256, (3, 64, 64), dtype="uint8")
    alpha = np.where(x < 32, 255, 0).astype("uint8")
    alpha[5] = 128
    return rgb, noise, alpha


@pytest.mark.parametrize(
    "options",
    [
        {"quality": 75, "lossless": False},
        {},
        {"LOSSLESS": "YES", "QUALITY": "5"},
        {"lossless": True, "exact": 0},
        {"lossless": True, "exact": 1},
    ],
    ids=["tile-server-profile", "defaults", "lossless", "exact-0", "exact-1"],
)
def test_a_webp_tile_holds_the_rendered_pixels(options):
    """Every sample, the RGB under fully transparent pixels included; GDAL's `LOSSLESS=YES`
    pixels where they are visible and its alpha band (under fully transparent pixels libwebp
    writes values of its own choosing unless `EXACT=1`), and every pixel of GDAL's `EXACT=1`."""
    rgb, noise, alpha = _arrays()
    for data in (
        rgb,
        noise,
        np.concatenate([noise, alpha[None]]),
        np.concatenate([rgb, alpha[None]]),
    ):
        ours = _encode(MemoryFile, "WEBP", data, **options)
        assert ours[:4] == b"RIFF" and ours[8:16] == b"WEBPVP8L", ours[:16]
        decoded = _gdal_read(ours)
        np.testing.assert_array_equal(decoded, data)
        gdal_lossless = _gdal_read(_encode(GdalMemoryFile, "WEBP", data, lossless=True))
        visible = data[3] != 0 if data.shape[0] == 4 else np.ones(data.shape[1:], bool)
        np.testing.assert_array_equal(decoded[:, visible], gdal_lossless[:, visible])
        np.testing.assert_array_equal(decoded[3:], gdal_lossless[3:])
        gdal_exact = _gdal_read(_encode(GdalMemoryFile, "WEBP", data, lossless=True, exact=1))
        np.testing.assert_array_equal(decoded, gdal_exact)


def test_an_opaque_alpha_band_reads_back_as_three_bands_as_in_gdal():
    rgb, _, _ = _arrays()
    data = np.concatenate([rgb, np.full((1, 64, 64), 255, "uint8")])
    ours, gdals = (
        _gdal_read(_encode(m, "WEBP", data, lossless=True)) for m in (MemoryFile, GdalMemoryFile)
    )
    assert ours.shape == gdals.shape == (3, 64, 64)
    np.testing.assert_array_equal(ours, rgb)


def test_a_webp_written_by_terrio_reads_back_as_gdal_reads_it(tmp_path):
    _, noise, alpha = _arrays()
    data = np.concatenate([noise, alpha[None]])
    path = str(tmp_path / "tile.webp")
    with terrio.rio.open(
        path, "w", driver="WEBP", width=64, height=64, count=4, dtype="uint8"
    ) as dst:
        dst.write(data)
    with terrio.rio.open(path) as t, rasterio.open(path) as g:
        assert (
            (t.driver, t.count, t.dtypes)
            == (g.driver, g.count, g.dtypes)
            == ("WEBP", 4, ("uint8",) * 4)
        )
        assert t.colorinterp == tuple(g.colorinterp)
        assert t.tags(ns="IMAGE_STRUCTURE") == g.tags(ns="IMAGE_STRUCTURE")
        np.testing.assert_array_equal(t.read(), g.read())
    with open(path, "rb") as f, MemoryFile(f.read(), ext=".webp") as m, m.open() as t:
        assert t.driver == "WEBP"
        np.testing.assert_array_equal(t.read()[3], alpha)


@pytest.mark.parametrize(
    ("shape", "dtype", "options", "message"),
    [
        ((1, 4, 4), "uint8", {}, _BANDS.format(1)),
        ((2, 4, 4), "uint8", {}, _BANDS.format(2)),
        ((5, 4, 4), "uint8", {}, _BANDS.format(5)),
        ((3, 4, 4), "uint16", {}, _TYPE),
        ((3, 1, 16384), "uint8", {}, "WEBP maximum image dimensions are 16383 x 16383."),
        ((3, 4, 4), "uint8", {"quality": 101}, "QUALITY=101 is not a legal value."),
        ((3, 4, 4), "uint8", {"lossless": True, "exact": 2}, "EXACT=2 is not a legal value."),
    ],
)
def test_webp_refuses_what_gdal_refuses(shape, dtype, options, message):
    data = np.zeros(shape, dtype)
    with pytest.raises(Exception, match=re.escape(message)):
        _encode(GdalMemoryFile, "WEBP", data, **options)
    with pytest.raises(RasterioIOError, match=re.escape(message)):
        _encode(MemoryFile, "WEBP", data, **options)
