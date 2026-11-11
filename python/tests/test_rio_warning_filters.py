"""terrio changes no warning filter when it reads, so warnings already shown stay shown (#728).

`terrio.rio` multiplied affine transforms inside `warnings.catch_warnings()`, to silence affine 3's
deprecation of `*`. Every change of the filters makes Python forget which warnings it has shown, so
a warning shown once per location (Python's default) was shown again after each window transform,
and the filter list, which the whole process shares, was replaced from reader threads.
"""

import importlib
import random
import sys
import warnings
from concurrent.futures import ThreadPoolExecutor

import numpy as np
import pytest
from affine import Affine

import terrio.pyogrio as tpg
import terrio.rio as rio
from terrio.rio.transform import apply, compose, from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.windows import Window

APPLICATION = "a warning of the application"
TRANSFORM = from_origin(500000, 5000000, 10, 10)
# Rotated by 30 degrees, written as coefficients: bounds of a rotated grid go through `apply`.
ROTATED = Affine(8.660254037844387, -5.0, 500000, 5.0, 8.660254037844387, 5000000)
# The calls that change the process's warning filters.
FILTER_CHANGES = ("catch_warnings", "simplefilter", "filterwarnings", "resetwarnings")


def _raster(tmp_path):
    """A tiled 256 x 256 GeoTIFF in EPSG:32611, with a nodata value."""
    path = tmp_path / "r.tif"
    profile = dict(driver="GTiff", width=256, height=256, count=1, dtype="uint8", tiled=True)
    profile.update(crs="EPSG:32611", transform=TRANSFORM, nodata=255)
    with rio.open(path, "w", **profile) as dst:
        dst.write(np.arange(256 * 256, dtype="uint8").reshape(1, 256, 256))
    return str(path)


