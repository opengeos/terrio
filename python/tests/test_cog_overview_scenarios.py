"""The COG overview scenarios of #165, against GDAL's COG driver.

The Rust tests of #165 (`crates/terrio/src/drivers/gtiff_write.rs`) check COG overviews
against the model of #165: each level reduced from the full-resolution source, with sizes
rounded up. GDAL's COG driver computes each level from the previous one, with sizes rounded down
(65 x 35 gives 32 x 17 and 16 x 8 with 16-pixel tiles), and terrio writes GDAL's overviews.
These are the same scenarios, with GDAL (rasterio) as the oracle: each input is written by GDAL as
a GeoTIFF and converted to a COG by GDAL and by terrio as an ingestion pipeline converts uploads
(`rasterio.shutil.copy(..., driver="COG")`), and every level is compared bit for bit, pixels and
masks. Skipped without rasterio.
"""

import numpy as np
import pytest

import terrio.rio as trio
import terrio.rio.shutil  # noqa: F401

rasterio = pytest.importorskip("rasterio")
import rasterio.shutil  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

COLS, ROWS = 65, 35
NAN = float("nan")


def _p(x, y):
    """The 2 x 2 pattern index of the tests of #165."""
    return (y % 2) * 2 + x % 2


def _scenarios():
    """name: (array (bands, rows, cols), dtype, nodata, method, options), as #165 builds them."""
    out = {}
    classes = [0, 3, 9, 17]
    a = np.array([[classes[(x + 2 * y) % 4] for x in range(COLS)] for y in range(ROWS)], "uint8")[None]
    palette = {i: (i * 13, 255 - i * 13, i * 7, 255) for i in range(18)}
    out["nearest-palette-u8"] = (a, "uint8", None, "nearest", {"palette": palette})
    for bands in (2, 4):
        a = np.zeros((bands, ROWS, COLS), "uint8")
        for y in range(ROWS):
            for x in range(COLS):
                for b in range(bands - 1):
                    a[b, y, x] = [150, 20, 60, 110][_p(x, y)] + b * 17
                a[bands - 1, y, x] = 0 if (8 <= x < 16 and y < 8) else [0, 64, 128, 255][_p(x, y)]
        out[f"average-alpha-{bands}b-u8"] = (a, "uint8", None, "average", {"alpha": True, "rgb": bands == 4})
    for dtype in ("float32", "float64"):
        for bands in (1, 2, 4):
            alpha = bands > 1
            a = np.zeros((bands, ROWS, COLS))
            for b in range(bands):
                for y in range(ROWS):
                    for x in range(COLS):
                        i = y * COLS + x
                        if alpha and b + 1 == bands:
                            v = 0.0 if (24 <= x < 32 and y < 8) else [0.0, 0.125, 0.5, 1.0][(x + 3 * y) % 4]
                        elif 16 <= x < 24 and y < 8:
                            v = 1.0 + [0.0, 2.0**-23, 0.0, 0.0][_p(x, y)]
                        else:
                            large = 2.0**60 * (b + 1)
                            v = [large, (i % 29 + 1) * 0.125, -large, (i % 31 + 1) * 0.25][_p(x, y)]
                        if (8 <= x < 16 and y < 8) or (i + b * 13) % 47 == 0:
                            v = -9999.0
                        elif (i + b * 7) % 53 == 0:
                            v = NAN
                        a[b, y, x] = v
            opts = {"alpha": alpha, "rgb": bands == 4}
            out[f"average-order-{dtype}-{bands}b"] = (a.astype(dtype), dtype, -9999.0, "average", opts)
    a = np.array([[((x * 17 + y * 7) % 101) * 0.25 for x in range(COLS)] for y in range(ROWS)], "float32")
    a[0:8, 8:16] = -9999.0
    for y in range(4):
        for x in range(4):
            a[y, x] = -9999.0 if (x < 2 and y < 2) else 12.0
    a[0, 0] = 0.0
    for method in (None, "average"):
        out[f"valid-counts-f32-{method or 'default'}"] = (a[None].copy(), "float32", -9999.0, method, {})
    a = np.array([[(x * 17 + y * 7) % 101 for x in range(COLS)] for y in range(ROWS)], "uint16")
    a[0:8, 8:16] = 65535
    for y in range(4):
        for x in range(4):
            a[y, x] = 65535 if (x < 2 and y < 2) else 12
        a[y, 16:20] = 0
    a[0, 0] = 0
    for x, y in [(16, 0), (17, 0), (18, 0), (19, 0), (16, 2), (17, 2)]:
        a[y, x] = 1
    out["rounding-u16-average"] = (a[None].copy(), "uint16", 65535.0, "average", {})
    a = np.array([[[1, 5, 5, 9][_p(x, y)] for x in range(COLS)] for y in range(ROWS)], "uint8")[None]
    for method in ("min", "max", "mode"):
        out[f"minmaxmode-u8-{method}"] = (a.copy(), "uint8", None, method, {})
    base = 1 << 53
    pattern = [base, base + 1, base + 3, base + 3]
    a = np.array([[pattern[_p(x, y)] for x in range(COLS)] for y in range(ROWS)], "uint64")[None]
    for method in ("nearest", "min", "max", "mode"):
        out[f"categorical-u64-{method}"] = (a.copy(), "uint64", float(base), method, {})
    # Int64, whose nodata GDAL also holds apart from a double's.
    pattern = [-9999, 5, -(1 << 40), 7]
    a = np.array([[pattern[_p(x, y)] for x in range(COLS)] for y in range(ROWS)], "int64")[None]
    out["categorical-i64-nearest"] = (a, "int64", -9999.0, "nearest", {})
    cols, rows, base = 257, 65, 1 << 63
    a = np.array([base + 1000 + i for i in range(cols * rows)], "uint64").reshape(rows, cols)
    for x, y, v in [(0, 0, base + 1), (1, 0, base + 3), (0, 15, base + 3), (0, 16, base + 1), (30, 31, base + 1), (31, 31, base + 3)]:
        a[y, x] = v
    out["mode-highcard-u64"] = (a[None].copy(), "uint64", None, "mode", {})
    patterns = [[1.0, -0.0, 0.0, 1.0], [NAN, NAN, 7.0, NAN], [NAN] * 4, [2.0, 2.0, -0.0, 0.0], [3.0, -0.0, 0.0, NAN], [3.0, 0.0, -0.0, NAN]]
    a = np.full((ROWS, COLS), NAN)
    for y in range(8):
        for x in range(48):
            a[y, x] = patterns[x // 8][_p(x, y)]
    for dtype in ("float32", "float64"):
        out[f"mode-signed-zero-{dtype}"] = (a.astype(dtype)[None], dtype, -9999.0, "mode", {})
    return out


SCENARIOS = _scenarios()
# GDAL has no MIN or MAX overview resampling (GDALGetResampleFunction): both refuse.
REFUSED = {"minmaxmode-u8-min", "minmaxmode-u8-max", "categorical-u64-min", "categorical-u64-max"}
# GDAL 3.12.1 writes no GDAL_NODATA on the overview IFDs of an Int64 or UInt64 raster, so GDAL
# masks no overview pixel of such a COG by value: terrio's are written the same way, and the masks
# below are compared like every other scenario's.
INT64_NODATA = ("categorical-u64-nearest", "categorical-i64-nearest")


def _source(path, a, dtype, nodata, options):
    bands, rows, cols = a.shape
    profile = dict(driver="GTiff", width=cols, height=rows, count=bands, dtype=dtype, nodata=nodata,
                   crs="EPSG:32633", transform=from_origin(500000, 5000000, 10, 10))
    if options.get("alpha"):
        profile.update(alpha="YES", photometric="RGB" if options.get("rgb") else "MINISBLACK")
    with rasterio.open(path, "w", **profile) as dst:
        dst.write(a)
        if options.get("palette"):
            dst.write_colormap(1, options["palette"])
    return str(path)


def _convert(module, src, dst, method):
    with rasterio.open(src) as ds:
        profile = ds.profile.copy()
    profile.update(driver="COG", blocksize=16, compress="NONE")
    if method is not None:
        profile["overview_resampling"] = method
    module.shutil.copy(src, str(dst), **profile)
    return str(dst)


def _levels(path):
    with rasterio.open(path) as ds:
        count = len(ds.overviews(1))
        out = [(ds.read(), ds.read_masks())]
    for i in range(count):
        with rasterio.open(path, overview_level=i) as ds:
            out.append((ds.read(), ds.read_masks()))
    return out


def _same(a, b):
    """Bit for bit, except that every NaN equals every NaN; -0.0 differs from 0.0."""
    if a.dtype.kind == "f":
        return ((a == b) & (np.signbit(a) == np.signbit(b))) | (np.isnan(a) & np.isnan(b))
    return a == b


@pytest.mark.parametrize("name", [n for n in SCENARIOS if n not in REFUSED])
def test_cog_overview_scenarios_are_gdals(tmp_path, name):
    a, dtype, nodata, method, options = SCENARIOS[name]
    src = _source(tmp_path / "src.tif", a, dtype, nodata, options)
    expected = _levels(_convert(rasterio, src, tmp_path / "gdal.tif", method))
    actual = _levels(_convert(trio, src, tmp_path / "terrio.tif", method))
    assert [x.shape for x, _ in actual] == [x.shape for x, _ in expected], "levels"
    for i, ((ex, em), (ac, am)) in enumerate(zip(expected, actual)):
        assert _same(ex, ac).all(), f"level {i}: {(~_same(ex, ac)).sum()} pixels differ"
        if name in INT64_NODATA and i > 0:
            # GDAL's own overviews are unmasked, though they hold the nodata value.
            assert not (em == 0).any() and (ex == np.array(int(nodata), ex.dtype)).any()
        np.testing.assert_array_equal(am, em, err_msg=f"level {i} masks")


@pytest.mark.parametrize("name", sorted(REFUSED) + ["not-a-resampler"])
def test_resamplings_gdal_refuses_are_refused(tmp_path, name):
    if name == "not-a-resampler":
        a, dtype, nodata, method = np.zeros((1, ROWS, COLS), "uint8"), "uint8", None, "not-a-resampler"
    else:
        a, dtype, nodata, method, _ = SCENARIOS[name]
    src = _source(tmp_path / "src.tif", a, dtype, nodata, {})
    with pytest.raises(Exception):
        _convert(rasterio, src, tmp_path / "gdal.tif", method)
    with pytest.raises(Exception, match="resampling"):
        _convert(trio, src, tmp_path / "terrio.tif", method)
