"""The performance matrix counts rounds, windows and needed bytes, and ranks only valid cases."""

import json
import os
import struct
import subprocess
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


def test_equal_sums_with_different_bytes_are_not_exact():
    """Rank a case as `exact` only when every sample's bytes hash as GDAL's."""
    exact = summarize(_row(terrio=[{**_sample(2), "digest": "a"}], gdal=[{**_sample(1), "digest": "a"}]))
    nan = summarize(_row(terrio=[{**_sample(2, s=float("nan")), "digest": "a"}], gdal=[{**_sample(1, s=float("nan")), "digest": "a"}]))
    assert nan["values"] == "exact"  # equal bytes holding a NaN sum to NaN, which no sum check accepts
    assert exact["values"] == "exact" and "digest" not in exact["terrio"]
    # Two compensating errors keep the sum and count: the bytes still differ.
    swapped = summarize(_row(terrio=[{**_sample(2), "digest": "b"}], gdal=[{**_sample(1), "digest": "a"}]))
    assert swapped["values"] == "checksum_match"
    text = report([exact, swapped])
    assert "values not byte-identical to GDAL's (sum within 1e-6 and count equal; ranked): 1\n  u8_none" in text
    # A later sample whose bytes differ is caught too.
    later = _row(terrio=[{**_sample(2), "digest": "a"}, {**_sample(2), "digest": "b"}], gdal=[{**_sample(1), "digest": "a"}])
    assert summarize(later)["values"] == "checksum_match"


def test_digest_compares_values_not_their_layout():
    """Hash the values read and their shape: not a band's leading 1, a table's chunks, column order or integer width."""
    import numpy as np

    a = np.arange(12, dtype="float32").reshape(3, 4)
    assert perf_matrix.digest([a]) == perf_matrix.digest([a[None]]) == perf_matrix.digest([np.asfortranarray(a)])
    assert perf_matrix.digest([a]) != perf_matrix.digest([a.astype("float64")])
    assert perf_matrix.digest([a]) != perf_matrix.digest([a[::-1]])
    assert perf_matrix.digest([a]) != perf_matrix.digest([a.reshape(a.shape[::-1])])  # same bytes, other shape
    pa = pytest.importorskip("pyarrow")
    xy = pa.struct([pa.field("x", pa.float64(), False), pa.field("y", pa.float64(), False)])
    rings = pa.array([[[{"x": 1.0, "y": 2.0}]], [[{"x": 3.0, "y": 4.0}, {"x": 5.0, "y": 6.0}]]], pa.list_(pa.list_(xy)))
    t = pa.table({"id": pa.array([1, 2], pa.int32()), "class": ["a", "bb"], "geometry": rings, "wkb": [b"x", b"yy"]})
    padded = pa.concat_tables([t.slice(0, 1), t]).slice(1)  # sliced, in two chunks
    wide = t.set_column(0, "id", t.column("id").cast(pa.int64())).select(["wkb", "geometry", "class", "id"])
    assert perf_matrix.digest([t]) == perf_matrix.digest([padded]) == perf_matrix.digest([(None, wide)])
    assert perf_matrix.digest([t]) != perf_matrix.digest([t.set_column(0, "id", pa.array([2, 1], pa.int32()))])
    assert perf_matrix.digest([t]) != perf_matrix.digest([t.rename_columns(["ie", "class", "geometry", "wkb"])])
    big = pa.table({"id": pa.array([2**63, 1], pa.uint64())})  # past int64: hashed, not a failed sample
    assert perf_matrix.digest([big]) != perf_matrix.digest([pa.table({"id": pa.array([1, 1], pa.uint64())})])


_MACHINE = {"cpu": "A", "cpus": 4, "cpu_quota": "max", "mem_gb": 16, "os": "Linux", "on_ac": True, "load": 0.1, "versions": {}}


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


