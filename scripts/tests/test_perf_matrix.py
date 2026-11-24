"""The performance matrix counts rounds, windows and needed bytes, and ranks only valid cases."""

import struct
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import perf_matrix
from perf_matrix import WIN, Zarr, report, rounds, summarize, windows


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


def _sample(t, s=10.0, n=4):
    return {"init_s": 0.0, "open_s": 0.0, "read_s": t, "open_bytes": 0, "read_bytes": 100, "rss_mb": 1.0, "sum": s, "n": n}


def _row(**tools):
    return {"format": "geotiff", "fixture": "u8_none", "access": "window", "source": "local", "need_bytes": 100,
            "codec": "NONE", "dtype": "uint8", "layout": "cog", **tools}


def test_summarize_ranks_only_complete_matching_samples():
    same = summarize(_row(terrio=[_sample(2), _sample(2)], gdal=[_sample(1), _sample(1)]))
    assert same["time_ratio"] == 2 and same["values"] == "same"
    # A later sample that differs is caught, not only the first.
    later = summarize(_row(terrio=[_sample(2), _sample(2, s=11)], gdal=[_sample(1), _sample(1)]))
    assert later["values"].startswith("DIFFER: terrio n 4 sum 11")
    # A tool that failed after one sample gets no ratio: its partial samples stay for diagnosis.
    partial = summarize(_row(terrio=[_sample(2)], gdal=[_sample(1), _sample(1)], terrio_error="timeout: over 120 s"))
    assert "time_ratio" not in partial and partial["terrio"]["read_s"] == 2


def test_report_lists_mismatches_and_failures_apart_and_warns_on_mixed_machines():
    ok = summarize(_row(terrio=[_sample(2)], gdal=[_sample(1)]))
    differ = summarize({**_row(terrio=[_sample(9, s=0)], gdal=[_sample(1)]), "fixture": "f32_none"})
    failed = summarize({**_row(terrio=[_sample(5)], gdal=[_sample(1)], terrio_error="exit 1"), "fixture": "f64_deflate_p3"})
    m = {"cpu": "A", "cpus": 4, "cpu_quota": "max", "mem_gb": 16, "os": "Linux", "on_ac": True, "load": 0.1, "versions": {}}
    text = report([{"machine": m}, {"machine": {**m, "cpu": "B"}}, ok, differ, failed])
    worst = text.split("## Worst cases")[1].split("## Geometric")[0]
    assert "u8_none" in worst and "f32_none" not in worst and "f64_deflate_p3" not in worst
    assert "values differ from GDAL (not ranked): 1\n  f32_none" in text
    assert "errors: 1\n  f64_deflate_p3" in text and "terrio exit 1" in text
    assert "different machines" in text


def test_zarr_needed_bytes_come_from_the_shard_index(tmp_path, monkeypatch):
    # One 4x4 shard of four 2x2 inner chunks, the last one empty; read whole.
    monkeypatch.setitem(Zarr.FIXTURES, "tiny", (3, "uint8", 1, 4, 2, 4, "zstd"))
    shard = tmp_path / "tiny.zarr" / "c" / "0" / "0"
    shard.parent.mkdir(parents=True)
    index = [0, 10, 10, 20, 30, 30, 2**64 - 1, 2**64 - 1]
    shard.write_bytes(b"\0" * 60 + struct.pack("<8Q", *index) + b"crc!")
    # The three chunks' bytes, and the index (with its checksum) once.
    assert Zarr.meta(tmp_path / "tiny.zarr", "full")["need_bytes"] == 10 + 20 + 30 + 16 * 4 + 4


def test_checksum_sums_without_overflow():
    import numpy as np

    a = np.full((3, 1000), 60000, dtype="uint16")
    assert perf_matrix.checksum(a) == (180_000_000.0, 3000)


@pytest.mark.parametrize("platform", ["linux", "darwin"])
def test_worker_records_time_and_platform_rss_before_validation(monkeypatch, platform):
    """Keep validation outside measurements and normalize both supported RSS units.

    Args:
        monkeypatch: Scoped dependency replacements.
        platform: Platform whose RSS units are returned.
    """
    clock = [0]

    def open_(path):
        """Advance the simulated open time.

        Args:
            path: Dataset path.

        Returns:
            The opened path.
        """
        clock[0] += 1
        return path

    def read(_dataset):
        """Advance the simulated read time.

        Args:
            _dataset: Open dataset.

        Returns:
            A result for subsequent validation.
        """
        clock[0] += 2
        return 1

    def check(_result):
        """Simulate expensive validation.

        Args:
            _result: Read result.

        Returns:
            Its sum and count.
        """
        clock[0] += 100
        return 1.0, 1

    fake = SimpleNamespace(EXT=".tif", reader=lambda *_: (open_, [read], check, (open_, read)))
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix.time, "perf_counter", lambda: clock[0])
    monkeypatch.setattr(perf_matrix, "_rchar", lambda local: pytest.fail("HTTP must not use local counters") if local else 0)
    monkeypatch.setattr(perf_matrix.resource, "getrusage", lambda _: SimpleNamespace(ru_maxrss=clock[0] * (1024**2 if platform == "darwin" else 1024)))
    monkeypatch.setattr(perf_matrix.sys, "platform", platform)
    result = perf_matrix.worker({"format": "fake", "tool": "terrio", "src": "http://localhost/file.tif", "access": "full", "meta": {}})
    assert (result["open_s"], result["read_s"], result["rss_mb"]) == (1, 2, 6)
    assert (result["open_bytes"], result["read_bytes"]) == (0, 0)


def test_unsettled_http_accounting_fails_instead_of_returning_a_partial_log(monkeypatch):
    """Refuse a sample whose request accounting has not settled before timeout.

    Args:
        monkeypatch: Scoped dependency replacements.
    """
    monkeypatch.setattr(perf_matrix, "_busy", 1)
    monkeypatch.setattr(perf_matrix._lock, "wait_for", lambda *_args, **_kwargs: False)
    with pytest.raises(TimeoutError, match="HTTP handlers did not finish accounting"):
        perf_matrix._settled_log()
