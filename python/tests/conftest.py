"""Shared fixtures: the real-data directory and GDAL's Python bindings, when available."""

import os
import sys
from pathlib import Path

import pytest

import terrio

# Tests of the memory checks: their child processes set `RLIMIT_AS` and read `/proc`, or expect
# `usable_memory`, which reads `/proc`.
NEEDS_LINUX_MEMORY_LIMITS = {
    "test_grib.py::test_huge_grid_is_an_error_not_an_abort",
    "test_hardening.py::test_a_jpeg2000_too_large_to_allocate_raises",
    "test_hardening.py::test_a_jpeg_too_large_to_allocate_raises",
    "test_hardening.py::test_a_png_too_large_to_allocate_raises",
    "test_hardening.py::test_a_raster_too_large_to_allocate_raises",
    "test_hardening.py::test_a_tiff_chunk_too_large_to_allocate_raises",
    "test_hardening.py::test_a_tile_pyramid_too_wide_to_allocate_raises",
    "test_hardening.py::test_a_webp_chunk_too_large_to_allocate_raises",
    "test_hardening.py::test_a_zarr_chunk_of_1_gib_is_read_or_refused_never_aborted",
    "test_hardening.py::test_a_zarr_store_declaring_a_huge_shape_is_read_decimated_and_refused_whole",
    "test_hardening.py::test_a_zip_member_declaring_more_than_memory_is_streamed",
    "test_hardening.py::test_vsi_read_of_a_file_larger_than_memory_raises",
    "test_hardening.py::test_vsi_read_refuses_a_zip_member_declaring_more_than_memory",
    "test_vsi.py::test_a_remote_file_larger_than_the_memory_is_refused_after_its_first_response",
}
# Comparisons to the bit with GDAL, which hold where GDAL and terrio share a libm and floating-point
# contraction: Linux on x86-64. Elsewhere they differ by rounding, so they run without being required.
BIT_EXACT_ON_LINUX = {
    "test_alpha_reproject_parity.py::test_float32_bilinear_alpha_matches_rasterio",
    "test_cog_overview_scenarios.py::test_cog_overview_scenarios_are_gdals",
    "test_cog_overviews.py::test_masked_build_overviews_are_gdals",
    "test_cog_overviews.py::test_masked_overviews_are_gdals",
    "test_cog_overviews.py::test_overviews_are_gdals",
    "test_grib.py::test_encodings",
    "test_grib.py::test_grib_fixtures_read_as_gdal",
    "test_rasterio_call_parity.py::test_from_gcps_matches_gdal",
    "test_resampled_reads.py::test_resampled_reads_match_rasterio",
    "test_utm_parity.py::test_utm_transforms_are_gdal_s",
    "test_warp_parity.py::test_float32_kernels_match_rasterio_without_a_projection",
    "test_warp_parity.py::test_working_type_matches_rasterio",
    "test_warpedvrt_parity.py::test_float32_reads_match_rasterio",
    "test_warpedvrt_parity.py::test_nodata_widens_the_type_as_rasterio",
}


def pytest_collection_modifyitems(items):
    """Marks the tests that only hold on Linux, on other systems.

    Args:
        items: The collected tests.
    """
    if sys.platform == "linux":
        return
    for item in items:
        name = item.nodeid.split("[")[0].rsplit("/", 1)[-1]
        if name in NEEDS_LINUX_MEMORY_LIMITS:
            item.add_marker(pytest.mark.skip(reason="needs RLIMIT_AS and /proc"))
        elif name in BIT_EXACT_ON_LINUX:
            item.add_marker(
                pytest.mark.xfail(strict=False, reason="GDAL's rounding differs off Linux")
            )


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
