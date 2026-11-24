"""The performance matrix counts sequential request rounds and windows as it documents."""

import sys
import argparse
import json
import subprocess
import threading
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from perf_matrix import WIN, rounds, windows
import perf_matrix as matrix


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


def sample(total=10.0):
    """Build a successful worker sample for report regressions.

    Args:
        total: Numeric checksum.

    Returns:
        A worker result with finite timing and byte counts.
    """
    return {"open_s": 1.0, "read_s": 2.0, "init_s": 0.5, "open_bytes": 10, "read_bytes": 20,
            "rss_mb": 4, "sum": total, "n": 4}


@pytest.mark.parametrize("failure", ["timeout", "later mismatch", "first mismatch"])
def test_incomplete_or_wrong_samples_are_never_ranked(failure):
    """Keep failures out of rankings even after a successful sample.

    Args:
        failure: Failure or mismatch position to exercise.
    """
    row = {"format": "geotiff", "fixture": "u8_none", "access": "window", "source": "local",
           "need_bytes": 30, "samples": 2, "terrio": [sample(), sample()], "gdal": [sample(), sample()]}
    if failure == "timeout":
        row["terrio"].pop()
        row["terrio_error"] = "timeout"
    else:
        row["terrio"][1 if failure == "later mismatch" else 0] = sample(99)
    result = matrix.summarize(row)
    assert "time_ratio" not in result
    report = matrix.report([result])
    assert "errors: 1" in report
    assert "codec   \n" in report
    assert "error:" in matrix.fmt_row(result)
    good = matrix.summarize({**row, "terrio": [sample(), sample()], "terrio_error": "still failed"})
    assert "time_ratio" not in good


def test_projected_attributes_are_validated_without_id():
    """Detect wrong numeric and categorical projected values, even with equal row counts."""
    pa = pytest.importorskip("pyarrow")
    original = matrix.result_check(pa.table({"value": [1.0, 2.0], "class": ["water", "forest"]}))
    wrong_value = matrix.result_check(pa.table({"value": [1.0, 9.0], "class": ["water", "forest"]}))
    wrong_class = matrix.result_check(pa.table({"value": [1.0, 2.0], "class": ["water", "water"]}))
    assert original["n"] == wrong_value["n"] == wrong_class["n"]
    assert not matrix.same_checks(original, wrong_value)
    assert not matrix.same_checks(original, wrong_class)