def test_geopackage_pages_come_from_the_table_b_tree_and_the_r_tree(tmp_path):
    """Find every table page from the interior pages, and an R-tree's rows as SQLite does.

    Args:
        tmp_path: Isolated fixture directory.
    """
    import sqlite3

    path = tmp_path / "t.gpkg"
    con = sqlite3.connect(path)
    con.execute("PRAGMA page_size = 1024")
    con.execute("CREATE TABLE data (fid INTEGER PRIMARY KEY, geom BLOB)")
    con.executemany("INSERT INTO data VALUES (?, ?)", [(i, b"x" * 200) for i in range(1, 3001)])
    con.commit()
    pages = con.execute("PRAGMA page_count").fetchone()[0]
    with open(path, "rb") as f:
        leaves = perf_matrix.GeoPackage.leaves(f, 1024, 2)
    interior = {p for _, _, up in leaves for p in up}
    # Two levels of interior pages; every page but the schema's belongs to the table.
    assert max(len(up) for _, _, up in leaves) == 2
    assert len(leaves) + len(interior) == pages - 1
    assert [hi for hi, _, _ in leaves] == sorted(hi for hi, _, _ in leaves) and leaves[-1][0] == float("inf")
    try:
        con.execute("CREATE VIRTUAL TABLE rtree_data_geom USING rtree(id, minx, maxx, miny, maxy)")
    except sqlite3.OperationalError:
        pytest.skip("SQLite without the R-tree module")
    con.executemany("INSERT INTO rtree_data_geom VALUES (?, ?, ?, ?, ?)",
                    [(i, i % 100, i % 100 + 1.5, i // 100, i // 100 + 1.5) for i in range(1, 3001)])
    con.commit()
    boxes = [(10, 3, 20, 7), (95, 25, 99, 28)]
    ids, nodes = perf_matrix.GeoPackage.rtree(con, "rtree_data_geom", boxes)
    expected = {i for b in boxes for (i,) in con.execute(
        "SELECT id FROM rtree_data_geom WHERE minx <= ? AND maxx >= ? AND miny <= ? AND maxy >= ?", (b[2], b[0], b[3], b[1]))}
    assert ids == expected and len(ids) > 50
    total = con.execute("SELECT sum(length(data)) FROM rtree_data_geom_node").fetchone()[0]
    assert 0 < nodes < total


def test_geopackage_needed_bytes_read_a_fixture_under_a_directory_with_a_hash(tmp_path):
    """Open a fixture by its path, even when a directory's name holds a URI fragment's `#`.

    Args:
        tmp_path: Isolated fixture directory.
    """
    ogr = pytest.importorskip("osgeo.ogr")
    path = tmp_path / "matrix#1" / "t.gpkg"
    path.parent.mkdir()
    ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(path))
    lyr = ds.CreateLayer(perf_matrix.GeoPackage.LAYER, None, ogr.wkbPoint)
    for i in range(3):
        f = ogr.Feature(lyr.GetLayerDefn())
        f.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({i} {i})"))
        lyr.CreateFeature(f)
    lyr = ds = None
    need = perf_matrix.GeoPackage.meta(path, "full")["need_bytes"]
    assert 0 < need <= path.stat().st_size


def test_http_ranges_larger_than_a_piece_are_served_whole_and_counted(tmp_path, monkeypatch):
    """Serve a range across several 1 MiB pieces, and count its bytes once.

    Args:
        tmp_path: Isolated served directory.
        monkeypatch: Scoped handler latency.
    """
    import threading
    import urllib.request
    from http.server import ThreadingHTTPServer

    data = bytes(range(256)) * (3 * 4096 + 7)
    (tmp_path / "f.bin").write_bytes(data)
    monkeypatch.setattr(perf_matrix._Handler, "latency", 0)
    server = ThreadingHTTPServer(("127.0.0.1", 0), lambda *a, **k: perf_matrix._Handler(*a, directory=str(tmp_path), **k))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with perf_matrix._lock:
            perf_matrix._log.clear()
        request = urllib.request.Request(f"http://127.0.0.1:{server.server_address[1]}/f.bin", headers={"Range": "bytes=100-"})
        with urllib.request.urlopen(request) as answer:
            assert answer.read() == data[100:]
        assert [x[2] for x in perf_matrix._settled_log()] == [len(data) - 100]
    finally:
        server.shutdown()
        server.server_close()


def test_flatgeobuf_needed_bytes_come_from_the_spatial_index(tmp_path, monkeypatch):
    """Count the header, the index nodes visited and the features found by a bbox.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped dependency replacements.
    """
    ogr = pytest.importorskip("osgeo.ogr")
    if ogr.GetDriverByName("FlatGeobuf") is None:
        pytest.skip("FlatGeobuf driver is required")
    ogr.UseExceptions()
    # Three features of one size; only the first is in the single bbox (40500..50500 each way).
    points = [(45000, 45000), (1000, 1000), (99000, 99000)]
    for index in (True, False):
        name = "tiny_index" if index else "tiny_noindex"
        monkeypatch.setitem(perf_matrix.FlatGeobuf.FIXTURES, name, ("point", 3, index))
        ds = ogr.GetDriverByName("FlatGeobuf").CreateDataSource(str(tmp_path / f"{name}.fgb"))
        lyr = ds.CreateLayer("data", None, ogr.wkbPoint, [f"SPATIAL_INDEX={'YES' if index else 'NO'}"])
        lyr.CreateField(ogr.FieldDefn("id", ogr.OFTInteger64))
        for i, (x, y) in enumerate(points):
            f = ogr.Feature(lyr.GetLayerDefn())
            f["id"] = i
            f.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({x} {y})"))
            lyr.CreateFeature(f)
        lyr = ds = None
    size = (tmp_path / "tiny_index.fgb").stat().st_size
    header = 12 + int.from_bytes((tmp_path / "tiny_index.fgb").read_bytes()[8:12], "little")
    index_bytes = 4 * 40  # a root and three leaves, all visited
    feature = (size - header - index_bytes) // 3
    meta = perf_matrix.FlatGeobuf.meta
    assert meta(tmp_path / "tiny_index.fgb", "bbox")["need_bytes"] == header + index_bytes + feature
    assert meta(tmp_path / "tiny_index.fgb", "full")["need_bytes"] == size - index_bytes
    # Without an index, every feature is read to find the bbox's.
    unindexed = (tmp_path / "tiny_noindex.fgb").stat().st_size
    assert meta(tmp_path / "tiny_noindex.fgb", "bbox")["need_bytes"] == unindexed == size - index_bytes


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


@pytest.mark.parametrize("platform", ["linux", "darwin", "proc"])
def test_worker_records_time_and_platform_rss_before_validation(monkeypatch, tmp_path, platform):
    """Keep validation outside measurements, prefer `VmHWM` and normalize both `ru_maxrss` units.

    Args:
        monkeypatch: Scoped dependency replacements.
        tmp_path: Where the /proc status file is, or is absent.
        platform: Platform whose `ru_maxrss` units are returned, or `proc`: Linux's `VmHWM`.
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

    def reader(_tool, _access, _meta):
        """Include backend construction in initialization time.

        Args:
            _tool: Reader backend.
            _access: Read pattern.
            _meta: Fixture metadata.

        Returns:
            Reader operations and validation.
        """
        clock[0] += 4
        return open_, [read], check, (open_, read)

    fake = SimpleNamespace(EXT=".tif", reader=reader)
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix.time, "perf_counter", lambda: clock[0])
    monkeypatch.setattr(perf_matrix, "_rchar", lambda local: pytest.fail("HTTP must not use local counters") if local else 0)
    monkeypatch.setattr(perf_matrix.resource, "getrusage", lambda _: SimpleNamespace(ru_maxrss=clock[0] * (1024**2 if platform == "darwin" else 1024)))
    monkeypatch.setattr(perf_matrix.sys, "platform", platform)
    status = tmp_path / "status"  # absent: the `ru_maxrss` fallback
    if platform == "proc":
        status.write_text("VmRSS:\t    2048 kB\nVmHWM:\t   30720 kB\n")  # not `ru_maxrss`'s 10 MiB
    monkeypatch.setattr(perf_matrix, "PROC_STATUS", status)
    result = perf_matrix.worker({"format": "fake", "tool": "terrio", "src": "http://localhost/file.tif", "access": "full", "meta": {}})
    assert (result["init_s"], result["open_s"], result["read_s"], result["rss_mb"]) == (7, 1, 2, 30 if platform == "proc" else 10)
    assert (result["open_bytes"], result["read_bytes"]) == (0, 0)


@pytest.mark.skipif(not perf_matrix.PROC_STATUS.exists(), reason="VmHWM needs Linux's /proc")
def test_peak_rss_is_the_workers_own_not_the_runners():
    """A runner that allocated 400 MiB launches a worker as `run` does: the worker's `ru_maxrss`
    carries the runner's peak, its `peak_rss_mb` does not."""
    worker = "import resource, perf_matrix; print(perf_matrix.peak_rss_mb(), resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024)"
    runner = (
        "import subprocess, sys; held = b'x' * (400 << 20); "
        f"print(subprocess.run([sys.executable, '-c', {worker!r}], capture_output=True, text=True, check=True).stdout)"
    )
    env = {**os.environ, "PYTHONPATH": os.pathsep.join(filter(None, [str(Path(perf_matrix.__file__).parent), os.environ.get("PYTHONPATH")]))}
    out = subprocess.run([sys.executable, "-c", runner], env=env, capture_output=True, text=True, check=True).stdout
    peak, maxrss = map(float, out.split())
    assert maxrss >= 400  # the runner's peak, which `ru_maxrss` reported as the worker's
    assert peak < 100


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
    fake = SimpleNamespace(FIXTURES={"fixture": None}, ACCESS=[])
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


@pytest.mark.parametrize("directory", [False, True])
@pytest.mark.parametrize("interrupted", [False, True])
def test_fixture_publishers_preserve_a_concurrent_completed_winner(tmp_path, directory, interrupted):
    """Keep one writer's staging and completed fixture independent of another.

    Args:
        tmp_path: Isolated cache directory.
        directory: Whether the fixture is a populated directory.
        interrupted: Whether the first writer fails after the other publishes.
    """
    path = tmp_path / ("fixture.zarr" if directory else "fixture.tif")

    def write(staged, value):
        """Write a tiny file or directory fixture.

        Args:
            staged: Writer-specific staging path.
            value: Fixture contents.
        """
        assert staged.suffix == path.suffix
        if directory:
            staged.mkdir()
            staged = staged / "data"
        staged.write_text(value)

    def first(staged):
        """Overlap a completed publisher with this writer's staging.

        Args:
            staged: First writer's staging path.

        Raises:
            RuntimeError: When interruption is requested.
        """
        write(staged, "first")
        perf_matrix.publish(path, lambda other: write(other, "winner"))
        own = staged / "data" if directory else staged
        assert own.read_text() == "first"
        if interrupted:
            raise RuntimeError("interrupted publisher")

    if interrupted:
        with pytest.raises(RuntimeError, match="interrupted publisher"):
            perf_matrix.publish(path, first)
    else:
        perf_matrix.publish(path, first)
    assert (path / "data" if directory else path).read_text() == "winner"
    assert list(tmp_path.iterdir()) == [path]


def test_a_mismatched_or_unvalidated_base_is_not_reported_as_successful():
    """Require independent GDAL agreement before calling a base build successful."""
    for gdal, extra in [([_sample(1)], {}), ([], {"gdal_error": "failed"})]:
        row = summarize(_row(terrio=[], gdal=gdal, base=[_sample(3, n=5)],
                             terrio_error="failed", **extra))
        assert row.get("base_values_match") is not True
        assert "this build fails, the base succeeded: 0" in report([row])


def test_quick_runs_rotate_tools_between_cases_and_close_the_server(tmp_path, monkeypatch):
    """Avoid fixed first-tool bias and release the HTTP listener on return.

    Args:
        tmp_path: Isolated results and base directory.
        monkeypatch: Scoped dependency replacements.
    """
    fake = SimpleNamespace(FIXTURES={"a": None, "b": None}, ACCESS=["full"], EXT=".tif",
                           meta=lambda *_: {"need_bytes": 1},
                           axes=lambda _: {"codec": "none", "dtype": "uint8", "layout": "cog"})
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix, "machine", lambda: {"machine": {"on_ac": True}})
    monkeypatch.setattr(perf_matrix, "report", lambda _: "recorded")
    monkeypatch.setattr(perf_matrix, "_settled_log", lambda: [])
    calls, servers = [], []
    server_type = perf_matrix.ThreadingHTTPServer

    def server(*args, **kwargs):
        """Record the actual HTTP listener created for the run.

        Args:
            *args: Server arguments.
            **kwargs: Server options.

        Returns:
            The real server instance.
        """
        instance = server_type(*args, **kwargs)
        servers.append(instance)
        return instance

    def worker(cmd, **_kwargs):
        """Return deterministic validation and worker samples.

        Args:
            cmd: Base validation or worker command.
            **_kwargs: Process options.

        Returns:
            Successful process output.
        """
        if cmd[1] == "-c":
            data = [str(tmp_path / "base" / "terrio" / "__init__.py"), "0.9.0"]
        else:
            calls.append(json.loads(cmd[-1])["tool"])
            data = _sample(1)
        return SimpleNamespace(returncode=0, stdout=json.dumps(data), stderr="")

    monkeypatch.setattr(perf_matrix, "ThreadingHTTPServer", server)
    monkeypatch.setattr(perf_matrix.subprocess, "run", worker)
    args = SimpleNamespace(format="fake", dir=tmp_path, latency_ms=0, threads=1, only=None,
                           quick=True, samples=1, base=tmp_path / "base", out=tmp_path / "results.jsonl",
                           source=["http"], allow_battery=False, access=None, timeout=1)
    perf_matrix.run(args)
    assert calls == ["terrio", "gdal", "base", "gdal", "base", "terrio"]
    assert servers[0].socket.fileno() == -1
    assert servers[0].request_queue_size >= 64  # the threaded reads' connections are not dropped


def test_a_fixture_filter_matching_nothing_is_an_error(tmp_path, monkeypatch):
    """Reject a typo instead of publishing a successful empty benchmark.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped format replacement.
    """
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", SimpleNamespace(FIXTURES={"actual": None}))
    args = SimpleNamespace(format="fake", dir=tmp_path, latency_ms=0, threads=1, only=["typo"])
    with pytest.raises(SystemExit, match="no fixtures match"):
        perf_matrix.run(args)


def test_every_path_pattern_names_files_of_the_repository():
    """Keep the path map in step with the sources: a renamed file must not silence a format."""
    import fnmatch
    import subprocess

    root = Path(__file__).resolve().parents[2]
    files = subprocess.run(["git", "ls-files"], cwd=root, capture_output=True, text=True, check=True).stdout.split()
    for pattern in [p for ps in perf_matrix.PATHS.values() for p in ps] + perf_matrix._SHARED:
        assert any(fnmatch.fnmatch(f, pattern) for f in files), pattern


def test_changed_files_choose_the_formats_to_measure():
    """A format's own file picks it; a shared one picks every format; others pick none."""
    # GeoPackage's `where` read goes through `terrio.sql` and the eager vector read.
    assert all("geopackage" in perf_matrix.touched([f]) for f in ("python/src/ops.rs", "crates/terrio/src/drivers/vector/mod.rs"))
    assert perf_matrix.touched(["crates/terrio/src/drivers/vector/geoparquet_arrow.rs"]) == ["geoparquet", "geoparquet-real"]
    assert perf_matrix.touched(["crates/terrio/src/drivers/zarr/cube.rs", "README.md"]) == ["zarr"]
    assert perf_matrix.touched(["Cargo.lock"]) == ["geotiff", "zarr", "geoparquet", "flatgeobuf", "geopackage", "geotiff-write",
                                                   "geoparquet-write", "rasterize", "zonal", "warp", "terrain", "vectorize", "geotiff-real", "geoparquet-real"]
    assert perf_matrix.touched(["crates/terrio/src/stream/sqlite.rs"]) == ["geoparquet", "flatgeobuf", "geopackage", "geoparquet-real"]
    assert perf_matrix.touched(["crates/terrio/src/stream/mod.rs"]) == ["geoparquet", "flatgeobuf", "geopackage", "geoparquet-real"]
    assert perf_matrix.touched(["crates/terrio/src/ops/sql/select.rs"]) == ["geopackage"]
    assert perf_matrix.touched(["python/src/raster.rs"]) == ["geotiff", "zarr", "geotiff-write", "geotiff-real"]
    assert perf_matrix.touched(["crates/terrio/src/drivers/gtiff_write.rs"]) == ["geotiff-write"]
    assert perf_matrix.touched(["crates/terrio/src/drivers/vector/geoparquet.rs"]) == ["geoparquet", "geoparquet-write", "geoparquet-real"]
    assert perf_matrix.touched(["python/src/vector.rs"]) == ["flatgeobuf", "geoparquet-write", "vectorize"]
    assert perf_matrix.touched(["crates/terrio/src/ops/warp/window.rs"]) == ["warp"]
    assert perf_matrix.touched(["crates/terrio/src/ops/dem.rs", "crates/terrio/src/ops/calc/eval.rs"]) == ["terrain"]
    assert perf_matrix.touched(["crates/terrio/src/ops/contour.rs", "python/src/analysis.rs"]) == ["zonal", "vectorize"]
    assert perf_matrix.touched(["crates/terrio/src/drivers/vector/flatgeobuf.rs", "docs/benchmarks.md"]) == ["flatgeobuf"]
    assert perf_matrix.touched(["docs/benchmarks.md"]) == []


@pytest.mark.parametrize("option,value", [("samples", "0"), ("threads", "-1"),
                                         ("timeout", "0"), ("timeout", "nan"), ("timeout", "inf")])
def test_run_rejects_invalid_measurement_limits(tmp_path, monkeypatch, option, value):
    """Reject invalid limits before creating fixtures or launching workers.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped argument replacement.
        option: Measurement limit to validate.
        value: Invalid command line value.
    """
    monkeypatch.setattr(sys, "argv", ["perf_matrix.py", "run", "--dir", str(tmp_path),
                                      f"--{option}", value])
    with pytest.raises(SystemExit) as exc:
        perf_matrix.main()
    assert exc.value.code == 2
    assert not (tmp_path / "geotiff").exists()


def test_operations_refuse_an_http_source(tmp_path, monkeypatch):
    """Operations run from a local path only, so `--source http` is refused before any work."""
    monkeypatch.setattr(sys, "argv", ["perf_matrix.py", "run", "--format", "warp", "--dir", str(tmp_path), "--source", "http"])
    with pytest.raises(SystemExit) as exc:
        perf_matrix.main()
    assert exc.value.code == 2
    assert not (tmp_path / "geotiff").exists()


def test_writes_refuse_an_http_source(tmp_path, monkeypatch):
    """Writes read their source from a local path only, so `--source http` is refused before any work."""
    monkeypatch.setattr(sys, "argv", ["perf_matrix.py", "run", "--format", "geotiff-write", "--dir", str(tmp_path), "--source", "http"])
    with pytest.raises(SystemExit) as exc:
        perf_matrix.main()
    assert exc.value.code == 2


def test_posted_artifact_markdown_cannot_close_its_text_fence(tmp_path):
    """Execute the posting step with a PR artifact containing Markdown delimiters.

    Args:
        tmp_path: Isolated workflow workspace with a local GitHub CLI stub.
    """
    import os
    import subprocess
    import textwrap

    root = Path(__file__).resolve().parents[2]
    workflow = (root / ".github/workflows/perf-matrix.yml").read_text()
    script = textwrap.dedent(workflow.rsplit("        run: |\n", 1)[1])
    (tmp_path / "out").mkdir()
    artifact = "```\n[untrusted](https://example.com)\n````````\n"
    (tmp_path / "out/geotiff.md").write_text(artifact)
    cli = tmp_path / "gh"
    cli.write_text("#!/bin/sh\ncp body.md posted.md\n")
    cli.chmod(0o755)
    env = {**os.environ, "PATH": f"{tmp_path}:{os.environ['PATH']}", "PR": "1",
           "HEAD_SHA": "a" * 40, "BASE_SHA": "b" * 40, "FORMATS": "geotiff",
           "ARGS": "--quick", "RUN_URL": "https://example.com/run"}
    subprocess.run(["bash", "-e", "-c", script], cwd=tmp_path, env=env, check=True)
    posted = (tmp_path / "posted.md").read_text()
    fence = "`" * 9
    assert f"{fence}text\n{artifact}\n{fence}\n" in posted


def test_benchmark_request_rejects_a_read_only_collaborator(tmp_path):
    """Check repository permission before preparing any expensive benchmark work.

    Args:
        tmp_path: Isolated request workspace with a GitHub permission response.
    """
    import os
    import subprocess
    import textwrap

    root = Path(__file__).resolve().parents[2]
    workflow = (root / ".github/workflows/perf-matrix.yml").read_text()
    gate, prepare = workflow.split("\n  prepare:", 1)
    assert "concurrency:" not in gate
    assert "    needs: authorize\n" in prepare
    script = textwrap.dedent(gate.split("        run: |\n", 1)[1])
    cli = tmp_path / "gh"
    cli.write_text('#!/bin/sh\ncase "$*" in *collaborators*) echo read ;; *) exit 99 ;; esac\n')
    cli.chmod(0o755)
    env = {**os.environ, "PATH": f"{tmp_path}:{os.environ['PATH']}", "PR": "1",
           "BODY": "/perf-matrix", "REQUESTER": "reader", "GITHUB_REPOSITORY": "test/repo",
           "GITHUB_OUTPUT": str(tmp_path / "outputs")}
    result = subprocess.run(["bash", "-e", "-c", script], cwd=tmp_path, env=env,
                            text=True, capture_output=True)
    assert result.returncode == 1
    assert "Repository write permission is required" in result.stdout
    assert not (tmp_path / "outputs").exists()


def _mem(np, gdal, a, gt=(500000, 10, 0, 5000000, 0, -10), nodata=None):
    """A GDAL in-memory raster of one band, in UTM 31N.

    Args:
        np: The numpy module.
        gdal: GDAL's module.
        a: The band's values.
        gt: The geotransform.
        nodata: The nodata value, if any.

    Returns:
        The dataset.
    """
    ds = gdal.GetDriverByName("MEM").Create("", a.shape[1], a.shape[0], 1, gdal.GetDataTypeByName({"float32": "Float32", "uint8": "Byte"}[a.dtype.name]))
    ds.SetGeoTransform(gt)
    ds.SetProjection("EPSG:32631")
    ds.GetRasterBand(1).WriteArray(a)
    if nodata is not None:
        ds.GetRasterBand(1).SetNoDataValue(nodata)
    return ds


def test_an_operation_output_is_compared_with_gdal_pixel_by_pixel():
    """Count every differing pixel, and put all of them beyond the tolerance on a metadata mismatch."""
    np = pytest.importorskip("numpy")
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    ref = np.arange(12, dtype="float32").reshape(3, 4)
    ref[0, 0] = np.nan
    g = _mem(np, gdal, ref, nodata=-1.0)

    def out(a, gt=(500000, 10, 0, 5000000, 0, -10), nodata=-1.0):
        return SimpleNamespace(read=lambda: a[None], nodata=nodata, transform=gt, crs_wkt=g.GetProjection())

    same = perf_matrix.compare_raster(out(ref.copy()), g, 0.0)
    assert (same["differing"], same["beyond_tol"], same["n"], same["mismatch"]) == (0, 0, 12, "")
    near = ref.copy()
    near[1, 1] += 0.005
    near[2, 2] += 0.5
    near[0, 0] = 3.0  # a value where GDAL's is NaN differs infinitely
    c = perf_matrix.compare_raster(out(near), g, 1e-2)
    assert (c["differing"], c["beyond_tol"]) == (3, 2) and c["max_diff"] == pytest.approx(0.5)
    # A nodata value is compared as the output's type holds it.
    g.GetRasterBand(1).SetNoDataValue(3.402823466e+38)
    assert not perf_matrix.compare_raster(out(ref.copy(), nodata=float(np.finfo("float32").max)), g, 0.0)["mismatch"]
    g.GetRasterBand(1).SetNoDataValue(-1.0)
    for bad in (out(ref.copy(), nodata=None), out(ref.copy(), nodata=-1.5), out(ref.copy(), gt=(500010, 10, 0, 5000000, 0, -10)),
                out(ref.astype("float64")), out(ref[:2].copy())):
        c = perf_matrix.compare_raster(bad, g, 1.0)
        assert c["mismatch"] and c["beyond_tol"] == 12


class _Layer:
    """A stand-in for a `terrio.Layer`: features as `(value, WKT)`."""

    def __init__(self, rows, crs_wkt, field="DN"):
        self.rows, self.crs_wkt, self.fields = rows, crs_wkt, {field: "Integer"}

    def __len__(self):
        return len(self.rows)

    def columns(self):
        return {f: [v for v, _ in self.rows] for f in self.fields}

    def to_wkb(self):
        from osgeo import ogr

        return [ogr.CreateGeometryFromWkt(w).ExportToWkb() for _, w in self.rows]


def test_features_match_gdal_whatever_their_order_start_and_direction():
    """Match features by value and vertices, then each value's total area; anything else differs."""
    ogr = pytest.importorskip("osgeo.ogr")
    osr = pytest.importorskip("osgeo.osr")
    ogr.UseExceptions()
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(32631)
    holed = "POLYGON ((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 2, 1 1), (3 3, 3.5 3, 3.5 3.5, 3 3))"
    rows = [(1, holed), (2, "POLYGON ((4 0, 5 0, 5 1, 4 1, 4 0))")]
    ref = ogr.GetDriverByName("MEM").CreateDataSource("")
    layer = ref.CreateLayer("polygons", crs, ogr.wkbPolygon)
    layer.CreateField(ogr.FieldDefn("DN", ogr.OFTInteger))
    for v, w in rows:
        f = ogr.Feature(layer.GetLayerDefn())
        f.SetField("DN", v)
        f.SetGeometry(ogr.CreateGeometryFromWkt(w))
        layer.CreateFeature(f)
    wkt = crs.ExportToWkt()
    # The other feature first, each ring from another vertex and the other way round, holes swapped.
    same = [(2, "POLYGON ((5 1, 5 0, 4 0, 4 1, 5 1))"),
            (1, "POLYGON ((4 4, 0 4, 0 0, 4 0, 4 4), (3 3, 3.5 3.5, 3.5 3, 3 3), (2 2, 1 2, 1 1, 2 1, 2 2))")]
    c = perf_matrix.compare_features(_Layer(same, wkt), ref, 1e-9)
    assert (c["n"], c["differing"], c["beyond_tol"], c["max_diff"], c["mismatch"]) == (2, 0, 0, 0.0, "")
    # Moved by half a pixel: the same areas, but no match.
    c = perf_matrix.compare_features(_Layer([(2, "POLYGON ((4 0.5, 5 0.5, 5 1.5, 4 1.5, 4 0.5))"), rows[0]], wkt), ref, 1e-9)
    assert (c["differing"], c["beyond_tol"], c["mismatch"]) == (1, 1, ""), c
    # Another value, or the same vertices in another sequence: no match, and a value's total differs.
    for bad, totals in (([(3, rows[1][1]), rows[0]], ("DN 2: total area differs by 1", "DN 3: total area differs by 1")),
                        ([(2, "POLYGON ((4 0, 5 1, 5 0, 4 1, 4 0))"), rows[0]], ("DN 2: total area differs by",))):
        c = perf_matrix.compare_features(_Layer(bad, wkt), ref, 1e-9)
        assert (c["differing"], c["beyond_tol"]) == (1, 2) and c["mismatch"].startswith(totals), c
    c = perf_matrix.compare_features(_Layer(rows[:1], wkt), ref, 1e-9)
    assert c["mismatch"].startswith("features 1 vs 2") and c["beyond_tol"] == 2
    assert "crs" in perf_matrix.compare_features(_Layer(rows, None), ref, 1e-9)["mismatch"]
    assert perf_matrix.compare_features(_Layer(rows, wkt, field="ELEV"), ref, 1e-9)["beyond_tol"] == 2
    # A closed line from another start, and the other way round, matches; its length is compared.
    lines = ogr.GetDriverByName("MEM").CreateDataSource("")
    layer = lines.CreateLayer("contour", crs, ogr.wkbLineString)
    layer.CreateField(ogr.FieldDefn("ID", ogr.OFTInteger))
    layer.CreateField(ogr.FieldDefn("ELEV", ogr.OFTReal))
    f = ogr.Feature(layer.GetLayerDefn())
    f.SetField("ELEV", 200.0)
    f.SetGeometry(ogr.CreateGeometryFromWkt("LINESTRING (0 0, 1 0, 1 1, 0 0)"))
    layer.CreateFeature(f)
    c = perf_matrix.compare_features(_Layer([(200.0, "LINESTRING (1 1, 1 0, 0 0, 1 1)")], wkt, field="ELEV"), lines, 1e-10)
    assert (c["differing"], c["beyond_tol"], c["mismatch"]) == (0, 0, "")
    # A ring through its smallest vertex twice may start at either; an open line may run either way.
    def shapes(w):
        return perf_matrix._shapes([(1, ogr.CreateGeometryFromWkt(w))])[0]

    assert shapes("POLYGON ((0 0, 1 0, 1 1, 0 0, 0 2, 1 2, 0 0))") == shapes("POLYGON ((0 0, 0 2, 1 2, 0 0, 1 0, 1 1, 0 0))")
    assert shapes("LINESTRING (0 0, 1 0, 2 1)") == shapes("LINESTRING (2 1, 1 0, 0 0)") != shapes("LINESTRING (0 0, 2 1, 1 0)")
    assert shapes("LINESTRING (-1 0, 5 5)") != shapes("LINESTRING (-2 0, 5 5)")  # hash(-1.0) == hash(-2.0) in CPython
    # An open line and the closed line through the same vertices differ, and so do a closed line
    # and a polygon of the same ring.
    assert shapes("LINESTRING (0 0, 0 1, 1 1, 1 0)") != shapes("LINESTRING (0 0, 0 1, 1 1, 1 0, 0 0)")
    assert shapes("LINESTRING (0 0, 0 4, 4 4, 4 0, 0 0)") != shapes("POLYGON ((0 0, 0 4, 4 4, 4 0, 0 0))")


def _compare(differing=0, beyond=0, mismatch=""):
    return {"n": 4, "tol": 0.01, "differing": differing, "beyond_tol": beyond, "max_diff": 0.001 * differing, "mismatch": mismatch}


def test_an_operation_is_exact_within_tolerance_or_differs_by_its_pixels():
    """Label operations by their pixel comparison, never as a checksum match."""
    def row(*compares):
        return summarize(_row(terrio=[{**_sample(2, s=c["beyond_tol"]), "compare": c} for c in compares],
                              gdal=[_sample(1, s=0)] * len(compares)))

    exact = row(_compare(), _compare())
    assert exact["values"] == "exact" and "compare" not in exact["terrio"]
    within = row(_compare(), _compare(differing=3))  # the worst sample decides
    assert within["values"] == "within_tol" and within["compare"]["differing"] == 3
    beyond = row(_compare(differing=3, beyond=1))
    assert beyond["values"] == "DIFFER: 1 of 4 values beyond 0.01 (max 0.003)"
    shape = row(_compare(beyond=4, mismatch="shape (1, 2, 2) vs (1, 1, 4)"))
    assert shape["values"] == "DIFFER: shape (1, 2, 2) vs (1, 1, 4)"
    based = summarize(_row(terrio=[{**_sample(2, s=0), "compare": _compare()}], gdal=[_sample(1, s=0)],
                           base=[{**_sample(2, s=0), "compare": _compare(mismatch="CRS differs")}]))
    assert based["values"] == "exact"  # a base build's output does not label terrio's
    text = report([exact, {**within, "access": "aspect"}, {**beyond, "fixture": "f32_none"}])
    worst = text.split("## Worst cases")[1].split("## Geometric")[0]
    assert " exact" in worst and " within_tol" in worst and "f32_none" not in worst
    assert "within tolerance of GDAL's (ranked): 1\n  u8_none/aspect: 3 of 4 values differ, max 0.003 <= 0.01" in text
    assert "values differ from GDAL (not ranked): 1\n  f32_none" in text
    assert "within tolerance" not in report([exact])


@pytest.mark.parametrize("fmt,access", [("warp", "bilinear"), ("warp", "near_t1"), ("terrain", "slope_t1"), ("terrain", "aspect"),
                                        ("terrain", "proximity"), ("terrain", "calc"), ("vectorize", "polygonize"),
                                        ("vectorize", "polygonize8"), ("vectorize", "contour")])
def test_an_operation_runs_in_a_worker_and_matches_gdal(tmp_path, monkeypatch, fmt, access):
    """Run an operation as the runner's worker does, on a small DEM, against GDAL's output.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped environment and case replacements.
        fmt: The operation's format.
        access: The operation.
    """
    np = pytest.importorskip("numpy")
    gdal = pytest.importorskip("osgeo.gdal")
    pytest.importorskip("terrio")
    gdal.UseExceptions()
    op = perf_matrix.FORMATS[fmt]
    if access == "near_t1":
        monkeypatch.setitem(op.CASES, access, ("near", 0, 0.0))
    for name in ("RAYON_NUM_THREADS", "GDAL_NUM_THREADS"):
        monkeypatch.setenv(name, "2")
    dem = perf_matrix.terrain(np, 256, 1, "float32")[0]
    gdal.Translate(str(tmp_path / "f32_none.tif"), _mem(np, gdal, dem), format="GTiff")
    gdal.Translate(str(tmp_path / "u8_none.tif"), _mem(np, gdal, perf_matrix.terrain(np, 256, 1, "uint8")[0]), format="GTiff")
    gdal.Translate(str(tmp_path / "u8_classes.tif"), _mem(np, gdal, ((dem + 1000) // 200).astype("uint8")), format="GTiff")
    src = tmp_path / f"{op.CASES[access][0] if fmt in ('terrain', 'vectorize') else 'f32_none'}.tif"
    if fmt in ("terrain", "vectorize"):  # an operation is measured on its own input only
        assert op.meta(tmp_path / f"{next(f for f in op.FIXTURES if f != src.stem)}.tif", access) is None
    warm = _mem(np, gdal, np.ones((64, 64), "float32"), gt=(0, 1, 0, 0, 0, -1))
    warm.SetProjection("")
    gdal.Translate(str(tmp_path / "_warmup.tif"), warm, format="GTiff")
    meta = op.meta(src, access)
    samples = {tool: perf_matrix.worker({"format": fmt, "tool": tool, "src": str(src), "access": access, "meta": meta})
               for tool in ("gdal", "terrio")}
    assert os.environ["RAYON_NUM_THREADS"] == ("1" if access.endswith("_t1") else "2")
    c = samples["terrio"]["compare"]
    assert c["n"] == samples["gdal"]["n"] > 0 and c["beyond_tol"] == 0 and not c["mismatch"], c
    row = summarize({**_row(terrio=[samples["terrio"]], gdal=[samples["gdal"]]), "format": fmt, "access": access})
    assert (row["values"] == "exact") if fmt == "vectorize" else (row["values"] in ("exact", "within_tol"))


def _compared(beyond=0, differing=0, max_diff=0.0, mismatch=None):
    c = {"tol": 24, "n": 100, "differing": differing, "beyond_tol": beyond, "max_diff": max_diff, "sum_diff": 0.0, "mismatch": mismatch}
    return {**_sample(2, s=beyond + bool(mismatch), n=100), "compare": c}


def test_compared_outputs_label_a_case_by_its_worst_sample():
    """Exact, within tolerance or DIFFER, from the worst sample; only the first two are ranked."""
    gdal = [_sample(1, s=0, n=100)]
    exact = summarize(_row(terrio=[_compared(), _compared()], gdal=gdal))
    assert exact["values"] == "exact" and "compare" not in exact["terrio"]
    within = summarize(_row(terrio=[_compared(), _compared(differing=5, max_diff=3)], gdal=gdal))
    assert within["values"] == "within_tol" and within["compare"]["max_diff"] == 3
    beyond = summarize(_row(terrio=[_compared(beyond=2, differing=5, max_diff=30)], gdal=gdal))
    assert beyond["values"] == "DIFFER: 2 of 100 values beyond 24 (max 30)"
    meta = summarize(_row(terrio=[_compared(differing=9, max_diff=9), _compared(mismatch="CRS differs")], gdal=gdal))
    assert meta["values"] == "DIFFER: CRS differs"
    based = summarize(_row(terrio=[_compared()], gdal=gdal, base=[{**_compared(mismatch="CRS differs"), "sum": 0}]))
    assert based["values"] == "exact"  # a base build's output does not label terrio's
    assert "base_ratio" not in based  # nor is a base whose output differs ranked against
    assert "base_ratio" in summarize(_row(terrio=[_compared()], gdal=gdal, base=[{**_compared(), "sum": 0}]))
    text = report([exact, {**within, "fixture": "f32_none"}, {**beyond, "fixture": "u8_rgb_jpeg"}])
    worst = text.split("## Worst cases")[1].split("## Geometric")[0]
    assert "u8_none" in worst and "f32_none" in worst and "within_tol" in worst and "u8_rgb_jpeg" not in worst
    assert "values differ from GDAL (not ranked): 1\n  u8_rgb_jpeg" in text


def test_worker_keeps_the_worst_comparison_of_its_items(monkeypatch):
    results = iter([(0, 10, {"mismatch": None, "beyond_tol": 0, "differing": 1, "max_diff": 1}),
                    (1, 10, {"mismatch": None, "beyond_tol": 1, "differing": 1, "max_diff": 30})])
    fake = SimpleNamespace(EXT=".tif", reader=lambda *_: (str, [len, len], lambda _: next(results), (str, len)))
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    result = perf_matrix.worker({"format": "fake", "tool": "terrio", "src": "http://localhost/f.tif", "access": "full", "meta": {}})
    assert (result["sum"], result["n"], result["compare"]["max_diff"]) == (1, 20, 30)


def test_worker_hashes_no_digest_for_compared_outputs(monkeypatch, tmp_path):
    """A write's or an operation's output is compared by `check`: the worker hashes no values.

    Args:
        monkeypatch: Scoped format replacement.
        tmp_path: Where the fake write puts its file.
    """
    written = tmp_path / "out.tif"
    compare = {"mismatch": None, "beyond_tol": 0, "differing": 0, "max_diff": 0}
    fake = SimpleNamespace(EXT=".tif", COMPARES_OUTPUT=True,
                           reader=lambda *_: (str, [lambda _: written], lambda _: (0, 10, compare), (str, len)))
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    for tool in ("terrio", "gdal"):
        result = perf_matrix.worker({"format": "fake", "tool": tool, "src": "http://localhost/f.tif", "access": "cog", "meta": {}})
        assert result["digest"] is None and result["compare"] == compare


def test_write_comparison_reads_every_level_and_reports_what_differs(tmp_path):
    """Exact copies, one changed pixel, a tolerance, a color interpretation and an overview count."""
    gdal = pytest.importorskip("osgeo.gdal")
    np = pytest.importorskip("numpy")
    gdal.UseExceptions()

    def write(name, data, overviews=(2,), interp=None):
        ds = gdal.GetDriverByName("GTiff").Create(str(tmp_path / name), 64, 64, 1, gdal.GDT_Float32)
        ds.SetGeoTransform((0, 1, 0, 0, 0, -1))
        ds.SetProjection("EPSG:32631")
        ds.GetRasterBand(1).WriteArray(data)
        if interp is not None:
            ds.GetRasterBand(1).SetColorInterpretation(interp)
        if overviews:
            ds.BuildOverviews("NEAREST", list(overviews))
        ds = None
        return tmp_path / name

    data = np.arange(64 * 64, dtype="float32").reshape(64, 64)
    data[0, 0] = np.nan
    ref = write("ref.tif", data)
    same = perf_matrix.GeoTiffWrite.compare(write("same.tif", data), ref, 0)
    assert same["mismatch"] is None and same["differing"] == 0 and same["n"] == 64 * 64 + 32 * 32
    changed = data.copy()
    changed[10, 10] += 3  # also at (5, 5) of the overview, decimated by nearest
    off = perf_matrix.GeoTiffWrite.compare(write("off.tif", changed), ref, 0)
    assert (off["differing"], off["beyond_tol"], off["max_diff"]) == (2, 2, 3)
    assert perf_matrix.GeoTiffWrite.compare(tmp_path / "off.tif", ref, 3)["beyond_tol"] == 0
    nan = data.copy()
    nan[1, 1] = np.nan
    assert perf_matrix.GeoTiffWrite.compare(write("nan.tif", nan, ()), write("ref1.tif", data, ()), 0)["max_diff"] == np.inf
    infinite = data.copy()
    infinite[2, 2], infinite[3, 3] = np.inf, -np.inf  # equal infinities agree
    assert perf_matrix.GeoTiffWrite.compare(write("inf.tif", infinite, ()), write("inf1.tif", infinite, ()), 0)["differing"] == 0
    interp = perf_matrix.GeoTiffWrite.compare(write("interp.tif", data, interp=gdal.GCI_RedBand), ref, 0)
    assert interp["mismatch"] == "band 1 color interpretation Red vs GDAL's Gray" and interp["n"] == 64 * 64 + 32 * 32
    levels = perf_matrix.GeoTiffWrite.compare(write("levels.tif", data, (2, 4)), ref, 0)
    assert levels["mismatch"].startswith("band 1 overview sizes") and levels["n"] == 0


def test_write_cases_give_both_writers_the_same_options():
    """A COG gets its overviews and block size, a GeoTIFF tiles; RGB JPEG is GeoTIFF only."""
    w = perf_matrix.GeoTiffWrite
    assert w.options("u16_deflate_p2_t256", "cog") == ("COG", {"COMPRESS": "DEFLATE", "INTERLEAVE": "BAND", "PREDICTOR": "2", "BLOCKSIZE": "256",
                                                            "OVERVIEW_RESAMPLING": "AVERAGE", "OVERVIEWS": "IGNORE_EXISTING"})
    assert w.options("u16_deflate_p2_t256", "cog_no_overviews")[1]["OVERVIEWS"] == "NONE"
    assert w.options("u8_rgb_jpeg_rgb_t512", "gtiff") == ("GTiff", {"COMPRESS": "JPEG", "INTERLEAVE": "PIXEL", "PHOTOMETRIC": "RGB", "TILED": "YES",
                                                                 "BLOCKXSIZE": "512", "BLOCKYSIZE": "512"})
    assert w.options("u16_zstd_p2_t512", "gtiff")[1]["ZSTD_LEVEL"] == "1"
    assert w.meta(Path("/nonexistent/u8_rgb_jpeg_rgb_t512.tif"), "cog") is None
    assert all(src in perf_matrix.GeoTiff.FIXTURES and w.axes(name) for name, (src, *_) in w.FIXTURES.items())


def test_geoparquet_write_comparison_checks_features_and_file_layout(tmp_path, monkeypatch):
    """Equal files, a changed attribute, and GDAL's options other than terrio's."""
    gdal = pytest.importorskip("osgeo.gdal")
    ogr = pytest.importorskip("osgeo.ogr")
    pytest.importorskip("pyarrow")
    if gdal.GetDriverByName("Parquet") is None or ogr.GetGEOSVersionMajor() == 0:
        pytest.skip("GDAL's Parquet driver and GEOS are required")
    gdal.UseExceptions()

    def write(name, value=1.5, lco=perf_matrix.GeoParquetWrite.LCO):
        src = gdal.GetDriverByName("MEM").CreateVector("")
        srs = gdal.osr.SpatialReference()
        srs.ImportFromEPSG(32631)
        layer = src.CreateLayer("data", srs, ogr.wkbPoint)
        layer.CreateField(ogr.FieldDefn("value", ogr.OFTReal))
        for i in range(10):
            f = ogr.Feature(layer.GetLayerDefn())
            f["value"] = value if i == 3 else i
            f.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({i} {2 * i})"))
            layer.CreateFeature(f)
        gdal.VectorTranslate(str(tmp_path / name), src, format="Parquet", layerCreationOptions=lco)
        return tmp_path / name

    ref = write("ref.parquet")
    same = perf_matrix.GeoParquetWrite.compare(write("same.parquet"), ref)
    assert same["mismatch"] is None and same["n"] == 10
    assert "attributes" in perf_matrix.GeoParquetWrite.compare(write("value.parquet", value=2.5), ref)["mismatch"]
    groups = perf_matrix.GeoParquetWrite.compare(write("groups.parquet", lco=["ROW_GROUP_SIZE=4"]), ref)
    assert groups["mismatch"] == "row groups 3 vs GDAL's 1"
    zstd = perf_matrix.GeoParquetWrite.compare(write("zstd.parquet", lco=["COMPRESSION=ZSTD"]), ref)
    assert zstd["mismatch"] == "compression ['ZSTD'] vs GDAL's ['SNAPPY']"
    pq = pytest.importorskip("pyarrow.parquet")
    table = pq.read_table(tmp_path / "same.parquet")
    pq.write_table(table.replace_schema_metadata({}), tmp_path / "plain.parquet")
    # Even when the features compare equal, a file without GeoParquet metadata is labelled.
    monkeypatch.setattr(pytest.importorskip("benchmark_validation"), "compare_vectors", lambda a, b: [])
    plain = perf_matrix.GeoParquetWrite.compare(tmp_path / "plain.parquet", ref)
    # A runner without GEOS or sort fails the check rather than labelling the output.
    no_geos = ["vector validation requires GDAL built with GEOS for geometry normalization"]
    monkeypatch.setattr(pytest.importorskip("benchmark_validation"), "compare_vectors", lambda a, b: no_geos)
    with pytest.raises(RuntimeError, match="requires GDAL built with GEOS"):
        perf_matrix.GeoParquetWrite.compare(tmp_path / "same.parquet", ref)
    sort_failed = ["vector validation sort failed (requires -S/-T support): sort: invalid option"]
    monkeypatch.setattr(pytest.importorskip("benchmark_validation"), "compare_vectors", lambda a, b: sort_failed)
    with pytest.raises(RuntimeError, match="sort failed"):
        perf_matrix.GeoParquetWrite.compare(tmp_path / "same.parquet", ref)
    assert plain["mismatch"] == "geometry encoding None vs GDAL's WKB"


def test_gdal_cog_writes_get_the_cog_drivers_option_names(monkeypatch, tmp_path):
    """GDAL's COG driver takes LEVEL for ZSTD_LEVEL and no PHOTOMETRIC; GTiff takes both as given."""
    gdal = pytest.importorskip("osgeo.gdal")
    calls = []
    monkeypatch.setattr(gdal, "Translate", lambda dst, ds, format, creationOptions: calls.append((format, creationOptions)))
    monkeypatch.setenv("GDAL_NUM_THREADS", "3")
    opts = {"COMPRESS": "ZSTD", "PHOTOMETRIC": "RGB", "ZSTD_LEVEL": "1"}
    perf_matrix.GeoTiffWrite.gdal_write(None, tmp_path / "a.tif", "COG", opts)
    perf_matrix.GeoTiffWrite.gdal_write(None, tmp_path / "b.tif", "GTiff", opts)
    assert calls == [("COG", ["COMPRESS=ZSTD", "LEVEL=1", "NUM_THREADS=3"]),
                     ("GTiff", ["COMPRESS=ZSTD", "PHOTOMETRIC=RGB", "ZSTD_LEVEL=1", "NUM_THREADS=3"])]


def test_threads4_is_measured_with_each_thread_count():
    """Expand a format's `threads4` to 1, 2, 4 and 8 threads, and keep the explicit name valid."""
    assert perf_matrix.accesses(perf_matrix.GeoTiff) == ["window", "windows16", "full", "overview",
                                                         "threads1", "threads2", "threads4", "threads8"]
    assert [perf_matrix.threads(a) for a in ("threads1", "threads8", "windows16", "bbox16")] == [1, 8, 0, 0]
    assert perf_matrix.accesses(SimpleNamespace(ACCESS=["full"])) == ["full"]


@pytest.mark.parametrize("access,ok", [(["threads4"], True), (["threads8", "window"], True), (["threads3"], False)])
def test_run_accepts_only_the_measured_thread_counts(tmp_path, monkeypatch, access, ok):
    """Accept `threads4` as before and the other measured counts, and reject any other count.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped argument and runner replacements.
        access: The `--access` values.
        ok: Whether the command line is accepted.
    """
    ran = []
    monkeypatch.setattr(perf_matrix, "run", ran.append)
    monkeypatch.setattr(sys, "argv", ["perf_matrix.py", "run", "--dir", str(tmp_path), "--access", *access])
    if ok:
        perf_matrix.main()
        assert ran[0].access == access
    else:
        with pytest.raises(SystemExit) as exc:
            perf_matrix.main()
        assert exc.value.code == 2 and not ran


@pytest.mark.parametrize("n", [1, 2, 8])
def test_threaded_reads_open_a_handle_per_thread_and_keep_the_parts_in_order(monkeypatch, n):
    """Read a format's 16 `threads4` parts with N handles in N threads, in the parts' order.

    Args:
        monkeypatch: Scoped format replacement.
        n: The number of reading threads.
    """
    import threading

    opened, given = [], []

    def reader(_tool, access, _meta):
        """Give 16 parts that record the thread reading them.

        Args:
            _tool: Reader backend.
            access: Read pattern the format is asked for.
            _meta: Fixture metadata.

        Returns:
            Reader operations and validation.
        """
        given.append(access)

        def open_(path):
            opened.append(threading.get_ident())
            return path

        items = [lambda _d, i=i: [i] for i in range(16)]
        return open_, items, perf_matrix.checksum, (lambda p: p, lambda _d: None)

    monkeypatch.setitem(perf_matrix.FORMATS, "fake", SimpleNamespace(EXT=".tif", reader=reader))
    result = perf_matrix.worker({"format": "fake", "tool": "gdal", "src": "http://localhost/f.tif",
                                 "access": f"threads{n}", "meta": {}})
    assert given == ["threads4"] and len(set(opened)) == n  # n handles, on n distinct threads
    assert result["digest"] == perf_matrix.digest([[i] for i in range(16)])


def test_report_gives_each_tools_speedup_over_one_thread():
    """Compare each thread count with one thread, per tool, over the cases that have both."""
    rows = [summarize({**_row(terrio=[_sample(t)], gdal=[_sample(g)]), "access": f"threads{n}"})
            for n, t, g in ((1, 8.0, 4.0), (2, 4.0, 2.0), (8, 4.0, 1.0))]
    other = summarize({**_row(terrio=[_sample(1.0)], gdal=[_sample(1.0)]), "format": "zarr", "access": "threads8"})
    text = report([*rows, other])  # a zarr case without one thread is not mixed into GeoTIFF's
    assert "geotiff local threads2  terrio x2.00  gdal x2.00  terrio/gdal time x2.00  (1)" in text
    assert "geotiff local threads8  terrio x2.00  gdal x4.00  terrio/gdal time x4.00  (1)" in text
    # Files run with different library pools are not merged into one scaling section.
    mixed = report([{"machine": {**_MACHINE, "threads": 1}}, {"machine": {**_MACHINE, "threads": 4}}, *rows])
    assert "mix --threads [1, 4]" in mixed and "## Thread scaling" not in mixed
    # Thread counts pair within a results file: another file's one-thread run is not A's baseline.
    def case(n, t, g):
        return summarize({**_row(terrio=[_sample(t)], gdal=[_sample(g)]), "access": f"threads{n}"})

    two_runs = report([{"machine": _MACHINE}, case(1, 8.0, 4.0), case(8, 2.0, 1.0), {"machine": _MACHINE}, case(1, 80.0, 40.0)])
    assert "geotiff local threads8  terrio x4.00  gdal x4.00  terrio/gdal time x2.00  (1)" in two_runs


def _fs_type(path):
    """The file system type of a path.

    Args:
        path: The path.

    Returns:
        The type of the longest mount point above it, such as `ext4` or `tmpfs`.
    """
    path = str(Path(path).resolve()) + "/"  # a mount point is above itself
    mounts = [line.split()[1:3] for line in Path("/proc/mounts").read_text().splitlines()]
    return max(((m, k) for m, k in mounts if path.startswith(m.rstrip("/") + "/")), key=lambda x: len(x[0]))[1]


# File systems whose files `POSIX_FADV_DONTNEED` drops from the page cache: the eviction must work there.
EVICTING = ("ext4", "xfs", "btrfs")


@pytest.mark.skipif(not hasattr(os, "posix_fadvise") or not Path("/proc/mounts").exists(), reason="Linux page cache")
def test_eviction_drops_every_file_of_a_fixture_from_the_page_cache(tmp_path):
    """Evict a fixture's files, a directory store's nested ones too, and count the pages before and after.

    Args:
        tmp_path: A directory for the fixture.
    """
    store = tmp_path / "f.zarr"
    (store / "c" / "0").mkdir(parents=True)
    (store / "zarr.json").write_bytes(b"{}")
    (store / "c" / "0" / "0").write_bytes(os.urandom(1 << 20))  # just written: dirty pages until flushed
    (store / "empty").write_bytes(b"")
    page = perf_matrix.resource.getpagesize()
    first = perf_matrix.evict(store)
    fs = _fs_type(tmp_path)
    if first["resident_after"] and fs not in EVICTING:  # tmpfs, or overlayfs in some containers
        pytest.skip(f"{fs} keeps pages that POSIX_FADV_DONTNEED drops elsewhere")
    assert first["cache_pages"] == 1 + (1 << 20) // page and first["resident_after"] == 0
    perf_matrix.rewarm(store)
    again = perf_matrix.evict(store)
    if not again["resident_before"]:
        pytest.skip("reads do not fill the page cache here (ZFS keeps its own cache)")
    assert again["cache_pages"] == first["cache_pages"] and again["resident_before"] > 0 and again["resident_after"] == 0
    one = store / "c" / "0" / "0"
    assert perf_matrix.evict(one)["cache_pages"] == (1 << 20) // page


def test_cold_reads_evict_before_each_sample_and_refuse_a_cached_fixture(tmp_path, monkeypatch):
    """Read a cold case from the local path with eviction, fail a sample whose fixture stayed cached,
    and read the fixture into the page cache before the warm case.

    Args:
        tmp_path: Isolated fixture and results directory.
        monkeypatch: Scoped dependency replacements.
    """
    fake = SimpleNamespace(FIXTURES={"a": None}, ACCESS=["full"], EXT=".tif", meta=lambda *_: {"need_bytes": 1},
                           axes=lambda _: {"codec": "none", "dtype": "uint8", "layout": "cog"})
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix, "machine", lambda: {"machine": {"on_ac": True}})
    monkeypatch.setattr(perf_matrix, "report", lambda _: "recorded")
    monkeypatch.setattr(perf_matrix, "_settled_log", lambda: [])
    rewarmed, specs = [], []
    monkeypatch.setattr(perf_matrix, "rewarm", rewarmed.append)

    def worker(cmd, **_kwargs):
        spec = json.loads(cmd[-1])
        specs.append(spec)
        stayed = 3 if spec["tool"] == "gdal" and spec["cold"] else 0
        cold = {"cache_pages": 8, "resident_before": 8, "resident_after": stayed, "disk_bytes": 4096}
        sample = {**_sample(1), **cold} if spec["cold"] else _sample(1)
        return SimpleNamespace(returncode=0, stdout=json.dumps(sample), stderr="")

    monkeypatch.setattr(perf_matrix.subprocess, "run", worker)
    args = SimpleNamespace(format="fake", dir=tmp_path, latency_ms=0, threads=1, only=None, quick=True, samples=1,
                           base=None, out=tmp_path / "results.jsonl", source=["local", "cold"], allow_battery=False,
                           access=None, timeout=1)
    perf_matrix.run(args)
    assert {(s["src"], s["cold"]) for s in specs} == {(str(tmp_path / "a.tif"), False), (str(tmp_path / "a.tif"), True)}
    assert rewarmed == [tmp_path / "a.tif"]  # before the warm case only
    rows = [json.loads(line) for line in (tmp_path / "results.jsonl").read_text().splitlines()[1:]]
    assert [r["source"] for r in rows] == ["local", "cold"]
    assert "gdal_error" not in rows[0] and rows[1]["gdal_error"] == "not evicted: 3 of 8 pages stayed in the page cache"


@pytest.mark.parametrize("fmt,ok", [("geotiff", True), ("zarr", True), ("warp", False), ("geotiff-write", False)])
def test_cold_reads_are_a_source_of_the_read_formats_on_request(tmp_path, monkeypatch, fmt, ok):
    """Accept `--source cold` for the reads, not for operations and writes.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped argument and runner replacements.
        fmt: The format.
        ok: Whether the command line is accepted.
    """
    ran = []
    monkeypatch.setattr(perf_matrix, "run", ran.append)
    monkeypatch.setattr(sys, "argv", ["perf_matrix.py", "run", "--dir", str(tmp_path), "--format", fmt, "--source", "cold"])
    if ok:
        perf_matrix.main()
        assert ran[0].source == ["cold"]
    else:
        with pytest.raises(SystemExit) as exc:
            perf_matrix.main()
        assert exc.value.code == 2 and not ran


def test_report_compares_cold_reads_with_warm_ones():
    """Give each tool's cold time over its warm time, and both ratios, over the cases read both ways."""
    def case(source, t, g):
        return summarize({**_row(terrio=[_sample(t)], gdal=[_sample(g)]), "source": source})

    text = report([{"machine": _MACHINE}, case("local", 1.0, 2.0), case("cold", 4.0, 4.0)])
    assert "geotiff cold time / warm time: terrio x4.00  gdal x2.00  terrio/gdal time warm x0.50 cold x1.00  (1)" in text
    assert "## Cold page cache" not in report([case("local", 1.0, 2.0)])


def test_a_cold_read_that_took_nothing_from_storage_is_refused(tmp_path, monkeypatch):
    """Refuse a cold sample that read no bytes from a block device: a cache `mincore` does not see served it.

    Args:
        tmp_path: Isolated fixture and results directory.
        monkeypatch: Scoped dependency replacements.
    """
    fake = SimpleNamespace(FIXTURES={"a": None}, ACCESS=["full"], EXT=".tif", meta=lambda *_: {"need_bytes": 1},
                           axes=lambda _: {"codec": "none", "dtype": "uint8", "layout": "cog"})
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix, "machine", lambda: {"machine": {"on_ac": True}})
    monkeypatch.setattr(perf_matrix, "report", lambda _: "recorded")
    monkeypatch.setattr(perf_matrix, "_settled_log", lambda: [])
    monkeypatch.setattr(perf_matrix, "rewarm", lambda _: None)
    sample = {**_sample(1), "cache_pages": 8, "resident_before": 0, "resident_after": 0, "disk_bytes": 0}
    monkeypatch.setattr(perf_matrix.subprocess, "run", lambda *_a, **_k: SimpleNamespace(returncode=0, stdout=json.dumps(sample), stderr=""))
    args = SimpleNamespace(format="fake", dir=tmp_path, latency_ms=0, threads=1, only=None, quick=True, samples=1,
                           base=None, out=tmp_path / "results.jsonl", source=["cold"], allow_battery=False, access=None, timeout=1)
    perf_matrix.run(args)
    row = json.loads((tmp_path / "results.jsonl").read_text().splitlines()[1])
    assert row["terrio_error"] == row["gdal_error"] == "not cold: the read took no bytes from a block device"


def test_a_cold_worker_evicts_after_the_warm_up_and_counts_storage_bytes_over_the_read(monkeypatch):
    """Evict after the warm-up and outside `init`, and count the block-device bytes of the timed read only.

    Args:
        monkeypatch: Scoped format and counter replacements.
    """
    events = []

    def reader(_tool, _access, _meta):
        return (lambda p: events.append("open") or p), [lambda _d: events.append("read") or [1]], perf_matrix.checksum, (
            lambda p: p, lambda _d: events.append("warm-up"))

    def evict(path):
        events.append("evict")
        return {"cache_pages": 2, "resident_before": 2, "resident_after": 0}

    monkeypatch.setitem(perf_matrix.FORMATS, "fake", SimpleNamespace(EXT=".tif", reader=reader))
    monkeypatch.setattr(perf_matrix, "evict", evict)
    # The block device gives 4096 bytes to the read, none to the warm-up or the eviction.
    monkeypatch.setattr(perf_matrix, "_proc_io", lambda: {"rchar": 0, "read_bytes": 1000 + 4096 * ("read" in events)})
    out = perf_matrix.worker({"format": "fake", "tool": "terrio", "src": "/d/f.tif", "access": "full", "meta": {}, "cold": True})
    assert events == ["warm-up", "evict", "open", "read"]
    assert out["disk_bytes"] == 4096 and out["resident_after"] == 0 and out["cache_pages"] == 2


def test_flatgeobuf_index_levels_end_at_one_root_and_refuse_no_features():
    """Lay out a packed R-tree's levels down to one root, and refuse an index of no features."""
    assert perf_matrix.FlatGeobuf.levels(1) == [(1, 2), (0, 1)]
    assert perf_matrix.FlatGeobuf.levels(17)[-1] == (0, 1)
    with pytest.raises(ValueError):
        perf_matrix.FlatGeobuf.levels(0)
    with pytest.raises(ValueError, match="unknown"):
        perf_matrix.FlatGeobuf.levels(-1)


def test_flatgeobuf_needed_bytes_walk_every_level_of_a_deep_index(tmp_path, monkeypatch):
    """Count the nodes a bbox search reads through a root, branch levels and leaves.

    A node is read when its parent's box meets the bbox, so the expected count is the root's
    plus the children of every branch node that meets it, computed from the whole index rather
    than by walking it level by level.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped dependency replacements.
    """
    ogr = pytest.importorskip("osgeo.ogr")
    np = pytest.importorskip("numpy")
    if ogr.GetDriverByName("FlatGeobuf") is None:
        pytest.skip("FlatGeobuf driver is required")
    ogr.UseExceptions()
    # 300 points on a 20 x 15 grid over the extent: 300 leaves, 19, 2 and a root.
    points = [(2500 + 5000 * i, 3333 + 6666 * j) for i in range(20) for j in range(15)]
    name = "grid_index"
    monkeypatch.setitem(perf_matrix.FlatGeobuf.FIXTURES, name, ("point", len(points), True))
    path = tmp_path / f"{name}.fgb"
    ds = ogr.GetDriverByName("FlatGeobuf").CreateDataSource(str(path))
    lyr = ds.CreateLayer("data", None, ogr.wkbPoint, ["SPATIAL_INDEX=YES"])
    lyr.CreateField(ogr.FieldDefn("id", ogr.OFTInteger64))
    for i, (x, y) in enumerate(points):
        f = ogr.Feature(lyr.GetLayerDefn())
        f["id"] = i
        f.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({x} {y})"))
        lyr.CreateFeature(f)
    lyr = ds = None

    levels = perf_matrix.FlatGeobuf.levels(len(points))
    assert len(levels) == 4
    raw = path.read_bytes()
    header = 12 + int.from_bytes(raw[8:12], "little")
    num_nodes = levels[0][1]
    item = np.dtype([("x0", "<f8"), ("y0", "<f8"), ("x1", "<f8"), ("y1", "<f8"), ("off", "<u8")])
    index = np.frombuffer(raw[header:header + num_nodes * 40], item)
    feature = (len(raw) - header - num_nodes * 40) // len(points)  # equal-sized features

    for access, boxes in (("bbox", perf_matrix.GeoParquet.bboxes(1)), ("bbox16", perf_matrix.GeoParquet.bboxes(16))):
        hit = np.zeros(num_nodes, bool)
        for bx0, by0, bx1, by1 in boxes:
            hit |= (index["x0"] <= bx1) & (index["y0"] <= by1) & (index["x1"] >= bx0) & (index["y1"] >= by0)
        read = np.zeros(num_nodes, bool)
        read[levels[-1][0]:levels[-1][1]] = True
        for i in np.flatnonzero(hit[:levels[0][0]]):  # branch nodes that meet a box
            level_end = next(b for a, b in levels if a <= int(index["off"][i]) < b)
            read[int(index["off"][i]):min(int(index["off"][i]) + perf_matrix.FlatGeobuf.NODE_SIZE, level_end)] = True
        # Every hit at a level below the root was read: its parent met the box too.
        assert not (hit & ~read)[levels[-1][1]:].any()
        found = sum(any(bx0 <= x <= bx1 and by0 <= y <= by1 for bx0, by0, bx1, by1 in boxes) for x, y in points)
        assert 0 < found < len(points)
        assert read.sum() < num_nodes  # the search skips part of the index
        expected = header + int(read.sum()) * 40 + found * feature
        assert perf_matrix.FlatGeobuf.meta(path, access)["need_bytes"] == expected


def test_flatgeobuf_gdal_read_names_the_geometry_as_terrio(tmp_path):
    """Name GDAL's FlatGeobuf geometry column `geometry`, as terrio does, so the digests compare."""
    ogr = pytest.importorskip("osgeo.ogr")
    pytest.importorskip("pyogrio")
    pytest.importorskip("pyarrow")
    if ogr.GetDriverByName("FlatGeobuf") is None:
        pytest.skip("FlatGeobuf driver is required")
    ogr.UseExceptions()
    path = tmp_path / "tiny.fgb"
    ds = ogr.GetDriverByName("FlatGeobuf").CreateDataSource(str(path))
    lyr = ds.CreateLayer("data", None, ogr.wkbPoint)
    lyr.CreateField(ogr.FieldDefn("id", ogr.OFTInteger64))
    f = ogr.Feature(lyr.GetLayerDefn())
    f["id"] = 1
    f.SetGeometry(ogr.CreateGeometryFromWkt("POINT (1 2)"))
    lyr.CreateFeature(f)
    lyr = ds = None
    open_, items, check, _ = perf_matrix.FlatGeobuf.reader("gdal", "full", {})
    table = items[0](open_(str(path)))
    assert table.column_names == ["id", "geometry"]
    assert check(table) == (1.0, 1)


def test_geopackage_gdal_read_names_the_geometry_as_terrio(tmp_path):
    """Name GDAL's GeoPackage geometry column (`geom`) `geometry`, as terrio does, so the digests compare."""
    ogr = pytest.importorskip("osgeo.ogr")
    pytest.importorskip("pyogrio")
    pytest.importorskip("pyarrow")
    ogr.UseExceptions()
    path = tmp_path / "tiny.gpkg"
    ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(path))
    lyr = ds.CreateLayer(perf_matrix.GeoPackage.LAYER, None, ogr.wkbPoint)
    lyr.CreateField(ogr.FieldDefn("id", ogr.OFTInteger64))
    f = ogr.Feature(lyr.GetLayerDefn())
    f["id"] = 1
    f.SetGeometry(ogr.CreateGeometryFromWkt("POINT (1 2)"))
    lyr.CreateFeature(f)
    lyr = ds = None
    open_, items, _, _ = perf_matrix.GeoPackage.reader("gdal", "full", {})
    assert items[0](open_(str(path))).column_names == ["id", "geometry"]


def test_a_download_is_kept_only_when_it_is_the_pinned_file(tmp_path):
    """Publish a download whose size and SHA-256 are the pinned ones; refuse any other; reuse a kept one."""
    import hashlib

    src = tmp_path / "source.bin"
    src.write_bytes(b"real data" * 1000)
    size, sha = src.stat().st_size, hashlib.sha256(src.read_bytes()).hexdigest()
    out = tmp_path / "out"
    out.mkdir()
    with pytest.raises(ValueError, match="not the pinned"):
        perf_matrix.fetch(out / "a.tif", src.as_uri(), size, "0" * 64)
    with pytest.raises(ValueError, match="not the pinned"):
        perf_matrix.fetch(out / "a.tif", src.as_uri(), size + 1, sha)
    big = tmp_path / "big.bin"
    big.write_bytes(bytes(3 << 20))
    with pytest.raises(ValueError, match="more than the pinned 1 bytes"):  # stopped at the first piece past the pinned size
        perf_matrix.fetch(out / "a.tif", big.as_uri(), 1, sha)
    assert list(out.iterdir()) == []  # neither the file nor its staging directory is left
    perf_matrix.fetch(out / "a.tif", src.as_uri(), size, sha)
    assert (out / "a.tif").read_bytes() == src.read_bytes()
    perf_matrix.fetch(out / "a.tif", (tmp_path / "gone.bin").as_uri(), size, sha)  # kept: not downloaded again
    (out / "a.tif").write_bytes(b"edited")
    with pytest.raises(ValueError, match="not the pinned"):  # a kept file is checked too
        perf_matrix.fetch(out / "a.tif", (tmp_path / "gone.bin").as_uri(), size, sha)


def test_real_data_formats_are_registered_and_pinned():
    """Each real-data fixture is a pinned download or made from one, and its format is measured like its parent's."""
    for name, parent in (("geotiff-real", "geotiff"), ("geoparquet-real", "geoparquet")):
        fmt = perf_matrix.FORMATS[name]
        assert perf_matrix.PATHS[name] == perf_matrix.PATHS[parent]
        made = {**fmt.DOWNLOADS, **getattr(fmt, "DERIVED", {})}
        assert set(fmt.FIXTURES) == set(made)
        for fixture in fmt.FIXTURES:
            assert set(fmt.axes(fixture)) == {"codec", "dtype", "layout"}
            assert fixture.isidentifier()  # the `where` read names the layer after the file
        for url, size, sha in fmt.DOWNLOADS.values():
            assert url.startswith("https://") and size > 0 and len(sha) == 64 and set(sha) <= set("0123456789abcdef")
        assert all(src in fmt.DOWNLOADS for src, _ in getattr(fmt, "DERIVED", {}).values())
    assert perf_matrix.accesses(perf_matrix.GeoTiffReal)[-1] == "masks"
    assert "masks" not in perf_matrix.accesses(perf_matrix.GeoTiff)


def test_geoparquet_real_reads_its_own_extent_columns_and_filter(tmp_path):
    """Boxes lie in the file's extent, and the filter's own column prunes row groups by their statistics."""
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    pytest.importorskip("pyogrio")  # the GDAL reader
    real = perf_matrix.GeoParquetReal
    x0, y0, x1, y1 = real.EXTENT
    assert all(x0 <= a < c <= x1 and y0 <= b < d <= y1 for a, b, c, d in real.bboxes(16))
    assert perf_matrix.GeoParquet.bboxes(1) != real.bboxes(1)
    table = pa.table({"area_in_meters": [1.0, 2.0, 50.0, 60.0], "bf_source": ["a"] * 4, "geometry": [b"x"] * 4})
    geo = {"primary_column": "geometry", "columns": {"geometry": {}}}
    path = tmp_path / "buildings.parquet"
    pq.write_table(table.replace_schema_metadata({b"geo": json.dumps(geo).encode()}), path, row_group_size=2)
    md = pq.ParquetFile(path).metadata
    first = sum(md.row_group(0).column(c).total_compressed_size for c in range(md.num_columns))
    assert real.meta(path, "where")["need_bytes"] == md.serialized_size + 8 + first
    check = real.reader("gdal", "columns", {})[2]
    assert check(table) == (113.0, 4)


def test_geotiff_reads_of_a_raster_that_is_not_square_cover_it_whole(tmp_path):
    """The whole and decimated reads cover both sides; the windows stay inside; an all-valid mask needs no block."""
    np = pytest.importorskip("numpy")
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    path = tmp_path / "tall.tif"
    gdal.Translate(str(path), _mem(np, gdal, np.arange(600 * 1100, dtype="float32").reshape(1100, 600)), format="GTiff",
                   creationOptions=["TILED=YES", "BLOCKXSIZE=256", "BLOCKYSIZE=256", "COMPRESS=DEFLATE"])
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1)
    every = sum(int(band.GetMetadataItem(f"BLOCK_SIZE_{x}_{y}", "TIFF")) for y in range(5) for x in range(3))
    ds = band = None
    meta = perf_matrix.GeoTiff.meta(path, "full")
    assert (meta["size"], meta["xsize"], meta["ysize"], meta["need_bytes"]) == (600, 600, 1100, every)
    assert perf_matrix.GeoTiffReal.meta(path, "masks")["need_bytes"] == 0  # all valid: no pixel is read
    assert all(x + w <= 600 and y + h <= 600 for x, y, w, h in windows(meta["size"], 16))
    open_, items, _, _ = perf_matrix.GeoTiff.reader("gdal", "overview", meta)
    assert items[0](open_(str(path))).shape == (1100 // 8, 600 // 8)


@pytest.mark.parametrize("mask_band", [False, True])
def test_masks_are_read_by_both_tools_and_need_their_own_blocks(tmp_path, mask_band):
    """terrio's `read_masks` and GDAL's mask bands hash the same: 0 where nodata or masked, 255 elsewhere.

    A mask from nodata needs the pixel blocks; a mask band needs its own blocks only.

    Args:
        tmp_path: Isolated fixture directory.
        mask_band: Whether the file has an internal mask band in place of nodata.
    """
    np = pytest.importorskip("numpy")
    gdal = pytest.importorskip("osgeo.gdal")
    pytest.importorskip("terrio.rio")
    gdal.UseExceptions()
    a = np.ones((64, 64), "uint8")
    a[:20] = 0
    path = tmp_path / "masked.tif"
    opts = {"maskBand": "mask,1", "noData": "none"} if mask_band else {}
    gdal.Translate(str(path), _mem(np, gdal, a, nodata=0), format="GTiff", creationOptions=["TILED=YES", "BLOCKXSIZE=32", "BLOCKYSIZE=32"], **opts)
    gdal.Translate(str(tmp_path / "_warmup.tif"), _mem(np, gdal, a), format="GTiff")
    meta = perf_matrix.GeoTiffReal.meta(path, "masks")
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1).GetMaskBand() if mask_band else ds.GetRasterBand(1)
    assert meta["need_bytes"] == sum(int(band.GetMetadataItem(f"BLOCK_SIZE_{x}_{y}", "TIFF")) for y in range(2) for x in range(2))
    ds = band = None
    samples = {tool: perf_matrix.worker({"format": "geotiff-real", "tool": tool, "src": str(path),
                                         "access": "masks", "meta": meta}) for tool in ("gdal", "terrio")}
    assert samples["gdal"]["sum"] == 255 * 44 * 64 and samples["gdal"]["n"] == 64 * 64
    assert samples["terrio"]["digest"] == samples["gdal"]["digest"]


