# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Regression tests for MemoryFile path containment, rio calc --name, and CancelledError export."""

from pathlib import Path

import numpy as np
import pytest

import terrio
from terrio import rio
from terrio.rio.cli import main as rio_main


def test_cancelled_error_is_reexported_at_top_level():
    """Verify CancelledError is re-exported on the top-level terrio module."""
    assert terrio.CancelledError is terrio.errors.CancelledError


def test_memoryfile_rejects_escaping_filenames(tmp_path):
    """Verify MemoryFile rejects absolute and parent-traversing filenames."""
    target = tmp_path / "outside.tif"
    for bad in [
        str(target),
        target,
        "../outside.tif",
        Path("../outside.tif"),
        "a/../../outside.tif",
        r"..\..\outside.tif",
    ]:
        with pytest.raises(ValueError, match="MemoryFile filename"):
            rio.MemoryFile(b"data", filename=bad)
    assert not target.exists()

    # Nested relative filename (str or PathLike) inside the memory file directory works.
    with rio.MemoryFile(b"hello", filename="nested/sample.bin") as mem:
        assert mem.read() == b"hello"
    with rio.MemoryFile(b"world", filename=Path("nested/pathlike.bin")) as mem:
        assert mem.read() == b"world"


def test_rio_calc_named_inputs_and_equals_in_path(tmp_path):
    """Verify rio calc resolves --name aliases and paths containing '='."""
    src = tmp_path / "a=1.tif"
    out = tmp_path / "out.tif"
    arr = np.array([[[10, 20], [30, 40]]], dtype="uint8")
    with rio.open(
        src,
        "w",
        driver="GTiff",
        width=2,
        height=2,
        count=1,
        dtype="uint8",
        transform=rio.transform.from_bounds(0, 0, 2, 2, 2, 2),
    ) as dst:
        dst.write(arr)

    code = rio_main(["calc", '(sieve (band "a" 1) 1)', "--name", f"a={src}", str(out)])
    assert code == 0
    with rio.open(out) as ds:
        np.testing.assert_array_equal(ds.read(), arr)

    out2 = tmp_path / "out2.tif"
    code2 = rio_main(["calc", "(+ a 1)", "--name", f"a={src}", str(out2)])
    assert code2 == 0
    with rio.open(out2) as ds:
        np.testing.assert_array_equal(ds.read(), arr + 1)

    # Multi-window sieve via --mem-limit 1 (chunk_size=1000 < width=1024) removes 1-px islands
    # in both windows while preserving a 2x2 region straddling the col=1000 window boundary.
    big_src = tmp_path / "big=1.tif"
    big_out = tmp_path / "big_out.tif"
    big = np.full((1, 16, 1024), 5, dtype="uint8")
    big[0, 4, 10] = 99
    big[0, 4, 1010] = 99
    big[0, 8:10, 999:1001] = 77
    expected = np.full((1, 16, 1024), 5, dtype="uint8")
    expected[0, 8:10, 999:1001] = 77
    with rio.open(
        big_src,
        "w",
        driver="GTiff",
        width=1024,
        height=16,
        count=1,
        dtype="uint8",
        transform=rio.transform.from_bounds(0, 0, 1024, 16, 1024, 16),
    ) as dst:
        dst.write(big)
    code3 = rio_main(
        [
            "calc",
            '(sieve (band "a" 1) 2)',
            "--name",
            f"a={big_src}",
            "--mem-limit",
            "1",
            str(big_out),
        ]
    )
    assert code3 == 0
    with rio.open(big_out) as ds:
        np.testing.assert_array_equal(ds.read(), expected)
