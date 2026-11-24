"""The performance matrix counts rounds, windows and needed bytes, and ranks only valid cases."""

import json
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
    assert same["time_ratio"] == 2 and same["values"] == "checksum_match"
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


def test_geoparquet_without_statistics_counts_all_row_groups(tmp_path):
    """Count required columns conservatively when statistics are absent.

    Args:
        tmp_path: Isolated fixture directory.
    """
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    table = pa.table({"id": [5000, 5001], "geometry": [b"x", b"y"],
                      "bounds": [{"xmin": 0., "ymin": 0., "xmax": 1., "ymax": 1.}] * 2})
    geo = {"primary_column": "geometry", "columns": {"geometry": {"covering": {"bbox": {
        key: ["bounds", key] for key in ("xmin", "ymin", "xmax", "ymax")}}}}}
    table = table.replace_schema_metadata({b"geo": json.dumps(geo).encode()})
    path = tmp_path / "no-stats.parquet"
    pq.write_table(table, path, write_statistics=False, row_group_size=1)
    md = pq.ParquetFile(path).metadata
    expected = md.serialized_size + 8 + sum(md.row_group(g).column(c).total_compressed_size
                                          for g in range(md.num_row_groups) for c in range(md.num_columns))
    assert perf_matrix.GeoParquet.meta(path, "where")["need_bytes"] == expected
    assert perf_matrix.GeoParquet.meta(path, "bbox")["need_bytes"] == expected


def test_metadata_errors_are_recorded_for_each_case(tmp_path, monkeypatch):
    """Keep a metadata failure from aborting the remaining benchmark cases.

    Args:
        tmp_path: Isolated results directory.
        monkeypatch: Scoped dependency replacements.
    """
    def broken_meta(_path, _access):
        """Raise a fixture error.

        Args:
            _path: Fixture path.
            _access: Access pattern.

        Raises:
            ValueError: For the malformed fixture.
        """
        raise ValueError("missing geo metadata")

    fake = SimpleNamespace(FIXTURES={"a": None, "b": None}, ACCESS=["full"], EXT=".parquet",
                           meta=broken_meta, axes=lambda _: {"codec": "none", "dtype": "point", "layout": "arrow"})
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix, "machine", lambda: {"machine": {"on_ac": True}})
    monkeypatch.setattr(perf_matrix, "report", lambda _: "recorded")
    out = tmp_path / "results.jsonl"
    args = SimpleNamespace(format="fake", dir=tmp_path, latency_ms=0, threads=1, only=None,
                           quick=True, samples=1, base=None, out=out, source=["http"], allow_battery=False, access=None)
    perf_matrix.run(args)
    rows = [json.loads(line) for line in out.read_text().splitlines()][1:]
    assert [row["fixture"] for row in rows] == ["a", "b"]
    assert all(row["meta_error"] == "ValueError: missing geo metadata" for row in rows)
    assert all("time_ratio" not in row for row in rows)


@pytest.mark.parametrize("failure", ["empty", "json", "accounting"])
def test_bad_worker_samples_do_not_abort_remaining_cases(tmp_path, monkeypatch, failure):
    """Record malformed output and unsettled accounting as unranked case errors.

    Args:
        tmp_path: Isolated results directory.
        monkeypatch: Scoped dependency replacements.
        failure: Worker failure to simulate.
    """
    fake = SimpleNamespace(FIXTURES={"a": None, "b": None}, ACCESS=["full"], EXT=".parquet",
                           meta=lambda *_: {"need_bytes": 1},
                           axes=lambda _: {"codec": "none", "dtype": "point", "layout": "arrow"})
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix, "machine", lambda: {"machine": {"on_ac": True}})
    monkeypatch.setattr(perf_matrix, "report", lambda _: "recorded")
    monkeypatch.setattr(perf_matrix.subprocess, "run", lambda *_a, **_k: SimpleNamespace(
        returncode=0, stdout="" if failure == "empty" else "not JSON", stderr=""))

    def settled():
        """Return completed accounting or simulate a timeout.

        Returns:
            Empty request accounting.

        Raises:
            TimeoutError: When handlers have not settled.
        """
        if failure == "accounting":
            raise TimeoutError("unsettled")
        return []

    monkeypatch.setattr(perf_matrix, "_settled_log", settled)
    out = tmp_path / "results.jsonl"
    args = SimpleNamespace(format="fake", dir=tmp_path, latency_ms=0, threads=1, only=None,
                           quick=True, samples=1, base=None, out=out, source=["http"], allow_battery=False,
                           access=None, timeout=1)
    perf_matrix.run(args)
    rows = [json.loads(line) for line in out.read_text().splitlines()][1:]
    assert [row["fixture"] for row in rows] == ["a", "b"]
    assert all("terrio_error" in row and "gdal_error" in row and "time_ratio" not in row for row in rows)


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


