"""Pin irreversible 9/7 decoding independently of the platform's GDAL build."""

from pathlib import Path

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.io import MemoryFile

REFERENCE = Path(__file__).parent / "data" / "jp2_decode"
CASES = ("uint8_quality10", "uint8_irreversible", "int16_quality25", "uint16_quality25")


@pytest.mark.parametrize("case", CASES)
def test_irreversible_decode_matches_fixed_reference(case):
    """Require every sample to match the frozen Linux reference, on every platform.

    Args:
        case: Frozen payload and expected-array key, documented beside the fixtures.
    """
    with np.load(REFERENCE / "decoded.npz", allow_pickle=False) as arrays:
        expected = arrays[case]
    path = REFERENCE / f"{case}.jp2"
    with rio.open(path) as dataset:
        actual = dataset.read()
    assert actual.dtype == expected.dtype
    assert actual.shape == expected.shape
    np.testing.assert_array_equal(actual, expected)
    with MemoryFile(path.read_bytes(), ext=".jp2") as mem, mem.open() as dataset:
        actual = dataset.read()
    assert actual.dtype == expected.dtype
    assert actual.shape == expected.shape
    np.testing.assert_array_equal(actual, expected)