@pytest.mark.parametrize("fmt", ["geoparquet", "geoparquet-real", "geopackage"])
def test_sql_filter_reads_are_labelled_against_gdals_attribute_filter(tmp_path, monkeypatch, fmt):
    """terrio's `where` is `terrio.sql` and GDAL's `read_arrow(where=...)`: the row says so for each vector format.

    Args:
        tmp_path: Isolated results directory.
        monkeypatch: Scoped dependency replacements.
        fmt: A format whose `where` read is terrio's SQL.
    """
    fake = SimpleNamespace(FIXTURES={"a": None}, ACCESS=["where"], EXT=".parquet", meta=lambda *_: {"need_bytes": 1},
                           axes=lambda _: {"codec": "none", "dtype": "polygon", "layout": "x"})
    monkeypatch.setitem(perf_matrix.FORMATS, fmt, fake)
    monkeypatch.setattr(perf_matrix, "machine", lambda: {"machine": {"on_ac": True}})
    monkeypatch.setattr(perf_matrix, "report", lambda _: "recorded")
    monkeypatch.setattr(perf_matrix, "_settled_log", lambda: [])
    monkeypatch.setattr(perf_matrix.subprocess, "run", lambda *_a, **_k: SimpleNamespace(returncode=0, stdout=json.dumps(_sample(1)), stderr=""))
    args = SimpleNamespace(format=fmt, dir=tmp_path, latency_ms=0, threads=1, only=None, quick=True, samples=1, base=None,
                           out=tmp_path / "results.jsonl", source=["local"], allow_battery=False, access=None, timeout=1)
    perf_matrix.run(args)
    rows = [json.loads(line) for line in (tmp_path / "results.jsonl").read_text().splitlines()[1:]]
    assert [r["access"] for r in rows] == ["where/sql-vs-filter"]


