"""A WarpedVRT of a raster terrio reads itself reads and warps each source window in one call
without the GIL (`RegionWarp._warp_from`): the window never becomes a Python object. The pixels
are those of the Python path (`_source_window` and `RegionWarp.warp`), which a dataset whose
`_read_level` is replaced still takes."""

import json
import os
import subprocess
import sys
import textwrap
import threading
import warnings
from pathlib import Path

import numpy as np
import pytest

import terrio
import terrio.rio
from terrio import _terrio
from terrio.rio.enums import Resampling
from terrio.rio.errors import RasterioIOError, WarpOptionsError
from terrio.rio.io import DatasetReader
from terrio.rio.vrt import WarpedVRT

RESAMPLINGS = [
    Resampling.nearest,
    Resampling.bilinear,
    Resampling.cubic,
    Resampling.cubic_spline,
    Resampling.lanczos,
    Resampling.average,
    Resampling.rms,
    Resampling.mode,
    Resampling.max,
    Resampling.min,
    Resampling.med,
    Resampling.q1,
    Resampling.q3,
    Resampling.sum,
]
DTYPES = [
    "uint8",
    "int8",
    "uint16",
    "int16",
    "uint32",
    "int32",
    "uint64",
    "int64",
    "float32",
    "float64",
]
# Complex GeoTIFFs of the crate's tests, without georeferencing: complex_int16 and complex64
# bands.
COMPLEX = (
    Path(__file__).resolve().parents[2] / "crates" / "terrio" / "tests" / "data" / "zstd_samples"
)
# In `numpy.exceptions` from NumPy 1.25, and at the top level before; terrio supports NumPy 1.21.
COMPLEX_WARNING = getattr(np, "exceptions", np).ComplexWarning
# 10 m pixels in UTM zone 33N, warped into Web Mercator.
GT = (447090.0, 10.0, 0.0, 6022390.0, 0.0, -10.0)
CRS = "EPSG:32633"


def bits(a):
    """An array's bits, to compare floating-point values bit for bit (NaN included)."""
    a = np.ascontiguousarray(a)
    return a.view(f"u{a.dtype.itemsize}")


def values(dtype, bands, rows, cols, seed=0):
    """Random values of `dtype`, the whole range of an integer type; for floats, values around
    the integer limits' halves and the edge values the conversions treat apart."""
    rng = np.random.default_rng(seed)
    dt = np.dtype(dtype)
    shape = (bands, rows, cols)
    if dt.kind in "iu":
        info = np.iinfo(dt)
        return rng.integers(info.min, info.max, size=shape, dtype=dt, endpoint=True)
    v = (rng.standard_normal(shape) * 300).astype(dt)
    edges = [np.nan, np.inf, -np.inf, 0.5, -2.5, -0.0, 255.5, 127.49999, -128.5, 65535.5]
    edges += [32767.5, -32768.5, 4294967295.5, 2.5e9, -2.5e9, 1e30, -1e30, 1.5, -1.5]
    v.reshape(-1)[: len(edges)] = np.array(edges, dtype="float64").astype(dt)
    return v


def write(path, data, nodata=None, mask=None, compress=None, gt=GT):
    """`data` as a tiled GeoTIFF of 16 x 16 tiles with overviews /2 and /4, and an internal
    mask with `mask`."""
    bands, rows, cols = data.shape
    profile = {
        "driver": "GTiff",
        "width": cols,
        "height": rows,
        "count": bands,
        "dtype": data.dtype.name,
        "crs": CRS,
        "transform": terrio.rio.Affine.from_gdal(*gt),
        "tiled": True,
        "blockxsize": 16,
        "blockysize": 16,
        "nodata": nodata,
    }
    if compress:
        profile["compress"] = compress
    with terrio.rio.open(path, "w", **profile) as dst:
        dst.write(data)
        if mask is not None:
            dst.write_mask(mask)
        dst.build_overviews([2, 4], Resampling.nearest)
    return str(path)


def force_python_path(src):
    """Replaces the source's `_read_level` by a function calling it, so that its WarpedVRT reads
    take the Python path."""
    own = src._read_level
    src._read_level = lambda bands, window, level: own(bands, window, level)


