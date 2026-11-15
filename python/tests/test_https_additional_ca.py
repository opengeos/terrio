"""Additional native HTTPS roots preserve verification, scoped trust and range reads."""

import http.server
import os
import select
import shutil
import socket
import ssl
import subprocess
import threading
import time
from contextlib import contextmanager

import numpy as np
import pytest
from test_remote_reads import HttpTestServer, Server, write_cog

import terrio
import terrio.rio as rio
from terrio.rio.errors import RasterioIOError
from terrio.rio.windows import Window


def openssl(root, *arguments):
    """Run the certificate fixture generator without printing private key material.

    Args:
        root: Directory holding the generated certificates.
        *arguments: OpenSSL command arguments.
    """
    result = subprocess.run(
        ["openssl", *arguments], cwd=root, capture_output=True, text=True, timeout=20
    )
    assert result.returncode == 0, result.stderr


@pytest.fixture(scope="module")
def https_source(tmp_path_factory):
    """Serve a tiled TIFF over HTTP and HTTPS using an ephemeral localhost CA.

    Args:
        tmp_path_factory: Factory for the private fixture directory.

    Yields:
        dict: Certificate paths, servers and source pixels.
    """
    if shutil.which("openssl") is None:
        if os.environ.get("CI") == "true":
            pytest.fail("OpenSSL is required for the HTTPS trust checks in CI")
        pytest.skip("OpenSSL is required to generate test certificates")
    root = tmp_path_factory.mktemp("additional-ca")
    for name in ["ca", "wrong"]:
        openssl(
            root,
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-keyout",
            f"{name}.key",
            "-out",
            f"{name}.pem",
            "-days",
            "2",
            "-subj",
            f"/CN=Terrio {name}",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        )
    openssl(
        root,
        "req",
        "-newkey",
        "ec",
        "-pkeyopt",
        "ec_paramgen_curve:prime256v1",
        "-nodes",
        "-keyout",
        "server.key",
        "-out",
        "server.csr",
        "-subj",
        "/CN=localhost",
    )
    (root / "server.ext").write_text("subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n")
    openssl(
        root,
        "x509",
        "-req",
        "-in",
        "server.csr",
        "-CA",
        "ca.pem",
        "-CAkey",
        "ca.key",
        "-CAcreateserial",
        "-out",
        "server.pem",
        "-days",
        "2",
        "-extfile",
        "server.ext",
    )
    pixels = write_cog(root / "source.tif", size=1024, tile=128)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(root / "server.pem", root / "server.key")
    secure = Server(root, tls_context=context)
    plain = Server(root)
    try:
        yield dict(root=root, secure=secure, plain=plain, pixels=pixels)
    finally:
        secure.close()
        plain.close()


@pytest.fixture(autouse=True)
def isolated_http_options(monkeypatch):
    """Remove ambient trust configuration and cached responses between checks.

    Args:
        monkeypatch: Scoped environment changes.

    Yields:
        None: An isolated test scope.
    """
    for name in [
        "TERRIO_HTTP_CAINFO",
        "GDAL_HTTP_CAINFO",
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NO_PROXY",
        "no_proxy",
    ]:
        monkeypatch.delenv(name, raising=False)
    terrio.clear_http_cache()
    with rio.Env(
        TERRIO_HTTP_MAX_RETRY=0,
        TERRIO_HTTP_CONNECTTIMEOUT=3,
        TERRIO_HTTP_TIMEOUT=5,
        TERRIO_DISABLE_READDIR_ON_OPEN="EMPTY_DIR",
        TERRIO_HTTP_CACHE_SIZE=0,
    ):
        yield
    terrio.clear_http_cache()


def read_window(source, *, hostname="localhost", plain=False, clear_cache=True):
    """Read actual raster pixels through a new native remote reader.

    Args:
        source: HTTPS fixture.
        hostname: Certificate hostname or a deliberately mismatched IP address.
        plain: Whether to use the non-TLS endpoint.
        clear_cache: Whether to force a new fetch instead of exercising cached responses.

    Returns:
        ndarray: A small window from the served TIFF.
    """
    server = source["plain" if plain else "secure"]
    url = server.url.replace("127.0.0.1", hostname) + "/source.tif"
    if clear_cache:
        terrio.clear_http_cache()
    with rio.open(url) as dataset:
        return dataset.read(1, window=Window(300, 400, 16, 16))


