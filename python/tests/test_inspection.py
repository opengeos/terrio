"""Bounded, read-only dataset identification through the public Python API."""

import numpy as np
import pytest

import terrio


@pytest.fixture
def source(tmp_path):
    path = tmp_path / "source.tif"
    raster = terrio.Raster.from_array(
        np.arange(12, dtype="uint8").reshape(3, 4),
        (0, 1, 0, 3, 0, -1),
        "EPSG:4326",
    )
    raster.write(str(path))
    return path


def test_identify_paths_sequences_uploads_and_failures(source, tmp_path):
    single = terrio.identify(source)
    assert single[0]["driver"] == "GTiff"
    assert single[0]["kinds"] == ["raster"]
    assert not single[0]["deep"]
    assert len(terrio.identify([source, str(source)])) == 2
    uploaded = terrio.identify(source.read_bytes(), filename="renamed.bin")
    assert uploaded[0]["driver"] == "GTiff"
    unknown = tmp_path / "invalid.tif"
    unknown.write_text("not a dataset")
    assert terrio.identify(unknown) == []
    deep_failure = terrio.identify(unknown, deep=True, report_failures=True)
    assert deep_failure[0]["error"] == "format not recognized from the available header"
    assert terrio.identify(unknown, deep=True) == []
    failures = terrio.identify([unknown, tmp_path / "missing"], report_failures=True)
    assert len(failures) == 2
    assert all("error" in record and "driver" not in record for record in failures)
    with pytest.raises(ValueError, match="native paths"):
        terrio.identify(source.read_bytes(), recursive=True)
    with pytest.raises(ValueError, match="paths"):
        terrio.identify([42])


def test_identify_recursive_dataset_directories(source, tmp_path):
    directory = tmp_path / "store.zarr"
    directory.mkdir()
    (directory / ".zgroup").write_text('{"zarr_format": 2}')
    (directory / "nested.tif").write_bytes(source.read_bytes())
    found = terrio.identify(tmp_path, recursive=True)
    assert sorted(record["driver"] for record in found) == ["GTiff", "Zarr"]
    forced = terrio.identify(tmp_path, force_recursive=True)
    assert sorted(record["driver"] for record in forced) == ["GTiff", "GTiff", "Zarr"]


def test_remote_identification_keeps_bounded_reads(source):
    import re
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    with source.open("r+b") as stream:
        stream.truncate(64 * 1024 * 1024)
    size = source.stat().st_size
    counts = {"gets": 0, "bytes": 0}
    mode = {"ranges": True}
    finished = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            done = threading.Event()
            finished.append(done)
            counts["gets"] += 1
            match = re.fullmatch(r"bytes=(\d+)-(\d+)", self.headers.get("Range", ""))
            start, end = 0, size - 1
            if mode["ranges"] and match:
                start, end = int(match[1]), min(int(match[2]), end)
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            else:
                self.send_response(200)
            self.send_header("Content-Length", str(end - start + 1))
            self.end_headers()
            try:
                with source.open("rb") as stream:
                    stream.seek(start)
                    remaining = end - start + 1
                    while remaining:
                        payload = stream.read(min(remaining, 64 * 1024))
                        self.wfile.write(payload)
                        counts["bytes"] += len(payload)
                        remaining -= len(payload)
            except (BrokenPipeError, ConnectionResetError):
                pass
            finally:
                done.set()

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}/source.tif"
            results = []
            for ranges in [True, False]:
                mode["ranges"] = ranges
                counts.update(gets=0, bytes=0)
                finished.clear()
                assert terrio.identify(url)[0]["driver"] == "GTiff"
                for done in finished:
                    assert done.wait(2)
                assert counts["gets"] == 1
                if ranges:
                    assert counts["bytes"] == 16 * 1024
                else:
                    assert counts["bytes"] < size
                results.append(counts.copy())
            print("identify HTTP I/O (range, ignored range):", results)
        finally:
            server.shutdown()
            worker.join()


def test_deep_unknown_http_redacts_names():
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            body = b"unknown dataset"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}/data.unknown?X-Amz-Signature=private"
            result = terrio.identify(url, deep=True, report_failures=True)
            assert result[0]["error"] == "format not recognized from the available header"
            assert "private" not in str(result)
        finally:
            server.shutdown()
            worker.join()
