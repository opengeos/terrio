"""The APIs an application written for rasterio needs from terrio: driver allow-lists per kind,
`terrio.compat`, network statistics with GDAL's counters, and thread-option enumeration.

Allow-lists last for the process and `install()` changes the import system, so those tests run in
a fresh interpreter each.
"""

import http.server
import importlib.util
import subprocess
import sys
import textwrap
import threading

import numpy as np
import pytest

import terrio
import terrio.rio as rio


def _run(code: str) -> str:
    """Runs `code` in a fresh interpreter and returns its standard output; fails on an error."""
    result = subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code)], capture_output=True, text=True, timeout=120
    )
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


def _write_tif(path, driver="GTiff"):
    with rio.open(
        path,
        "w",
        driver=driver,
        width=16,
        height=16,
        count=1,
        dtype="uint8",
        crs="EPSG:32631",
        transform=rio.transform.from_origin(0, 16, 1, 1),
    ) as dst:
        dst.write(np.full((1, 16, 16), 7, "uint8"))


@pytest.fixture
def files(tmp_path):
    _write_tif(tmp_path / "a.tif")
    _write_tif(tmp_path / "a.png", driver="PNG")
    (tmp_path / "a.geojson").write_text(
        '{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"v":1},'
        '"geometry":{"type":"Point","coordinates":[1,2]}}]}'
    )
    (tmp_path / "a.csv").write_text("x,y,v\n1,2,3\n")
    return tmp_path


def test_a_later_broader_call_cannot_re_enable_a_driver(files):
    """The first call for a kind sets its list, later calls only narrow it, and
    lifting it raises, as a de-registered GDAL driver stays de-registered."""
    _run(
        f"""
        import pytest, terrio, terrio.rio as rio
        from terrio.errors import InvalidArgumentError
        d = {str(files)!r}
        assert terrio.allowed_drivers() is None and terrio.allowed_drivers("raster") is None
        terrio.set_allowed_drivers(None)  # lifting no list is a no-op
        terrio.set_allowed_drivers(["GTiff", "COG"], kind="raster")
        assert terrio.allowed_drivers("raster") == ["GTIFF", "COG"]
        assert terrio.allowed_drivers("vector") is None
        with pytest.raises(rio.errors.RasterioIOError):
            rio.open(d + "/a.png")
        terrio.set_allowed_drivers(["GTiff", "PNG", "VRT"], kind="raster")
        assert terrio.allowed_drivers("raster") == ["GTIFF"], "PNG and VRT are not re-enabled"
        with pytest.raises(rio.errors.RasterioIOError):
            rio.open(d + "/a.png")
        with rio.open(d + "/a.tif") as src:
            assert src.read(1)[0, 0] == 7
        for kind in ("raster", None):
            with pytest.raises(InvalidArgumentError, match="cannot be lifted"):
                terrio.set_allowed_drivers(None, kind=kind)
        with pytest.raises(ValueError, match="kind must be"):
            terrio.set_allowed_drivers(["GTiff"], kind="pointcloud")
        assert terrio.allowed_drivers("raster") == ["GTIFF"]
        """
    )


def test_the_vector_allow_list_refuses_other_drivers(files):
    """The vector list decides which vector drivers open, and
    leaves rasters alone."""
    _run(
        f"""
        import pytest, terrio, terrio.rio as rio
        d = {str(files)!r}
        terrio.set_allowed_drivers(["GeoJSON", "ESRI Shapefile"], kind="vector")
        assert terrio.allowed_drivers("raster") is None
        assert len(terrio.open_vector(d + "/a.geojson").layers[0]) == 1
        with pytest.raises(ValueError, match="not allowed"):
            terrio.open_vector(d + "/a.csv")
        with rio.open(d + "/a.png") as src:
            assert src.read(1)[0, 0] == 7, "rasters are not restricted"
        terrio.set_allowed_drivers(["ESRI Shapefile", "CSV"], kind="vector")
        with pytest.raises(ValueError, match="not allowed"):
            terrio.open_vector(d + "/a.geojson")
        with pytest.raises(ValueError, match="not allowed"):
            terrio.open_vector(d + "/a.csv")
        """
    )


