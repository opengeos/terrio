# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
# Fixture adapted from gmoralessanchez's reproduction in Terrio issue #973.
"""Lossy VP8 frames retain image quality without runs of damaged macroblocks.

Pillow/libwebp independently decodes standalone files; GDAL 3.12.1 via rasterio
decodes GeoTIFF tiles. The thresholds concern corruption, not encoder parity.
"""

import io
import time

import numpy as np
import pytest
from test_webp_writer import MemoryFile, _encode, _gdal_read

import terrio
from terrio.rio.warp import transform

Image = pytest.importorskip("PIL.Image")
pytestmark = pytest.mark.filterwarnings("ignore:Dataset has no geotransform")
OFFSET = 20037508.342789244
SPAN = 2 * OFFSET / 2**13
QUALITIES = [10, 25, 40, 50, 60, 70, 75, 80, 85, 90, 95]


def natural_rgb(size, seed):
    """Generate the seeded smooth fields, blobs, stripes and texture from issue #973.

    Args:
        size: Width and height of the square image.
        seed: NumPy random-generator seed.

    Returns:
        numpy.ndarray: Three Byte bands with values between 10 and 245.
    """
    rng = np.random.default_rng(seed)
    y, x = np.mgrid[0:size, 0:size].astype("float32") / size
    bands = []
    for band in range(3):
        field = np.zeros((size, size), "float32")
        for _ in range(12):
            fx, fy, phase = rng.uniform(0.5, 9), rng.uniform(0.5, 9), rng.uniform(0, 6.3)
            field += rng.uniform(5, 25) * np.sin(2 * np.pi * (fx * x + fy * y) + phase)
        for _ in range(25):
            cx, cy, radius = rng.uniform(0, 1), rng.uniform(0, 1), rng.uniform(0.01, 0.12)
            field += rng.uniform(-60, 60) * np.exp(
                -((x - cx) ** 2 + (y - cy) ** 2) / (2 * radius * radius)
            )
        field += 40 * ((x * 7 + y * 3 + band) % 1 > 0.85)
        field += rng.normal(0, 4, (size, size))
        bands.append(field)
    image = np.stack(bands)
    return ((image - image.min()) / (image.max() - image.min()) * 235 + 10).astype("uint8")


@pytest.fixture(scope="module")
def source():
    """Create the original seeded UTM source once for the module.

    Returns:
        terrio.Raster: RGB input on a 10 metre grid.
    """
    return terrio.Raster.from_array(
        natural_rgb(2048, 12), (500000, 10, 0, 5400000, 0, -10), crs="EPSG:32633"
    )


def tile(source, x, y):
    """Warp one zoom-13 Web Mercator tile with nearest-neighbour sampling.

    Args:
        source: Seeded RGB raster.
        x: Tile column.
        y: Tile row.

    Returns:
        numpy.ndarray: Three Byte bands of a 256 by 256 tile.
    """
    grid = (-OFFSET + x * SPAN, SPAN / 256, 0, OFFSET - y * SPAN, 0, -SPAN / 256)
    return np.asarray(
        terrio.warp(
            source,
            t_srs="EPSG:3857",
            dst_transform=grid,
            ts=(256, 256),
            resampling="near",
            ot="Byte",
        ).read()
    ).astype("uint8")


@pytest.fixture(scope="module")
def first_tile(source):
    """Return the reported tile that method zero damaged at qualities 70 and 75.

    Args:
        source: Seeded RGB input.

    Returns:
        numpy.ndarray: The z13/x4440/y2825 regression tile.
    """
    return tile(source, 4440, 2825)


@pytest.fixture(scope="module")
def covered_tiles(source):
    """Build all 30 fully covered tiles, skipping uncovered candidates before warping.

    Args:
        source: Seeded RGB input.

    Returns:
        list: Tile indices and RGB arrays, in the reproduction's stored order.
    """
    result = []
    for y in range(2818, 2834):
        for x in range(4436, 4446):
            xs = [
                -OFFSET + x * SPAN + (i + 0.5) * SPAN / 256
                for j in (0, 127, 255)
                for i in (0, 127, 255)
            ]
            ys = [
                OFFSET - y * SPAN - (j + 0.5) * SPAN / 256
                for j in (0, 127, 255)
                for i in (0, 127, 255)
            ]
            east, north = transform("EPSG:3857", "EPSG:32633", xs, ys)
            if not all(
                500000 <= e < 520480 and 5379520 < n <= 5400000
                for e, n in zip(east, north, strict=True)
            ):
                continue
            pixels = tile(source, x, y)
            assert not (pixels == 0).all(axis=0).any()
            result.append((x, y, pixels))
    assert len(result) == 30
    return result


