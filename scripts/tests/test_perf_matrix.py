"""The performance matrix counts sequential request rounds and windows as it documents."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from perf_matrix import WIN, rounds, windows


def test_rounds_counts_requests_that_waited_for_the_previous_ones():
    # Two overlapping requests, then one after both ended, then two more in parallel.
    log = [(0.0, 1.0, 0, ""), (0.5, 1.2, 0, ""), (1.3, 2.0, 0, ""), (2.1, 3.0, 0, ""), (2.2, 2.5, 0, "")]
    assert rounds(log) == 3
    assert rounds([]) == 0
    # A request that starts while a long one runs belongs to its round, even after a short one ended.
    assert rounds([(0.0, 5.0, 0, ""), (0.1, 0.2, 0, ""), (1.0, 2.0, 0, "")]) == 1


def test_windows_stay_inside_the_raster_off_the_chunk_grid():
    for size in (2048, 4096):
        for n in (1, 16):
            ws = windows(size, n)
            assert len(ws) == n
            assert all(0 <= x and 0 <= y and x + w <= size and y + h <= size and w == h == WIN for x, y, w, h in ws)
            assert all(x % 256 and y % 256 for x, y, _, _ in ws)
        assert len(set(windows(size, 16))) == 16
