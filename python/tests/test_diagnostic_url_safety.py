# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Dataset diagnostics must not disclose authentication carried by input URLs."""

import io
import logging
import subprocess
import sys
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
import pytest

import terrio
import terrio.pyogrio as pg
import terrio.rio as rio
from terrio._terrio import _redact_diagnostic_location
from terrio.rio import shutil
from terrio.rio.merge import merge
from terrio.rio.stack import stack

SECRET = "PRIVATE-DIAGNOSTIC-TEST-SECRET"


@pytest.mark.parametrize(
    "location,expected",
    [
        ("/data/what?.tif", "/data/what?.tif"),
        ("/data/a?signal=1&monkey=2", "/data/a?signal=1&monkey=2"),
        (
            "/data/a?b.tif: unexpected token, missing key",
            "/data/a?b.tif: unexpected token, missing key",
        ),
        (r"\\?\C:\token-cache\a.tif", r"\\?\C:\token-cache\a.tif"),
        (f"/data/a?token%3D{SECRET}", "/data/a?<redacted>"),
        (f"/data/a?%74oken%3d{SECRET}", "/data/a?<redacted>"),
        (f"/vsimem/a.zip?sig={SECRET}", "/vsimem/a.zip?<redacted>"),
        (f"/data/a?next=https://host/x&sig={SECRET}", "/data/a?<redacted>"),
    ],
)
def test_schemeless_credential_parameters_and_literal_paths(location, expected):
    """Redact real credential parameters while preserving legal filenames and prose.

    Args:
        location: A literal filename or location containing a fake credential.
        expected: Its diagnostic representation.
    """
    assert _redact_diagnostic_location(location) == expected
    assert _redact_diagnostic_location(expected) == expected


@pytest.fixture
def diagnostic_server(tmp_path):
    """Serve real rasters, vectors and archives over loopback with range support.

    Args:
        tmp_path: Directory for generated raster fixtures.

    Yields:
        tuple: Base URL, requests, denied paths, and mutable response fixtures.
    """
    files = {
        "/empty.kml": b'<kml xmlns="http://www.opengis.net/kml/2.2"><Document/></kml>',
        "/notjson.json": b"not JSON",
        "/search": b"{}",
        "/multi.gpx": (
            b'<gpx version="1.1" creator="test"><wpt lat="1" lon="2"><name>point</name></wpt>'
            b'<trk><trkseg><trkpt lat="1" lon="2"/><trkpt lat="2" lon="3"/>'
            b"</trkseg></trk></gpx>"
        ),
    }
    for name, crs in [("a.tif", "EPSG:4326"), ("b.tif", "EPSG:3857")]:
        path = tmp_path / name
        terrio.Raster.from_array(
            np.ones((2, 2), dtype="uint8"), (0, 1, 0, 2, 0, -1), crs=crs
        ).write(str(path))
        files[f"/{name}"] = path.read_bytes()
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w") as z:
        z.writestr("a.tif", files["/a.tif"])
    files["/a.zip"] = archive.getvalue()
    requests, denied = [], set()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            """Suppress server access logs containing the deliberately private URLs."""

        def do_GET(self):
            """Return fixture ranges or controlled errors, recording actual requests."""
            requests.append(self.path)
            path = self.path.split("?")[0]
            data = files.get(path, b"")
            status = (
                403
                if path.startswith("/denied") or path in denied
                else 200
                if path in files
                else 404
            )
            if status == 200 and self.headers.get("Range"):
                first, _, last = self.headers["Range"][6:].partition("-")
                first, last = int(first), min(int(last) if last else len(data) - 1, len(data) - 1)
                status = 206
                self.send_response(status)
                self.send_header("Content-Range", f"bytes {first}-{last}/{len(data)}")
                data = data[first : last + 1]
            else:
                self.send_response(status)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(data)

        do_HEAD = do_GET

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}", requests, denied, files
        finally:
            server.shutdown()
            thread.join()


@pytest.mark.parametrize(
    "auth",
    [
        "sig",
        "X-Amz-Signature",
        "X-Goog-Signature",
        "token",
        "code",
        "auth",
        "jwt",
        "client_secret",
        "session",
        "userinfo",
    ],
)
@pytest.mark.parametrize(
    "op", [rio.open, terrio.open_raster, terrio.info, pg.read_info, pg.list_layers, shutil.delete]
)
def test_failed_remote_opens_redact_credentials(diagnostic_server, auth, op):
    """Keep authentication out of exceptions across native and compatibility APIs.

    Args:
        diagnostic_server: Loopback fixtures and recorded requests.
        auth: Credential placement in the URL.
        op: API expected to fail on a denied dataset.
    """
    base, _, _, _ = diagnostic_server
    url = f"{base}/denied.tif?{auth}={SECRET}"
    if auth == "userinfo":
        url = base.replace("http://", f"http://user:{SECRET}@") + "/denied.tif"
    with pytest.raises(Exception) as caught:
        op(url)
    assert SECRET not in str(caught.value)


