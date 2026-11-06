"""`terrio.rio.windows.get_data_window` against the per-pixel `nonzero()` bounds it replaced."""

import numpy as np
import pytest

from terrio.rio.windows import Window, get_data_window


def _nonzero_window(valid):
    if valid.ndim == 3:
        valid = np.any(valid, axis=0)
    ranges = [(int(nz.min()), int(nz.max()) + 1) if nz.size else (0, 0) for nz in valid.nonzero()]
    if valid.ndim == 1:
        ranges.append((0, 0))
    return Window.from_slices(ranges[0], ranges[1])


@pytest.mark.parametrize("shape", [(17,), (9, 13), (3, 9, 13), (1, 1), (0, 4)])
@pytest.mark.parametrize("rate", [0.0, 0.02, 0.5, 1.0])
def test_data_window_bounds_the_valid_pixels(shape, rate):
    rng = np.random.default_rng(len(shape) * 100 + int(rate * 50))
    arr = rng.integers(1, 9, size=shape).astype("float32")
    arr[rng.random(shape) >= rate] = -1.0
    assert get_data_window(arr, nodata=-1.0) == _nonzero_window(arr != -1.0)
    nan = np.where(arr == -1.0, np.nan, arr)
    assert get_data_window(nan, nodata=np.nan) == _nonzero_window(~np.isnan(nan))
    masked = np.ma.masked_equal(arr, -1.0)
    if np.ma.is_masked(masked):
        assert get_data_window(masked) == _nonzero_window(~np.ma.getmaskarray(masked))
