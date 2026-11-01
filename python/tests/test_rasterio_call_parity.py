"""What applications rely on from rasterio/GDAL that terrio.rio reproduces, through Python.

The Rust unit tests cover the logic (header origins, driver identification, decoder limits); these
cover the Python glue no Rust test sees, and compare with rasterio where it is installed.
"""

import http.server
import threading
import warnings
from pathlib import Path

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.control import GroundControlPoint
from terrio.rio.errors import NotGeoreferencedWarning, RasterioIOError


def _write_tif(path, nodata=None):
    data = np.arange(64 * 64, dtype="uint16").reshape(1, 64, 64)
    profile = dict(driver="GTiff", width=64, height=64, count=1, dtype="uint16", nodata=nodata)
    with rio.open(
        path, "w", crs="EPSG:32631", transform=rio.transform.from_origin(0, 64, 1, 1), **profile
    ) as dst:
        dst.write(data)
    return data


@pytest.fixture
def server(tmp_path):
    """A loopback HTTP server with range support, serving `tmp_path`, that records headers."""
    seen = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_HEAD(self):
            self._serve(body=False)

        def do_GET(self):
            self._serve(body=True)

        def _serve(self, body):
            seen.append((self.path, self.headers.get("Authorization")))
            try:
                data = (tmp_path / self.path.lstrip("/")).read_bytes()
            except OSError:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            start, end, status = 0, len(data) - 1, 200
            rng = self.headers.get("Range")
            if rng and rng.startswith("bytes="):
                a, _, b = rng[6:].partition("-")
                start, status = int(a), 206
                end = min(int(b), len(data) - 1) if b else len(data) - 1
            self.send_response(status)
            self.send_header("Content-Length", str(end - start + 1))
            self.send_header("Accept-Ranges", "bytes")
            if status == 206:
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
            self.end_headers()
            if body:
                self.wfile.write(data[start : end + 1])

    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{httpd.server_address[1]}", seen
    httpd.shutdown()


