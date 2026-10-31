"""8-bit CMYK GeoTIFFs read by `terrio.rio` as GDAL reads them: RGBA.

GDAL 3.12.1 reads PHOTOMETRIC_SEPARATED images of at most 8 bits through libtiff's RGBA interface
(`GTiffRGBABand`): four bands, red, green, blue and alpha, the colors computed by libtiff's
`putRGBcontig8bitCMYKtile` (`k = 255 - K`, `R = k * (255 - C) / 255`), the alpha 255, and the
alpha band as every band's mask. terrio read the samples as stored, colorinterp cyan, magenta,
yellow, black, so a COG of a CMYK upload differed in 99.79% of its pixels (cog_matrix `cmyk-u8`).
GDAL is the oracle. Skipped without rasterio.
"""

import warnings

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
pytest.importorskip("rasterio.shutil")

from rasterio.transform import from_origin  # noqa: E402
from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402

LAYOUTS = {
    "striped": dict(blockysize=7),
    "tiled": dict(tiled=True, blockxsize=64, blockysize=32),
    "band": dict(interleave="band"),
}


def _write(path, dtype="uint8", compress="NONE", layout="striped"):
    rng = np.random.default_rng(5)
    top = np.iinfo(dtype).max
    data = rng.integers(0, top, (4, 101, 133), endpoint=True).astype(dtype)
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=133,
        height=101,
        count=4,
        dtype=dtype,
        photometric="CMYK",
        crs="EPSG:32633",
        transform=from_origin(500000, 5000000, 10, 10),
        compress=compress,
        **LAYOUTS[layout],
    ) as dst:
        dst.write(data)
    return data


def _describe(ds):
    return (
        ds.count,
        [c.name for c in ds.colorinterp],
        [[f.name for f in fl] for fl in ds.mask_flag_enums],
        ds.profile.get("photometric"),
        ds.profile.get("interleave"),
    )


@pytest.mark.parametrize("layout", list(LAYOUTS))
@pytest.mark.parametrize("compress", ["NONE", "DEFLATE", "LZW"])
def test_cmyk_reads_as_rgba_like_rasterio(tmp_path, compress, layout):
    path = str(tmp_path / "cmyk.tif")
    cmyk = _write(path, compress=compress, layout=layout)
    with rasterio.open(path) as g, trio.open(path) as t:
        want = g.read()
        # GDAL's colors are libtiff's formula: the oracle is the one documented above.
        k = 255 - cmyk[3].astype(np.uint32)
        np.testing.assert_array_equal(want[0], (k * (255 - cmyk[0].astype(np.uint32)) // 255))
        np.testing.assert_array_equal(t.read(), want)
        assert _describe(t) == _describe(g)
        np.testing.assert_array_equal(t.read_masks(), g.read_masks())
        np.testing.assert_array_equal(
            t.read([3, 1], window=TW(5, 7, 60, 41)), g.read([3, 1], window=RW(5, 7, 60, 41))
        )


def test_16_bit_cmyk_is_read_as_stored_like_rasterio(tmp_path):
    """The control: GDAL reads CMYK of more than 8 bits without the RGBA interface."""
    path = str(tmp_path / "cmyk16.tif")
    _write(path, dtype="uint16")
    with rasterio.open(path) as g, trio.open(path) as t:
        np.testing.assert_array_equal(t.read(), g.read())
        assert _describe(t) == _describe(g)


def test_a_cmyk_cog_through_the_ingestion_call_is_gdals(tmp_path):
    """An ingestion pipeline's call: the source's profile, `driver="COG"`, `compress="deflate"`."""
    src = str(tmp_path / "cmyk.tif")
    _write(src)
    outs = {}
    for name, lib in (("gdal", rasterio), ("terrio", trio)):
        with lib.open(src) as ds:
            profile = ds.profile | {"driver": "COG", "compress": "deflate"}
        outs[name] = str(tmp_path / f"{name}.tif")
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            lib.shutil.copy(src, outs[name], **profile)
    with rasterio.open(outs["gdal"]) as g, rasterio.open(outs["terrio"]) as t:
        assert _describe(t) == _describe(g)
        np.testing.assert_array_equal(t.read(), g.read())
        np.testing.assert_array_equal(t.read_masks(), g.read_masks())
