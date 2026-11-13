"""Raster signatures select readers when a filename supplies no raster format."""

import numpy as np
import pytest
from test_remote_reads import Server

import terrio
import terrio.rio as rio
from terrio.rio import Env


@pytest.mark.parametrize("extension,driver", [("png", "PNG"), ("jpg", "JPEG"), ("tif", "GTiff")])
@pytest.mark.parametrize("remote", [False, True])
@pytest.mark.parametrize("fallback", ["NO", "YES"])
def test_extensionless_rasters_use_content(tmp_path, extension, driver, remote, fallback):
    """Read the same encoded pixels through whole and lazy public entry points.

    Args:
        tmp_path: Temporary image directory.
        extension: Original filename extension.
        driver: Expected raster driver.
        remote: Whether to serve the extensionless image over HTTP.
        fallback: Legacy fallback option, which must not affect native detection.
    """
    values = np.arange(3 * 8 * 8, dtype="uint8").reshape(3, 8, 8)
    original = tmp_path / f"image.{extension}"
    with rio.open(original, "w", driver=driver, width=8, height=8, count=3, dtype="uint8") as dst:
        dst.write(values)
    with rio.open(original) as src:
        expected = src.read()
    path = tmp_path / "image"
    path.write_bytes(original.read_bytes())
    server = Server(tmp_path) if remote else None
    location = server.url + "/image" if server else str(path)
    try:
        with Env(TERRIO_WBRASTER_FALLBACK=fallback):
            np.testing.assert_array_equal(terrio.open_raster(location).read(), expected)
            for options in ({}, {"driver": driver}):
                with rio.open(location, **options) as src:
                    np.testing.assert_array_equal(src.read(), expected)
            with pytest.raises(rio.errors.RasterioIOError, match="not allowed"):
                rio.open(location, driver="WEBP")
        with Env(TERRIO_ALLOWED_DRIVERS="WEBP"):
            with pytest.raises(terrio.errors.UnsupportedError, match="not allowed"):
                terrio.open_raster(location)
    finally:
        if server:
            server.close()


def test_unknown_content_has_no_unusable_fallback_hint(tmp_path):
    """Refuse unknown local content without promising a reader that cannot detect it.

    Args:
        tmp_path: Temporary file directory.
    """
    path = tmp_path / "unknown"
    path.write_bytes(b"not an image")
    with pytest.raises(terrio.errors.UnsupportedError) as error:
        terrio.open_raster(str(path))
    assert "TERRIO_WBRASTER_FALLBACK" not in str(error.value)


@pytest.mark.parametrize("extension,driver", [("png", "PNG"), ("jpg", "JPEG"), ("tif", "GTiff")])
@pytest.mark.parametrize("surface", ["whole", "generic"])
def test_extensionless_remote_images_measure_whole_reads(
    tmp_path, record_property, extension, driver, surface
):
    """Measure the whole-image download for direct and generic raster opens.

    Args:
        tmp_path: Temporary image directory.
        record_property: Record observed transfer sizes in the test report.
        extension: Image encoding to write before dropping its extension.
        driver: Expected native image driver.
        surface: Whole-raster or generic dataset entry point.
    """
    dimension = 1024 if extension == "tif" else 512
    values = np.random.default_rng(19).integers(0, 256, (3, dimension, dimension), dtype="uint8")
    original = tmp_path / f"image.{extension}"
    with rio.open(
        original, "w", driver=driver, width=dimension, height=dimension, count=3, dtype="uint8"
    ) as dst:
        dst.write(values)
    with rio.open(original) as src:
        expected = src.read()
    path = tmp_path / "image"
    path.write_bytes(original.read_bytes())
    server = Server(tmp_path)
    terrio.clear_http_cache()
    try:
        opener = terrio.open_raster if surface == "whole" else terrio.open
        result = opener(server.url + "/image")
        np.testing.assert_array_equal(result.read(), expected)
        transferred = server.bytes_sent()
        size = path.stat().st_size
        record_property("source_bytes", size)
        record_property("transferred_bytes", transferred)
        assert size <= transferred <= size + 65536
        assert all(row[2] == 206 for row in server.log if row[0] == "GET" and row[3])
        if extension == "tif":
            before = server.bytes_sent()
            with rio.open(server.url + "/image?lazy=1") as lazy:
                sample = lazy.read(window=rio.windows.Window(0, 0, 1, 1))
            np.testing.assert_array_equal(sample, expected[:, :1, :1])
            lazy_bytes = server.bytes_sent() - before
            record_property("lazy_window_bytes", lazy_bytes)
            assert 0 < lazy_bytes < size / 2
    finally:
        server.close()