@pytest.mark.parametrize("platform", ["linux", "darwin"])
def test_http_worker_measures_before_validation_without_proc(monkeypatch, platform):
    """Exclude expensive validation from timing and RSS, without Linux byte counters.

    Args:
        monkeypatch: Scoped dependency replacements.
        platform: Platform whose RSS units are used.
    """
    clock = [0]

    def open_(path):
        """Simulate opening a dataset.

        Args:
            path: Input path.

        Returns:
            The opened path.
        """
        clock[0] += 1
        return path

    def read(_dataset):
        """Simulate a read.

        Args:
            _dataset: Open dataset.

        Returns:
            A result consumed only after measurement.
        """
        clock[0] += 2
        return 1

    def validate(_result):
        """Simulate expensive validation after the measurement.

        Args:
            _result: Read result.

        Returns:
            A valid summary.
        """
        clock[0] += 100
        return {"sum": 1.0, "n": 1}

    fake = SimpleNamespace(EXT=".tif", reader=lambda *_: (open_, [read], (open_, read)))
    monkeypatch.setitem(matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(matrix.time, "perf_counter", lambda: clock[0])
    monkeypatch.setattr(matrix, "_rchar", lambda: pytest.fail("HTTP must not read /proc"))
    monkeypatch.setattr(matrix.resource, "getrusage", lambda _: SimpleNamespace(ru_maxrss=clock[0] * (1024**2 if platform == "darwin" else 1024)))
    monkeypatch.setattr(matrix, "result_check", validate)
    monkeypatch.setattr(matrix.sys, "platform", platform)
    result = matrix.worker({"format": "fake", "tool": "terrio", "src": "http://localhost/file.tif", "access": "full", "meta": {}})
    assert (result["open_s"], result["read_s"], result["rss_mb"]) == (1, 2, 6)
    assert (result["open_bytes"], result["read_bytes"]) == (0, 0)


def test_threaded_reads_open_four_handles_and_propagate_errors(monkeypatch):
    """Preserve the reader error and avoid an unused fifth measured handle.

    Args:
        monkeypatch: Scoped dependency replacements.
    """
    opens = []

    def open_(path):
        """Record a dataset open.

        Args:
            path: Dataset path.

        Returns:
            The path.
        """
        opens.append(path)
        return path

    def fail(_dataset):
        """Raise an identifiable reader error.

        Args:
            _dataset: Open dataset.

        Raises:
            ValueError: Always, to exercise propagation.
        """
        raise ValueError("reader sentinel")

    fake = SimpleNamespace(EXT=".tif", reader=lambda *_: (open_, [fail] * 4, (open_, lambda _: None)))
    monkeypatch.setitem(matrix.FORMATS, "fake", fake)
    with pytest.raises(ValueError, match="reader sentinel"):
        matrix.worker({"format": "fake", "tool": "terrio", "src": "http://localhost/file.tif", "access": "threads4", "meta": {}})
    assert len(opens) == 5  # one warm-up and four measured handles


def test_accounting_waits_for_a_late_handler_before_clearing():
    """Keep the final request in its sample even if accounting finishes after the client."""
    finished = threading.Event()
    result = []

    def collect():
        """Collect the completed sample's request log."""
        result.extend(matrix.request_log(clear=True, timeout=2))
        finished.set()

    with matrix._lock:
        matrix._log.clear()
        matrix._active = 1
    thread = threading.Thread(target=collect)
    thread.start()
    try:
        assert not finished.wait(0.05)
        with matrix._lock:
            matrix._log.append((0, 1, 123, "/fixture"))
            matrix._active = 0
            matrix._lock.notify_all()
        assert finished.wait(2)
        assert result == [(0, 1, 123, "/fixture")]
        assert matrix.request_log() == []
    finally:
        with matrix._lock:
            matrix._active = 0
            matrix._lock.notify_all()
        thread.join(2)


def test_run_rejects_invalid_access_and_existing_results(tmp_path):
    """Reject unsafe run configuration before opening fixtures or replacing results.

    Args:
        tmp_path: Isolated results directory.
    """
    out = tmp_path / "results.jsonl"
    out.write_text("previous run\n")
    args = argparse.Namespace(format="geotiff", dir=tmp_path, access=["band"], samples=1, threads=1,
                              timeout=1, latency_ms=0, source=["http"], only=["u8_none"], quick=True, out=out)
    with pytest.raises(ValueError, match="unsupported geotiff access"):
        matrix.run(args)
    args.access = ["window"]
    with pytest.raises(FileExistsError):
        matrix.run(args)
    assert out.read_text() == "previous run\n"


def test_relative_local_paths_are_absolute_and_samples_remain_separate(tmp_path, monkeypatch):
    """Normalize local paths and keep alternating samples in a fresh output.

    Args:
        tmp_path: Isolated run directory.
        monkeypatch: Scoped dependency replacements.
    """
    if not Path("/proc/self/io").is_file():
        pytest.skip("local accounting requires Linux")
    monkeypatch.chdir(tmp_path)
    seen = []

    def measure(command, **_kwargs):
        """Capture the worker specification and return a successful sample.

        Args:
            command: Worker command.
            **_kwargs: Subprocess options.

        Returns:
            A successful process result.
        """
        spec = json.loads(command[-1])
        assert Path(spec["src"]).is_absolute()
        seen.append(spec["tool"])
        return subprocess.CompletedProcess(command, 0, json.dumps(sample()), "")

    monkeypatch.setattr(matrix.GeoTiff, "meta", lambda *_: {"size": 1024, "need_bytes": 30})
    monkeypatch.setattr(matrix.subprocess, "run", measure)
    args = argparse.Namespace(format="geotiff", dir=Path("fixtures"), access=["window"], samples=2, threads=1,
                              timeout=1, latency_ms=0, source=["local"], only=["u8_none"], quick=False, out=tmp_path / "results.jsonl")
    matrix.run(args)
    result = json.loads(args.out.read_text())
    assert seen == ["terrio", "gdal", "gdal", "terrio"]
    assert result["samples"] == 2
    assert result["time_ratio"] == 1


def test_geoparquet_byte_estimate_uses_covering_paths_and_missing_statistics(tmp_path):
    """Use declared covering paths, with a conservative estimate without statistics.

    Args:
        tmp_path: Isolated Parquet directory.
    """
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    cover = {key: ["geometry_bbox", key] for key in ("xmin", "ymin", "xmax", "ymax")}
    geo = {"primary_column": "geometry", "columns": {"geometry": {"covering": {"bbox": cover}}}}
    bbox = pa.array([dict(xmin=x, ymin=x, xmax=x + 1, ymax=x + 1) for x in (0, 1, 49_000, 50_000)])
    table = pa.table({"geometry": [b"a"] * 4, "geometry_bbox": bbox, "id": range(4)})
    table = table.replace_schema_metadata({b"geo": json.dumps(geo).encode()})
    for statistics in (True, False):
        path = tmp_path / f"stats-{statistics}.parquet"
        pq.write_table(table, path, row_group_size=2, write_statistics=statistics)
        md = pq.ParquetFile(path).metadata
        groups = [1] if statistics else [0, 1]
        expected = md.serialized_size + 8 + sum(md.row_group(g).column(i).total_compressed_size for g in groups for i in range(md.num_columns))
        assert matrix.GeoParquet.meta(path, "bbox")["need_bytes"] == expected


def test_zarr_fixture_is_published_only_after_all_chunks_are_written(tmp_path, monkeypatch):
    """Discard a failed staged store and never reuse an unmarked destination.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped dependency replacements.
    """
    zarr = pytest.importorskip("zarr")
    np = pytest.importorskip("numpy")
    monkeypatch.setattr(matrix.Zarr, "FIXTURES", {"tiny": (3, "float32", 1, 16, 8, None, "none")})
    create = zarr.create_array

    class BrokenArray:
        """An array whose fixture write fails."""

        def __setitem__(self, _key, _value):
            """Fail the fixture write.

            Args:
                _key: Destination slice.
                _value: Samples to write.

            Raises:
                OSError: Always, to simulate an interrupted write.
            """
            raise OSError("interrupted fixture")

    def interrupted(path, **kwargs):
        """Create a partial store that cannot finish.

        Args:
            path: Store path.
            **kwargs: Array creation options.

        Returns:
            A failing fixture array or a real warm-up array.
        """
        array = create(path, **kwargs)
        return array if "_warmup" in path else BrokenArray()

    monkeypatch.setattr(zarr, "create_array", interrupted)
    with pytest.raises(OSError, match="interrupted fixture"):
        matrix.Zarr.make(tmp_path)
    target = tmp_path / "tiny.zarr"
    assert not target.exists()
    assert not list(tmp_path.glob(".tiny-*"))
    monkeypatch.setattr(zarr, "create_array", create)
    matrix.Zarr.make(tmp_path)
    assert (target / ".terrio-matrix-complete").is_file()
    assert np.any(zarr.open_array(str(target))[:])
    matrix.Zarr.make(tmp_path)
    (target / ".terrio-matrix-complete").unlink()
    with pytest.raises(ValueError, match="unmarked or incomplete"):
        matrix.Zarr.make(tmp_path)