def test_gdals_second_configuration_gets_its_own_ratio_and_its_failure_drops_only_that_ratio():
    """Report terrio against GDAL in both configurations; a failure in the second keeps the others."""
    both = summarize(_row(terrio=[_sample(2)], gdal=[_sample(1)], gdal_allcpus=[_sample(4)], base=[_sample(4)]))
    assert (both["time_ratio"], both["gdal_allcpus_ratio"], both["base_ratio"], both["values"]) == (2, 0.5, 0.5, "checksum_match")
    failed = summarize(_row(terrio=[_sample(2)], gdal=[_sample(1)], gdal_allcpus=[], base=[_sample(4)],
                            gdal_allcpus_error="timeout: over 120 s"))
    assert (failed["time_ratio"], failed["base_ratio"], failed["values"]) == (2, 0.5, "checksum_match")
    assert "gdal_allcpus_ratio" not in failed
    # Values unlike GDAL's drop its ratio too, and leave the case's `values` to terrio's.
    differ = summarize(_row(terrio=[_sample(2)], gdal=[_sample(1)], gdal_allcpus=[_sample(4, n=5)]))
    assert differ["values"] == "checksum_match" and "gdal_allcpus_ratio" not in differ
    # Equal digests match although a NaN read's sums do not.
    nan = summarize(_row(**{t: [{**_sample(k, s=float("nan")), "digest": "a"}] for t, k in (("terrio", 2), ("gdal", 1), ("gdal_allcpus", 4))}))
    assert (nan["values"], nan["gdal_allcpus_ratio"], nan["gdal_allcpus_values_match"]) == ("exact", 0.5, True)
    # Both GDAL configurations failed: the case is an error, not a failure of the second only.
    both_failed = summarize(_row(terrio=[_sample(2)], gdal=[], gdal_allcpus=[], gdal_error="exit 1", gdal_allcpus_error="exit 1"))
    text = report([{"machine": {**_MACHINE, "gdal_allcpus": True}}, both, {**failed, "fixture": "f32_none"},
                   {**differ, "fixture": "f64_deflate_p3"}, {**both_failed, "fixture": "i16_lzw_strip"}])
    assert "all     x2.00 / x0.50(1)" in text and "source  local x2.00 / x0.50(1)" in text
    assert "terrio faster than GDAL in one configuration only: 1\n  u8_none" in text
    section = text.split("this configuration only (no second ratio): ")[1].split("\n\n")[0]
    assert section.startswith("2\n") and "i16_lzw_strip" not in section
    assert "f32_none" in section and "gdal ALL_CPUS timeout: over 120 s" in section
    assert "f64_deflate_p3" in section and "gdal ALL_CPUS values differ from the first configuration" in section
    assert "ALL_CPUS" not in report([{"machine": _MACHINE}, summarize(_row(terrio=[_sample(2)], gdal=[_sample(1)]))])
    # A second-configuration failure is listed when terrio's own values differ from GDAL's too.
    unranked = summarize(_row(terrio=[_sample(2, s=11.0)], gdal=[_sample(1)], gdal_allcpus=[], gdal_allcpus_error="timeout: over 120 s"))
    text = report([{"machine": {**_MACHINE, "gdal_allcpus": True}}, both, {**unranked, "fixture": "u16_zstd"}])
    section = text.split("this configuration only (no second ratio): ")[1].split("\n\n")[0]
    assert section.startswith("1\n") and "u16_zstd" in section and "gdal ALL_CPUS timeout: over 120 s" in section


