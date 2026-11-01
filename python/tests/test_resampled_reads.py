"""Resampled reads of `terrio.rio` against rasterio's: `read(window=..., out_shape=...,
resampling=...)` with kernels GDAL's `RasterIOResampled` applies.

Each source is written by GDAL (through rasterio), and read by both with integer and fractional
windows, at the window's size, down- and upsampled, and whole. Pixels and the masks of
`masked=True` reads must be identical: no tolerance. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.enums import Resampling as RR  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402
from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.enums import Resampling as TR  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402

METHODS = ["bilinear", "cubic", "cubic_spline", "lanczos", "average", "mode", "rms", "gauss"]
# (window, out_shape): integer and fractional windows, at the window's size, down- and
# upsampled; whole reads.
READS = {
    "fractional": ((100.3, 50.7, 300.2, 250.6), None),
    "integer-down": ((100, 50, 300, 250), (130, 160)),
    "fractional-up": ((10.5, 20.25, 400.5, 300.75), (520, 610)),
    "whole-down": (None, (233, 301)),
    "whole-up": (None, (1100, 1500)),
}


@pytest.fixture(scope="module")
def sources(tmp_path_factory):
    """uint8 RGB with a per-dataset mask, uint16 and float32 with nodata, all valid."""
    d = tmp_path_factory.mktemp("resampled")
    rng = np.random.default_rng(0)
    y, x = np.mgrid[0:700, 0:900]
    paths = {}
    for name, count, dtype, nodata, mask, overviews in (
        ("rgb-mask", 3, "uint8", None, True, False),
        ("u16-nodata", 1, "uint16", 0, False, True),
        ("f32-nodata", 1, "float32", -9999.0, False, False),
        ("valid", 2, "uint8", None, False, False),
    ):
        data = (
            np.stack(
                [
                    np.sin(x / (9 + b)) * 60 + np.cos(y / 13) * 50 + 120 + rng.normal(0, 8, x.shape)
                    for b in range(count)
                ]
            )
            .clip(1, 250)
            .astype(dtype)
        )
        if nodata is not None:
            data[:, rng.random(x.shape) < 0.03] = nodata
            data[:, 200:260, 300:380] = nodata
        path = str(d / f"{name}.tif")
        with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
            with rasterio.open(
                path,
                "w",
                driver="GTiff",
                width=900,
                height=700,
                count=count,
                dtype=dtype,
                nodata=nodata,
                crs="EPSG:3857",
                transform=from_origin(0, 0, 1, 1),
                tiled=True,
                blockxsize=256,
                blockysize=256,
            ) as dst:
                dst.write(data)
                if mask:
                    m = np.full(x.shape, 255, "uint8")
                    m[:, :120] = 0
                    m[400:470, 500:600] = 0
                    dst.write_mask(m)
                if overviews:
                    dst.build_overviews([2, 4], RR.nearest)
        paths[name] = path
    return paths


@pytest.mark.parametrize("read", list(READS))
@pytest.mark.parametrize("method", METHODS)
@pytest.mark.parametrize("kind", ["rgb-mask", "u16-nodata", "f32-nodata", "valid"])
def test_resampled_reads_match_rasterio(sources, kind, method, read):
    win, shape = READS[read]
    with rasterio.open(sources[kind]) as s:
        out_shape = None if shape is None else (s.count, *shape)
        a = s.read(
            window=RW(*win) if win else None,
            out_shape=out_shape,
            resampling=RR[method],
            masked=True,
        )
    with trio.open(sources[kind]) as s:
        b = s.read(
            window=TW(*win) if win else None,
            out_shape=out_shape,
            resampling=TR[method],
            masked=True,
        )
    assert a.shape == b.shape
    x, y = a.data, b.data
    same = (x == y) | (np.isnan(x) & np.isnan(y)) if x.dtype.kind == "f" else x == y
    assert same.all(), (
        f"{np.mean(~same):.4%} of pixels differ, max |d| {np.abs(x.astype(float) - y)[~same].max()}"
    )
    am, bm = np.ma.getmaskarray(a), np.ma.getmaskarray(b)
    assert np.array_equal(am, bm), f"{np.mean(am != bm):.4%} of the mask differs"


@pytest.mark.parametrize("window", [(-40.5, -30.25, 300.5, 250.75), (700.2, 500.6, 400.3, 350.1)])
@pytest.mark.parametrize("shape", [(130, 160), (520, 610)])
@pytest.mark.parametrize("kind", ["rgb-mask", "u16-nodata", "valid"])
def test_gauss_boundless_reads_match_rasterio(sources, kind, shape, window):
    """Gauss through a boundless read (a VRT GDAL resamples with `RasterIOResampled`)."""
    with rasterio.open(sources[kind]) as s:
        out_shape = (s.count, *shape)
        a = s.read(
            window=RW(*window),
            out_shape=out_shape,
            resampling=RR.gauss,
            boundless=True,
            masked=True,
        )
    with trio.open(sources[kind]) as s:
        b = s.read(
            window=TW(*window),
            out_shape=out_shape,
            resampling=TR.gauss,
            boundless=True,
            masked=True,
        )
    am, bm = np.ma.getmaskarray(a), np.ma.getmaskarray(b)
    assert np.array_equal(am, bm), f"{np.mean(am != bm):.4%} of the mask differs"
    valid = ~am
    assert np.array_equal(a.data[valid], b.data[valid]), (
        f"{np.mean(a.data[valid] != b.data[valid]):.4%} differ"
    )
