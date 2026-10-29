"""Shared fixtures: the real-data directory and GDAL's Python bindings, when available."""

import os
from pathlib import Path

import pytest

import terrio


@pytest.fixture(scope="session")
def data_dir():
    """The directory of real-world test files, from `TERRIO_TEST_DATA`.

    Returns:
        Path: The directory. The test is skipped when the variable is not set.
    """
    value = os.environ.get("TERRIO_TEST_DATA")
    if not value:
        pytest.skip("TERRIO_TEST_DATA is not set")
    path = Path(value)
    if not path.is_dir():
        pytest.skip(f"TERRIO_TEST_DATA={value} is not a directory")
    return path


@pytest.fixture(scope="session")
def gdal():
    """GDAL's Python bindings, with exceptions enabled.

    Returns:
        module: `osgeo.gdal`. The test is skipped when GDAL is not installed.
    """
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    return gdal


@pytest.fixture(scope="session")
def ogr():
    """GDAL's vector bindings, with exceptions enabled.

    Returns:
        module: `osgeo.ogr`. The test is skipped when GDAL is not installed.
    """
    ogr = pytest.importorskip("osgeo.ogr")
    ogr.UseExceptions()
    return ogr


@pytest.fixture(autouse=True)
def _fresh_http_cache():
    """Each test starts with an empty process-wide HTTP cache (GDAL's `VSICurlClearCache`).

    terrio keeps what remote files' requests returned, as GDAL's `/vsicurl/` cache does: a
    loopback server of a later test may get an earlier one's port, and its files must not be
    read from the earlier test's bytes.
    """
    terrio.clear_http_cache()
    yield
