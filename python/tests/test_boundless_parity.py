"""Boundless reads of `terrio.rio` against rasterio's (a VRT read by GDAL).

The sources are written by GDAL (through rasterio), with overviews, so the overview a decimating
read uses is GDAL's own. Reads must match exactly: every pixel, masked or not, and the masks.
GDAL is deterministic here, so there is no tolerance. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from affine import Affine  # noqa: E402
from rasterio.enums import ColorInterp as RCI  # noqa: E402
from rasterio.enums import MaskFlags  # noqa: E402
from rasterio.enums import Resampling as RR  # noqa: E402
from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.enums import Resampling as TR  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402

SIZE = (600, 520)


def _data(bands, dtype):
    yy, xx = np.mgrid[0 : SIZE[1], 0 : SIZE[0]]
    return np.stack([(xx * (3 + b) + yy * (5 + 2 * b)) % 251 + 1 for b in range(bands)]).astype(
        dtype
    )


@pytest.fixture(scope="module")
def sources(tmp_path_factory):
    """One source per kind of validity: per-dataset mask, nodata, alpha band (Byte and UInt16),
    none; and RGB with an undefined fourth band, tiled and striped."""
    d = tmp_path_factory.mktemp("boundless")
    common = dict(
        driver="GTiff",
        width=SIZE[0],
        height=SIZE[1],
        crs="EPSG:3857",
        tiled=True,
        blockxsize=128,
        blockysize=128,
        transform=Affine(2, 0, -11800000, 0, -2, 4900000),
    )
    paths = {}
    p = str(d / "mask.tif")
    with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
        with rasterio.open(p, "w", count=3, dtype="uint8", **common) as dst:
            dst.write(_data(3, "uint8"))
            m = np.full(SIZE[::-1], 255, "uint8")
            m[:, :70] = 0
            m[300:350, 200:260] = 0
            dst.write_mask(m)
            dst.build_overviews([2, 4], RR.nearest)
    paths["mask"] = p
    p = str(d / "nodata.tif")
    with rasterio.open(p, "w", count=2, dtype="uint16", nodata=0, **common) as dst:
        v = _data(2, "uint16") * 10
        v[:, 100:140, :] = 0
        dst.write(v)
        dst.build_overviews([2, 4], RR.nearest)
    paths["nodata"] = p
    p = str(d / "alpha.tif")
    # `alpha="YES"` writes ExtraSamples as an alpha band; GDAL then masks bands 1-3 by band 4.
    with rasterio.open(
        p, "w", count=4, dtype="uint8", photometric="RGB", alpha="YES", **common
    ) as dst:
        v = _data(4, "uint8")
        v[3] = 255
        v[3, :, 500:] = 0
        v[3, 200:240, 100:180] = 0
        v[3, 300:310, 300:400] = 128
        dst.write(v)
        dst.build_overviews([2, 4], RR.nearest)
    with rasterio.open(p) as src:
        assert src.colorinterp[3] == RCI.alpha
        assert MaskFlags.alpha in src.mask_flag_enums[0]
    paths["alpha"] = p
    p = str(d / "alpha16.tif")
    # A UInt16 alpha band masks band 1 through a `GDALRescaledAlphaBand`, which has no overviews
    # and maps 1-256 to 1 and other values to v * 255 // 65535: partial values included, 30000
    # (116, where v >> 8 gives 117) inside the windows that upsample band 1 with its mask.
    with rasterio.open(
        p, "w", count=2, dtype="uint16", photometric="MINISBLACK", alpha="YES", **common
    ) as dst:
        v = _data(2, "uint16") * 257
        v[1] = 65535
        v[1, :, 450:] = 0
        v[1, 200:240, 100:180] = 0
        v[1, 150:190, 140:260] = 30000
        v[1, 300:310, 300:400] = 128
        v[1, 400:420, 300:400] = 300
        v[1, 500:530, 50:150] = 30000
        dst.write(v)
        dst.build_overviews([2, 4], RR.nearest)
    with rasterio.open(p) as src:
        assert src.colorinterp[1] == RCI.alpha
        assert MaskFlags.alpha in src.mask_flag_enums[0]
    paths["alpha16"] = p
    # RGB and an undefined fourth band (ExtraSamples unspecified), pixel-interleaved: four bands
    # for GDAL, none of them alpha. The `tiff` crate returns three samples per pixel for it.
    striped = {k: v for k, v in common.items() if k != "blockxsize"}
    striped.update(tiled=False, blockysize=7)
    for kind, layout in (("extra", common), ("extra_strip", striped)):
        p = str(d / f"{kind}.tif")
        with rasterio.open(p, "w", count=4, dtype="uint8", photometric="RGB", **layout) as dst:
            dst.write(_data(4, "uint8"))
            dst.build_overviews([2, 4], RR.nearest)
        with rasterio.open(p) as src:
            assert src.colorinterp[3] == RCI.undefined
        paths[kind] = p
    p = str(d / "none.tif")
    with rasterio.open(p, "w", count=1, dtype="uint8", **common) as dst:
        dst.write(_data(1, "uint8"))
        dst.build_overviews([2, 4], RR.nearest)
    paths["none"] = p
    return paths


WINDOWS = {
    "left": (-40.5, 100.25, 200.3, 150.6),
    "right": (500.7, 50.2, 180.4, 160.1),
    "top": (200.1, -60.4, 150.6, 140.2),
    "bottom": (120.6, 450.3, 160.2, 140.8),
    "top-left": (-70.3, -80.6, 230.4, 210.2),
    "bottom-right": (480.2, 430.7, 220.6, 190.3),
    "around": (-120.4, -90.7, 860.2, 760.6),
    "outside": (700.5, 600.5, 100.0, 80.0),
    "inside": (100.3, 120.6, 200.2, 180.4),
}
SHAPES = {"window": None, "down": (64, 70), "up": (400, 380)}


@pytest.mark.parametrize(
    "kind", ["mask", "nodata", "alpha", "alpha16", "none", "extra", "extra_strip"]
)
@pytest.mark.parametrize("where", list(WINDOWS))
@pytest.mark.parametrize("shape", list(SHAPES))
@pytest.mark.parametrize("fill_value", [None, 7])
def test_nearest_boundless_reads_match_rasterio(sources, kind, where, shape, fill_value):
    """Boundless nearest-neighbour reads give rasterio's pixels and masks.

    For an alpha band, the alpha band decides the other bands' validity; for a
    per-dataset mask read with a fill value, the VRT's mask band starts invalid whatever the fill
    value.
    """
    path = sources[kind]
    win = WINDOWS[where]
    with rasterio.open(path) as s:
        n = s.count
        out_shape = None if SHAPES[shape] is None else (n, *SHAPES[shape])
        a = s.read(
            window=RW(*win), out_shape=out_shape, boundless=True, masked=True, fill_value=fill_value
        )
        a_plain = s.read(
            window=RW(*win), out_shape=out_shape, boundless=True, fill_value=fill_value
        )
    with trio.open(path) as s:
        b = s.read(
            window=TW(*win), out_shape=out_shape, boundless=True, masked=True, fill_value=fill_value
        )
        b_plain = s.read(
            window=TW(*win), out_shape=out_shape, boundless=True, fill_value=fill_value
        )
    _assert_same(a, b)
    assert np.array_equal(a_plain, b_plain), f"{(a_plain != b_plain).mean():.3%} of pixels differ"


def _assert_same(a, b):
    """Masked reads of rasterio and terrio are the same: shape, mask and every value."""
    am, bm = np.ma.getmaskarray(a), np.ma.getmaskarray(b)
    assert a.shape == b.shape
    assert np.array_equal(am, bm), f"{(am != bm).mean():.3%} of the mask differs"
    ad, bd = np.ma.getdata(a), np.ma.getdata(b)
    assert np.array_equal(ad, bd), f"{(ad != bd).mean():.3%} of pixels differ"


@pytest.mark.parametrize("resampling", ["bilinear", "cubic"])
@pytest.mark.parametrize("kind", ["mask", "nodata", "alpha", "alpha16", "none"])
@pytest.mark.parametrize("where", ["top-left", "left", "bottom-right", "around", "inside"])
@pytest.mark.parametrize("shape", ["down", "up"])
@pytest.mark.parametrize("fill_value", [None, 7])
def test_resampled_boundless_reads_match_rasterio(
    sources, resampling, kind, where, shape, fill_value
):
    """Boundless reads resampled with a kernel: the first band decimates through the VRT's
    virtual overviews, whose sources lose the `resampling` attribute (nearest neighbour), and the
    others resample the VRT with its mask band (the source's mask, or the alpha band). The alpha
    band itself is a mask band (`VRTRasterBand::IsMaskBand()`): it is resampled over the VRT,
    background included, not read from its source."""
    win = WINDOWS[where]
    with rasterio.open(sources[kind]) as s:
        out_shape = (s.count, *SHAPES[shape])
        kw = dict(out_shape=out_shape, boundless=True, resampling=RR[resampling])
        a = s.read(window=RW(*win), masked=True, fill_value=fill_value, **kw)
        a_plain = s.read(window=RW(*win), fill_value=fill_value, **kw)
    with trio.open(sources[kind]) as s:
        kw = dict(out_shape=out_shape, boundless=True, resampling=TR[resampling])
        b = s.read(window=TW(*win), masked=True, fill_value=fill_value, **kw)
        b_plain = s.read(window=TW(*win), fill_value=fill_value, **kw)
    _assert_same(a, b)
    assert np.array_equal(a_plain, b_plain), f"{(a_plain != b_plain).mean():.3%} of pixels differ"