def test_env_headers_reach_only_the_thread_that_set_them(tmp_path, server):
    """Per-request credentials (`Env(GDAL_HTTP_HEADERS=...)` in a threaded server) must not leak
    to requests other threads make meanwhile, as they did when `Env` set `os.environ`."""
    url, seen = server
    expected = _write_tif(tmp_path / "a.tif")
    _write_tif(tmp_path / "b.tif")
    inside, done = threading.Event(), threading.Event()
    results = {}

    def with_token():
        with rio.Env(GDAL_HTTP_HEADERS="Authorization: Bearer token-a"):
            with rio.open(f"{url}/a.tif") as src:
                results["a"] = src.read()
            inside.set()
            done.wait(30)

    def without():
        inside.wait(30)
        with rio.open(f"{url}/b.tif") as src:
            results["b"] = src.read()
        done.set()

    terrio.reset_network_stats()
    threads = [threading.Thread(target=with_token), threading.Thread(target=without)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(60)
    assert np.array_equal(results["a"], expected) and np.array_equal(results["b"], expected)
    assert {auth for path, auth in seen if path == "/a.tif"} == {"Bearer token-a"}
    assert {auth for path, auth in seen if path == "/b.tif"} == {None}
    stats = terrio.network_stats()
    host = url.split("://")[1]
    assert stats["hosts"][host]["requests"] == stats["requests"] > 0


def test_driver_list_refuses_other_formats_without_opening_them(tmp_path):
    """`DatasetReader(path, driver=[...])`, as a tile server's reader opens untrusted assets: a VRT
    (a pointer to other files) must be refused, whatever its name, as GDAL refuses it."""
    _write_tif(tmp_path / "real.tif")
    for name in ("pointer.vrt", "pointer.tif"):
        (tmp_path / name).write_text(
            '<VRTDataset rasterXSize="64" rasterYSize="64"><VRTRasterBand dataType="UInt16" band="1">'
            '<SimpleSource><SourceFilename relativeToVRT="1">real.tif</SourceFilename>'
            "<SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
        )
        with pytest.raises(
            RasterioIOError, match="not recognized as being in a supported file format"
        ):
            rio.io.DatasetReader(str(tmp_path / name), driver=["GTiff"])
    with rio.io.DatasetReader(str(tmp_path / "real.tif"), driver=["GTiff"]) as src:
        assert src.driver == "GTiff"


def test_empty_dir_ignores_sidecars(tmp_path):
    """`GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` (a tile server's forced-driver reads of untrusted
    GeoTIFF, PNG and JPEG assets): no sidecar is read, so a planted `.aux.xml` or world file
    cannot change the dataset."""
    tif, png = tmp_path / "x.tif", tmp_path / "y.png"
    _write_tif(tif)
    with rio.open(png, "w", driver="PNG", width=8, height=8, count=1, dtype="uint8") as dst:
        dst.write(np.ones((1, 8, 8), "uint8"))
    (tmp_path / "y.wld").write_text("10\n0\n0\n-10\n1005\n1995\n")
    for path in (tif, png):
        Path(f"{path}.aux.xml").write_text(
            '<PAMDataset><PAMRasterBand band="1"><NoDataValue>7</NoDataValue></PAMRasterBand></PAMDataset>'
        )
    backends = [rio]
    try:
        import rasterio

        backends.append(rasterio)
    except ImportError:
        pass
    warnings.simplefilter("ignore", NotGeoreferencedWarning)
    for backend in backends:
        with backend.open(tif) as src:
            assert src.nodata == 7, backend
        with backend.open(png) as src:
            assert (src.transform.c, src.nodata) == (1000.0, 7), backend
        with backend.Env(GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR"):
            with backend.open(tif) as src:
                assert src.nodata is None, backend
            with backend.open(png) as src:
                assert (src.transform.c, src.nodata) == (0.0, None), backend


def test_empty_dir_ignores_msk_sidecars(tmp_path):
    """A `.msk` sidecar (#201) is a sidecar too: GDAL 3.12.1 reads it as a per-dataset
    mask, and under `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` does not (the mask is all valid).
    GDAL writes the sidecar and is the reference."""
    rasterio = pytest.importorskip("rasterio")
    tif = tmp_path / "m.tif"
    mask = np.full((64, 64), 255, "uint8")
    mask[:32] = 0
    profile = dict(driver="GTiff", width=64, height=64, count=1, dtype="uint16")
    with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=False):
        with rasterio.open(
            tif, "w", crs="EPSG:32631", transform=rio.transform.from_origin(0, 64, 1, 1), **profile
        ) as dst:
            dst.write(np.arange(64 * 64, dtype="uint16").reshape(1, 64, 64))
            dst.write_mask(mask)
    assert Path(f"{tif}.msk").is_file()
    for backend in (rio, rasterio):
        with backend.open(tif) as src:
            assert int((src.read_masks(1) == 0).sum()) == 32 * 64, backend
        with backend.Env(GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR"):
            with backend.open(tif) as src:
                assert int((src.read_masks(1) == 0).sum()) == 0, backend


def _mem_name(data):
    """The name a tile server gives an array to open it as a dataset (rasterio PR 2512)."""
    info = data.__array_interface__
    count, height, width = data.shape
    bands, lines, pixels = info["strides"] or (
        data.itemsize * height * width,
        data.itemsize * width,
        data.itemsize,
    )
    typename = rio.dtypes._gdal_typename(data.dtype.name)
    return (
        f"MEM:::DATAPOINTER={info['data'][0]},PIXELS={width},LINES={height},BANDS={count},"
        f"DATATYPE={typename},BANDOFFSET={bands},LINEOFFSET={lines},PIXELOFFSET={pixels}"
    )


def test_mem_datapointer_is_opt_in_and_works_in_place():
    """A `MEM:::` name makes the process read and write the memory it names: refused unless
    `GDAL_MEM_ENABLE_OPEN` allows it (GDAL 3.10+), read-only in `r` mode, in place in `r+`."""
    base = np.arange(2 * 6 * 8, dtype="uint16").reshape(2, 6, 8)
    data = base[:, ::2, 1::2]  # strided, as a caller may pass a view
    original = data.copy()
    name = _mem_name(data)
    with pytest.raises(RasterioIOError, match="GDAL_MEM_ENABLE_OPEN"):
        rio.open(name)
    with rio.Env(GDAL_MEM_ENABLE_OPEN="NO"), pytest.raises(RasterioIOError):
        rio.open(name)
    with rio.Env(GDAL_MEM_ENABLE_OPEN=True):
        with rio.open(name) as src:
            assert (src.driver, src.count, src.shape, src.dtypes[0]) == ("MEM", 2, (3, 4), "uint16")
            assert np.array_equal(src.read(), original)
            with pytest.raises(ValueError):
                src.write(np.zeros_like(original))
        assert np.array_equal(data, original)
        with rio.open(name, "r+") as src:
            src.write(np.full((3, 4), 9, "uint16"), 2)
    # Band 2 of the view changed, and nothing else in the array it views.
    expected = np.arange(96, dtype="uint16").reshape(2, 6, 8)
    expected[1, ::2, 1::2] = 9
    assert np.array_equal(base, expected)


def test_from_gcps_matches_gdal():
    """`rasterio.transform.from_gcps` is GDAL's `GDALGCPsToGeoTransform`: the same transform, to
    the bit, including the least squares fit GDAL leaves behind when it reports failure. terrio
    then warns, as rasterio means to: rasterio 1.5.0 never does, comparing GDAL's TRUE/FALSE
    result with CE_Failure."""
    rasterio = pytest.importorskip("rasterio")
    from rasterio.control import GroundControlPoint as GdalGCP

    rng = np.random.default_rng(7)

    def exact(points):
        return [
            (r, c, 0.5 * c + 0.1 * r + 500000.0, -0.05 * c - 0.5 * r + 4000000.0, 0.0)
            for r, c in points
        ]

    grid = [(r, c) for r in (0.0, 512.0, 1023.0) for c in (0.0, 700.5, 1400.0)]
    cases = {
        "two": [(0.0, 0.0, 10.0, 20.0, 0.0), (100.0, 200.0, 30.0, -5.0, 0.0)],
        # Corners GDAL solves exactly; its least squares path would round differently.
        "four corners": [
            (0.0, 0.0, 349465.1616, 4386611.5918, 0.0),
            (0.0, 18978.0, 396910.1616, 4386611.5918, 0.0),
            (5536.0, 18978.0, 396910.1616, 4372771.5918, 0.0),
            (5536.0, 0.0, 349465.1616, 4372771.5918, 0.0),
        ],
        "exact grid": exact(grid),
        "noisy grid": [
            (r, c, x + rng.normal(0, 0.01), y + rng.normal(0, 0.01), z)
            for r, c, x, y, z in exact(grid)
        ],
        "poor fit": [(r, c, x + rng.normal(0, 5.0), y, z) for r, c, x, y, z in exact(grid)],
    }
    for label, gcps in cases.items():
        with warnings.catch_warnings(record=True) as w_gdal:
            warnings.simplefilter("always")
            expected = rasterio.transform.from_gcps([GdalGCP(*g) for g in gcps])
        with warnings.catch_warnings(record=True) as w_terrio:
            warnings.simplefilter("always")
            got = rio.transform.from_gcps([GroundControlPoint(*g) for g in gcps])
        assert tuple(got) == tuple(expected), label
        assert not w_gdal
        assert [w.category for w in w_terrio] == (
            [NotGeoreferencedWarning] if label == "poor fit" else []
        ), label
    with pytest.warns(NotGeoreferencedWarning):
        rio.transform.from_gcps([GroundControlPoint(0, 0, 1, 1)] * 3)
