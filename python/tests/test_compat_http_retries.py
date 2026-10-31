"""`terrio.compat` logs each HTTP request terrio retries as rasterio logs GDAL's warning for it.

Before each retry GDAL warns `HTTP error code: <status> - <url>. Retrying again in <delay> secs`
(`CPLE_AppDefined`, status 0 for a connection error), and rasterio's error handler logs it at
WARNING on the `rasterio._env` logger. The same read, from a loopback server (in this process) that
resets the first two connections and answers the next request 503, runs in a fresh interpreter
with terrio standing in for rasterio and, when rasterio is installed, with rasterio itself.
"""

import importlib.util
import json
import re
import socket
import struct
import subprocess
import sys
import textwrap
import threading
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, HTTPServer

import numpy as np
import pytest

import terrio.rio as trio
from terrio.rio.transform import from_bounds

_SCRIPT = textwrap.dedent(
    """
    import json, logging, sys

    lib, url = sys.argv[1], sys.argv[2]
    if lib == "terrio":
        import terrio.compat
        terrio.compat.install(rioxarray=False, pyogrio=False)
    import rasterio

    records = []

    class Collect(logging.Handler):
        def emit(self, record):
            records.append([record.name, record.levelname, record.getMessage()])

    logger = logging.getLogger("rasterio._env")
    logger.addHandler(Collect())
    logger.setLevel(logging.DEBUG)
    env = dict(GDAL_HTTP_MAX_RETRY=3, GDAL_HTTP_RETRY_DELAY=0.1, GDAL_HTTP_TIMEOUT=20)
    with rasterio.Env(**env), rasterio.open(url) as ds:
        values = ds.read(1).tolist()
    retries = [r for r in records if "Retrying again in" in r[2]]
    origin = rasterio.__spec__.origin or ""
    print(json.dumps({"retries": retries, "values": values, "origin": origin}))
    """
)


@pytest.fixture(scope="module")
def raster(tmp_path_factory) -> bytes:
    """A 4 x 4 uint8 GeoTIFF of 7s, as bytes."""
    path = tmp_path_factory.mktemp("retries") / "raster.tif"
    with trio.open(
        str(path),
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="uint8",
        crs="EPSG:4326",
        transform=from_bounds(0, 0, 1, 1, 4, 4),
    ) as ds:
        ds.write(np.full((1, 4, 4), 7, "uint8"))
    return path.read_bytes()


@contextmanager
def _flaky_server(data: bytes) -> Iterator[str]:
    """Serves `data` at a loopback URL after resetting the first two `GET`s (a connection error)
    and answering the third 503. `HEAD` is answered."""
    gets = []

    class Handler(BaseHTTPRequestHandler):
        def do_HEAD(self) -> None:
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()

        def do_GET(self) -> None:
            gets.append(self.path)
            if len(gets) <= 2:
                linger = struct.pack("ii", 1, 0)
                self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, linger)
                self.close_connection = True
                self.connection.close()
                return
            if len(gets) == 3:
                self.send_error(503)
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}/raster.tif"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(10)


def _read(lib: str, data: bytes) -> tuple[str, dict]:
    """The read in a fresh interpreter, with `lib` as rasterio.

    Args:
        lib: "terrio" (`terrio.compat`) or "rasterio".
        data: The GeoTIFF served.

    Returns:
        tuple: The URL, and the retry records (logger, level, message), the values read and the
            origin of the module imported as rasterio.
    """
    with _flaky_server(data) as url:
        args = [sys.executable, "-c", _SCRIPT, lib, url]
        result = subprocess.run(args, capture_output=True, text=True, timeout=120)
    assert result.returncode == 0, result.stderr
    return url, json.loads(result.stdout.strip().splitlines()[-1])


def _without_later_delays(retries: list, url: str) -> list:
    """The records with the URL replaced, and the delays of the second and later retries."""
    out = []
    for i, (name, level, message) in enumerate(retries):
        message = message.replace(url, "URL")
        if i:
            message = re.sub(r"in [0-9.]+ secs$", "in N secs", message)
        out.append([name, level, message])
    return out


def test_compat_logs_retried_requests_as_gdal_warnings(raster):
    """Two connection errors and a 503: three warnings, the delay doubling from 0.1 s."""
    url, out = _read("terrio", raster)
    assert "terrio" in out["origin"]
    assert out["values"] == [[7] * 4] * 4
    expected = [
        f"CPLE_AppDefined in HTTP error code: {code} - {url}. Retrying again in {delay} secs"
        for code, delay in ((0, "0.1"), (0, "0.2"), (503, "0.4"))
    ]
    assert [r[2] for r in out["retries"]] == expected
    assert [r[:2] for r in out["retries"]] == [["rasterio._env", "WARNING"]] * 3


@pytest.mark.skipif(importlib.util.find_spec("rasterio") is None, reason="rasterio not installed")
def test_compat_retry_warnings_match_rasterios(raster):
    """rasterio's records (GDAL's warnings), but for the delays after the first: GDAL's grow by a
    random factor between 2 and 2.5, terrio's by 2."""
    gdal_url, gdal = _read("rasterio", raster)
    if "terrio" in gdal["origin"]:
        pytest.skip("rasterio is terrio's in this environment")
    url, ours = _read("terrio", raster)
    assert ours["values"] == gdal["values"]
    assert len(gdal["retries"]) == 3
    assert _without_later_delays(ours["retries"], url) == _without_later_delays(
        gdal["retries"], gdal_url
    )
