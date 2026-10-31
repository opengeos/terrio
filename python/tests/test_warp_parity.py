"""Plain `reproject` and `calculate_default_transform` against rasterio's (GDAL's warper).

GDAL's general warp kernel (`GWKGeneralCaseThread`) skips an output pixel whose source pixel is
invalid under a unified source mask (nodata on every band, as rasterio's `UNIFIED_SRC_NODATA=YES`
gives); and PROJ maps the poles to finite Web Mercator northings, which `GDALSuggestedWarpOutput2`
samples for a global grid. Exact comparisons; skipped without rasterio.
"""

import re

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from affine import Affine  # noqa: E402
from rasterio.crs import CRS as RCRS  # noqa: E402
from rasterio.warp import Resampling as RR  # noqa: E402
from rasterio.warp import calculate_default_transform as r_cdt  # noqa: E402
from rasterio.warp import reproject as r_reproject  # noqa: E402
from rasterio.warp import transform as r_transform  # noqa: E402

from terrio.rio.crs import CRS as TCRS  # noqa: E402
from terrio.rio.warp import Resampling as TR  # noqa: E402
from terrio.rio.warp import calculate_default_transform as t_cdt  # noqa: E402
from terrio.rio.warp import reproject as t_reproject  # noqa: E402
from terrio.rio.warp import transform as t_transform  # noqa: E402


@pytest.mark.parametrize("resampling", ["bilinear", "cubic", "lanczos"])
def test_pixels_over_invalid_source_pixels_are_skipped(resampling):
    """A float32 source with a NaN hole, NaN nodata: the pixels GDAL leaves empty."""
    yy, xx = np.mgrid[0:96, 0:120]
    src = ((np.sin(xx / 7) + np.cos(yy / 5) + 2) * 50).astype("float32")[np.newaxis]
    src[:, 30:55, 40:70] = np.nan
    out = []
    for reproject, enum in ((r_reproject, RR), (t_reproject, TR)):
        dst = np.zeros((1, 200, 250), "float32")
        reproject(
            src,
            dst,
            src_transform=Affine(10.0, 0, 498000.0, 0, -10.0, 4985000.0),
            src_crs="EPSG:32633",
            dst_transform=Affine(6.0, 0, 1666900.0, 0, -6.0, 5624300.0),
            dst_crs="EPSG:3857",
            src_nodata=np.nan,
            dst_nodata=np.nan,
            resampling=enum[resampling],
        )
        out.append(dst)
    np.testing.assert_array_equal(out[1], out[0])


def test_global_default_transform_to_web_mercator():
    """A global geographic grid to Web Mercator: GDAL's 33 x 401 grid, poles included."""
    args = (360, 180, -180, -90, 180, 90)
    assert t_cdt("EPSG:4326", "EPSG:3857", *args) == r_cdt("EPSG:4326", "EPSG:3857", *args)
    lon, lat = [-180.0, 0.0, 180.0, 0.0], [90.0, -90.0, 89.0, 0.0]
    expected = r_transform(RCRS.from_epsg(4326), RCRS.from_epsg(3857), lon, lat)
    assert t_transform(TCRS.from_epsg(4326), TCRS.from_epsg(3857), lon, lat) == expected


@pytest.mark.parametrize("resampling", ["nearest", "bilinear"])
def test_one_band_values_avoid_the_output_nodata(resampling):
    """One band without source nodata, output nodata 255: the output starts at 255 (rasterio's
    `INIT_DEST=NO_DATA`), so GDAL 3.12 makes an output validity mask and its kernels move warped
    255s to 254 (`AvoidNoData()`)."""
    yy, xx = np.mgrid[0:60, 0:80]
    src = ((xx * 7 + yy * 3) % 256).astype("uint8")
    t = Affine(0.01, 0, 10.0, 0, -0.01, 50.0)
    out = []
    for reproject, enum in ((r_reproject, RR), (t_reproject, TR)):
        dst = np.zeros((60, 80), "uint8")
        reproject(
            src,
            dst,
            src_transform=t,
            src_crs="EPSG:4326",
            dst_transform=t,
            dst_crs="EPSG:4326",
            dst_nodata=255,
            resampling=enum[resampling],
        )
        out.append(dst)
    np.testing.assert_array_equal(out[1], out[0])
    np.testing.assert_array_equal(out[1], np.where(src == 255, 254, src))