@pytest.mark.parametrize("setting", ["TERRIO_HTTP_CAINFO", "GDAL_HTTP_CAINFO"])
def test_additional_ca_allows_verified_window_reads(https_source, setting):
    """Add a CA through either alias while preserving bounded HTTP range reads.

    Args:
        https_source: Local HTTPS server and certificate fixture.
        setting: Primary or GDAL-compatible option name.
    """
    server = https_source["secure"]
    server.log.clear()
    with rio.Env(**{setting: str(https_source["root"] / "ca.pem")}):
        result = read_window(https_source)
    np.testing.assert_array_equal(result, https_source["pixels"][0, 400:416, 300:316])
    transfers = [row for row in server.log if row[0] == "GET" and row[2] < 300]
    assert transfers and all(row[1] and row[2] == 206 for row in transfers)
    assert server.bytes_sent() < (https_source["root"] / "source.tif").stat().st_size


def test_ca_scopes_do_not_reuse_another_scopes_trust(https_source):
    """Reject untrusted roots before and after a trusted scope, including nested aliases.

    Args:
        https_source: Local HTTPS server and certificate fixture.
    """
    with pytest.raises(RasterioIOError, match="UnknownIssuer"):
        read_window(https_source)
    with rio.Env(TERRIO_HTTP_CAINFO=str(https_source["root"] / "ca.pem")):
        read_window(https_source)
        with rio.Env(GDAL_HTTP_CAINFO=str(https_source["root"] / "wrong.pem")):
            with pytest.raises(RasterioIOError, match="UnknownIssuer"):
                read_window(https_source)
        read_window(https_source)
    with pytest.raises(RasterioIOError, match="UnknownIssuer"):
        read_window(https_source)


@pytest.mark.parametrize("setting", ["TERRIO_HTTP_CAINFO", "GDAL_HTTP_CAINFO"])
def test_environment_bundle_is_used_by_native_reads(https_source, monkeypatch, setting):
    """Load the CA from either environment spelling, as native CLI reads do.

    Args:
        https_source: Local HTTPS server and certificate fixture.
        monkeypatch: Scoped environment changes.
        setting: Primary or compatible environment name.
    """
    monkeypatch.setenv(setting, str(https_source["root"] / "ca.pem"))
    np.testing.assert_array_equal(
        read_window(https_source), https_source["pixels"][0, 400:416, 300:316]
    )


def test_additional_ca_preserves_hostname_validation(https_source):
    """A trusted issuer must not authorize a different hostname.

    Args:
        https_source: Local HTTPS server and certificate fixture.
    """
    with rio.Env(TERRIO_HTTP_CAINFO=str(https_source["root"] / "ca.pem")):
        with pytest.raises(RasterioIOError, match="not valid for name|NotValidForName"):
            read_window(https_source, hostname="127.0.0.1")


def test_relative_bundle_cannot_reuse_working_directory_trust(https_source, monkeypatch):
    """Refuse ambiguous relative bundle identities, even when the file is valid.

    Args:
        https_source: HTTPS server with a valid private CA bundle.
        monkeypatch: Scoped working-directory change.
    """
    with rio.Env(TERRIO_HTTP_CAINFO=str(https_source["root"] / "ca.pem")):
        read_window(https_source, clear_cache=False)
    monkeypatch.chdir(https_source["root"])
    with rio.Env(TERRIO_HTTP_CAINFO="ca.pem"):
        with pytest.raises(RasterioIOError, match="CAINFO.*path must be absolute"):
            read_window(https_source, clear_cache=False)


@pytest.mark.parametrize(
    "contents",
    [None, "", "not PEM", "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"],
)
def test_invalid_ca_bundle_refuses_https_but_not_plain_http(https_source, tmp_path, contents):
    """Invalid explicit roots fail closed without changing plain HTTP reads.

    Args:
        https_source: Local HTTP/HTTPS server fixture.
        tmp_path: Directory for the invalid bundle.
        contents: Missing, empty, non-PEM or invalid DER certificate data.
    """
    bundle = tmp_path / "invalid.pem"
    if contents is not None:
        bundle.write_text(contents)
    with rio.Env(TERRIO_HTTP_CAINFO=str(bundle)):
        with pytest.raises(RasterioIOError, match="TERRIO_HTTP_CAINFO"):
            read_window(https_source)
        result = read_window(https_source, plain=True)
    np.testing.assert_array_equal(result, https_source["pixels"][0, 400:416, 300:316])


@pytest.mark.parametrize("rejected", [None, "wrong.pem", "missing.pem"])
def test_cached_responses_do_not_cross_trust_scopes(https_source, rejected):
    """A warm response cache must not bypass another scope's certificate policy.

    Args:
        https_source: Local HTTPS server and certificate fixture.
        rejected: Default trust, another CA, or a nonexistent bundle.
    """
    with rio.Env(TERRIO_HTTP_CACHE_SIZE=2 << 20):
        with rio.Env(TERRIO_HTTP_CAINFO=str(https_source["root"] / "ca.pem")):
            expected = read_window(https_source, clear_cache=False)
            requests = len(https_source["secure"].log)
            np.testing.assert_array_equal(read_window(https_source, clear_cache=False), expected)
            assert len(https_source["secure"].log) == requests
        path = str(https_source["root"] / rejected) if rejected else None
        with rio.Env(TERRIO_HTTP_CAINFO=path):
            reason = "TERRIO_HTTP_CAINFO" if rejected == "missing.pem" else "UnknownIssuer"
            with pytest.raises(RasterioIOError, match=reason):
                read_window(https_source, clear_cache=False)