@pytest.mark.parametrize(
    "case",
    [
        "driver",
        "overview",
        "copy",
        "copyfiles",
        "merge",
        "stack",
        "zip-rio",
        "zip-pg",
        "zip-legacy",
        "infer",
        "empty",
        "stacta",
        "stacta-name",
        "stacit",
        "wfs",
    ],
)
def test_diagnostic_prefixes_redact_locations(diagnostic_server, case):
    """Exercise diagnostic branches that format locations after a successful open.

    Args:
        diagnostic_server: Loopback fixtures and recorded requests.
        case: Diagnostic branch to execute.
    """
    base, _, _, _ = diagnostic_server
    a, b = (f"{base}/{name}.tif?sig={SECRET}" for name in ["a", "b"])
    archive = f"/vsizip/{{/vsicurl/{base}/a.zip?sig={SECRET}}}/missing.tif"
    actions = {
        "driver": lambda: rio.open(a, driver="PNG"),
        "overview": lambda: rio.open(a, overview_level=2),
        "copy": lambda: shutil.copy(a, a),
        "copyfiles": lambda: shutil.copyfiles(a, a),
        "merge": lambda: merge([a, b]),
        "stack": lambda: stack([a, b]),
        "zip-rio": lambda: rio.open(archive),
        "zip-pg": lambda: pg.read_info(archive),
        "zip-legacy": lambda: pg.read_info(f"zip://{base}/a.zip?sig={SECRET}"),
        "infer": lambda: pg.detect_write_driver(f"{base}/out.gpkg?sig={SECRET}"),
        "empty": lambda: pg.read_info(f"{base}/empty.kml?sig={SECRET}"),
        "stacta": lambda: terrio.info(f'STACTA:"{base}/notjson.json?sig={SECRET}"'),
        "stacta-name": lambda: terrio.info(f'STACTA:"{base}/notjson.json?sig={SECRET}'),
        "stacit": lambda: terrio.open_raster(f'STACIT:"{base}/search?sig={SECRET}"'),
        "wfs": lambda: terrio.open_vector(
            base.replace("http://", f"http://user:{SECRET}@")
            + "/wfs?SERVICE=WFS&REQUEST=GetFeature&TYPENAMES=t"
        ),
    }
    with pytest.raises(Exception) as caught:
        actions[case]()
    assert SECRET not in str(caught.value)


def test_warning_and_display_log_redact_locations(diagnostic_server, caplog):
    """Redact multilayer warnings and notebook display failure log records.

    Args:
        diagnostic_server: Loopback fixtures and controlled response switches.
        caplog: Captured Python log records.
    """
    base, requests, denied, files = diagnostic_server
    with pytest.warns(UserWarning, match="More than one layer") as warnings:
        pg.read_info(f"{base}/multi.gpx?sig={SECRET}")
    assert all(SECRET not in str(w.message) for w in warnings)
    # A small TIFF is fully buffered during open, so denying later requests cannot
    # exercise the display failure branch. Keep most pixel bytes beyond the header.
    raster = terrio.Raster.from_array(np.ones((2048, 2048), dtype="uint8"), (0, 1, 0, 2048, 0, -1))
    files["/big.tif"] = raster.to_bytes("big.tif", compress="NONE")
    with rio.open(f"{base}/big.tif?sig={SECRET}") as dataset:
        denied.add("/big.tif")
        terrio.clear_http_cache()
        before = len(requests)
        with caplog.at_level(logging.DEBUG, logger="terrio.rio.io"):
            assert dataset._repr_png_() is None
        assert len(requests) > before
    assert "cannot draw" in caplog.text
    assert SECRET not in caplog.text


@pytest.mark.parametrize("command", ["info", "gcps"])
def test_command_line_stderr_redacts_locations(diagnostic_server, command):
    """Keep URL authentication out of command errors and verbose tracebacks.

    Args:
        diagnostic_server: Loopback fixtures.
        command: CLI command whose error is printed.
    """
    base, _, _, _ = diagnostic_server
    name = "denied.tif" if command == "info" else "a.tif"
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            "import sys; from terrio.rio.cli import main; sys.exit(main(sys.argv[1:]))",
            "-v",
            command,
            f"{base}/{name}?sig={SECRET}",
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 1
    assert "error:" in result.stderr
    assert SECRET not in result.stderr


def test_redaction_does_not_change_network_urls(diagnostic_server):
    """Send the full query to the server while suppressing it only in diagnostics.

    Args:
        diagnostic_server: Loopback fixtures and recorded requests.
    """
    base, requests, _, _ = diagnostic_server
    with rio.open(f"{base}/a.tif?sig={SECRET}") as dataset:
        np.testing.assert_array_equal(dataset.read(1), np.ones((2, 2), dtype="uint8"))
    assert any(f"sig={SECRET}" in path for path in requests)