def _tile_request(src, i):
    """What a tile server does for tile `i`: the window's transform and bounds, and its pixels."""
    window = Window(64 * (i % 4), 64 * (i // 4 % 4), 64, 64)
    src.window_transform(window)
    bounds = src.window_bounds(window)
    rio.windows.transform(window, src.transform)
    rio.windows.bounds(window, src.transform)
    rio.windows.from_bounds(*bounds, transform=src.transform)
    return src.read(1, window=window)


@pytest.fixture
def filter_changes(monkeypatch):
    """The changes of the warning filters that terrio's own modules make, as they happen."""
    calls = []

    def note(name):
        # The frame of the call (or of the `with` statement), two frames up from here.
        caller = sys._getframe(2).f_globals.get("__name__", "")
        if caller == "terrio" or caller.startswith("terrio."):
            calls.append(f"{caller}: warnings.{name}")

    class CatchWarnings(warnings.catch_warnings):
        # A subclass, not a function: some modules subclass `warnings.catch_warnings` on import.
        def __enter__(self):
            note("catch_warnings")
            return super().__enter__()

    def counting(name, real):
        def counted(*args, **kwargs):
            note(name)
            return real(*args, **kwargs)

        return counted

    monkeypatch.setattr(warnings, "catch_warnings", CatchWarnings)
    for name in FILTER_CHANGES[1:]:
        monkeypatch.setattr(warnings, name, counting(name, getattr(warnings, name)))
    return calls


def _affines_product(a, b):
    """affine's own product: `@` on affine 3, `*` on affine 2, which has no `@`."""
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", PendingDeprecationWarning)
        return a @ b if hasattr(Affine, "__matmul__") else a * b


def test_compose_and_apply_give_affines_products_bit_for_bit_and_warn_nothing():
    rng = random.Random(728)
    transforms = [Affine(*(rng.uniform(-1e7, 1e7) for _ in range(6))) for _ in range(200)]
    transforms += [TRANSFORM, Affine.scale(0.5, -2), Affine.rotation(30), Affine.identity()]
    pairs = list(zip(transforms, transforms[::-1], strict=True))
    points = [(rng.uniform(-1e5, 1e5), rng.uniform(-1e5, 1e5)) for _ in pairs]
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        got = [(compose(a, b), apply(a, *p)) for (a, b), p in zip(pairs, points, strict=True)]
    want = [
        (_affines_product(a, b), _affines_product(a, p))
        for (a, b), p in zip(pairs, points, strict=True)
    ]

    def bits(values):
        return [float(v).hex() for v in values]

    assert all(type(product) is Affine for product, _ in got)
    assert [(bits(t), bits(p)) for t, p in got] == [(bits(t), bits(p)) for t, p in want]


BETWEEN = {
    "nothing": lambda: None,
    "transform.xy": lambda: rio.transform.xy(TRANSFORM, 3, 4),
    "transform.rowcol": lambda: rio.transform.rowcol(TRANSFORM, 500030, 4999960),
    "windows.transform": lambda: rio.windows.transform(Window(1, 2, 3, 4), TRANSFORM),
    "windows.bounds": lambda: rio.windows.bounds(Window(1, 2, 3, 4), TRANSFORM),
    "transform.compose": lambda: compose(TRANSFORM, TRANSFORM),
    "transform.apply": lambda: apply(TRANSFORM, 3, 4),
    "transform.array_bounds": lambda: rio.transform.array_bounds(4, 3, ROTATED),
}


@pytest.mark.parametrize("between", BETWEEN.values(), ids=BETWEEN.keys())
def test_a_warning_raised_five_times_from_one_place_is_shown_once(between):
    # The issue's reproduction, under Python's default action: once per location.
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("default")
        for _ in range(5):
            warnings.warn(APPLICATION, UserWarning, stacklevel=1)
            between()
    assert [str(w.message) for w in caught] == [APPLICATION]


def test_a_warning_shown_once_stays_shown_over_tile_requests_on_eight_threads(tmp_path):
    path = _raster(tmp_path)

    def request(src, i):
        warnings.warn(APPLICATION, UserWarning, stacklevel=1)
        return _tile_request(src, i)

    with rio.open(path) as src, warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("default")
        with ThreadPoolExecutor(8) as pool:
            tiles = list(pool.map(lambda i: request(src, i), range(64)))
    assert [str(w.message) for w in caught] == [APPLICATION]
    assert [int(t.sum()) for t in tiles[:16]] == [int(t.sum()) for t in tiles[16:32]]


def test_reads_change_no_warning_filter(tmp_path, filter_changes):
    path = _raster(tmp_path)
    filter_changes.clear()
    for i in range(16):
        with rio.open(path) as src:
            _tile_request(src, i)
            src.xy(3, 4)
            src.index(*src.xy(3, 4))
            rio.transform.array_bounds(src.height, src.width, ROTATED)
            with WarpedVRT(src, width=128, height=128) as vrt:
                vrt.read(1)
            with WarpedVRT(src, crs="EPSG:4326", width=64, height=64) as vrt:
                vrt.read(1)
    assert filter_changes == []


def test_the_xarray_nodata_check_changes_no_warning_filter(tmp_path, filter_changes):
    pytest.importorskip("xarray")
    txr = importlib.import_module("terrio.xarray")
    path = _raster(tmp_path)
    filter_changes.clear()
    da = txr.open_rasterio(path)
    values = [da.terrio.nodata, da.terrio.encoded_nodata, da.terrio.write_nodata(0).terrio.nodata]
    assert (values, filter_changes) == ([255, None, 0], [])
    with pytest.raises(OverflowError, match="Unable to convert nodata value"):
        da.terrio.write_nodata(256)
    assert filter_changes == []


@pytest.mark.parametrize(
    ("as_utc", "times"),
    [
        (True, ["2024-01-02 02:04:05+00:00", "2024-05-06 05:08:09+00:00"]),
        (False, ["2024-01-02 03:04:05+01:00", "2024-05-06 07:08:09+02:00"]),
    ],
)
def test_read_dataframe_parses_date_times_and_terrio_changes_no_warning_filter(
    tmp_path, filter_changes, as_utc, times
):
    # pandas itself changes the filters (`pandas_dtype` enters `catch_warnings`): only terrio's
    # own changes are counted.
    pytest.importorskip("geopandas")
    pytest.importorskip("pandas")
    path = tmp_path / "t.geojson"
    path.write_text(
        '{"type": "FeatureCollection", "features": ['
        '{"type": "Feature", "properties": {"t": "2024-01-02T03:04:05+01:00"},'
        ' "geometry": {"type": "Point", "coordinates": [1, 2]}},'
        '{"type": "Feature", "properties": {"t": "2024-05-06T07:08:09+02:00"},'
        ' "geometry": {"type": "Point", "coordinates": [3, 4]}}]}'
    )
    filter_changes.clear()
    df = tpg.read_dataframe(str(path), mixed_offsets_as_utc=as_utc)
    assert ([str(v) for v in df["t"]], filter_changes) == (times, [])


def test_concurrent_reads_leave_the_warning_filters_as_they_were(tmp_path):
    path = _raster(tmp_path)
    interval = sys.getswitchinterval()
    with warnings.catch_warnings():
        warnings.filterwarnings("ignore", message="an application's own filter")
        filters = warnings.filters
        before = list(filters)
        sys.setswitchinterval(1e-6)  # many thread switches, so that a race has its chance
        try:
            with rio.open(path) as src, ThreadPoolExecutor(8) as pool:
                list(pool.map(lambda i: _tile_request(src, i), range(256)))
        finally:
            sys.setswitchinterval(interval)
        assert warnings.filters is filters
        assert warnings.filters == before


def test_a_warped_vrt_of_a_size_warns_nothing(tmp_path):
    path = _raster(tmp_path)
    with rio.open(path) as src, warnings.catch_warnings():
        warnings.simplefilter("error")
        with WarpedVRT(src, width=128, height=128) as vrt:
            assert vrt.transform == from_origin(500000, 5000000, 20, 20)
        with WarpedVRT(src, crs="EPSG:4326", width=64, height=64) as vrt:
            assert (vrt.width, vrt.height) == (64, 64)
