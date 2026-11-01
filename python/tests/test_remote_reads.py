"""Remote reads as GDAL's ``/vsicurl/`` makes them: what is requested, and what is refused.

Each test serves files from a loopback server that logs every request (method, ``Range`` and the
body bytes it sent), and compares terrio's requests with GDAL 3.12.1's, measured on the same
server.
"""

import http.server
import json
import re
import subprocess
import sys
import tempfile
import threading

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.windows import Window


class Server:
    """A loopback server of the files in a directory, logging every request."""

    def __init__(self, root, ranges=True, failures=0):
        self.root, self.ranges, self.failures = root, ranges, failures
        self.log = []
        server = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_HEAD(self):
                self.serve(body=False)

            def do_GET(self):
                self.serve(body=True)

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                self.rfile.read(length)
                self.serve(body=True)

            def serve(self, body):
                rng = self.headers.get("Range")
                auth = self.headers.get("Authorization") is not None
                path = server.root / self.path.split("?")[0].lstrip("/")
                if body and server.failures > 0:
                    server.failures -= 1
                    server.log.append((self.command, rng, 503, 0, auth))
                    self.send_response(503)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if not path.is_file():
                    server.log.append((self.command, rng, 404, 0, auth))
                    self.send_response(404)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                data = path.read_bytes()
                status, start, end = 200, 0, len(data) - 1
                match = re.match(r"bytes=(\d+)-(\d*)", rng or "")
                if server.ranges and match and body:
                    start = int(match.group(1))
                    end = min(int(match.group(2) or end), end)
                    status = 206
                chunk = data[start : end + 1] if body else b""
                server.log.append((self.command, rng, status, len(chunk), auth))
                try:
                    self.send_response(status)
                    if status == 206:
                        self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
                    self.send_header("Content-Length", str(end - start + 1))
                    self.end_headers()
                    self.wfile.write(chunk)
                except (BrokenPipeError, ConnectionResetError):
                    self.close_connection = True

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"

    def bytes_sent(self):
        return sum(row[3] for row in self.log)

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


@pytest.fixture
def serve(tmp_path):
    servers = []

    def start(root=None, **kwargs):
        servers.append(Server(root or tmp_path, **kwargs))
        return servers[-1]

    terrio.clear_http_cache()
    yield start
    for s in servers:
        s.close()
    terrio.clear_http_cache()


def write_cog(path, size=1024, tile=256):
    pixels = (
        (np.arange(size * size, dtype="uint32") % 65521).astype("uint16").reshape(1, size, size)
    )
    profile = dict(
        driver="GTiff",
        width=size,
        height=size,
        count=1,
        dtype="uint16",
        crs="EPSG:32631",
        tiled=True,
        blockxsize=tile,
        blockysize=tile,
        compress="deflate",
        transform=rio.transform.from_origin(500000, 4600000, 10, 10),
    )
    with rio.open(path, "w", **profile) as dst:
        dst.write(pixels)
    return pixels


CHILD = r"""
import json, sys
import terrio, terrio.rio as rio
terrio.set_allowed_drivers(["GTiff", "COG", "PNG", "JPEG"], kind="raster")
out = {}
for url, driver in json.loads(sys.argv[1]):
    try:
        rio.open(url, driver=driver).read(1, window=((0, 16), (0, 16)))
        out[url] = "OPENED"
    except Exception as e:
        out[url] = f"REFUSED {e}"
print(json.dumps(out))
"""


def test_a_remote_file_that_is_not_what_its_name_says_is_refused_after_gdals_first_read(
    tmp_path, serve
):
    """64 MiB of noise (67,174,400 bytes) named ``.tif``, ``.png`` or ``.jpg`` is not downloaded
    whole under an application's allow-list; GDAL reads 16,384 bytes and refuses."""
    noise = np.random.default_rng(5).bytes(4 << 20)
    names = {"junk.tif": "GTiff", "junk.png": "PNG", "junk.jpg": "JPEG"}
    for name in names:
        (tmp_path / name).write_bytes(noise)
    server = serve()
    cases = [(f"{server.url}/{name}", driver) for name, driver in names.items()]
    child = subprocess.run(
        [sys.executable, "-c", CHILD, json.dumps(cases)],
        capture_output=True,
        text=True,
        timeout=300,
    )
    assert child.returncode == 0, child.stderr
    results = json.loads(child.stdout)
    assert all(r.startswith("REFUSED") for r in results.values()), results
    assert [row[1] for row in server.log] == ["bytes=0-16383"] * 3, server.log
    assert server.bytes_sent() == 3 * 16384