@pytest.mark.parametrize("resampling", ["nearest", "bilinear"])
def test_multiband_nodata_values_avoid_the_output_nodata(resampling):
    """Two bands with nodata 9999, unified (rasterio's `UNIFIED_SRC_NODATA=YES`): band 2 is valid
    everywhere, so band 1's nodata values are warped, and GDAL 3.12.2 leaves them at the output
    nodata value: it moves a pixel off it only where every band is at it."""
    yy, xx = np.mgrid[0:120, 0:160]
    b1 = 200.0 + np.sin(xx / 9.0) * 50 + yy * 0.5
    b2 = 1000.0 + np.cos(yy / 7.0) * 80 + xx
    b1[30:70, 40:100] = 9999.0
    src = np.stack([b1, b2])
    out = []
    for reproject, enum in ((r_reproject, RR), (t_reproject, TR)):
        dst = np.zeros((2, 185, 185), "float64")
        reproject(
            src,
            dst,
            src_transform=Affine(0.05, 0, -100.0, 0, -0.05, 45.0),
            src_crs="EPSG:4326",
            dst_transform=Affine(5000.0, 0, -11140000.0, 0, -5000.0, 5630000.0),
            dst_crs="EPSG:3857",
            src_nodata=9999.0,
            dst_nodata=9999.0,
            resampling=enum[resampling],
        )
        out.append(dst)
    np.testing.assert_array_equal(out[1], out[0])


def _u8_starts():
    """Destinations for `init_dest_nodata=False`: without a 255, with one (outside the warped
    part), and random values (255s among them)."""
    one = np.zeros((60, 80), "uint8")
    one[59, 79] = 255
    rand = np.random.default_rng(5).integers(0, 256, (60, 80)).astype("uint8")
    return {"zeros": np.zeros((60, 80), "uint8"), "one 255": one, "random": rand}


@pytest.mark.parametrize("resampling", ["nearest", "bilinear", "cubic", "lanczos", "average"])
@pytest.mark.parametrize("start", ["zeros", "one 255", "random"])
def test_without_init_dest_nodata_matches_rasterio(resampling, start):
    """`init_dest_nodata=False`: the warp starts from the destination's values (GDAL without
    `INIT_DEST`), which pixels nothing is warped to keep. GDAL makes the output validity mask from
    them, so its Byte kernels without masks (nearest, bilinear, cubic) leave warped 255s on the
    nodata value when no value is 255, and move them to 254 when one is; Lanczos and average have
    no such kernel and always move them."""
    yy, xx = np.mgrid[0:60, 0:80]
    src = ((xx * 7 + yy * 3) % 256).astype("uint8")
    out = []
    for reproject, enum in ((r_reproject, RR), (t_reproject, TR)):
        dst = _u8_starts()[start]
        reproject(
            src,
            dst,
            src_transform=Affine(0.01, 0, 10.0, 0, -0.01, 50.0),
            src_crs="EPSG:4326",
            # Shifted: part of the destination is outside the source.
            dst_transform=Affine(0.01, 0, 10.05, 0, -0.01, 49.98),
            dst_crs="EPSG:4326",
            dst_nodata=255,
            resampling=enum[resampling],
            init_dest_nodata=False,
        )
        out.append(dst)
    np.testing.assert_array_equal(out[1], out[0])
    if start == "zeros" and resampling == "nearest":
        assert np.count_nonzero(out[1] == 255) > 0
    if start == "one 255":
        assert out[1][59, 79] == 255 and np.count_nonzero(out[1] == 255) == 1


@pytest.mark.parametrize("resampling", ["nearest", "bilinear"])
def test_multiband_without_init_dest_nodata_matches_rasterio(resampling):
    """Two bands with nodata 9999 warped onto zeros (`init_dest_nodata=False`): GDAL makes the
    output validity mask of several bands always, and band 1's warped nodata values stay on it."""
    yy, xx = np.mgrid[0:120, 0:160]
    b1 = 200.0 + np.sin(xx / 9.0) * 50 + yy * 0.5
    b2 = 1000.0 + np.cos(yy / 7.0) * 80 + xx
    b1[30:70, 40:100] = 9999.0
    out = []
    for reproject, enum in ((r_reproject, RR), (t_reproject, TR)):
        dst = np.zeros((2, 185, 185), "float64")
        dst[:, ::7] = 9999.0
        reproject(
            np.stack([b1, b2]),
            dst,
            src_transform=Affine(0.05, 0, -100.0, 0, -0.05, 45.0),
            src_crs="EPSG:4326",
            dst_transform=Affine(5000.0, 0, -11140000.0, 0, -5000.0, 5630000.0),
            dst_crs="EPSG:3857",
            src_nodata=9999.0,
            dst_nodata=9999.0,
            resampling=enum[resampling],
            init_dest_nodata=False,
        )
        out.append(dst)
    np.testing.assert_array_equal(out[1], out[0])


