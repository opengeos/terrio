"""A region warp reads a read-only source window where it is, and copies any other first."""

import json
import os
import subprocess
import sys
import textwrap
import threading

import numpy as np
import pytest

import terrio
from terrio import _terrio
from terrio.rio.warp import calculate_default_transform

RESAMPLINGS = [
    "nearest",
    "bilinear",
    "cubic",
    "cubicspline",
    "lanczos",
    "average",
    "rms",
    "mode",
    "min",
    "max",
    "med",
    "q1",
    "q3",
    "sum",
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

# A source of 37 x 29 pixels of 10 m in UTM zone 33N, and three bands of it.
SRC_SIZE = (37, 29)
SRC_GT = (447090.0, 10.0, 0.0, 6022390.0, 0.0, -10.0)
BANDS = 3

needs_proc = pytest.mark.skipif(
    not os.path.exists("/proc/self/status"), reason="reads the peak RSS from /proc"
)


def values(dtype, nodata, seed=0):
    """Three bands of the source in `dtype`, the whole range of an integer type, floats with
    NaNs; with `nodata`, nodata values in two bands, partly at the same pixels."""
    rng = np.random.default_rng(seed)
    shape = (BANDS, SRC_SIZE[1], SRC_SIZE[0])
    dt = np.dtype(dtype)
    if dt.kind in "iu":
        info = np.iinfo(dt)
        v = rng.integers(info.min, info.max, size=shape, dtype=dt, endpoint=True)
    else:
        v = (rng.standard_normal(shape) * 1000).astype(dt)
        v[1, 3:5, 7:12] = np.nan
    if nodata is not None:
        v[0, :12, :20] = nodata
        v[1, 8:20, 15:30] = nodata
    return v


def bits(a):
    """An array's bits, to compare floating-point values bit for bit (NaN included)."""
    a = np.ascontiguousarray(a)
    return a.view(f"u{a.dtype.itemsize}")


def read_only(a):
    a.flags.writeable = False
    return a


@pytest.mark.parametrize("nodata", [None, 3.0])
@pytest.mark.parametrize("dtype", DTYPES)
def test_a_read_only_window_warps_as_its_copy(dtype, nodata):
    # Into Web Mercator at about 2.4 source pixels a pixel, so that the kernels weigh several.
    left, top = SRC_GT[0], SRC_GT[3]
    right, bottom = left + SRC_SIZE[0] * SRC_GT[1], top + SRC_SIZE[1] * SRC_GT[5]
    dst, width, height = calculate_default_transform(
        "EPSG:32633", "EPSG:3857", *SRC_SIZE, left, bottom, right, top, resolution=41.0
    )
    region = (0, 0, width, height)
    v = values(dtype, nodata)
    for resampling in RESAMPLINGS:
        rw = _terrio.RegionWarp(
            SRC_SIZE,
            SRC_GT,
            "EPSG:32633",
            "EPSG:3857",
            dst.to_gdal(),
            resampling,
            0.125,
            dtype,
            [nodata] * BANDS,
        )
        win = rw.source_window(region)
        x, y, w, h = win
        window = np.ascontiguousarray(v[:, y : y + h, x : x + w])
        # Read where it is; copied while writeable; copied in C order when Fortran-ordered or
        # strided, though read-only.
        borrowed = rw.warp(region, win, read_only(window.copy()))
        copied = rw.warp(region, win, window.copy())
        fortran = rw.warp(region, win, read_only(np.asfortranarray(window)))
        strided = rw.warp(region, win, read_only(np.repeat(window, 2, axis=2))[:, :, ::2])
        assert np.asarray(borrowed[1]).any(), resampling
        for other in (copied, fortran, strided):
            np.testing.assert_array_equal(bits(borrowed[0]), bits(other[0]), err_msg=resampling)
            np.testing.assert_array_equal(
                np.asarray(borrowed[1]), np.asarray(other[1]), err_msg=resampling
            )


def run(code, *args):
    """Runs `code` in a new interpreter, and returns what it prints as JSON."""
    out = subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code), *map(str, args)],
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(out.stdout.splitlines()[-1])


# This interpreter's own peak RSS in bytes (VmHWM), not getrusage's, which keeps the peak of the
# process forked to start it.
PEAK = """
def peak():
    with open("/proc/self/status") as f:
        return next(int(line.split()[1]) for line in f if line.startswith("VmHWM:")) * 1024
"""


@needs_proc
def test_a_read_only_window_is_warped_without_a_copy():
    # A window of 8192 x 4096 uint16 (64 MiB) warped into a 256 x 128 region: read where it is,
    # its peak RSS grows by less than a quarter of the window; writeable, by its copy.
    grown = run(
        PEAK
        + """
import json
import numpy as np
from terrio import _terrio

gt = (0.0, 1.0, 0.0, 4096.0, 0.0, -1.0)
dst = (0.0, 32.0, 0.0, 4096.0, 0.0, -32.0)
rw = _terrio.RegionWarp((8192, 4096), gt, "EPSG:3857", "EPSG:3857", dst, "nearest", 0.125, "uint16",
                        [None])
region = (0, 0, 256, 128)
win = rw.source_window(region)
# A small region first, so that what warping starts (threads) is not counted below.
small = (0, 0, 2, 2)
swin = rw.source_window(small)
rw.warp(small, swin, np.full((1, swin[3], swin[2]), 7, "uint16"))
window = np.full((1, win[3], win[2]), 7, "uint16")
window.flags.writeable = False
base = peak()
rw.warp(region, win, window)
borrowed = peak() - base
window.flags.writeable = True
base = peak()
rw.warp(region, win, window)
print(json.dumps([window.nbytes, borrowed, peak() - base]))
"""
    )
    nbytes, borrowed, copied = grown
    assert nbytes == 8192 * 4096 * 2
    assert borrowed < nbytes / 4, grown
    assert copied > nbytes * 3 / 4, grown


