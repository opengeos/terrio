"""JPEG files written through `terrio.rio` as a tile server writes its tiles, against GDAL.

A tile server encodes its tiles with rasterio's writers, `MemoryFile().open(driver="JPEG", ...)`
with the creation option `quality`. terrio's JPEG writer ignored `QUALITY` (it always wrote quality
90) and kept full-resolution chroma, so its tiles decoded to other pixels than GDAL's and were 2.4
to 3.1 times their size. GDAL's JPEG driver writes three bands as YCbCr with 4:2:0 chroma
subsampling and one as grayscale, at `QUALITY` (default 75). GDAL (rasterio 1.5.0, GDAL 3.12.1) is
the oracle. Skipped without rasterio.
"""

import math
import re

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.io import MemoryFile as GdalMemoryFile  # noqa: E402

from terrio.rio.errors import RasterioIOError  # noqa: E402
from terrio.rio.io import MemoryFile  # noqa: E402

pytestmark = pytest.mark.filterwarnings("ignore:Dataset has no geotransform")


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


def _psnr(a, b):
    mse = float(np.mean((a.astype("float64") - b.astype("float64")) ** 2))
    return math.inf if mse == 0 else 10 * math.log10(255**2 / mse)


def _segments(b):
    """The marker segments of a JPEG up to its first scan, `(marker, payload)`."""
    out, i = [], 2
    while i + 4 <= len(b) and b[i] == 0xFF:
        marker, length = b[i + 1], int.from_bytes(b[i + 2 : i + 4], "big")
        out.append((marker, b[i + 4 : i + 2 + length]))
        if marker == 0xDA:
            break
        i += 2 + length
    return out


def _jpeg_layout(b):
    """The first values of the luminance table and the components' sampling factors."""
    luma = None
    for marker, p in _segments(b):
        if marker == 0xDB:
            for j in range(0, len(p), 65):
                if p[j] & 0x0F == 0:
                    luma = list(p[j + 1 : j + 4])
    sof = next(p for m, p in _segments(b) if m == 0xC0)
    return luma, [sof[6 + 3 * c + 1] for c in range(sof[5])]


@pytest.mark.parametrize("quality", [None, 50, 75, 85, 90, 95])
def test_a_jpeg_tile_has_gdals_tables_and_chroma_subsampling(quality):
    rgb, _, _ = _arrays()
    options = {} if quality is None else {"quality": quality}
    for data in (rgb, rgb[:1]):
        ours, gdals = (_encode(m, "JPEG", data, **options) for m in (MemoryFile, GdalMemoryFile))
        # The same quantization (QUALITY, default 75) and the same sampling: Y at 2 x 2 and
        # Cb, Cr at 1 x 1 (4:2:0) for RGB, one component for gray.
        assert _jpeg_layout(ours) == _jpeg_layout(gdals)


@pytest.mark.parametrize("quality", [75, 90])
def test_a_jpeg_tile_is_close_to_gdals_in_size_and_pixels(quality):
    rgb, noise, _ = _arrays()
    for data in (rgb, noise, rgb[:1]):
        ours, gdals = (
            _encode(m, "JPEG", data, quality=quality) for m in (MemoryFile, GdalMemoryFile)
        )
        assert 0.8 <= len(ours) / len(gdals) <= 1.25, (len(ours), len(gdals))
        assert _psnr(_gdal_read(ours), _gdal_read(gdals)) >= 38


def test_jpeg_refuses_a_quality_gdal_refuses():
    rgb, _, _ = _arrays()
    for q in (0, 101):
        message = re.escape(f"QUALITY={q} is not a legal value in the range 1-100.")
        with pytest.raises(Exception, match=message):
            _encode(GdalMemoryFile, "JPEG", rgb, quality=q)
        with pytest.raises(RasterioIOError, match=message):
            _encode(MemoryFile, "JPEG", rgb, quality=q)
