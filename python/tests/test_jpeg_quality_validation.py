"""JPEG and JPEG tile quality options use complete finite numbers in the documented range."""

import re
import sqlite3

import numpy as np
import pytest

import terrio
from terrio.rio.errors import RasterioIOError
from terrio.rio.io import MemoryFile


@pytest.fixture(scope="module")
def image():
    """Create a patterned RGB raster aligned with one Web Mercator tile.

    Returns:
        terrio.Raster: Byte raster with varying samples sensitive to JPEG quality.
    """
    y, x = np.mgrid[0:256, 0:256]
    values = np.stack([(x * 3 + y * 5 + b * 70) % 251 for b in range(3)]).astype("uint8")
    half_world = 20037508.342789244
    resolution = 2 * half_world / 256 / 2048
    transform = (
        -half_world + 1000 * 256 * resolution,
        resolution,
        0,
        half_world - 700 * 256 * resolution,
        0,
        -resolution,
    )
    return terrio.Raster.from_array(values, transform, crs="EPSG:3857")


@pytest.mark.parametrize(
    "driver,extension", [("JPEG", "jpg"), ("MBTiles", "mbtiles"), ("PMTiles", "pmtiles")]
)
@pytest.mark.parametrize(
    "quality",
    ["50abc", "75.5xyz", "0x50", "", " ", "nan", "inf", "-inf", "0", "-5", "500", "100.9"],
)
def test_invalid_jpeg_quality_preserves_existing_output(
    tmp_path, image, driver, extension, quality
):
    """Reject malformed and out-of-range options before replacing an existing output.

    Args:
        tmp_path: Temporary output directory.
        image: Patterned source raster.
        driver: Standalone or tile-archive writer.
        extension: Output extension.
        quality: Invalid quality option.
    """
    path = tmp_path / ("output." + extension)
    path.write_bytes(b"existing output")
    options = {"QUALITY": quality}
    if driver != "JPEG":
        options["TILE_FORMAT"] = "JPEG"
    message = re.escape(f"QUALITY={quality} is not a legal value in the range 1-100.")
    with pytest.raises(terrio.InvalidArgumentError, match=message):
        image.write(str(path), driver=driver, options=options)
    assert path.read_bytes() == b"existing output"


def encoded_quality(image, path, driver, quality):
    """Read JPEG output as encoded bytes, tile bytes or decoded PMTiles pixels.

    Args:
        image: Patterned source raster.
        path: Output filename.
        driver: Selected writer.
        quality: Quality option, or the default when None.

    Returns:
        bytes: Comparable encoded bytes or decoded pixels.
    """
    options = {} if quality is None else {"QUALITY": quality}
    if driver != "JPEG":
        options["TILE_FORMAT"] = "JPEG"
    image.write(str(path), driver=driver, options=options)
    if driver == "MBTiles":
        with sqlite3.connect(path) as db:
            tiles = db.execute(
                "SELECT tile_data FROM tiles ORDER BY zoom_level, tile_column, tile_row"
            ).fetchall()
        assert tiles
        assert all(tile[0].startswith(b"\xff\xd8") for tile in tiles)
        return b"".join(tile[0] for tile in tiles)
    if driver == "PMTiles":
        return np.asarray(terrio.open_raster(str(path))).tobytes()
    return path.read_bytes()


@pytest.mark.parametrize(
    "driver,extension", [("JPEG", "jpg"), ("MBTiles", "mbtiles"), ("PMTiles", "pmtiles")]
)
@pytest.mark.parametrize(
    "quality,canonical",
    [
        (None, "75"),
        ("1", "1"),
        ("100", "100"),
        ("80.0", "80"),
        (" 75 ", "75"),
        ("1e2", "100"),
        ("7.5e1", "75"),
        ("80.9", "80"),
    ],
)
def test_complete_jpeg_quality_matches_canonical_encoding(
    tmp_path, image, driver, extension, quality, canonical
):
    """Keep valid encodings, interpreting exponents completely and truncating fractions.

    Args:
        tmp_path: Temporary output directory.
        image: Patterned source raster.
        driver: Standalone or tile-archive writer.
        extension: Output extension.
        quality: Numeric option to validate.
        canonical: Equivalent integer JPEG quality.
    """
    path = tmp_path / ("output." + extension)
    actual = encoded_quality(image, path, driver, quality)
    expected = encoded_quality(image, path, driver, canonical)
    assert actual == expected


@pytest.mark.parametrize("quality", ["50abc", "nan", "0", "500"])
def test_rio_memory_jpeg_rejects_invalid_quality(image, quality):
    """Propagate complete-number validation through the rasterio-compatible memory writer.

    Args:
        image: Patterned source raster.
        quality: Invalid quality option.
    """
    message = re.escape(f"QUALITY={quality} is not a legal value in the range 1-100.")
    with pytest.raises(RasterioIOError, match=message), MemoryFile() as memory:
        with memory.open(
            driver="JPEG", width=256, height=256, count=3, dtype="uint8", QUALITY=quality
        ) as target:
            target.write(np.asarray(image))