def test_gdals_second_configuration_runs_with_its_default_cache_and_every_cpu(tmp_path, monkeypatch):
    """Time GDAL a second time with GDAL_CACHEMAX unset and GDAL_NUM_THREADS=ALL_CPUS, in turn with the others.

    Args:
        tmp_path: Isolated results directory.
        monkeypatch: Scoped dependency replacements.
    """
    fake = SimpleNamespace(FIXTURES={"a": None}, ACCESS=["full"], EXT=".tif", meta=lambda *_: {"need_bytes": 1},
                           axes=lambda _: {"codec": "none", "dtype": "uint8", "layout": "cog"})
    monkeypatch.setitem(perf_matrix.FORMATS, "fake", fake)
    monkeypatch.setattr(perf_matrix, "machine", lambda: {"machine": {"on_ac": True}})
    monkeypatch.setattr(perf_matrix, "report", lambda _: "recorded")
    monkeypatch.setattr(perf_matrix, "_settled_log", lambda: [])
    monkeypatch.setenv("GDAL_CACHEMAX", "64")  # a shell's value is not the default either
    envs = {}

    def worker(cmd, env, **_kwargs):
        envs.setdefault(json.loads(cmd[-1])["tool"], []).append(env)
        return SimpleNamespace(returncode=0, stdout=json.dumps(_sample(1)), stderr="")

    monkeypatch.setattr(perf_matrix.subprocess, "run", worker)
    args = SimpleNamespace(format="fake", dir=tmp_path, latency_ms=0, threads=4, only=None, quick=False, samples=2,
                           base=None, out=tmp_path / "results.jsonl", source=["local"], allow_battery=False,
                           access=None, timeout=1, gdal_allcpus=True)
    perf_matrix.run(args)
    assert {t: len(e) for t, e in envs.items()} == {"terrio": 2, "gdal": 2, "gdal_allcpus": 2}
    second = envs["gdal_allcpus"][0]
    assert "GDAL_CACHEMAX" not in second and second["GDAL_NUM_THREADS"] == "ALL_CPUS"
    assert envs["gdal"][0]["GDAL_CACHEMAX"] == "512" and envs["gdal"][0]["GDAL_NUM_THREADS"] == "4"
    head = json.loads((tmp_path / "results.jsonl").read_text().splitlines()[0])
    assert head["machine"]["gdal_allcpus"] is True


