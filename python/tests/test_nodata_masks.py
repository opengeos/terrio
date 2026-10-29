"""Masks of floating-point bands with a nodata value, as GDAL's nodata mask band gives them.

GDAL's `GDALNoDataMaskBand` compares Float32 and Float64 values with the nodata value as its
`ARE_REAL_EQUAL` does (`gcore/gdal_cpp_functions.h`): equal, or
`|v - nodata| < FLT_EPSILON * |v + nodata| * 2`, in single precision for Float32 bands and in
double precision, still with FLT_EPSILON, for Float64 ones. Values a warp moved just off the
nodata value (9999 to 9999.000000000002) are therefore masked. terrio compared them exactly and
kept them valid: 9,713 and 3,058 mask pixels of the two bands of a WarpedVRT differed from GDAL's.
GDAL is the oracle. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.enums import Resampling  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

import terrio.rio  # noqa: E402

NODATA = [9999.0, -9999.0, 0.001, 0.0, -3.0e38]


def _near(nodata, dtype, n):
    """Values from the nodata value out past GDAL's tolerance on both sides, values one and two
    steps of the type's precision away, NaN, infinities and the nodata value itself."""
    t = np.dtype(dtype).type
    nd = t(nodata)
    eps = t(np.finfo(np.float32).eps)
    scale = max(abs(nd), t(1e-30))
    k = np.linspace(-6.0, 6.0, n - 8).astype(dtype)
    with np.errstate(over="ignore"):
        values = (nd + k * eps * scale).astype(dtype)
    # One and two steps of the type's precision away, as a warp nudges values off the nodata.
    steps = [np.nextafter(nd, t(np.inf)), np.nextafter(np.nextafter(nd, t(np.inf)), t(np.inf))]
    steps += [np.nextafter(nd, t(-np.inf))]
    specials = np.array([*steps, np.nan, np.inf, -np.inf, nd], dtype)
    return np.concatenate([values, specials])


def _source(path, dtype, nodata):
    """A 2-band raster: values about the nodata value on a background of 1.0, in blocks."""
    height, width = 64, 96
    near = _near(nodata, dtype, height * width // 4)
    bands = []
    for b in range(2):
        a = np.ones((height, width), dtype)
        a[b * 16 : b * 16 + 32, :] = np.resize(np.roll(near, b * 7), (32, width))
        bands.append(a)
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=width,
        height=height,
        count=2,
        dtype=dtype,
        nodata=nodata,
        crs="EPSG:4326",
        transform=from_origin(10.0, 50.0, 0.01, 0.01),
    ) as dst:
        dst.write(np.stack(bands))
    return str(path)


def _masks(ds, **kw):
    masked = np.ma.getmaskarray(ds.read(masked=True, **kw))
    return ds.read_masks(**kw), ds.dataset_mask(**kw), masked


def _assert_same(g, t, what):
    for name, gm, tm in zip(("read_masks", "dataset_mask", "masked read"), g, t, strict=True):
        assert gm.shape == tm.shape, f"{what}: {name} shape"
        assert int((gm != tm).sum()) == 0, f"{what}: {name} differs in {int((gm != tm).sum())}"


@pytest.mark.parametrize("dtype", ["float32", "float64"])
@pytest.mark.parametrize("nodata", NODATA)
def test_masks_of_values_about_the_nodata_value_are_gdals(tmp_path, dtype, nodata):
    src = _source(tmp_path / "src.tif", dtype, nodata)
    with rasterio.open(src) as g, terrio.rio.open(src) as t:
        np.testing.assert_array_equal(t.read(), g.read())
        if nodata != 0.0:
            # Values GDAL masks besides the nodata value itself (none about 0).
            exact = int((g.read() == g.read().dtype.type(nodata)).sum())
            assert int((g.read_masks() == 0).sum()) > exact
        _assert_same(_masks(g), _masks(t), "full")
        win = ((3, 33), (5, 45))
        _assert_same(_masks(g, window=win), _masks(t, window=win), "window")
        far = ((-10, 40), (-20, 40))
        kw = {"window": far, "boundless": True}
        _assert_same(_masks(g, **kw), _masks(t, **kw), "boundless")
        # rasterio's boundless VRT drops source values about the nodata value: they read as it.
        np.testing.assert_array_equal(t.read(**kw), g.read(**kw))
        for r in (Resampling.nearest, Resampling.bilinear):
            kw = {"out_shape": (2, 32, 48), "resampling": r}
            _assert_same(_masks(g, **kw), _masks(t, **kw), f"decimated {r.name}")


def test_resampled_float64_reads_drop_values_about_the_nodata_value_as_gdal(tmp_path):
    """Averaging skips the pixels GDAL's mask band masks: 9999.004 is within FLT_EPSILON's
    tolerance of 9999 (a double-precision epsilon would keep it)."""
    src = str(tmp_path / "src.tif")
    a = np.full((1, 32, 32), 5.0)
    a[0, ::2, ::2] = 9999.004
    a[0, 1::4, 1::4] = 9999.0
    with rasterio.open(
        src, "w", driver="GTiff", width=32, height=32, count=1, dtype="float64", nodata=9999.0
    ) as dst:
        dst.write(a)
    with rasterio.open(src) as g, terrio.rio.open(src) as t:
        for r in (Resampling.average, Resampling.bilinear):
            kw = {"out_shape": (1, 16, 16), "resampling": r}
            np.testing.assert_array_equal(t.read(**kw), g.read(**kw), err_msg=r.name)
            np.testing.assert_array_equal(t.read_masks(**kw), g.read_masks(**kw), err_msg=r.name)


@pytest.mark.parametrize(("dtype", "nodata"), [("uint64", 2**63), ("int64", -(2**63)), ("uint64", 2**53 + 1)])
def test_64_bit_integer_nodata_is_gdals(tmp_path, dtype, nodata):
    # GDAL writes rasterio's nodata (a double) as `%.17g` text, `9.2233720368547758e+18`, and reads
    # the tag of 64-bit integer bands with `strtoull`/`strtoll`: 9 (and -9). terrio writes the
    # value's digits, which both read back exactly.
    data = np.full((1, 32, 32), 5, dtype)
    data[0, :8, :8] = nodata
    data[0, 16:24, 16:24] = 9 if dtype == "uint64" else -9
    for name, lib in (("gdal", rasterio), ("terrio", terrio.rio)):
        path = tmp_path / f"{name}.tif"
        with lib.open(
            path, "w", driver="GTiff", width=32, height=32, count=1, dtype=dtype, nodata=nodata,
            crs="EPSG:4326", transform=from_origin(0, 1, 0.01, 0.01),
        ) as dst:
            dst.write(data)
        with rasterio.open(path) as g, terrio.rio.open(path) as t:
            assert repr(t.nodata) == repr(g.nodata), name
            np.testing.assert_array_equal(t.read_masks(1), g.read_masks(1), err_msg=name)
            if name == "terrio":
                # terrio's own text: the value GDAL reads back, masking the pixels written with it.
                assert int((g.read_masks(1) == 0).sum()) == (64 if nodata != 2**53 + 1 else 0)