def test_bytes_and_layer_listings_are_checked_by_content_and_kind(files):
    """Datasets given as bytes, and layer listings, pass the allow-list of their kind by name and
    content, as files do (GDAL opens bytes through `/vsimem/`, with the same drivers)."""
    _run(
        f"""
        import pytest, terrio, terrio.rio as rio
        d = {str(files)!r}
        tif, png, gj = (open(d + name, "rb").read() for name in ("/a.tif", "/a.png", "/a.geojson"))
        terrio.set_allowed_drivers(["GTiff"], kind="raster")
        terrio.set_allowed_drivers(["ESRI Shapefile"], kind="vector")
        assert type(terrio.open_bytes(tif, "a.tif")).__name__ == "Raster"
        assert type(terrio.open_bytes(tif, "a.tif", lazy=True)).__name__ == "Raster"
        for data, name, lazy in [
            (png, "a.png", False), (png, "a.tif", False), (png, "a.png", True), (png, "a.tif", True),
            (gj, "a.geojson", False), (gj, "a.shp", False),
        ]:
            with pytest.raises(ValueError, match="not allowed"):
                terrio.open_bytes(data, name, lazy=lazy)
        with pytest.raises(ValueError, match="not allowed"):
            terrio.list_layers(d + "/a.geojson")
        with pytest.raises(rio.errors.RasterioIOError):
            rio.MemoryFile(png).open()
        """
    )


def test_compat_aliases_pyogrio_and_its_errors():
    """With terrio.pyogrio present, `pyogrio` and `pyogrio.errors` are terrio's modules."""
    _run(
        """
        import sys
        import terrio.compat
        terrio.compat.install()
        assert "pyogrio" in terrio.compat.installed()
        import pyogrio, pyogrio.errors
        from pyogrio.errors import DataSourceError
        assert pyogrio is sys.modules["terrio.pyogrio"]
        assert pyogrio.errors is sys.modules["terrio.pyogrio.errors"]
        assert DataSourceError is sys.modules["terrio.pyogrio.errors"].DataSourceError
        """
    )


def test_none_restricts_both_kinds_and_the_mem_gate_follows_the_raster_list():
    """`kind=None` sets both lists; a `MEM:::` name opens only while the raster list allows MEM."""
    _run(
        """
        import numpy as np, pytest, terrio, terrio.rio as rio
        data = np.arange(12, dtype="uint8").reshape(1, 3, 4)
        info = data.__array_interface__
        name = (f"MEM:::DATAPOINTER={info['data'][0]},PIXELS=4,LINES=3,BANDS=1,DATATYPE=Byte,"
                "BANDOFFSET=12,LINEOFFSET=4,PIXELOFFSET=1")
        terrio.set_allowed_drivers(["GeoJSON"], kind="vector")
        with rio.Env(GDAL_MEM_ENABLE_OPEN=True), rio.open(name) as src:
            assert np.array_equal(src.read(), data), "a vector list does not refuse MEM"
        terrio.set_allowed_drivers(["GTiff", "MEM", "GeoJSON"])
        assert terrio.allowed_drivers("raster") == ["GTIFF", "MEM", "GEOJSON"]
        assert terrio.allowed_drivers("vector") == ["GEOJSON"]
        with rio.Env(GDAL_MEM_ENABLE_OPEN=True), rio.open(name) as src:
            assert src.count == 1
        terrio.set_allowed_drivers(["GTiff"], kind="raster")
        with rio.Env(GDAL_MEM_ENABLE_OPEN=True), pytest.raises(rio.errors.RasterioIOError):
            rio.open(name)
        """
    )