@pytest.mark.parametrize("resampling", ["bilinear", "cubic", "cubic_spline", "lanczos"])
@pytest.mark.parametrize("res", [0.000568, 0.0004, 0.001], ids=["down 0.88", "up 1.25", "down 0.5"])
@pytest.mark.parametrize("nodata", [None, -5000.0], ids=["no nodata", "nodata"])
def test_float32_kernels_match_rasterio_without_a_projection(resampling, res, nodata):
    """Random float32 values warped within one CRS (the transformation is the geotransforms'
    alone): GDAL's kernels, with and without masks, at scales that take the 4-sample and the
    general formulas, give these values to the last bit. (The float32 pixels that differ through a
    projection come from source coordinates that differ in the last digits, not from the
    kernels.)"""
    src = np.random.default_rng(3).uniform(-1000, 1000, (1, 300, 400)).astype("float32")
    shape = (1, int(0.15 / res), int(0.2 / res))
    out = []
    for reproject, enum in ((r_reproject, RR), (t_reproject, TR)):
        dst = np.zeros(shape, "float32")
        reproject(
            src,
            dst,
            src_transform=Affine(0.0005, 0, -115.26, 0, -0.0005, 35.65),
            src_crs="EPSG:4326",
            dst_transform=Affine(res, 0, -115.26 + 0.000123, 0, -res, 35.65 - 0.000077),
            dst_crs="EPSG:4326",
            src_nodata=nodata,
            dst_nodata=nodata,
            resampling=enum[resampling],
        )
        out.append(dst)
    np.testing.assert_array_equal(out[1], out[0])


_TYPES = ["uint8", "uint16", "int16", "float32", "float64"]


def _typed_source(dtype):
    """60 x 80 values of `dtype`: halves (negative ones too), the nodata value 200, values that
    round onto it (199.6), and values out of the 16-bit types' ranges, as far as `dtype` holds
    them."""
    yy, xx = np.mgrid[0:60, 0:80]
    v = ((xx * 7 + yy * 3) % 256).astype("float64") - 60.0
    v[(xx + yy) % 3 == 0] += 0.5
    v[10:20, 10:30] = 200.0
    v[30:35, 40:70] = 199.6
    v[40:45, 5:25] = 70000.25
    v[50:55, 5:25] = -40000.5
    if np.dtype(dtype).kind in "iu":
        info = np.iinfo(dtype)
        v = np.clip(np.round(v), info.min, info.max)
    return v.astype(dtype)[np.newaxis]


@pytest.mark.parametrize("resampling", ["nearest", "bilinear"])
@pytest.mark.parametrize("src_nodata", [None, 200], ids=["output nodata", "both nodata"])
@pytest.mark.parametrize("dst_dtype", _TYPES)
@pytest.mark.parametrize("src_dtype", _TYPES)
def test_working_type_matches_rasterio(src_dtype, dst_dtype, src_nodata, resampling):
    """GDAL's kernels work in the union of the source's and the output's types
    (`GDALWarpResolveWorkingDataType()`): it picks them (Float64 and Int32 have none without
    masks), values are moved off the nodata value in it, and `GDALCopyWords()` converts the result
    to the output type, which can bring a moved value back onto the nodata value (Float64
    200.00000000000003 is Float32 200, Float32 200.00002 is Byte 200) and rounds halves away from
    zero for signed types."""
    src = _typed_source(src_dtype)
    out = []
    for reproject, enum in ((r_reproject, RR), (t_reproject, TR)):
        dst = np.zeros((1, 50, 70), dst_dtype)
        reproject(
            src,
            dst,
            src_transform=Affine(0.01, 0, 10.0, 0, -0.01, 50.0),
            src_crs="EPSG:4326",
            dst_transform=Affine(0.0113, 0, 10.004, 0, -0.0113, 49.996),
            dst_crs="EPSG:4326",
            src_nodata=src_nodata,
            dst_nodata=200,
            resampling=enum[resampling],
        )
        out.append(dst)
    np.testing.assert_array_equal(out[1], out[0])