def distortion(original, decoded):
    """Measure PSNR and damaged 16 by 16 macroblocks independently of the encoder.

    Args:
        original: Input RGB bands.
        decoded: RGB bands from an independent decoder.

    Returns:
        tuple: PSNR in decibels and the count of macroblocks with mean error above 20.
    """
    error = np.abs(decoded.astype("int16") - original.astype("int16"))
    mse = np.mean(error.astype(float) ** 2)
    psnr = float(10 * np.log10(255**2 / mse)) if mse else float("inf")
    blocks = error.mean(axis=0).reshape(16, 16, 16, 16).mean(axis=(1, 3))
    return psnr, int(np.count_nonzero(blocks > 20))


def pillow_read(encoded):
    """Decode standalone WebP through Pillow's independent libwebp backend.

    Args:
        encoded: WebP container bytes.

    Returns:
        numpy.ndarray: Three planar Byte bands.
    """
    with Image.open(io.BytesIO(encoded)) as image:
        return np.asarray(image.convert("RGB")).transpose(2, 0, 1)


@pytest.mark.parametrize("quality", [70, 75])
@pytest.mark.parametrize("driver", ["WEBP", "GTiff"])
def test_reported_lossy_tile_has_no_corrupt_macroblocks(
    first_tile, quality, driver, record_property
):
    """Reject the reported damaged frames through both public writer paths.

    Args:
        first_tile: Regression RGB tile.
        quality: The two qualities that corrupted this tile.
        driver: Standalone WebP or tiled GeoTIFF output.
        record_property: Pytest measurement collector.
    """
    options = (
        {"quality": quality}
        if driver == "WEBP"
        else {
            "compress": "WEBP",
            "webp_level": quality,
            "tiled": True,
            "blockxsize": 256,
            "blockysize": 256,
        }
    )
    started = time.perf_counter()
    encoded = _encode(MemoryFile, driver, first_tile, **options)
    record_property("encode_seconds", time.perf_counter() - started)
    decoded = pillow_read(encoded) if driver == "WEBP" else _gdal_read(encoded)
    psnr, damaged = distortion(first_tile, decoded)
    record_property("psnr_db", psnr)
    record_property("damaged_macroblocks", damaged)
    assert psnr >= 30, (driver, quality, psnr, damaged)
    assert damaged == 0, (driver, quality, psnr, damaged)


@pytest.mark.parametrize("quality", QUALITIES)
def test_all_covered_tiles_remain_sound_at_each_quality(covered_tiles, quality, record_property):
    """Check the reproduction's complete 30-tile, 11-quality standalone matrix.

    Args:
        covered_tiles: Covered source tiles.
        quality: Requested WebP quality.
        record_property: Pytest measurement collector.
    """
    failures = []
    minimum = float("inf")
    for x, y, pixels in covered_tiles:
        decoded = pillow_read(_encode(MemoryFile, "WEBP", pixels, quality=quality))
        psnr, damaged = distortion(pixels, decoded)
        minimum = min(minimum, psnr)
        if psnr < 30 or damaged:
            failures.append((x, y, psnr, damaged))
    record_property("minimum_psnr_db", minimum)
    assert not failures, (quality, failures)


def test_geotiff_default_webp_level_keeps_all_tiles_sound(covered_tiles):
    """Check GDAL read-back of all 30 GeoTIFF tiles at the default lossy level.

    Args:
        covered_tiles: Covered source tiles.
    """
    for x, y, pixels in covered_tiles:
        encoded = _encode(
            MemoryFile, "GTiff", pixels, compress="WEBP", tiled=True, blockxsize=256, blockysize=256
        )
        psnr, damaged = distortion(pixels, _gdal_read(encoded))
        assert psnr >= 30 and damaged == 0, (x, y, psnr, damaged)


def test_reference_tile_is_sound_with_libwebp(first_tile):
    """Confirm the input is encodable without damage by independent libwebp.

    Args:
        first_tile: Regression RGB tile.
    """
    output = io.BytesIO()
    Image.fromarray(first_tile.transpose(1, 2, 0)).save(output, format="WEBP", quality=75, method=4)
    psnr, damaged = distortion(first_tile, pillow_read(output.getvalue()))
    assert psnr >= 30 and damaged == 0
