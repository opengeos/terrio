"""Empty RegionWarp regions retain band axes and never enter row kernels."""

import numpy as np
import pytest

from terrio import _terrio


@pytest.mark.parametrize(
    "method", ["nearest", "bilinear", "cubic", "lanczos", "average", "mode", "sum"]
)
@pytest.mark.parametrize("grid_invariant", [False, True])
@pytest.mark.parametrize("region", [(154, 200, 0, 50), (100, 234, 50, 0), (0, 0, 0, 0)])
@pytest.mark.parametrize("bands", [1, 2])
def test_empty_region_returns_empty_arrays(method, grid_invariant, region, bands):
    """Empty destinations return the configured dtype and band count under both policies.

    Args:
        method: Resampling kernel.
        grid_invariant: Whether sampling is independent of chunk layout.
        region: Empty destination region.
        bands: Source and output band count.
    """
    warp = _terrio.RegionWarp(
        (360, 60),
        (-180.0, 1.0, 0.0, 90.0, 0.0, -1.0),
        "EPSG:4326",
        "EPSG:3413",
        (-3850000.0, 25000.0, 0.0, 5850000.0, 0.0, -25000.0),
        method,
        0.125,
        "float32",
        [None] * bands,
        0.0,
        grid_invariant=grid_invariant,
    )
    assert warp.source_window(region) == (0, 0, 0, 0)
    # Explicitly supplied nonempty windows must not enter row kernels either.
    for window in [(0, 0, 0, 0), (0, 0, 360, 9)]:
        source = np.ones((bands, window[3], window[2]), dtype="float32")
        output, written = warp.warp(region, window, source, None, 0.0, False)
        assert output.shape == (bands, region[3], region[2])
        assert output.dtype == np.dtype("float32")
        assert written.shape == (region[3], region[2])
        assert written.dtype == np.dtype("bool")
        assert output.size == written.size == 0


@pytest.mark.parametrize("region", [(0, 0, 0, 3), (0, 0, 3, 0), (0, 0, 0, 0)])
@pytest.mark.parametrize("bands", [1, 2])
@pytest.mark.parametrize("grid_invariant", [False, True])
def test_empty_region_from_raster_skips_source_read(region, bands, grid_invariant):
    """The raster-backed path returns empty arrays without borrowing its source.

    Args:
        region: Empty destination region.
        bands: Source and output band count.
        grid_invariant: Sampling policy.
    """
    from test_concurrent_reads import own_reader_held

    import terrio

    transform = (-180.0, 90.0, 0.0, 90.0, 0.0, -45.0)
    raster = terrio.Raster.from_array(
        np.ones((bands, 4, 4), dtype="uint8"), transform, crs="EPSG:4326"
    )
    warp = _terrio.RegionWarp(
        (4, 4),
        transform,
        "EPSG:4326",
        "EPSG:4326",
        transform,
        "nearest",
        0.125,
        "float32",
        [None] * bands,
        0.0,
        grid_invariant=grid_invariant,
    )
    with own_reader_held(raster):
        output, written = warp._warp_from(
            raster, None, region, warp.source_window(region), list(range(1, bands + 1))
        )
    assert output.shape == (bands, region[3], region[2])
    assert output.dtype == np.dtype("float32")
    assert written.shape == (region[3], region[2])
    assert output.size == written.size == 0