def test_gdals_second_configuration_reads_with_gdal(monkeypatch):
    """Read with GDAL's reader in a `gdal_allcpus` worker.

    Args:
        monkeypatch: Scoped format replacement.
    """
    tools = []

    def reader(tool, _access, _meta):
        tools.append(tool)
        return (lambda p: p), [lambda _d: [1]], perf_matrix.checksum, (lambda p: p, lambda _d: None)

    monkeypatch.setitem(perf_matrix.FORMATS, "fake", SimpleNamespace(EXT=".tif", reader=reader))
    perf_matrix.worker({"format": "fake", "tool": "gdal_allcpus", "src": "http://localhost/f.tif", "access": "full", "meta": {}})
    assert tools == ["gdal"]


def test_rasterize_and_zonal_are_chosen_by_their_operations():
    """The rasterizer picks both formats, as zonal statistics grid their zones with it."""
    assert perf_matrix.touched(["crates/terrio/src/ops/rasterize.rs"]) == ["rasterize", "zonal"]
    assert perf_matrix.touched(["crates/terrio/src/ops/zonal_reduce.rs", "python/src/analysis.rs"]) == ["zonal", "vectorize"]


def _polygons(ogr, osr, path, rings, points=False):
    """A FlatGeobuf file of polygons (or of their first vertices) with `id` and `value`, in UTM 31N.

    Args:
        ogr: OGR's module.
        osr: OSR's module.
        path: The file.
        rings: Each polygon's vertices.
        points: Write each ring's first vertex as a point instead.
    """
    srs = osr.SpatialReference()
    srs.ImportFromEPSG(32631)
    ds = ogr.GetDriverByName("FlatGeobuf").CreateDataSource(str(path))
    lyr = ds.CreateLayer("data", srs, ogr.wkbPoint if points else ogr.wkbPolygon)
    for name, kind in (("id", ogr.OFTInteger64), ("value", ogr.OFTReal)):
        lyr.CreateField(ogr.FieldDefn(name, kind))
    for i, ring in enumerate(rings):
        f = ogr.Feature(lyr.GetLayerDefn())
        f["id"], f["value"] = i, 10.5 * (i + 1)
        pts = ", ".join(f"{x} {y}" for x, y in [*ring, ring[0]])
        f.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({ring[0][0]} {ring[0][1]})" if points else f"POLYGON (({pts}))"))
        lyr.CreateFeature(f)
    lyr = ds = None