def test_a_remote_vector_is_read_by_range_and_a_server_without_ranges_is_refused(tmp_path, serve):
    """Remote vector reads send a ``Range``, and a server without ranges is refused, as GDAL
    refuses it ("Range downloading not supported by this server!")."""
    features = [
        {
            "type": "Feature",
            "properties": {"i": i},
            "geometry": {"type": "Point", "coordinates": [i % 90, 1]},
        }
        for i in range(5000)
    ]
    (tmp_path / "big.geojson").write_text(
        json.dumps({"type": "FeatureCollection", "features": features})
    )
    size = (tmp_path / "big.geojson").stat().st_size
    assert size > 10 * 16384
    norange = serve(ranges=False)
    with pytest.raises(terrio.TerrioError, match="range downloading not supported"):
        terrio.open_vector(f"{norange.url}/big.geojson")
    assert len(norange.log) == 1, norange.log
    ranged = serve()
    layer = terrio.open_vector(f"{ranged.url}/big.geojson")
    assert len(layer.layers[0]) == 5000
    assert all(row[1] is not None for row in ranged.log), ranged.log
    assert ranged.bytes_sent() == size


def test_a_reopened_file_is_read_from_the_process_wide_cache(tmp_path, serve):
    """GDAL's requests for a 4 x 4 GeoTIFF opened four times after a transient error are
    ``[503, 200]``, not ``[503, 200, 200, 200, 200]``. The server ignores ``Range``,
    as an ingestion pipeline's test server does."""
    with rio.open(
        tmp_path / "raster.tif",
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="uint8",
        crs="EPSG:4326",
        transform=rio.transform.from_bounds(0, 0, 1, 1, 4, 4),
    ) as dst:
        dst.write(np.ones((1, 4, 4), dtype="uint8"))
    server = serve(ranges=False, failures=1)
    with rio.Env(GDAL_HTTP_RETRY_DELAY=0.01):
        for _ in range(4):
            with rio.open(f"{server.url}/raster.tif") as ds:
                assert ds.read(1).tolist() == [[1] * 4] * 4
    assert [row[2] for row in server.log if row[0] == "GET"] == [503, 200]


