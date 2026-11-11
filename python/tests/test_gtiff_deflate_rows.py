"""Deflate GeoTIFFs and COGs of rasters made of uniform blocks, written with `terrio.rio`.

In a strip or a tile, a row inside a block of a class repeats the row above it, so each search of
the Deflate encoder also examines the position one row back, and the rows are coded as copies of
the rows above. Each file stays under its bound and reads back unchanged, for Byte, UInt16 and
Float32 pixels and three interleaved or separate bands, tiled, striped and as a COG. High-entropy
pixels are stored: their file is the size of the pixels plus the blocks' headers.
"""

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.transform import from_origin

_yy, _xx = np.mgrid[0:1024, 0:1024]
# 10 classes, one per block of 37 x 53 pixels, from the block's number.
_block = (_yy // 53) * 28 + _xx // 37
CLASSES = ((_block * 1103515245 + 12345) // 65536 % 10).astype(np.uint8)
SQUARES = ((_xx // 64 + _yy // 64) % 10).astype(np.uint8)
RGB = np.stack([CLASSES * 20, CLASSES * 7 + 3, 255 - CLASSES * 11]).astype(np.uint8)
TILED = dict(driver="GTiff", tiled=True, blockxsize=512, blockysize=512)
STRIPED = dict(driver="GTiff")

# (pixels, write options, at most this many bytes)
BLOCKS = {
    "64 x 64 squares, tiled": (SQUARES, TILED, 5700),
    "Byte, tiled": (CLASSES, TILED, 6200),
    "Byte, striped": (CLASSES, STRIPED, 14300),
    "Byte, COG": (CLASSES, dict(driver="COG"), 9800),
    "UInt16, tiled": (CLASSES.astype(np.uint16) * 1000 + 17, TILED, 13200),
    "Float32, striped": ((CLASSES * 2.5 + 0.1).astype(np.float32), STRIPED, 94500),
    "RGB, pixel-interleaved, tiled": (RGB, TILED, 20600),
    "RGB, band-interleaved, striped": (RGB, dict(STRIPED, interleave="band"), 45500),
}


def write(path, pixels, **options):
    """Writes `pixels` as a Deflate file, checks that they read back, and returns its size."""
    bands = pixels if pixels.ndim == 3 else pixels[None]
    with rio.open(
        path,
        "w",
        width=1024,
        height=1024,
        count=bands.shape[0],
        dtype=pixels.dtype.name,
        crs="EPSG:32633",
        transform=from_origin(500000, 5800000, 10, 10),
        compress="DEFLATE",
        **options,
    ) as dst:
        dst.write(bands)
    with rio.open(path) as src:
        assert (src.read() == bands).all()
    return path.stat().st_size


@pytest.mark.parametrize("name", list(BLOCKS))
def test_a_raster_of_uniform_blocks_is_written_as_copies_of_its_rows(tmp_path, name):
    pixels, options, bound = BLOCKS[name]
    assert write(tmp_path / "blocks.tif", pixels, **options) <= bound


def test_noise_is_stored(tmp_path):
    # Four tiles of 512 x 512 bytes, each a zlib stream of five stored blocks (31 bytes more), and
    # at most 1 KiB of TIFF structure.
    noise = np.random.default_rng(7).integers(0, 256, (1024, 1024)).astype(np.uint8)
    assert write(tmp_path / "noise.tif", noise, **TILED) <= noise.size + 4 * 31 + 1024