def read(path, python_path, **kw):
    """A full and a reduced read of the WarpedVRT of `path`, and for each whether its windows
    were read and warped in Rust (one flag per window). On the Python path, the source's
    `_read_level` is replaced by a function calling it, as a subclass or a test may replace it."""
    out = []
    for reduced in (False, True):
        with terrio.rio.open(path) as src:
            if python_path:
                force_python_path(src)
            with WarpedVRT(src, crs="EPSG:3857", **kw) as vrt:
                windows = []
                vrt._window_hook = lambda level, window, rust, seen=windows: seen.append(
                    (level, rust)
                )
                if reduced:
                    shape = (vrt.count, max(1, vrt.height // 3), max(1, vrt.width // 3))
                    pixels = vrt.read(out_shape=shape)
                else:
                    pixels = vrt.read()
                out.append((pixels, windows))
    return out


def case(
    source, dtype=None, nodata=None, bands=1, alpha=False, mask=False, size=(37, 29), limit=None
):
    return {
        "source": source,
        "dtype": dtype,
        "nodata": nodata,
        "bands": bands,
        "alpha": alpha,
        "mask": mask,
        "size": size,
        "limit": limit,
    }


CONVERTED = [
    ("uint16", "uint8"),
    ("int16", "uint8"),
    ("int32", "int16"),
    ("uint64", "int64"),
    ("int64", "uint32"),
    ("int8", "uint16"),
    ("uint8", "float32"),
    ("int64", "float32"),
    ("uint64", "float64"),
    ("float64", "float32"),
    ("float32", "float64"),
    ("float32", "uint8"),
    ("float32", "int16"),
    ("float64", "int32"),
    ("float64", "uint32"),
    ("float64", "int8"),
]
# Floats into integer types whose limits the float type does not hold exactly: the Python path.
PYTHON_ONLY = [("float32", "int32"), ("float32", "uint64"), ("float64", "int64")]
CASES = (
    [pytest.param(case(d), id=d) for d in DTYPES]
    + [
        pytest.param(case("uint16", nodata=7), id="uint16-nodata"),
        pytest.param(case("float32", nodata=-9999.0), id="float32-nodata"),
        pytest.param(case("uint8", bands=4, alpha=True), id="uint8-alpha"),
        pytest.param(case("uint8", bands=3, mask=True), id="uint8-mask"),
        pytest.param(case("float32", mask=True), id="float32-mask"),
        pytest.param(case("float32", size=(1200, 800), limit=1), id="float32-sum-parts"),
    ]
    + [pytest.param(case(s, d), id=f"{s}-to-{d}") for s, d in CONVERTED]
    + [pytest.param(case(s, d), id=f"{s}-to-{d}-python") for s, d in PYTHON_ONLY]
)


def test_the_one_call_converts_all_pairs_but_floats_into_integers_whose_limits_they_round():
    # The one call converts a window between any two of the ten types but a float type into an
    # integer type whose limits it does not hold exactly, as the crate decides it
    # (`terrio::ops::warp::window::converts`); the WarpedVRT asks it (`_terrio._window_converts`).
    # Names outside the ten are never converted.
    rounded = {("float32", t) for t in ("int32", "uint32", "int64", "uint64")}
    rounded |= {("float64", t) for t in ("int64", "uint64")}
    for src in DTYPES:
        for work in DTYPES:
            assert _terrio._window_converts(src, work) == ((src, work) not in rounded), (src, work)
    for other in ("complex64", "complex_int16", "float16", "bool"):
        assert not _terrio._window_converts(other, "float64"), other
        assert not _terrio._window_converts("float64", other), other


@pytest.mark.parametrize("c", CASES)
def test_the_rust_path_gives_the_python_paths_pixels(tmp_path, c):
    # A UTM raster warped into Web Mercator with each resampling, read whole and reduced (from
    # an overview): the Rust path's pixels are the Python path's, bit for bit, and the Rust path
    # read every window it could, the Python path none.
    cols, rows = c["size"]
    data = values(c["source"], c["bands"], rows, cols)
    mask = None
    if c["alpha"]:
        data[-1] = np.where(np.arange(cols)[None, :] < cols // 3, 0, 255)
    if c["mask"]:
        mask = (
            np.where(np.arange(cols)[None, :] < cols // 3, 0, 255).repeat(rows, 0).astype("uint8")
        )
    path = write(tmp_path / "src.tif", data, nodata=c["nodata"], mask=mask)
    rust_only = (c["source"], c["dtype"]) not in PYTHON_ONLY
    methods = [Resampling.sum] if c["limit"] else RESAMPLINGS
    for method in methods:
        kw = {"resampling": method}
        if c["dtype"]:
            kw["dtype"] = c["dtype"]
        if c["alpha"]:
            kw["src_alpha"] = c["bands"]
        if c["limit"]:
            kw["warp_mem_limit"] = c["limit"]
        rust = read(path, False, **kw)
        python = read(path, True, **kw)
        for (got, got_windows), (want, want_windows) in zip(rust, python, strict=True):
            np.testing.assert_array_equal(bits(got), bits(want), err_msg=str(method))
            assert [level for level, _ in got_windows] == [level for level, _ in want_windows]
            assert all(r == rust_only for _, r in got_windows), (method, got_windows)
            assert not any(r for _, r in want_windows), (method, want_windows)
        if c["limit"]:
            # A `sum` in parts: the full read takes several windows.
            assert len(rust[0][1]) > 1, rust[0][1]
        else:
            # The reduced read takes an overview.
            assert any(level is not None for level, _ in rust[1][1]), rust[1][1]


@pytest.mark.parametrize("on_python_path", [False, True], ids=["rust", "python"])
def test_an_alpha_the_working_type_rounds_to_zero_leaves_its_pixels_in(tmp_path, on_python_path):
    # A float32 source whose alpha band is 0 on its left third and 0.1 elsewhere, warped in uint8,
    # which rounds 0.1 to 0: the alpha leaves out the left third alone, as it is read before the
    # window is converted. The data band comes out as from the same source with an alpha of 1.
    rows, cols = 29, 37
    data = (np.arange(rows * cols) % 200 + 20).astype("float32").reshape(rows, cols)
    left = np.arange(cols)[None, :] < cols // 3
    got = {}
    for alpha in (0.1, 1.0):
        band = np.where(left, 0.0, alpha).repeat(rows, 0).astype("float32")
        path = write(tmp_path / f"{alpha}.tif", np.stack([data, band]))
        with terrio.rio.open(path) as src:
            if on_python_path:
                force_python_path(src)
            for method in (Resampling.nearest, Resampling.bilinear, Resampling.average):
                kw = {"dtype": "uint8", "src_alpha": 2, "resampling": method}
                with WarpedVRT(src, crs="EPSG:3857", **kw) as vrt:
                    flags = []
                    vrt._window_hook = lambda level, window, rust, seen=flags: seen.append(rust)
                    got[alpha, method] = vrt.read(1)
                assert flags == [not on_python_path], flags
    for method in (Resampling.nearest, Resampling.bilinear, Resampling.average):
        fraction = got[0.1, method]
        np.testing.assert_array_equal(fraction, got[1.0, method], err_msg=str(method))
        assert fraction.any() and not fraction.all(), method


@pytest.mark.parametrize("on_python_path", [False, True], ids=["rust", "python"])
def test_a_dataset_mask_leaves_out_the_pixels_an_alpha_band_would(tmp_path, on_python_path):
    # Three bands with an internal mask that is 0 on their left third, and the same three bands
    # with an alpha band that is 0 there instead: both leave the same pixels out, so the three
    # bands come out the same, bit for bit, for each resampling; and not as without either.
    rows, cols = 29, 37
    data = values("uint8", 3, rows, cols)
    valid = np.where(np.arange(cols)[None, :] < cols // 3, 0, 255).repeat(rows, 0).astype("uint8")
    sources = {
        "mask": (write(tmp_path / "mask.tif", data, mask=valid), {}),
        "alpha": (
            write(tmp_path / "alpha.tif", np.concatenate([data, valid[None]])),
            {"src_alpha": 4},
        ),
        "neither": (write(tmp_path / "neither.tif", data), {}),
    }
    for method in (Resampling.nearest, Resampling.bilinear, Resampling.average, Resampling.sum):
        got = {}
        for name, (path, kw) in sources.items():
            with terrio.rio.open(path) as src:
                if on_python_path:
                    force_python_path(src)
                with WarpedVRT(src, crs="EPSG:3857", resampling=method, **kw) as vrt:
                    flags = []
                    vrt._window_hook = lambda level, window, rust, seen=flags: seen.append(rust)
                    got[name] = vrt.read([1, 2, 3])
                assert flags == [not on_python_path], (name, flags)
        np.testing.assert_array_equal(bits(got["mask"]), bits(got["alpha"]), err_msg=str(method))
        assert not np.array_equal(got["mask"], got["neither"]), method


@pytest.mark.parametrize("src_alpha", [-1, -2, 3, 4])
def test_a_src_alpha_outside_the_sources_bands_is_refused_on_both_paths(tmp_path, src_alpha):
    # `src_alpha` is one of the source's bands, 1 to its count: any other value is refused when
    # the WarpedVRT is made, whichever path its reads would take. Taken as given, -1 made band 1
    # the alpha on the Python path (`data[-2]`), and negative values raised OverflowError on the
    # Rust path.
    path = write(tmp_path / "src.tif", values("uint8", 2, 29, 37))
    for on_python_path in (False, True):
        with terrio.rio.open(path) as src:
            if on_python_path:
                force_python_path(src)
            with pytest.raises(WarpOptionsError) as refused:
                WarpedVRT(src, crs="EPSG:3857", src_alpha=src_alpha)
            assert str(refused.value) == f"src_alpha {src_alpha} is not a source band (1-2)"
            for alpha in (1, 2):
                with WarpedVRT(src, crs="EPSG:3857", src_alpha=alpha) as vrt:
                    flags = []
                    vrt._window_hook = lambda level, window, rust, seen=flags: seen.append(rust)
                    vrt.read()
                assert flags == [not on_python_path], flags


def cut_short(tmp_path):
    """A COG cut short: its tiles past the cut cannot be read."""
    path = str(tmp_path / "src.tif")
    terrio.Raster.from_array(values("uint16", 1, 64, 64)[0], GT, crs=CRS).write(
        path, driver="COG", tile_size=16, options={"OVERVIEW_COUNT": "0"}
    )
    with open(path, "r+b") as f:
        f.truncate(f.seek(0, 2) // 2)
    return path


def test_a_window_that_cannot_be_read_raises_as_on_the_python_path(tmp_path):
    # Both paths raise the same RasterioIOError, "Read failed: ..." with the reader's error as
    # its cause.
    path = cut_short(tmp_path)
    errors = []
    for python_path in (False, True):
        with pytest.raises(RasterioIOError, match="^Read failed: ") as raised:
            read(path, python_path, resampling=Resampling.nearest)
        errors.append((str(raised.value), type(raised.value.__cause__)))
    assert errors[0] == errors[1], errors


def test_a_failed_read_raises_its_own_error_when_rasterioioerror_cannot_be_built(
    tmp_path, monkeypatch
):
    # The one-call path builds its RasterioIOError from `terrio.rio.errors`. Were that module
    # unavailable, the read raises its own error, not the import's.
    path = cut_short(tmp_path)
    with pytest.raises(RasterioIOError) as wrapped:
        read(path, False, resampling=Resampling.nearest)
    cause = wrapped.value.__cause__
    monkeypatch.setitem(sys.modules, "terrio.rio.errors", None)
    with pytest.raises(Exception) as raised:
        read(path, False, resampling=Resampling.nearest)
    assert type(raised.value) is type(cause), raised.value
    assert str(raised.value) == str(cause)


@pytest.mark.parametrize("closing", [False, True], ids=["closed", "closing"])
def test_a_source_closed_during_a_read_raises_as_on_the_python_path(tmp_path, closing):
    # The source closed on another thread once the read has chosen its path (here by the hook,
    # before the window is read). `close()` releases the Raster before it marks the dataset
    # closed: closed, both paths raise "Dataset is closed"; in between, "Read failed: ...".
    path = write(tmp_path / "src.tif", values("float32", 1, 29, 37))
    raised = []
    for python_path in (False, True):
        with terrio.rio.open(path) as src:
            if python_path:
                force_python_path(src)

            def close(level, window, rust, src=src):
                if closing:
                    src._raster = None
                else:
                    src.close()

            with WarpedVRT(src, crs="EPSG:3857") as vrt:
                vrt._window_hook = close
                with pytest.raises(RasterioIOError) as error:
                    vrt.read()
            raised.append(str(error.value))
    if closing:
        assert all(e.startswith("Read failed: ") for e in raised), raised
    else:
        assert raised == ["Dataset is closed"] * 2, raised


def test_a_window_read_by_another_datasets_method_comes_from_that_dataset(tmp_path):
    # A source whose `_read_level` is another dataset's: its windows come from that dataset, as
    # the Python path reads them, not from the source's own Raster.
    paths = [write(tmp_path / f"{i}.tif", values("uint16", 1, 29, 37, seed=i)) for i in (0, 1)]
    with terrio.rio.open(paths[0]) as src, terrio.rio.open(paths[1]) as other:
        src._read_level = other._read_level
        with WarpedVRT(src, crs="EPSG:3857") as vrt:
            flags = []
            vrt._window_hook = lambda level, window, rust: flags.append(rust)
            got = vrt.read()
        with WarpedVRT(other, crs="EPSG:3857") as vrt:
            want = vrt.read()
    assert flags == [False], flags
    np.testing.assert_array_equal(got, want)


def test_a_read_level_replaced_on_the_class_reads_the_windows(tmp_path, monkeypatch):
    # `DatasetReader._read_level` replaced on the class itself, as an application or a test may
    # replace it: the windows come from the replacement, through the Python path.
    path = write(tmp_path / "src.tif", values("uint16", 1, 29, 37))
    own, calls = DatasetReader._read_level, []

    def replacement(self, bands, window, level):
        calls.append(window)
        return own(self, bands, window, level)

    monkeypatch.setattr(DatasetReader, "_read_level", replacement)
    with terrio.rio.open(path) as src, WarpedVRT(src, crs="EPSG:3857") as vrt:
        flags = []
        vrt._window_hook = lambda level, window, rust: flags.append(rust)
        vrt.read()
    assert flags == [False], flags
    assert len(calls) == 1, calls


@pytest.mark.parametrize("name", ["cint16_strips.tif", "cfloat32_tiles.tif"])
def test_a_complex_source_is_warped_from_its_real_parts_as_before(tmp_path, name):
    # A complex raster, read in UTM (`src_crs`), warped into a real data type: its window comes
    # into Python as a read gives it, complex values, and the conversion keeps their real parts.
    # The pixels are those of the same warp of a raster holding the real parts.
    grid = {
        "src_crs": CRS,
        "crs": "EPSG:3857",
        "dtype": "float32",
        "resampling": Resampling.bilinear,
    }
    with terrio.rio.open(COMPLEX / name) as src:
        real = src.read().real.astype("float32")
        gt = src.transform.to_gdal()
        with WarpedVRT(src, **grid) as vrt, warnings.catch_warnings():
            # Converted into float32, the complex values keep their real parts, as meant here.
            warnings.simplefilter("ignore", COMPLEX_WARNING)
            flags = []
            vrt._window_hook = lambda level, window, rust: flags.append(rust)
            got = vrt.read()
    with terrio.rio.open(write(tmp_path / "real.tif", real, gt=gt)) as src:
        with WarpedVRT(src, **grid) as vrt:
            want = vrt.read()
    np.testing.assert_array_equal(bits(got), bits(want))
    assert flags == [False], flags


@pytest.mark.parametrize("masked", [False, True], ids=["no mask", "internal mask"])
def test_a_read_while_another_thread_reads_the_source_raises_as_a_read_does(tmp_path, masked):
    # Another thread reads the source's Raster, which a read holds until it returns: a WarpedVRT
    # read that needs the Raster meanwhile raises the RasterioIOError a read raises then, "Read
    # failed: Already borrowed", with the RuntimeError as its cause; with an internal mask as
    # well, whose presence the read does not ask the Raster. The other thread holds the Raster
    # from before the WarpedVRT reads until after it has raised: the band it reads is an object
    # whose `__index__`, which the read calls once it holds the Raster, waits until then.
    n = 64
    mask = np.full((n, n), 255, "uint8") if masked else None
    path = write(tmp_path / "src.tif", values("float32", 1, n, n), mask=mask)
    holding, done = threading.Event(), threading.Event()

    class Band:
        def __index__(self):
            holding.set()
            done.wait(60)
            return 1

    with terrio.rio.open(path) as src, WarpedVRT(src, crs="EPSG:3857") as vrt:
        thread = threading.Thread(target=src._raster.read, kwargs={"band": Band()})
        thread.start()
        try:
            assert holding.wait(60), "the other thread never held the Raster"
            with pytest.raises(RuntimeError, match="Already mutably borrowed"):
                _ = src._raster.width
            with pytest.raises(RasterioIOError) as raised:
                vrt.read(1, out_shape=(16, 16))
        finally:
            done.set()
            thread.join()
    assert str(raised.value) == "Read failed: Already borrowed", raised.value
    assert isinstance(raised.value.__cause__, RuntimeError), raised.value.__cause__


def run(code, *args, env=None, timeout=300):
    """Runs `code` in a new interpreter, for at most `timeout` seconds, and returns what it prints
    as JSON."""
    out = subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code), *map(str, args)],
        capture_output=True,
        text=True,
        check=True,
        env=env,
        timeout=timeout,
    )
    return json.loads(out.stdout.splitlines()[-1])


def cpus():
    """The CPUs this process may run on."""
    if hasattr(os, "sched_getaffinity"):
        return len(os.sched_getaffinity(0))
    return os.cpu_count() or 1


# A WarpedVRT read (of argv[1], with the resampling argv[2]) while another Python thread counts;
# then, for as long, that thread counting while this one hashes without the GIL (hashlib releases
# it for large inputs), so that in both a thread other than the counting one keeps a CPU busy.
# Prints both rates, in a fresh interpreter.
COUNTING = """
import hashlib, json, sys, threading, time
import terrio.rio
from terrio.rio.enums import Resampling
from terrio.rio.vrt import WarpedVRT

count, stop, started = [0], threading.Event(), threading.Event()

def counting():
    started.set()
    while not stop.is_set():
        count[0] += 1

payload = bytes(1 << 22)
with terrio.rio.open(sys.argv[1]) as src:
    with WarpedVRT(src, crs="EPSG:4326", resampling=Resampling[sys.argv[2]]) as vrt:
        paths = []
        vrt._window_hook = lambda level, window, rust: paths.append(rust)
        thread = threading.Thread(target=counting)
        thread.start()
        try:
            started.wait()
            first, start = count[0], time.perf_counter()
            vrt.read(1)
            read, took = count[0], time.perf_counter() - start
            start = time.perf_counter()
            while time.perf_counter() - start < took:
                hashlib.sha256(payload).digest()
            hashed, spent = count[0], time.perf_counter() - start
        finally:
            stop.set()
            thread.join()
print(json.dumps({"read": took, "during": (read - first) / took,
                  "alone": (hashed - read) / spent, "paths": paths}))
"""


def test_the_counting_interpreter_ends_when_its_read_fails(tmp_path):
    # The counting thread stops whatever the read does: a read that fails ends the interpreter
    # with the read's error, rather than leaving it waiting for that thread until the timeout.
    with pytest.raises(subprocess.CalledProcessError) as failed:
        run(COUNTING, cut_short(tmp_path), "nearest", timeout=60)
    assert "RasterioIOError: Read failed: " in failed.value.stderr, failed.value.stderr


@pytest.mark.skipif(cpus() < 2, reason="the warp and the counting thread need a CPU each")
@pytest.mark.parametrize(
    ("method", "compress"),
    [pytest.param("cubic", None, id="warp"), pytest.param("nearest", "deflate", id="read")],
)
def test_a_warped_vrt_lets_other_threads_run_while_it_warps(tmp_path, method, compress):
    # A 2048 x 2048 float32 raster in 16 x 16 tiles, warped by a WarpedVRT into WGS 84 and read
    # whole, on one thread of the warp's pool: with cubic resampling from uncompressed tiles the
    # warp takes most of the read, with nearest from DEFLATE tiles reading the window does.
    # Meanwhile another Python thread counts, then counts for as long while the reading thread
    # hashes without the GIL: with the GIL released while the window is read and warped, it
    # counts about as fast during the read as during the hashing, and at least half as fast;
    # held through either step, a third as fast or less. Both rates come from one process, one
    # after the other, each with one other thread busy, so a busy machine slows both alike.
    n = 2048
    data = (np.arange(n * n, dtype="float64") % 997).astype("float32").reshape(1, n, n)
    path = write(tmp_path / "big.tif", data, compress=compress)
    out = run(COUNTING, path, method, env={**os.environ, "RAYON_NUM_THREADS": "1"})
    assert out["read"] > 0.1, out
    assert out["during"] > out["alone"] / 2, out
    assert out["paths"] and all(out["paths"]), out
