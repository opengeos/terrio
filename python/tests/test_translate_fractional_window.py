"""`terrio.translate` resamples a window at a fractional offset, read at its own size, at that
offset, whatever reads its source (#757): a raster in memory, a dataset from `terrio.open`, a
format read whole (an Esri ASCII grid), or a GeoTIFF given by its path, read by window.
"""

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.windows import Window

# 12 x 8 pixels of 1 m from (0, 8), each holding ten times its column and a hundred times its
# row: a bilinear kernel at an output pixel's centre and an average over the output pixel both
# give the ramp's value there.
RAMP = np.add.outer(np.arange(8) * 100.0, np.arange(12) * 10.0)
# Windows of 6 x 4 pixels at a fractional offset in x, in y and in both.
OFFSETS = [(2.25, 1.0), (2.0, 1.25), (2.25, 1.75)]


@pytest.fixture
def sources(tmp_path):
    """Each way of giving the ramp to `terrio.translate`, by name."""
    profile = dict(
        width=12,
        height=8,
        count=1,
        dtype="float64",
        crs="EPSG:32610",
        transform=rio.transform.from_origin(0, 8, 1, 1),
    )
    for name, driver in (("ramp.tif", "GTiff"), ("ramp.asc", "AAIGrid")):
        with rio.open(tmp_path / name, "w", driver=driver, **profile) as dst:
            dst.write(RAMP[None])
    return {
        "raster": terrio.Raster.from_array(RAMP, (0.0, 1.0, 0.0, 8.0, 0.0, -1.0), crs="EPSG:32610"),
        "dataset": terrio.open(str(tmp_path / "ramp.tif")),
        "ascii grid": str(tmp_path / "ramp.asc"),
        "geotiff path": str(tmp_path / "ramp.tif"),
    }


@pytest.mark.parametrize("window", ["srcwin", "projwin"])
@pytest.mark.parametrize("offset", OFFSETS, ids=["x", "y", "xy"])
@pytest.mark.parametrize("method", ["nearest", "bilinear", "average"])
def test_a_fractional_window_is_resampled_at_its_offset_whatever_reads_its_source(
    sources, window, offset, method
):
    """Bilinear and average give each output pixel the ramp's value at its centre, and nearest
    neighbour the pixel holding that centre, from every source: the same values a windowed read
    through `terrio.rio` gives. A `projwin` read by nearest neighbour starts on a whole pixel."""
    x, y = offset
    bounds = {"srcwin": (x, y, 6, 4), "projwin": (x, 8 - y, x + 6, 4 - y)}[window]
    if method == "nearest" and window == "projwin":
        x, y = float(int(x)), float(int(y))
    cols, rows = np.meshgrid(np.arange(6) + x, np.arange(4) + y)
    expected = {
        "nearest": 10 * np.floor(cols + 0.5) + 100 * np.floor(rows + 0.5),
        "bilinear": 10 * cols + 100 * rows,
        "average": 10 * cols + 100 * rows,
    }[method]
    for name, src in sources.items():
        out = terrio.translate(src, resampling=method, **{window: bounds})
        pixels = np.asarray(out.read(1))[:4, :6]
        assert tuple(out.transform)[:6] == (x, 1.0, 0.0, 8 - y, 0.0, -1.0), name
        np.testing.assert_array_equal(pixels, expected, err_msg=name)
    if window == "srcwin" and method != "nearest":
        with rio.open(sources["geotiff path"]) as src:
            r = getattr(Resampling, method)
            pixels = src.read(1, window=Window(x, y, 6, 4), out_shape=(4, 6), resampling=r)
        np.testing.assert_array_equal(pixels, expected)
