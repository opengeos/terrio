"""`terrio.compat` logs each warp kernel run that moves values off the output nodata as rasterio
logs GDAL's warning for it.

When GDAL's warper stores a warped value equal to the output nodata value, it moves it off that
value (`AvoidNoData()`) and warns once per kernel run (`bWarnedAboutDstNoDataReplacement`): `Value
255 in the source dataset has been changed to 254 in the destination dataset to avoid being
treated as NoData. ...` (`CPLE_AppDefined`). A kernel run is a chunk of a `reproject` (one for
these sizes) or a region of a `WarpedVRT` read (a block, or a whole read). rasterio's error handler
logs it at WARNING on the `rasterio._err` logger. The same warps and reads run in a fresh
interpreter with terrio standing in for rasterio and, when rasterio is installed, with rasterio.
"""

import importlib.util
import json
import re
import subprocess
import sys
import textwrap

import numpy as np
import pytest

import terrio.rio as trio
from terrio.rio.transform import from_origin

# The GDAL of the installed rasterio, whose warps run in another interpreter below.
try:
    from rasterio import __gdal_version__ as _RASTERIO_GDAL
except ImportError:
    _RASTERIO_GDAL = ""

_SCRIPT = textwrap.dedent(
    """
    import json, logging, sys

    import numpy as np

    lib, path = sys.argv[1], sys.argv[2]
    if lib == "terrio":
        import terrio.compat
        terrio.compat.install(rioxarray=False, pyogrio=False)
    import rasterio
    from affine import Affine
    from rasterio.enums import Resampling
    from rasterio.vrt import WarpedVRT
    from rasterio.warp import reproject
    from rasterio.windows import Window

    records = []

    class Collect(logging.Handler):
        def emit(self, record):
            if "NoData" in record.getMessage():
                records.append([record.name, record.levelname, record.getMessage()])

    logging.getLogger().addHandler(Collect())
    logging.getLogger().setLevel(logging.DEBUG)

    yy, xx = np.mgrid[0:60, 0:80]
    u8 = ((xx * 7 + yy * 3) % 256).astype("uint8")
    t = Affine(0.01, 0, 10.0, 0, -0.01, 50.0)

    def u8_warp(resampling, dst=None, init=True, bands=1):
        src = u8 if bands == 1 else np.stack([u8] * bands)
        d = np.zeros(src.shape, "uint8") if dst is None else dst
        reproject(src, d, src_transform=t, src_crs="EPSG:4326", dst_transform=t,
                  dst_crs="EPSG:4326", dst_nodata=255, resampling=resampling,
                  init_dest_nodata=init)

    # Band 2 valid everywhere: band 1's 9999s are warped where the (unified) pixel is valid.
    yy2, xx2 = np.mgrid[0:120, 0:160]
    b1 = 200.0 + np.sin(xx2 / 9.0) * 50 + yy2 * 0.5
    b2 = 1000.0 + np.cos(yy2 / 7.0) * 80 + xx2
    b1[30:70, 40:100] = 9999.0

    def f64_warp(resampling):
        reproject(np.stack([b1, b2]), np.zeros((2, 185, 185)),
                  src_transform=Affine(0.05, 0, -100.0, 0, -0.05, 45.0), src_crs="EPSG:4326",
                  dst_transform=Affine(5000.0, 0, -11140000.0, 0, -5000.0, 5630000.0),
                  dst_crs="EPSG:3857", src_nodata=9999.0, dst_nodata=9999.0,
                  resampling=resampling)

    def vrt_reads():
        with rasterio.open(path) as src, WarpedVRT(src, crs="EPSG:3857", nodata=255) as vrt:
            vrt.read(1)
            vrt.read(1)
            vrt.read(1, window=Window(0, 0, 50, 50))
            vrt.read(1, window=Window(0, 0, 50, 50))

    def vrt_blocks():
        with rasterio.open(path) as src, WarpedVRT(src, crs="EPSG:3857", nodata=255) as vrt:
            for _, w in vrt.block_windows(1):
                vrt.read(1, window=w)

    one_nodata = np.zeros((60, 80), "uint8")
    one_nodata[59, 79] = 255
    steps = {
        "nearest": lambda: u8_warp(Resampling.nearest),
        "bilinear, twice": lambda: [u8_warp(Resampling.bilinear), u8_warp(Resampling.bilinear)],
        "two bands": lambda: u8_warp(Resampling.cubic, bands=2),
        "no init, no nodata value": lambda: u8_warp(Resampling.nearest, init=False),
        "no init, a nodata value": lambda: u8_warp(Resampling.nearest, dst=one_nodata, init=False),
        "no init, lanczos": lambda: u8_warp(Resampling.lanczos, init=False),
        "float64": lambda: f64_warp(Resampling.bilinear),
        "WarpedVRT reads": vrt_reads,
        "WarpedVRT blocks": vrt_blocks,
    }
    out = {}
    for name, step in steps.items():
        records.clear()
        step()
        out[name] = list(records)
    origin = rasterio.__spec__.origin or ""
    print(json.dumps({"steps": out, "origin": origin}))
    """
)