@contextmanager
def connect_proxy(source, secure):
    """Relay CONNECT tunnels through a loopback HTTP or HTTPS proxy.

    Args:
        source: Certificate fixture for the optional proxy TLS listener.
        secure: Whether to encrypt the connection to the proxy itself.

    Yields:
        tuple: Proxy URL and observed CONNECT targets.
    """
    targets = []
    stopped = threading.Event()

    class Handler(http.server.BaseHTTPRequestHandler):
        """Forward an opaque TLS stream after establishing a CONNECT tunnel."""

        protocol_version = "HTTP/1.1"

        def log_message(self, *args):
            """Suppress request logging.

            Args:
                *args: Standard handler log arguments.
            """

        def do_CONNECT(self):
            """Connect to the requested loopback endpoint and relay both directions."""
            targets.append(self.path)
            host, port = self.path.rsplit(":", 1)
            # The test servers listen on IPv4 loopback only. On Windows, "localhost" may resolve
            # to ::1 first, and a refused loopback connection there can take seconds, which
            # may exhaust the client's three-second connect timeout.
            if host == "localhost":
                host = "127.0.0.1"
            with socket.create_connection((host, int(port)), timeout=3) as upstream:
                self.send_response(200)
                self.end_headers()
                self.wfile.flush()
                peers = [self.connection, upstream]
                while not stopped.is_set():
                    ready = (
                        [self.connection]
                        if secure and self.connection.pending()
                        else select.select(peers, [], [], 0.1)[0]
                    )
                    for peer in ready:
                        try:
                            data = peer.recv(65536)
                            if not data:
                                return
                            other = upstream if peer is self.connection else self.connection
                            other.sendall(data)
                        except OSError:
                            return

    context = None
    if secure:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(source["root"] / "server.pem", source["root"] / "server.key")
    server = HttpTestServer(("127.0.0.1", 0), Handler, context)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        scheme = "https" if secure else "http"
        yield f"{scheme}://localhost:{server.server_port}", targets
    finally:
        stopped.set()
        server.shutdown()
        server.server_close()


@pytest.mark.parametrize(
    "secure,proxy_host", [(False, "localhost"), (True, "localhost"), (True, "127.0.0.1")]
)
def test_additional_ca_through_connect_proxy(https_source, monkeypatch, secure, proxy_host):
    """Verify both proxy and endpoint TLS while retaining CONNECT routing.

    Args:
        https_source: Local HTTPS server and certificates.
        monkeypatch: Scoped proxy environment configuration.
        secure: Whether the proxy itself uses TLS.
        proxy_host: Valid proxy certificate hostname or a deliberately mismatched IP.
    """
    with connect_proxy(https_source, secure) as (url, targets):
        for name in ["HTTPS_PROXY", "https_proxy"]:
            monkeypatch.setenv(name, url.replace("localhost", proxy_host))
        for name in ["NO_PROXY", "no_proxy"]:
            monkeypatch.setenv(name, "")
        with rio.Env(TERRIO_HTTP_CAINFO=str(https_source["root"] / "ca.pem")):
            if proxy_host == "127.0.0.1":
                with pytest.raises(RasterioIOError, match="not valid for name|NotValidForName"):
                    read_window(https_source)
                assert not targets
                return
            result = read_window(https_source)
        assert targets
        np.testing.assert_array_equal(result, https_source["pixels"][0, 400:416, 300:316])


def test_additional_ca_handshake_obeys_timeout(https_source):
    """Bound a TLS handshake even when the TCP peer never answers.

    Args:
        https_source: Certificate fixture for a valid explicit trust setting.
    """
    stopped = threading.Event()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()

        def stall():
            """Accept one connection and leave its TLS handshake unanswered."""
            connection, _ = listener.accept()
            with connection:
                stopped.wait(4)

        worker = threading.Thread(target=stall, daemon=True)
        worker.start()
        started = time.monotonic()
        try:
            with rio.Env(
                TERRIO_HTTP_CAINFO=str(https_source["root"] / "ca.pem"),
                TERRIO_HTTP_TIMEOUT=1,
                TERRIO_HTTP_CONNECTTIMEOUT=1,
            ):
                with pytest.raises(RasterioIOError, match="[Tt]imeout|timed out"):
                    rio.open(f"https://localhost:{listener.getsockname()[1]}/source.tif")
            assert time.monotonic() - started < 2.5
        finally:
            stopped.set()
            worker.join(timeout=5)
