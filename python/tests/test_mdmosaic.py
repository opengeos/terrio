"""Lazy coordinate-aligned scientific mosaics through the Python API."""

import json

import numpy as np
import pytest
from test_multidim import cube_store

import terrio


def sources():
    first, a = cube_store()
    second, b = cube_store()
    second["time/0"] = np.array([18, 24, 30], dtype="<i8").tobytes()
    for key in second:
        if key.startswith("cube/") and not key.split("/")[-1].startswith("."):
            second[key] = (np.frombuffer(second[key], dtype="<i8") + 100).tobytes()
    return first, second, np.concatenate([a, b + 100], axis=0)


def test_mosaic_uploaded_stores_handles_and_snapshots(tmp_path):
    first, second, expected = sources()
    a, b = terrio.open_multidim(first), terrio.open_multidim(second)
    path = tmp_path / "mosaic.nc"
    output = terrio.mosaic_multidim([a, b], path, arrays=["cube"])
    assert output.array_paths == ["/time", "/x", "/y", "/cube"]
    np.testing.assert_array_equal(output["cube"].read(), expected)
    np.testing.assert_array_equal(output["time"].read(), [0, 6, 12, 18, 24, 30])
    view = output["cube"].slice("time", 1, 3, 2).isel("y", 1).transpose(["x", "time"])
    np.testing.assert_array_equal(view.read(), expected[1::2, 1, :].T)
    np.testing.assert_array_equal(terrio.open_multidim(path)["cube"].read(), expected)
    np.testing.assert_array_equal(
        terrio.open_multidim(output.to_zarr_store())["cube"].read(), expected
    )
    original = path.read_bytes()
    with pytest.raises(terrio.InvalidArgumentError, match="exist"):
        terrio.mosaic_multidim([first, second], path)
    assert path.read_bytes() == original
    with pytest.raises(terrio.InvalidArgumentError, match="at least one"):
        terrio.mosaic_multidim([])
    with pytest.raises(terrio.InvalidArgumentError, match="missing"):
        terrio.mosaic_multidim([a, b], arrays=["missing"])


def test_mosaic_metadata_and_selected_views_skip_damaged_chunks():
    first, second, expected = sources()
    second["cube/0.0.0"] = b"damaged"
    output = terrio.mosaic_multidim([first, second])
    assert output["cube"].shape == (6, 2, 4)
    np.testing.assert_array_equal(output["cube"].slice("time", 0, 3).read(), expected[:3])
    with pytest.raises(terrio.InvalidDataError):
        output["cube"].isel("time", 3).read()


def test_remote_mosaic_fetches_only_selected_chunk_bytes(tmp_path):
    import functools
    import http.server
    import threading

    class Handler(http.server.SimpleHTTPRequestHandler):
        sent = 0
        requests = 0
        files = set()
        lock = threading.Lock()

        def log_message(self, *_):
            pass

        def copyfile(self, source, output):
            payload = source.read()
            with type(self).lock:
                type(self).sent += len(payload)
                type(self).requests += 1
                type(self).files.add(self.path.split("?")[0])
            output.write(payload)

    first, second, expected = sources()
    import json

    for directory, store in [("first.zarr", first), ("second.zarr", second)]:
        metadata = {
            key: json.loads(value)
            for key, value in store.items()
            if key.split("/")[-1] in [".zgroup", ".zattrs", ".zarray"]
        }
        store[".zmetadata"] = json.dumps(
            {"zarr_consolidated_format": 1, "metadata": metadata}
        ).encode()
        for key, value in store.items():
            path = tmp_path / directory / key
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(value)
    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), functools.partial(Handler, directory=str(tmp_path))
    )
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    measurements = []
    try:
        for mode in ["whole", "selected"]:
            urls = [
                f"http://127.0.0.1:{server.server_port}/{name}.zarr?mode={mode}"
                for name in ["first", "second"]
            ]
            mosaic = terrio.mosaic_multidim(urls)
            Handler.sent = Handler.requests = 0
            Handler.files = set()
            array = mosaic["cube"]
            if mode == "whole":
                np.testing.assert_array_equal(array.read(), expected)
            else:
                np.testing.assert_array_equal(
                    array.isel("time", 0).slice("x", 0, 2).read(), expected[0, :, :2]
                )
            assert all("/cube/" in path for path in Handler.files)
            measurements.append((Handler.sent, Handler.requests, len(Handler.files)))
        assert measurements == [(384, 12, 12), (32, 1, 1)]
        print("mosaic HTTP chunk I/O (bytes, GETs, files):", measurements)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.mark.parametrize("kind", ["string", "bytes", "store", "path", "handle", "integer"])
def test_mosaic_requires_input_sequence(tmp_path, kind):
    store, _ = cube_store()
    source = {
        "string": "a.nc",
        "bytes": b"a.nc",
        "store": store,
        "path": tmp_path / "a.nc",
        "handle": terrio.open_multidim(store),
        "integer": 42,
    }[kind]
    with pytest.raises(terrio.InvalidArgumentError, match="sequence.*wrap a single input"):
        terrio.mosaic_multidim(source)


def test_cf_missing_value_fills_gap_through_uploaded_stores():
    import json

    first, second, values = sources()
    second["time/0"] = np.array([24, 30, 36], dtype="<i8").tobytes()
    for store in [first, second]:
        attrs = json.loads(store["cube/.zattrs"])
        attrs["missing_value"] = -999
        store["cube/.zattrs"] = json.dumps(attrs).encode()
    output = terrio.mosaic_multidim([first, second], arrays=["cube"])
    expected = np.concatenate([values[:3], np.full((1, 2, 4), -999), values[3:]])
    np.testing.assert_array_equal(output["cube"].read(), expected)
    np.testing.assert_array_equal(
        terrio.open_multidim(output.to_zarr_store())["cube"].read(), expected
    )


def test_selected_auxiliary_coordinates_survive_uploaded_snapshots():
    first, second, expected = sources()
    latitude = np.arange(8, dtype="<f8").reshape(2, 4) + 30
    longitude = np.arange(8, dtype="<f8").reshape(2, 4) - 100
    for files in [first, second]:
        attrs = json.loads(files["cube/.zattrs"])
        attrs["coordinates"] = "latitude longitude"
        files["cube/.zattrs"] = json.dumps(attrs).encode()
        for name, values in [("latitude", latitude), ("longitude", longitude)]:
            files[f"{name}/.zarray"] = json.dumps(
                {
                    "zarr_format": 2,
                    "shape": [2, 4],
                    "chunks": [2, 4],
                    "dtype": "<f8",
                    "compressor": None,
                    "fill_value": None,
                    "order": "C",
                    "filters": None,
                }
            ).encode()
            files[f"{name}/.zattrs"] = json.dumps({"_ARRAY_DIMENSIONS": ["y", "x"]}).encode()
            files[f"{name}/0.0"] = values.tobytes()
    output = terrio.mosaic_multidim([first, second], arrays=["cube"])
    assert {"/latitude", "/longitude"} <= set(output["cube"].coordinates)
    assert output["cube"].attributes["coordinates"] == "latitude longitude"
    for snapshot in [
        output,
        terrio.open_multidim(output.to_bytes()),
        terrio.open_multidim(output.to_zarr_store()),
    ]:
        np.testing.assert_array_equal(snapshot["cube"].read(), expected)
        np.testing.assert_array_equal(snapshot["latitude"].read(), latitude)
        np.testing.assert_array_equal(snapshot["longitude"].read(), longitude)
