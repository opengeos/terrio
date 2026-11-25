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


def test_equal_sums_with_different_bytes_are_not_exact():
    """Rank a case as `exact` only when every sample's bytes hash as GDAL's."""
    exact = summarize(_row(terrio=[{**_sample(2), "digest": "a"}], gdal=[{**_sample(1), "digest": "a"}]))
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
    result = perf_matrix.worker({"format": "fake", "tool": "terrio", "src": "http://localhost/file.tif", "access": "full", "meta": {}})
    assert (result["init_s"], result["open_s"], result["read_s"], result["rss_mb"]) == (7, 1, 2, 10)
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
    assert perf_matrix.touched(["crates/terrio/src/drivers/vector/geoparquet_arrow.rs"]) == ["geoparquet"]
    assert perf_matrix.touched(["crates/terrio/src/drivers/zarr/cube.rs", "README.md"]) == ["zarr"]
    assert perf_matrix.touched(["Cargo.lock"]) == ["geotiff", "zarr", "geoparquet", "flatgeobuf", "geopackage"]
    assert perf_matrix.touched(["crates/terrio/src/stream/sqlite.rs"]) == ["geoparquet", "flatgeobuf", "geopackage"]
    assert perf_matrix.touched(["crates/terrio/src/stream/mod.rs"]) == ["geoparquet", "flatgeobuf", "geopackage"]
    assert perf_matrix.touched(["crates/terrio/src/ops/sql/select.rs"]) == ["geopackage"]
    assert perf_matrix.touched(["python/src/raster.rs"]) == ["geotiff", "zarr"]
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
    assert given == ["threads4"] and len(opened) == n
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