@needs_proc
def test_a_warped_vrt_holds_its_source_window_once(tmp_path):
    # 8192 x 4096 uint16 (64 MiB) without overviews, warped by a WarpedVRT into 256 x 128 pixels:
    # its one region reads the whole source. The read's peak RSS grows by what reading that window
    # alone adds (the window and the reader's own buffers, in another process), and by less than
    # half the window more, where a copy of the window for the warp added all of it.
    n = (8192, 4096)
    path = str(tmp_path / "u16.tif")
    v = (np.arange(n[0] * n[1], dtype="uint32") % 1009).astype("uint16").reshape(n[1], n[0])
    terrio.Raster.from_array(v, (0.0, 1.0, 0.0, float(n[1]), 0.0, -1.0), crs="EPSG:3857").write(
        path, driver="COG", tile_size=512, options={"OVERVIEW_COUNT": "0"}
    )
    read = run(
        PEAK
        + """
import json, sys
import terrio.rio

with terrio.rio.open(sys.argv[1]) as src:
    base = peak()
    window = src._read_level([1], (0, 0, 8192, 4096), None)
    print(json.dumps(peak() - base))
""",
        path,
    )
    warped = run(
        PEAK
        + """
import json, sys
import terrio.rio
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT

with terrio.rio.open(sys.argv[1]) as src:
    grid = {"crs": "EPSG:3857", "transform": from_origin(0, 4096, 32, 32), "width": 256}
    with WarpedVRT(src, resampling=Resampling.nearest, height=128, **grid) as vrt:
        base = peak()
        out = vrt.read(1)
        print(json.dumps([int(out[5, 7]), peak() - base]))
""",
        path,
    )
    assert warped[0] == int(v[5 * 32 + 16, 7 * 32 + 16])
    window = n[0] * n[1] * 2
    assert read >= window, read
    assert warped[1] < read + window / 2, (read, warped)


def test_a_borrowed_window_keeps_the_gil():
    # A read-only view may have writable aliases. No other Python thread may run while
    # RegionWarp holds a Rust slice of it. The worker deliberately never writes the data.
    ready, start, ran = threading.Event(), threading.Event(), threading.Event()

    def worker():
        ready.set()
        start.wait()
        ran.set()

    thread = threading.Thread(target=worker)
    thread.start()
    ready.wait()
    old_interval = sys.getswitchinterval()
    try:
        sys.setswitchinterval(100.0)
        n = 1024
        base = np.ones((1, n, n), dtype="float64")
        window = base.view()
        window.flags.writeable = False
        gt = (0.0, 1.0, 0.0, float(n), 0.0, -1.0)
        rw = _terrio.RegionWarp(
            (n, n),
            gt,
            "EPSG:3857",
            "EPSG:3857",
            gt,
            "nearest",
            0.125,
            "float64",
            [None],
        )
        start.set()
        out, _ = rw.warp((0, 0, n, n), (0, 0, n, n), window)
        assert not ran.is_set(), "borrowed NumPy storage was exposed without the GIL"
        np.testing.assert_array_equal(np.asarray(out), base)
    finally:
        sys.setswitchinterval(old_interval)
        start.set()
        thread.join()


def test_a_borrowed_window_forwards_nodata_warnings():
    gt = (0.0, 1.0, 0.0, 2.0, 0.0, -1.0)
    rw = _terrio.RegionWarp(
        (2, 2),
        gt,
        "EPSG:3857",
        "EPSG:3857",
        gt,
        "nearest",
        0.125,
        "uint8",
        [None],
        255.0,
    )
    data = read_only(np.full((1, 2, 2), 255, dtype="uint8"))
    messages = []
    _terrio._set_warp_nodata_logger(messages.append)
    try:
        out, _ = rw.warp((0, 0, 2, 2), (0, 0, 2, 2), data, init=255.0)
        assert len(messages) == 1
        assert "255" in messages[0] and "254" in messages[0]
        np.testing.assert_array_equal(np.asarray(out), 254)
    finally:
        _terrio._set_warp_nodata_logger(None)


def test_a_source_window_preserves_the_sources_writeable_flag(tmp_path, monkeypatch):
    import terrio.rio
    from terrio.rio.vrt import WarpedVRT

    path = str(tmp_path / "source.tif")
    terrio.Raster.from_array(
        np.ones((8, 8), dtype="uint16"), (0.0, 1.0, 0.0, 8.0, 0.0, -1.0), crs="EPSG:3857"
    ).write(path)
    with terrio.rio.open(path) as src, WarpedVRT(src) as vrt:
        cached = src.read()
        monkeypatch.setattr(src, "_read_level", lambda *args: cached)
        window, _ = vrt._source_window(None, (0, 0, 8, 8))
        assert cached.flags.writeable
        assert not window.flags.writeable
        assert np.shares_memory(window, cached)