_IDENTITY = """
    import importlib, sys
    import terrio.compat
    terrio.compat.install()
    import rasterio
    assert rasterio is sys.modules["terrio.rio"]
    for sub in terrio.compat.RASTERIO_SUBMODULES:
        alias, real = importlib.import_module("rasterio." + sub), importlib.import_module("terrio.rio." + sub)
        assert alias is real, sub
        assert sys.modules["rasterio." + sub] is sys.modules["terrio.rio." + sub], sub
        assert real.__spec__.name == "terrio.rio." + sub, (sub, real.__spec__.name)
    import rasterio.crs
    from rasterio.errors import RasterioIOError
    assert rasterio.crs.CRS is sys.modules["terrio.rio.crs"].CRS
    try:
        rasterio.open("/nonexistent/terrio-compat.tif")
    except RasterioIOError as e:
        assert type(e) is sys.modules["terrio.rio.errors"].RasterioIOError
    else:
        raise AssertionError("no error")
    try:
        import rasterio._base
    except ModuleNotFoundError:
        pass
    else:
        raise AssertionError("a module terrio does not have was found")
    print(sorted(terrio.compat.installed()))
"""


def test_compat_aliases_resolve_to_terrios_module_objects():
    """Every `rasterio.<submodule>` the caller imports is terrio.rio's module
    object, never a second copy of its source (which aliasing only `rasterio` would make)."""
    out = _run(_IDENTITY)
    assert "rasterio" in out and "rioxarray" in out


def test_compat_registers_terrios_accessor_as_rio():
    """`import rioxarray` gives terrio.xarray, and `.rio` is terrio's accessor: it is registered
    before `rioxarray` enters sys.modules, which `register_rio_accessor` requires."""
    pytest.importorskip("xarray")
    _run(
        """
        import sys
        import terrio.compat
        terrio.compat.install()
        import numpy as np, xarray as xr
        import rioxarray
        assert rioxarray is sys.modules["terrio.xarray"]
        da = xr.DataArray(np.zeros((2, 3)), dims=("y", "x"), coords={"y": [1.5, 0.5], "x": [0.5, 1.5, 2.5]})
        assert type(da.rio).__module__.startswith("terrio.xarray"), type(da.rio)
        assert da.rio.transform() == da.terrio.transform()
        """
    )


def test_compat_finds_rioxarray_as_without_terrio_when_xarray_is_absent():
    """Without xarray, `importlib.util.find_spec("rioxarray")` gives what the import system
    gives without terrio (None when rioxarray is not installed, its spec when it is) instead of
    raising terrio.xarray's ImportError; `import rioxarray` fails as for a missing module."""
    out = _run(
        """
        import importlib.machinery, importlib.util, sys
        sys.modules["xarray"] = None  # xarray is not installed: importing it fails
        expected = importlib.machinery.PathFinder.find_spec("rioxarray")
        import terrio.compat
        terrio.compat.install()
        assert "rioxarray" in terrio.compat.installed()
        got = importlib.util.find_spec("rioxarray")
        origin = lambda s: None if s is None else s.origin  # noqa: E731
        assert (got is None, origin(got)) == (expected is None, origin(expected)), (got, expected)
        assert "rioxarray" not in sys.modules and "terrio.xarray" not in sys.modules
        if expected is None:
            try:
                import rioxarray  # noqa: F401
            except ModuleNotFoundError as e:
                assert e.name == "rioxarray", e.name
            else:
                raise AssertionError("rioxarray imported without xarray")
        # The other libraries are still terrio's.
        import rasterio
        assert rasterio is sys.modules["terrio.rio"]
        print("found" if got is not None else "none")
        """
    )
    assert out.strip() in ("found", "none")