_WARNING = (
    "CPLE_AppDefined:Value {} in the source dataset has been changed to {} in the destination "
    "dataset to avoid being treated as NoData. To avoid this, select a different NoData value for "
    "the destination dataset."
)


@pytest.fixture(scope="module")
def tiled(tmp_path_factory) -> str:
    """A 1000 x 700 uint8 GeoTIFF of 256 x 256 tiles, with 255s, without nodata."""
    path = str(tmp_path_factory.mktemp("nodata-warnings") / "tiled.tif")
    yy, xx = np.mgrid[0:700, 0:1000]
    with trio.open(
        path,
        "w",
        driver="GTiff",
        width=1000,
        height=700,
        count=1,
        dtype="uint8",
        crs="EPSG:4326",
        transform=from_origin(10.0, 50.0, 0.001, 0.001),
        tiled=True,
        blockxsize=256,
        blockysize=256,
    ) as ds:
        ds.write(((xx * 7 + yy * 3) % 256).astype("uint8")[np.newaxis])
    return path


def _run(lib: str, path: str) -> dict:
    """The warps and reads in a fresh interpreter, with `lib` as rasterio.

    Args:
        lib: "terrio" (`terrio.compat`) or "rasterio".
        path: The tiled GeoTIFF the WarpedVRT reads.

    Returns:
        dict: The records (logger, level, message) of each step, and the origin of the module
            imported as rasterio.
    """
    result = subprocess.run(
        [sys.executable, "-c", _SCRIPT, lib, path], capture_output=True, text=True, timeout=300
    )
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout.strip().splitlines()[-1])


def test_compat_logs_values_moved_off_the_nodata_as_gdal_warnings(tiled):
    """Once per warp kernel run that moved a value, WARNING on `rasterio._err`, in GDAL's words."""
    out = _run("terrio", tiled)
    assert "terrio" in out["origin"]
    steps = out["steps"]
    record = ["rasterio._err", "WARNING", _WARNING.format(255, 254)]
    assert steps["nearest"] == [record]
    assert steps["bilinear, twice"] == [record, record]
    assert steps["two bands"] == [record]
    # The destination's values without a nodata value: GDAL's kernel without masks moves nothing.
    assert steps["no init, no nodata value"] == []
    assert steps["no init, a nodata value"] == [record]
    assert steps["no init, lanczos"] == [record]
    assert steps["float64"] == [["rasterio._err", "WARNING", _WARNING.format(9999, 9999)]]
    assert len(steps["WarpedVRT blocks"]) > 1


@pytest.mark.skipif(importlib.util.find_spec("rasterio") is None, reason="needs rasterio")
# terrio moves (and warns about) each band's values on their own, as GDAL 3.12.1 does.
@pytest.mark.xfail(
    tuple(int(n) for n in re.findall(r"\d+", _RASTERIO_GDAL)[:3]) >= (3, 12, 2),
    reason="GDAL 3.12.2 moves a pixel of several bands, and warns, only where every band is at "
    "the output nodata, and its warning gives every band's value",
    raises=AssertionError,
    strict=True,
)
def test_compat_nodata_warnings_match_rasterios(tiled):
    """The same records as rasterio's (GDAL 3.12), step by step."""
    terrio_steps = _run("terrio", tiled)["steps"]
    rasterio_out = _run("rasterio", tiled)
    assert "terrio" not in rasterio_out["origin"]
    assert terrio_steps == rasterio_out["steps"]
