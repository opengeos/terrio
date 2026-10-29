"""An imagery pipeline's alpha-band reprojection against rasterio's (GDAL's warper).

The pipeline resamples patch imagery onto its grid with an alpha band built from the validity
mask:
`reproject(source, destination, src_alpha=n, dst_alpha=n)` on float32 arrays whose last band is
`valid * 255`. terrio refused alpha bands; these tests pin the semantics now provided (a binary
alpha is a unified validity mask; the destination alpha marks warped pixels). Warps are compared
exactly, bilinear included: a pixel whose source pixel is invalid is skipped, as GDAL's
GWKGeneralCase skips it. The same machinery serves masked arrays, which rasterio warps with their
mask as an alpha band (as a tile server reprojects masked imagery). Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from affine import Affine  # noqa: E402
from rasterio.enums import Resampling as RR  # noqa: E402
from rasterio.warp import reproject as rio_reproject  # noqa: E402

from terrio.rio.enums import Resampling as TR  # noqa: E402
from terrio.rio.warp import reproject as t_reproject  # noqa: E402

SRC_T = Affine(10.0, 0, 498000.0, 0, -10.0, 4985000.0)
H, W = 96, 120


def _source(dtype, holes):
    yy, xx = np.mgrid[0:H, 0:W]
    data = np.stack(
        [(np.sin(xx / 7) + np.cos(yy / 5) + 2) * 50, (xx * 1.7 + yy) % 255, (xx * yy) % 251]
    ).astype(dtype)
    valid = np.ones((H, W), bool)
    if holes:
        valid[30:55, 40:70] = False
        valid[:, 110:] = False
    return np.concatenate([data, (valid * 255).astype(dtype)[np.newaxis]])


def _both(src, res, resampling, src_alpha=4, dst_alpha=4, count=4):
    """The same warp onto an EPSG:3857 grid partly outside the source, by rasterio and terrio."""
    w, h = int(1500 / res), int(1200 / res)
    dst_t = Affine(res, 0, 1666900.0, 0, -res, 5624300.0)
    out = []
    for fn, rs in ((rio_reproject, RR), (t_reproject, TR)):
        dst = np.zeros((count, h, w), src.dtype)
        fn(
            src,
            dst,
            src_transform=SRC_T,
            src_crs="EPSG:32633",
            dst_transform=dst_t,
            dst_crs="EPSG:3857",
            src_alpha=src_alpha,
            dst_alpha=dst_alpha,
            resampling=rs[resampling],
        )
        out.append(dst)
    return out


@pytest.mark.parametrize("res", [6.0, 14.0, 40.0])
@pytest.mark.parametrize("holes", [True, False])
@pytest.mark.parametrize("dtype", ["float32", "uint8"])
def test_alpha_reproject_matches_rasterio(dtype, holes, res):
    """Transparent pixels are not warped, and the destination alpha is GDAL's, band for band."""
    expected, got = _both(_source(dtype, holes), res, "nearest")
    assert (expected[3] == 0).any() and (expected[3] == 255).any()
    np.testing.assert_array_equal(got, expected)


@pytest.mark.parametrize("src_alpha,dst_alpha,count", [(4, 0, 4), (0, 4, 4), (4, 5, 5)])
def test_one_sided_alpha_matches_rasterio(src_alpha, dst_alpha, count):
    """Only a source alpha, only a destination alpha, and a destination alpha past the bands."""
    src = _source("float32", True)
    expected, got = _both(src, 14.0, "nearest", src_alpha, dst_alpha, count)
    np.testing.assert_array_equal(got, expected)


@pytest.mark.parametrize("holes", [True, False])
def test_float32_bilinear_alpha_matches_rasterio(holes):
    """The pipeline's own call: float32 bands plus `valid * 255`, bilinear."""
    expected, got = _both(_source("float32", holes), 6.0, "bilinear")
    np.testing.assert_array_equal(got, expected)


def test_partial_alpha_is_refused():
    """terrio's kernel cannot blend by density: partial transparency fails closed."""
    src = _source("float32", False)
    src[3, 10:20, 10:20] = 128
    with pytest.raises(ValueError, match="binary alpha"):
        t_reproject(
            src,
            np.zeros((4, 50, 50), "float32"),
            src_transform=SRC_T,
            src_crs="EPSG:32633",
            dst_transform=Affine(30.0, 0, 1666900.0, 0, -30.0, 5624300.0),
            dst_crs="EPSG:3857",
            src_alpha=4,
            dst_alpha=4,
        )


@pytest.mark.parametrize(
    "dtype,src_masked,dst_masked",
    [
        ("float32", True, True),
        ("float32", True, False),
        ("float32", False, True),
        ("uint16", True, True),
        ("uint16", False, True),
    ],
)
def test_masked_arrays_match_rasterio(dtype, src_masked, dst_masked):
    """A masked source's mask is its alpha; a masked destination's mask is where nothing warped."""
    data = _source(dtype, True)
    valid = data[3] > 0
    src = data[:3]
    if src_masked:
        src = np.ma.MaskedArray(src, mask=np.repeat(~valid[np.newaxis], 3, 0), fill_value=7)
    dst_t = Affine(14.0, 0, 1666900.0, 0, -14.0, 5624300.0)
    out = []
    for fn in (rio_reproject, t_reproject):
        dst = np.zeros((3, 85, 107), dtype)
        if dst_masked:
            dst = np.ma.masked_array(dst)
        got, _ = fn(
            src,
            dst,
            src_transform=SRC_T,
            src_crs="EPSG:32633",
            dst_transform=dst_t,
            dst_crs="EPSG:3857",
        )
        out.append(got)
    expected, got = out
    assert isinstance(got, np.ma.MaskedArray) == isinstance(expected, np.ma.MaskedArray)
    np.testing.assert_array_equal(np.ma.getdata(got), np.ma.getdata(expected))
    np.testing.assert_array_equal(np.ma.getmaskarray(got), np.ma.getmaskarray(expected))


def test_masked_16bit_into_unmasked_is_refused():
    """rasterio blends a masked 16-bit source by a 255/65535 density into a plain destination."""
    data = _source("uint16", True)
    src = np.ma.MaskedArray(data[:3], mask=np.repeat(~(data[3] > 0)[np.newaxis], 3, 0))
    with pytest.raises(ValueError, match="masked 16-bit"):
        t_reproject(
            src,
            np.zeros((3, 50, 50), "uint16"),
            src_transform=SRC_T,
            src_crs="EPSG:32633",
            dst_transform=Affine(30.0, 0, 1666900.0, 0, -30.0, 5624300.0),
            dst_crs="EPSG:3857",
        )
