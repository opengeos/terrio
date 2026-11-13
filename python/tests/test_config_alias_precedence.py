"""Thread scopes override outer scopes and environment values across option aliases."""

import contextlib
import http.server
import re
import threading

import numpy as np
import pytest

import terrio
import terrio.rio as rio

PRIMARY = "TERRIO_DISABLE_READDIR_ON_OPEN"
ALTERNATE = "GDAL_DISABLE_READDIR_ON_OPEN"


@pytest.fixture
def sidecar_source(tmp_path):
    """Create a real TIFF whose auxiliary file supplies its nodata value.

    Args:
        tmp_path: Temporary directory.

    Returns:
        Path: TIFF path.
    """
    path = tmp_path / "source.tif"
    terrio.Raster.from_array(
        np.zeros((4, 4), dtype="uint8"), (0, 1, 0, 4, 0, -1), crs="EPSG:3857"
    ).write(str(path))
    path.with_suffix(".tif.aux.xml").write_text(
        '<PAMDataset><PAMRasterBand band="1"><NoDataValue>7</NoDataValue>'
        "</PAMRasterBand></PAMDataset>"
    )
    return path


def assert_sidecars(path, disabled):
    """Check metadata and file discovery through the actual reader.

    Args:
        path: TIFF path.
        disabled: Whether auxiliary metadata must be ignored.
    """
    with rio.open(path) as dataset:
        assert dataset.nodata == (None if disabled else 7)
        assert (str(path) + ".aux.xml" in dataset.files) == (not disabled)


@pytest.mark.parametrize("environment", [None, PRIMARY, ALTERNATE])
@pytest.mark.parametrize("scope", [None, PRIMARY, ALTERNATE])
def test_scope_alias_overrides_environment(sidecar_source, monkeypatch, environment, scope):
    """Every scoped alias wins over either process-environment name.

    Args:
        sidecar_source: TIFF and auxiliary metadata.
        monkeypatch: Scoped process environment overrides.
        environment: Environment alias, if any.
        scope: Thread-scope alias, if any.
    """
    for key in [PRIMARY, ALTERNATE]:
        monkeypatch.delenv(key, raising=False)
    if environment:
        monkeypatch.setenv(environment, "NO")
    with rio.Env(**({scope: "EMPTY_DIR"} if scope else {})):
        assert_sidecars(sidecar_source, scope is not None)


@pytest.mark.parametrize("outer", [PRIMARY, ALTERNATE])
@pytest.mark.parametrize("inner", [PRIMARY, ALTERNATE])
@pytest.mark.parametrize("value", ["NO", "EMPTY_DIR"])
def test_nested_alias_scope_wins_and_restores(sidecar_source, monkeypatch, outer, inner, value):
    """The inner logical option wins and the outer value returns on exit.

    Args:
        sidecar_source: TIFF and auxiliary metadata.
        monkeypatch: Scoped process environment overrides.
        outer: Outer-scope alias.
        inner: Inner-scope alias.
        value: Outer value, reversed in the inner scope.
    """
    for key in [PRIMARY, ALTERNATE]:
        monkeypatch.delenv(key, raising=False)
    disabled = value == "EMPTY_DIR"
    with rio.Env(**{outer: value}):
        assert_sidecars(sidecar_source, disabled)
        with rio.Env(**{inner: "NO" if disabled else "EMPTY_DIR"}):
            assert_sidecars(sidecar_source, not disabled)
        assert_sidecars(sidecar_source, disabled)


@pytest.mark.parametrize("value", [None, "   "])
def test_inner_alias_can_unset_the_logical_option(sidecar_source, monkeypatch, value):
    """Unsetting an alias prevents an outer or environment alias from deciding the option.

    Args:
        sidecar_source: TIFF and auxiliary metadata.
        monkeypatch: Scoped process environment overrides.
        value: Explicit unset or blank value.
    """
    monkeypatch.setenv(PRIMARY, "EMPTY_DIR")
    with rio.Env(**{PRIMARY: "EMPTY_DIR"}):
        with rio.Env(**{ALTERNATE: value}):
            assert_sidecars(sidecar_source, False)
        assert_sidecars(sidecar_source, True)


@pytest.fixture
def retry_server(sidecar_source):
    """Serve a real TIFF after one transient failure while recording User-Agent values.

    Args:
        sidecar_source: TIFF supplying the response bytes.

    Yields:
        tuple: URL and observed source GET requests.
    """
    data = sidecar_source.read_bytes()
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            """Suppress server logging.

            Args:
                *args: Log record fields.
            """

        def do_HEAD(self):
            """Report the TIFF length without transferring pixels."""
            self.send_response(200 if self.path == "/source.tif" else 404)
            self.send_header(
                "Content-Length", str(len(data)) if self.path == "/source.tif" else "0"
            )
            self.end_headers()

        def do_GET(self):
            """Fail once, then serve the requested TIFF byte range."""
            if self.path != "/source.tif":
                self.send_error(404)
                return
            status = 503 if not requests else 206
            requests.append((self.headers.get("User-Agent"), status))
            self.send_response(status)
            if status == 503:
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            match = re.fullmatch(r"bytes=(\d+)-(\d*)", self.headers.get("Range", ""))
            start = int(match[1]) if match else 0
            end = min(int(match[2]) if match and match[2] else len(data) - 1, len(data) - 1)
            body = data[start : end + 1]
            self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    terrio.clear_http_cache()
    try:
        yield f"http://127.0.0.1:{server.server_port}/source.tif", requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join(5)
        terrio.clear_http_cache()


@pytest.mark.parametrize("outer", ["environment", "scope"])
def test_http_aliases_control_real_headers_and_retries(retry_server, monkeypatch, outer):
    """An inner HTTP alias sets both the transmitted header and actual retry count.

    Args:
        retry_server: Loopback TIFF URL and request log.
        monkeypatch: Scoped process environment overrides.
        outer: Where the primary aliases are configured.
    """
    values = {"TERRIO_HTTP_USERAGENT": "from-outer", "TERRIO_HTTP_MAX_RETRY": "0"}
    for key in [*values, "GDAL_HTTP_USERAGENT", "GDAL_HTTP_MAX_RETRY"]:
        monkeypatch.delenv(key, raising=False)
    if outer == "environment":
        for key, value in values.items():
            monkeypatch.setenv(key, value)
    url, requests = retry_server
    with rio.Env(**values) if outer == "scope" else contextlib.nullcontext():
        with rio.Env(
            GDAL_HTTP_USERAGENT="from-inner", GDAL_HTTP_MAX_RETRY="3", TERRIO_HTTP_RETRY_DELAY="0"
        ):
            with rio.open(url) as dataset:
                np.testing.assert_array_equal(dataset.read(1), np.zeros((4, 4), dtype="uint8"))
    assert [status for _, status in requests] == [503, 206]
    assert all(agent == "from-inner" for agent, _ in requests)