def test_interrupted_geoparquet_source_is_not_published_or_reused(tmp_path, monkeypatch):
    """Publish a FlatGeobuf source only after all its features are written.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped dependency replacements.
    """
    ogr = pytest.importorskip("osgeo.ogr")
    gdal = pytest.importorskip("osgeo.gdal")
    if ogr.GetDriverByName("FlatGeobuf") is None:
        pytest.skip("FlatGeobuf driver is required")
    fixtures = {name: (kind, 2, 2, "ZSTD", "WKB", False, False) for name, kind in
                [("poly", "polygon"), ("point", "point"), ("line", "line")]}
    monkeypatch.setattr(perf_matrix.GeoParquet, "FIXTURES", fixtures)
    original = ogr.Feature
    count = [0]

    def interrupted(definition):
        """Interrupt source generation after its first feature.

        Args:
            definition: Layer feature definition.

        Returns:
            A real feature before the simulated interruption.

        Raises:
            RuntimeError: On the second feature.
        """
        count[0] += 1
        if count[0] == 2:
            raise RuntimeError("interrupted source")
        return original(definition)

    monkeypatch.setattr(ogr, "Feature", interrupted)
    with pytest.raises(RuntimeError, match="interrupted source"):
        perf_matrix.GeoParquet.make(tmp_path)
    assert not (tmp_path / "_src_polygon.fgb").exists()
    monkeypatch.setattr(ogr, "Feature", original)

    def stop_after_sources(*_args, **_kwargs):
        """Stop before Parquet fixture creation to keep the regression focused.

        Args:
            *_args: Translation arguments.
            **_kwargs: Translation options.

        Raises:
            RuntimeError: After source generation has completed.
        """
        raise RuntimeError("sources complete")

    monkeypatch.setattr(gdal, "VectorTranslate", stop_after_sources)
    with pytest.raises(RuntimeError, match="sources complete"):
        perf_matrix.GeoParquet.make(tmp_path)
    for kind in ("polygon", "point", "line"):
        source = ogr.Open(str(tmp_path / f"_src_{kind}.fgb"))
        assert source.GetLayer(0).GetFeatureCount() == 2


def test_a_base_build_gets_its_own_ratio_and_values_check():
    """Validate base timings only when all samples have matching checksums."""
    row = summarize(_row(terrio=[_sample(2)], gdal=[_sample(1)], base=[_sample(4)]))
    assert (row["time_ratio"], row["base_ratio"], row["values"]) == (2, 0.5, "checksum_match")
    assert "faster by 10% or more: 1" in report([row])
    # The base's values are checked too.
    assert summarize(_row(terrio=[_sample(2)], gdal=[_sample(1)], base=[_sample(4, n=5)]))["values"].startswith("DIFFER: base")


def test_a_case_the_base_failed_is_reported_as_fixed_and_the_reverse_as_broken():
    """Report successful comparisons separately from fixed and broken cases."""
    fixed = summarize(_row(terrio=[_sample(2)], gdal=[_sample(1)], base=[], base_error="timeout: over 120 s"))
    assert fixed["time_ratio"] == 2 and "base_ratio" not in fixed and fixed["values"] == "checksum_match"
    broken = summarize({**_row(terrio=[], gdal=[_sample(1)], base=[_sample(3)], terrio_error="exit 1"), "fixture": "f32_none"})
    assert "time_ratio" not in broken
    text = report([fixed, broken])
    assert "the base failed, this build succeeds: 1\n  u8_none" in text and "timeout: over 120 s" in text
    assert "this build fails, the base succeeded: 1\n  f32_none" in text


def test_run_rejects_abbreviated_workflow_reserved_options(tmp_path, monkeypatch):
    """Prevent abbreviated options from overriding the workflow's format.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped argument replacements.
    """
    monkeypatch.setattr(sys, "argv", ["perf_matrix.py", "run", "--dir", str(tmp_path),
                                      "--format", "geotiff", "--for", "zarr"])
    with pytest.raises(SystemExit) as exc:
        perf_matrix.main()
    assert exc.value.code == 2
    assert not (tmp_path / "zarr").exists()


def test_base_import_must_be_inside_the_requested_directory(tmp_path, monkeypatch):
    """Reject an installed package whose directory merely shares the base prefix.

    Args:
        tmp_path: Isolated build directory.
        monkeypatch: Scoped dependency replacements.
    """
    fake = SimpleNamespace(FIXTURES={}, ACCESS=[])
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix, "machine", lambda: {"machine": {"on_ac": True}})
    wrong_file = tmp_path / "build-other" / "terrio" / "__init__.py"
    monkeypatch.setattr(perf_matrix.subprocess, "run", lambda *_a, **_k: SimpleNamespace(
        stdout=json.dumps([str(wrong_file), "0.9.0"])))
    monkeypatch.setattr(perf_matrix, "report", lambda _: "empty")
    args = SimpleNamespace(format="fake", dir=tmp_path, latency_ms=0, threads=1, only=None,
                           quick=True, samples=1, base=tmp_path / "build", out=tmp_path / "results.jsonl",
                           source=["http"], allow_battery=False, access=None)
    with pytest.raises(SystemExit, match="terrio is imported from"):
        perf_matrix.run(args)