@pytest.mark.parametrize("fmt,access", [("rasterize", "burn"), ("rasterize", "attribute_all_touched"), ("rasterize", "burn_t1"),
                                        ("zonal", "fields"), ("zonal", "fields_all_touched")])
def test_rasterize_and_zonal_run_in_a_worker_and_match_gdal(tmp_path, monkeypatch, fmt, access):
    """Run a rasterization or zonal statistics as the runner's worker does, on small files, against GDAL's output.

    Args:
        tmp_path: Isolated fixture directory.
        monkeypatch: Scoped environment and grid replacements.
        fmt: The operation's format.
        access: The operation.
    """
    np = pytest.importorskip("numpy")
    gdal = pytest.importorskip("osgeo.gdal")
    ogr, osr = pytest.importorskip("osgeo.ogr"), pytest.importorskip("osgeo.osr")
    pytest.importorskip("terrio")
    gdal.UseExceptions()
    for name in ("RAYON_NUM_THREADS", "GDAL_NUM_THREADS"):
        monkeypatch.setenv(name, "2")
    # Off the pixel grid, overlapping, one reaching past the raster's edge.
    rings = [[(x, y), (x + 431.3, y + 87.1), (x + 254.9, y + 503.7)]
             for x, y in ((500113.7, 4998021.3), (500301.2, 4998219.9), (502401.1, 4999800.4))]
    op = perf_matrix.FORMATS[fmt]
    if fmt == "rasterize":
        monkeypatch.setattr(op, "RES", 1000.0)  # 100 x 100 pixels over the fixtures' extent
        rings = [[(x - 500000, y - 4900000) for x, y in r] for r in rings]
        src = tmp_path / "poly_index.fgb"
        _polygons(ogr, osr, src, rings)
        _polygons(ogr, osr, tmp_path / "_warmup.fgb", rings, points=True)
    else:
        src = tmp_path / "f32_none.tif"
        gdal.Translate(str(src), _mem(np, gdal, perf_matrix.terrain(np, 256, 1, "float32")[0]), format="GTiff")
        warm = _mem(np, gdal, np.ones((64, 64), "float32"), gt=(500000, 10, 0, 4959680, 0, -10))
        gdal.Translate(str(tmp_path / "_zonal_warmup.tif"), warm, format="GTiff")
        gdal.Translate(str(tmp_path / "_warmup.tif"), warm, format="GTiff")
        _polygons(ogr, osr, tmp_path / "_zones_fields.fgb", rings)
        _polygons(ogr, osr, tmp_path / "_zones_warmup.fgb", [[(500000, 4959040), (500300, 4959040), (500300, 4959340)]])
    meta = op.meta(src, access)
    samples = {tool: perf_matrix.worker({"format": fmt, "tool": tool, "src": str(src), "access": access, "meta": meta})
               for tool in ("gdal", "terrio")}
    assert os.environ["RAYON_NUM_THREADS"] == ("1" if access.endswith("_t1") else "2")
    c = samples["terrio"]["compare"]
    assert c["n"] == samples["gdal"]["n"] == (10_000 if fmt == "rasterize" else 15) and c["beyond_tol"] == 0, c
    assert not c["mismatch"], c
    row = summarize({**_row(terrio=[samples["terrio"]], gdal=[samples["gdal"]]), "format": fmt, "access": access})
    assert row["values"] in ("exact", "within_tol")