def test_compat_find_spec_gives_terrios_spec_with_xarray():
    """With xarray, `find_spec("rioxarray")` is the alias's spec, loaded from terrio.xarray."""
    pytest.importorskip("xarray")
    _run(
        """
        import importlib.util, sys
        import terrio.compat
        terrio.compat.install()
        spec = importlib.util.find_spec("rioxarray")
        assert spec is not None and spec.name == "rioxarray", spec
        assert spec.loader is terrio.compat._IMPORTER, spec.loader
        assert "rioxarray" not in sys.modules
        """
    )


def test_compat_refuses_a_real_library_already_imported():
    """With the real library imported first, both would be in use: `install` raises instead."""
    _run(
        """
        import sys, types
        import pytest, terrio.compat
        sys.modules["rasterio"] = types.ModuleType("rasterio")
        with pytest.raises(terrio.compat.AlreadyImportedError, match="rasterio"):
            terrio.compat.install()
        assert terrio.compat.installed() == {}
        terrio.compat.install(rasterio=False)
        assert "rasterio" not in terrio.compat.installed()
        """
    )
    if importlib.util.find_spec("rasterio") is not None:
        _run(
            """
            import pytest, rasterio, terrio.compat
            assert rasterio.__name__ == "rasterio"
            with pytest.raises(terrio.compat.AlreadyImportedError, match="rasterio"):
                terrio.compat.install()
            """
        )


def test_the_pth_line_installs_compat_before_user_code(tmp_path):
    """The `.pth` line runs `install` when `site` processes the directory, as at interpreter
    start (`site-packages` itself is exercised when an application's environment is built)."""
    import terrio.compat

    (tmp_path / "terrio-compat.pth").write_text(terrio.compat.PTH_LINE)
    code = f"import site, sys; site.addsitedir({str(tmp_path)!r}); exec(sys.argv[1])"
    result = subprocess.run(
        [sys.executable, "-c", code, textwrap.dedent(_IDENTITY)],
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert result.returncode == 0, result.stderr


@pytest.fixture
def server(tmp_path):
    """A loopback HTTP server with ranges, serving `tmp_path`, that records what it sends."""
    sent = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            data = (tmp_path / self.path.lstrip("/").split("?")[0]).read_bytes()
            a, _, b = self.headers["Range"][6:].partition("-")
            start, end = int(a), min(int(b), len(data) - 1)
            body = data[start : end + 1]
            self.send_response(206)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
            self.end_headers()
            self.wfile.write(body)
            sent.append(len(body))

    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    yield f"127.0.0.1:{httpd.server_address[1]}", sent
    httpd.shutdown()


def test_network_stats_report_gdals_counters_per_host(tmp_path, server):
    """`network_stats` has GDAL's counters, in total and per host, equal to what the server sent."""
    host, sent = server
    _write_tif(tmp_path / "a.tif")
    terrio.reset_network_stats()
    with rio.open(f"http://{host}/a.tif?sig=x") as src:
        assert src.read(1)[0, 0] == 7
    stats = terrio.network_stats()
    expected = {"request_count": len(sent), "received_bytes": sum(sent), "sent_bytes": 0}
    assert expected["request_count"] > 0
    assert {k: stats[k] for k in expected} == expected
    assert {k: stats["hosts"][host][k] for k in expected} == expected
    assert (stats["requests"], stats["bytes"]) == (len(sent), sum(sent))
    terrio.reset_network_stats()
    assert terrio.network_stats()["request_count"] == 0 and terrio.network_stats()["hosts"] == {}


def test_config_options_enumerate_the_calling_threads_options():
    """`config_options` lists what the calling thread's `Env` scopes set, and nothing else's."""
    seen = {}
    with rio.Env(TERRIO_EXAMPLE_OPTION="1"):
        assert terrio.config_options()["TERRIO_EXAMPLE_OPTION"] == "1"
        thread = threading.Thread(target=lambda: seen.update(terrio.config_options()))
        thread.start()
        thread.join()
    assert "TERRIO_EXAMPLE_OPTION" not in seen
    assert "TERRIO_EXAMPLE_OPTION" not in terrio.config_options()