def test_env_headers_add_no_request(tmp_path, serve):
    """Under ``Env(GDAL_HTTP_HEADERS=...)``, ``rio.open`` does not ask for the first block
    twice (4 requests where GDAL makes 3 for the same window)."""
    pixels = write_cog(tmp_path / "cog.tif")
    server = serve()
    with rio.Env(GDAL_HTTP_HEADERS="X-Tenant: one", GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR"):
        with rio.open(f"/vsicurl/{server.url}/cog.tif?attempt=2") as ds:
            window = ds.read(1, window=Window(300, 300, 200, 200))
    np.testing.assert_array_equal(window, pixels[0, 300:500, 300:500])
    first_reads = [row for row in server.log if row[1] == "bytes=0-16383"]
    assert len(first_reads) == 1, server.log
    native = serve()
    terrio.open_raster(f"{native.url}/cog.tif", lazy=True).read(1, window=(300, 300, 200, 200))
    assert len(server.log) == len(native.log), (server.log, native.log)


def test_clear_http_cache_requests_a_file_again(tmp_path, serve):
    write_cog(tmp_path / "cog.tif", size=256)
    server = serve()
    terrio.open_raster(f"{server.url}/cog.tif", lazy=True)
    first = len(server.log)
    terrio.open_raster(f"{server.url}/cog.tif", lazy=True)
    assert len(server.log) == first, server.log
    terrio.clear_http_cache()
    terrio.open_raster(f"{server.url}/cog.tif", lazy=True)
    assert len(server.log) == 2 * first, server.log


def test_an_azure_token_never_reaches_a_host_a_dataset_name_chooses(tmp_path, serve):
    """``az://<host>?x=/container/blob`` and ``azure+<url>`` never choose the host the Azure
    token goes to: they fail before any request."""
    (tmp_path / "asset.tif").write_bytes(b"not a tiff")
    server = serve()
    host = server.url.removeprefix("http://")
    names = [
        f"azure+{server.url}/asset.tif",
        f"az://{host}?ignored=/container/asset.tif",
        f"/vsiaz/{host}?ignored=/container/asset.tif",
        f"az://acct@{host}/container/asset.tif",
        f"az://{host.replace(':', '%3A')}/container/asset.tif",
    ]
    with rio.Env(AZURE_STORAGE_ACCESS_TOKEN="dummy-token", CPL_AZURE_USE_HTTPS="NO"):
        for name in names:
            with pytest.raises(Exception, match="not a valid Azure location|not a dataset name"):
                rio.open(name).read(1)
    assert server.log == []


def test_an_azure_token_reaches_the_configured_endpoint(tmp_path, serve):
    container = tmp_path / "devstoreaccount1" / "container"
    container.mkdir(parents=True)
    pixels = write_cog(container / "asset.tif", size=256)
    server = serve()
    with rio.Env(
        AZURE_STORAGE_ACCESS_TOKEN="dummy-token",
        AZURE_STORAGE_ACCOUNT="devstoreaccount1",
        CPL_AZURE_ENDPOINT=f"{server.url}/devstoreaccount1",
    ):
        with rio.open("/vsiaz/container/asset.tif") as ds:
            np.testing.assert_array_equal(ds.read(), pixels)
    assert server.log and all(row[4] for row in server.log), server.log


STAC_CHILD = r"""
import json, sys
import terrio
if sys.argv[2] == "geojson-only":
    terrio.set_allowed_drivers(["GeoJSON", "ESRI Shapefile"], kind="vector")
layer = terrio.open_vector(sys.argv[1]).layers[0]
print(json.dumps({"features": len(layer), "text": json.dumps(layer.__geo_interface__)}))
"""


def stac_feature(name, x):
    return {
        "type": "Feature",
        "stac_version": "1.0.0",
        "id": name,
        "links": [],
        "assets": {},
        "properties": {"datetime": "2026-01-01T00:00:00Z", "name": name},
        "geometry": {"type": "Point", "coordinates": [x, 1]},
    }


@pytest.mark.parametrize("policy", ["geojson-only", "none"])
def test_a_stac_looking_geojson_reaches_no_host_its_links_choose(tmp_path, serve, policy):
    """A remote ``.geojson`` that looks like STAC makes terrio POST nothing, neither the document's
    headers nor its body, to a host the document chose, and no feature of that host reaches the
    result. Under a GeoJSON/Shapefile allow-list it is read as GeoJSON; without one, its links
    lead to its own origin only, and a GET search is not continued with a POST."""
    internal_root = tmp_path / "internal"
    internal_root.mkdir()
    sentinel = {
        "type": "FeatureCollection",
        "links": [],
        "features": [stac_feature("private-sentinel", 3)],
    }
    (internal_root / "private").write_text(json.dumps(sentinel))
    internal = serve(root=internal_root)
    link = {
        "rel": "next",
        "method": "POST",
        "href": f"{internal.url}/private",
        "type": "application/geo+json",
        "body": {"query": "review-only"},
        "headers": {"X-Internal": "1"},
    }
    doc = {"type": "FeatureCollection", "features": [stac_feature("own", 1)], "links": [link]}
    (tmp_path / "attack.geojson").write_text(json.dumps(doc))
    server = serve()
    child = subprocess.run(
        [sys.executable, "-c", STAC_CHILD, f"{server.url}/attack.geojson", policy],
        capture_output=True,
        text=True,
        timeout=300,
    )
    assert child.returncode == 0, child.stderr
    result = json.loads(child.stdout)
    assert result["features"] == 1
    assert "private-sentinel" not in result["text"]
    assert internal.log == []
    assert len(server.log) == 1, server.log


def test_a_remote_json_opened_as_a_raster_is_refused_after_its_first_bytes(tmp_path, serve):
    """Without an allow-list, a remote GeoJSON document opened as a raster was fetched whole
    (49,999,930 bytes of a 50,000,000-byte one); GDAL refuses it after 49,152. Its first bytes
    show no STAC document: refused after at most 65,536 bytes, by ``rio.open`` and by the whole
    read alike. It still opens as a vector."""
    pad = b"x" * (4 << 20)
    doc = (
        b'{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"s":"'
        + pad
        + b'"},"geometry":{"type":"Point","coordinates":[1,2]}}]}'
    )
    (tmp_path / "big.json").write_bytes(doc)
    for open_as_raster in (rio.open, lambda url: terrio.open_raster(url, lazy=False)):
        terrio.clear_http_cache()
        server = serve()
        with pytest.raises(Exception, match="not a raster dataset: its first 32768 bytes"):
            open_as_raster(f"{server.url}/big.json")
        assert server.bytes_sent() <= 65536, server.log
    server = serve()
    assert len(terrio.open_vector(f"{server.url}/big.json").layers[0]) == 1
    assert server.bytes_sent() == len(doc)


def test_a_remote_document_named_for_no_raster_driver_is_refused_after_its_first_bytes(
    tmp_path, serve
):
    """Without an allow-list, a remote document named for no raster driver (``/x``,
    ``/dir.json/x``, with a query, through ``/vsicurl/``) or for ENVI (``/x.dat``, read with a
    ``.hdr`` a remote file comes without) was fetched whole before a raster open refused it,
    naming its local copy: 49,999,930 bytes or more of a 50,000,000-byte GeoJSON, random, CSV or
    XML document, where GDAL reads 49,152. Its first bytes show no raster format: refused after
    at most 65,536 bytes, naming the dataset, by ``rio.open`` and the whole read alike. A COG
    under the same names still opens in place, with the requests of one named ``.tif``."""
    size = 4 << 20
    docs = {
        "geojson": b'{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"s":"'
        + b"x" * size
        + b'"},"geometry":{"type":"Point","coordinates":[1,2]}}]}',
        "noise": b"\0" + np.random.default_rng(5).bytes(size),
        "csv": b"id,name,value\n" + b"1,a,2\n" * (size // 6),
        "xml": b'<?xml version="1.0"?><root>' + b"<a>1</a>" * (size // 8),
    }
    for kind, doc in docs.items():
        for name in ("x", "x.dat", "dir.json/x"):
            (tmp_path / kind / name).parent.mkdir(parents=True, exist_ok=True)
            (tmp_path / kind / name).write_bytes(doc)
    scratch = tempfile.gettempdir()
    for open_as_raster in (rio.open, lambda url: terrio.open_raster(url, lazy=False)):
        for kind in docs:
            for name in ("x", "x.dat", "dir.json/x", "x?sig=secret", "/vsicurl/"):
                terrio.clear_http_cache()
                server = serve()
                url = f"{server.url}/{kind}/{name}"
                shown = url.split("?")[0]
                if name == "/vsicurl/":
                    url = shown = f"/vsicurl/{server.url}/{kind}/x"
                with pytest.raises(
                    Exception, match="not recognized as being in a supported raster"
                ) as e:
                    open_as_raster(url)
                assert server.bytes_sent() <= 65536 and len(server.log) <= 2, (url, server.log)
                message = str(e.value)
                assert shown in message, message
                # rio.open starts its errors with the name as given; terrio's own part names the
                # dataset without its query.
                own = message.removeprefix(f"{url}: ")
                assert scratch not in message and "secret" not in own, message
    (tmp_path / "cog").mkdir()
    write_cog(tmp_path / "cog" / "x.tif")
    for name in ("x", "x.dat"):
        (tmp_path / "cog" / name).write_bytes((tmp_path / "cog" / "x.tif").read_bytes())
    pixels = {}
    for name in ("x.tif", "x", "x.dat"):
        terrio.clear_http_cache()
        server = serve()
        with rio.open(f"{server.url}/cog/{name}") as ds:
            pixels[name] = (ds.profile["driver"], ds.read(1, window=Window(300, 300, 200, 200)))
        pixels[name] += ([row[:3] for row in server.log],)
    for name in ("x", "x.dat"):
        assert pixels[name][0] == pixels["x.tif"][0] == "GTiff"
        np.testing.assert_array_equal(pixels[name][1], pixels["x.tif"][1])
        assert pixels[name][2] == pixels["x.tif"][2]


VRT_CHILD = r"""
import json, sys
import terrio, terrio.rio as rio
render = ["COG", "GRIB", "GTiff", "JP2OpenJPEG", "JPEG", "MEM", "PNG", "VRT"]
terrio.set_allowed_drivers(render, kind="raster")
out = {}
for url in sys.argv[1:]:
    try:
        with rio.open(url) as ds:
            out[url] = ["OPENED", ds.driver, ds.read(1).tolist()]
    except Exception as e:
        out[url] = ["REFUSED", str(e)]
print(json.dumps(out))
"""


def test_a_remote_file_named_for_a_format_read_whole_is_refused_unless_its_first_bytes_show_it(
    tmp_path, serve
):
    """A remote document named for a format terrio reads whole (``.vrt``, ``.nc``, ``.webp``,
    ``.h5``, ``.gpkg``) was fetched whole before the format's reader refused it: 49,999,930 bytes
    of a 50,000,000-byte GeoJSON document, ``.vrt`` under a tile service's allow-list too, where
    GDAL reads 49,152. Its first bytes do not show the format: it is refused after them (one
    request of at most 16,384 bytes), naming the dataset. A genuine VRT, its GeoTIFF source beside
    it, still opens under that allow-list."""
    doc = (
        b'{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"s":"'
        + b"x" * (4 << 20)
        + b'"},"geometry":{"type":"Point","coordinates":[1,2]}}]}'
    )
    names = ["x.vrt", "x.nc", "x.webp", "x.h5", "x.gpkg"]
    for name in names:
        (tmp_path / name).write_bytes(doc)
    for open_as_raster in (rio.open, lambda url: terrio.open_raster(url, lazy=False)):
        for name in names:
            terrio.clear_http_cache()
            server = serve()
            url = f"{server.url}/{name}"
            refusal = f"'{re.escape(url)}' not recognized as being in a supported file format"
            with pytest.raises(Exception, match=refusal) as e:
                open_as_raster(url)
            assert "holds GEOJSON data" in str(e.value)
            assert server.bytes_sent() <= 16384 and len(server.log) == 1, (name, server.log)
    pixels = np.arange(64, dtype="uint8").reshape(8, 8)
    profile = dict(driver="GTiff", width=8, height=8, count=1, dtype="uint8", crs="EPSG:4326")
    profile["transform"] = rio.transform.from_origin(0, 8, 1, 1)
    real = tmp_path / "real"
    real.mkdir()
    with rio.open(real / "a.tif", "w", **profile) as dst:
        dst.write(pixels, 1)
    (real / "x.vrt").write_text(
        '<VRTDataset rasterXSize="8" rasterYSize="8"><SRS>EPSG:4326</SRS>'
        "<GeoTransform>0,1,0,8,0,-1</GeoTransform>"
        '<VRTRasterBand dataType="Byte" band="1"><SimpleSource>'
        '<SourceFilename relativeToVRT="1">a.tif</SourceFilename><SourceBand>1</SourceBand>'
        "</SimpleSource></VRTRasterBand></VRTDataset>"
    )
    refused, genuine = serve(), serve(root=real)
    urls = [f"{refused.url}/x.vrt", f"{genuine.url}/x.vrt"]
    child = subprocess.run(
        [sys.executable, "-c", VRT_CHILD, *urls], capture_output=True, text=True, timeout=300
    )
    assert child.returncode == 0, child.stderr
    out = json.loads(child.stdout)
    assert out[urls[0]][0] == "REFUSED", out
    assert "named for VRT, it holds GEOJSON data" in out[urls[0]][1], out
    assert refused.bytes_sent() <= 16384 and len(refused.log) == 1, refused.log
    assert out[urls[1]] == ["OPENED", "VRT", pixels.tolist()], out


SRTM_NAMES = [
    "N37W120.hgt",
    "N37W120.img",
    "N37W120.hgts",
    "N37W120.err",
    "N37W120.num",
    "N37W120.swb",
    "N37W120.raw",
    "N37W120.SRTMSWBD.raw",
]


def test_a_remote_srtm_tile_is_known_by_its_name_and_size(tmp_path, serve):
    """A remote SRTM tile named for Imagine (``N37W120.img``, as NASADEM names its tiles) was
    refused after its first 16,384 bytes, which show no Imagine header, and one named ``.hgts``,
    ``.err``, ``.num``, ``.swb`` or ``.raw`` after them too, where GDAL reads them all: a tile is
    known by its name and size, as GDAL's SRTMHGT driver knows one (``VSIStatL``). Under every
    name it opens as its local copy does, read in two requests (GDAL: a HEAD and eight, the same
    bytes). A file so named without a tile's size is refused after its first request (at most
    16,384 bytes), where one named ``.hgt`` was fetched whole before the reader refused it; and
    ``driver=`` without SRTMHGT refuses the tile named ``.img`` after its first request."""
    samples = (np.arange(1201 * 1201) % 3000).astype(">i2")
    samples[0] = -32768
    tile = samples.tobytes()
    for folder, data in (("tile", tile), ("wrong", tile + b"\0")):
        (tmp_path / folder).mkdir()
        for name in SRTM_NAMES:
            (tmp_path / folder / name).write_bytes(data)
    with rio.open(tmp_path / "tile" / "N37W120.hgt") as ds:
        profile, pixels = ds.profile, ds.read(1)
    assert profile["driver"] == "SRTMHGT" and profile["nodata"] == -32768
    for name in SRTM_NAMES:
        terrio.clear_http_cache()
        server = serve(root=tmp_path / "tile")
        with rio.open(f"{server.url}/{name}") as ds:
            assert ds.profile == profile, name
            np.testing.assert_array_equal(ds.read(1), pixels)
        ranges = [row[1] for row in server.log]
        assert ranges == ["bytes=0-16383", f"bytes=16384-{len(tile) - 1}"], (name, server.log)
        assert server.bytes_sent() == len(tile)
        terrio.clear_http_cache()
        server = serve(root=tmp_path / "wrong")
        with pytest.raises(Exception, match="not recognized as being in a supported"):
            rio.open(f"{server.url}/{name}")
        assert server.bytes_sent() <= 16384 and len(server.log) == 1, (name, server.log)
    # driver= without SRTMHGT refuses the tile named for Imagine after that first request, which
    # the open after finds in the process-wide cache.
    terrio.clear_http_cache()
    server = serve(root=tmp_path / "tile")
    url = f"{server.url}/N37W120.img"
    with pytest.raises(
        Exception, match="holds SRTMHGT data, and the SRTMHGT driver is not allowed"
    ):
        rio.open(url, driver="HFA")
    assert len(server.log) == 1, server.log
    with rio.open(url) as ds:
        np.testing.assert_array_equal(ds.read(1), pixels)
    assert len(server.log) == 2 and server.bytes_sent() == len(tile), server.log


def test_a_remote_imagine_file_is_read_under_a_tile_name_too(tmp_path, serve):
    """A genuine Imagine file opens as before, named as an SRTM tile too (``N37W120.img``, of
    another size than a tile's): its header shows Imagine, as GDAL's HFA driver identifies it."""
    rasterio = pytest.importorskip("rasterio")
    pixels = (np.arange(300 * 200) % 3000).astype("int16").reshape(200, 300)
    profile = dict(driver="HFA", width=300, height=200, count=1, dtype="int16", crs="EPSG:4326")
    profile["transform"] = rasterio.transform.from_origin(-120, 38, 0.001, 0.001)
    with rasterio.open(tmp_path / "x.img", "w", **profile) as dst:
        dst.write(pixels, 1)
    (tmp_path / "N37W120.img").write_bytes((tmp_path / "x.img").read_bytes())
    for name in ("x.img", "N37W120.img"):
        terrio.clear_http_cache()
        server = serve()
        with rio.open(f"{server.url}/{name}") as ds:
            assert ds.driver == "HFA", name
            np.testing.assert_array_equal(ds.read(1), pixels)
        assert server.bytes_sent() == (tmp_path / name).stat().st_size, server.log


def test_a_remote_file_under_a_tile_extension_not_named_as_a_tile_is_refused(tmp_path, serve):
    """A remote file under an SRTM tile's extension whose name is no tile's (``big.hgt``,
    ``N38W120_v2.hgt``), which GDAL's SRTMHGT driver does not take, was fetched whole before the
    reader refused it when named ``.hgt``. It is refused after its first request (at most 16,384
    bytes), a document as a genuine tile so named, by ``rio.open`` and the whole read alike."""
    samples = (np.arange(1201 * 1201) % 3000).astype(">i2")
    doc = b'{"type":"FeatureCollection","features":[]}' + b" " * (4 << 20)
    for kind, data in (("doc", doc), ("tile", samples.tobytes())):
        (tmp_path / kind).mkdir()
        for name in ("big.hgt", "N38W120_v2.hgt", "big.hgts", "big.raw"):
            (tmp_path / kind / name).write_bytes(data)
    for open_as_raster in (rio.open, lambda url: terrio.open_raster(url, lazy=False)):
        for kind in ("doc", "tile"):
            for name in ("big.hgt", "N38W120_v2.hgt", "big.hgts", "big.raw"):
                terrio.clear_http_cache()
                server = serve()
                with pytest.raises(Exception, match="not recognized as being in a supported"):
                    open_as_raster(f"{server.url}/{kind}/{name}")
                assert server.bytes_sent() <= 16384 and len(server.log) == 1, (
                    kind,
                    name,
                    server.log,
                )