def test_zonal_statistics_are_compared_zone_by_zone():
    """Match zones by id; a value missing in both tools agrees, one missing in one tool differs."""
    nan = float("nan")
    stats = perf_matrix.Zonal.STATS

    def ours(rows):
        return SimpleNamespace(columns=lambda: {"id": [r[0] for r in rows],
                                                **{f"raster1_b1_{s}": [r[k + 1] for r in rows] for k, s in enumerate(stats)}})

    def theirs(rows):
        return SimpleNamespace(GetLayer=lambda _: [{"id": r[0], **dict(zip(stats, r[1:], strict=True))} for r in rows])

    gdal_rows = [(1, 4.0, 10.0, 2.5, 1.0, 4.0), (2, 0.0, 0.0, nan, None, None)]  # zone 2 is empty
    same = perf_matrix.compare_zonal(ours([(2, 0, 0.0, None, None, None), (1, 4, 10.0 + 1e-12, 2.5, 1.0, 4.0)]), theirs(gdal_rows), 1e-9)
    assert (same["n"], same["differing"], same["beyond_tol"], same["mismatch"]) == (10, 1, 0, "")
    empty = perf_matrix.compare_zonal(ours([(1, 4, 10.0, 2.5, 1.0, 4.0), (2, 0, None, None, None, None)]), theirs(gdal_rows), 1e-9)
    assert (empty["beyond_tol"], empty["max_diff"], empty["worst"]) == (1, 0.0, "zone 2 sum: None vs GDAL's 0.0")
    off = perf_matrix.compare_zonal(ours([(1, 5, 12.0, 2.4, 1.0, 4.0), (2, 0, 0.0, None, None, None)]), theirs(gdal_rows), 1e-9)
    assert (off["differing"], off["beyond_tol"], off["max_diff"]) == (3, 3, 0.25)  # count 1/4, sum 2/10, mean 0.1/2.5
    other = perf_matrix.compare_zonal(ours([(1, 4, 10.0, 2.5, 1.0, 4.0)]), theirs(gdal_rows), 1e-9)
    assert other["mismatch"] == "zones 1 vs 2, 1 ids in one only" and other["beyond_tol"] == 10
