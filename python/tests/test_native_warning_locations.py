"""Warnings originating in Rust identify public callers and honor their module filters."""

import inspect
import subprocess
import sys
import warnings

import pytest

import terrio
import terrio.pyogrio as tpg


@pytest.mark.parametrize("callback", ["native", "helper"])
@pytest.mark.parametrize("context", ["atexit", "thread"])
def test_warning_without_python_caller(malformed_crs_shapefile, callback, context):
    """Keep warning emission working when C invokes a callback with no Python frame.

    Args:
        malformed_crs_shapefile: A readable layer that emits a native CRS warning.
        callback: Whether to invoke the native binding or warning helper directly.
        context: Whether C calls the callback during shutdown or in a new thread.
    """
    script = '''
import _thread
import atexit
import sys
import threading
import warnings

import terrio
from terrio._warnings import warn_user

warnings.simplefilter("always")
if sys.argv[2] == "native":
    callback, args = terrio.open_vector, (sys.argv[1],)
else:
    callback, args = warn_user, ("no caller warning",)
if sys.argv[3] == "atexit":
    atexit.register(callback, *args)
else:
    done = threading.Event()
    original_showwarning = warnings.showwarning

    def showwarning(*args, **kwargs):
        """Display the warning and signal that the native callback reached it."""
        original_showwarning(*args, **kwargs)
        done.set()

    def unraisablehook(args):
        """Report callback failures and wake the waiting main thread."""
        sys.__unraisablehook__(args)
        done.set()

    warnings.showwarning = showwarning
    sys.unraisablehook = unraisablehook
    _thread.start_new_thread(callback, args)
    assert done.wait(10), "warning callback did not finish"
'''
    result = subprocess.run(
        [sys.executable, "-c", script, malformed_crs_shapefile, callback, context],
        capture_output=True,
        text=True,
        timeout=20,
    )
    assert result.returncode == 0, result.stderr
    assert "ValueError" not in result.stderr
    assert "Exception ignored" not in result.stderr
    message = "holds no CRS definition" if callback == "native" else "no caller warning"
    assert "sys:1: UserWarning:" in result.stderr
    assert message in result.stderr


@pytest.fixture
def malformed_crs_shapefile(tmp_path):
    path = tmp_path / "points.shp"
    terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {},
                    "geometry": {"type": "Point", "coordinates": [10, 46]},
                }
            ],
        },
        name="points",
    ).write(str(path))
    path.with_suffix(".prj").write_text("not a coordinate reference system")
    return str(path)


@pytest.mark.parametrize("call", [terrio.open_vector, tpg.read_info, tpg.read_dataframe])
def test_native_warning_points_to_nested_public_call(malformed_crs_shapefile, call):
    if call is tpg.read_dataframe:
        pytest.importorskip("geopandas")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        line = inspect.currentframe().f_lineno + 1
        call(malformed_crs_shapefile)
    native = [w for w in caught if "holds no CRS definition" in str(w.message)]
    assert len(native) == 1
    assert native[0].filename == __file__
    assert native[0].lineno == line
    assert native[0].category is UserWarning


@pytest.mark.parametrize("call", [terrio.open_vector, tpg.read_info])
def test_module_level_native_warnings_use_client_location_and_filters(
    malformed_crs_shapefile, call
):
    client = {"__name__": "warning_client", "call": call, "path": malformed_crs_shapefile}
    source = compile("call(path)\ncall(path)\n", "warning_client.py", "exec")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        exec(source, client)
    native = [w for w in caught if "holds no CRS definition" in str(w.message)]
    assert [(w.filename, w.lineno) for w in native] == [
        ("warning_client.py", 1),
        ("warning_client.py", 2),
    ]
    with warnings.catch_warnings():
        warnings.filterwarnings(
            "error", message=".*holds no CRS definition", module="^warning_client$"
        )
        with pytest.raises(UserWarning, match="holds no CRS definition"):
            exec(source, client)


def test_native_warning_default_filter_suppresses_repeated_client_locations(
    malformed_crs_shapefile,
):
    # With stable filters, each client line warns once.
    client = {
        "__name__": "warning_client",
        "call": terrio.open_vector,
        "path": malformed_crs_shapefile,
    }
    source = compile("call(path)\ncall(path)\n", "warning_client.py", "exec")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("default")
        exec(source, client)
        exec(source, client)
    native = [w for w in caught if "holds no CRS definition" in str(w.message)]
    assert [(w.filename, w.lineno) for w in native] == [
        ("warning_client.py", 1),
        ("warning_client.py", 2),
    ]


@pytest.mark.parametrize("call", [terrio.open_vector, tpg.read_info])
def test_native_warning_stops_at_an_external_intermediary(malformed_crs_shapefile, call):
    bridge = {"__name__": "example_bridge", "call": call}
    exec(compile("def relay(path):\n    return call(path)\n", "example_bridge.py", "exec"), bridge)
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        bridge["relay"](malformed_crs_shapefile)
    native = [w for w in caught if "holds no CRS definition" in str(w.message)]
    assert [(w.filename, w.lineno) for w in native] == [("example_bridge.py", 2)]
