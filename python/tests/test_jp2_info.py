"""JPEG 2000 dataset information, read by terrio and by GDAL's JP2OpenJPEG driver (rasterio):
block shapes, overviews (the resolution levels), the `IMAGE_STRUCTURE` metadata of the dataset
(compression reversibility, interleave) and of its bands (compression, bits), and the default
metadata domain, as `gdalinfo -json` reports them.

The files are written by GDAL at test time: untiled and tiled, lossless and lossy, 8 and 16
bits, one band and three, single tiles past 1024 pixels, tiles wider than the image, few
resolution levels, and a file terrio writes. Skipped when rasterio (or its JPEG 2000 driver) is not installed.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
from rasterio.transform import from_origin  # noqa: E402

import terrio  # noqa: E402
import terrio.rio as trio  # noqa: E402

with rasterio.Env() as _env:
    if "JP2OpenJPEG" not in _env.drivers():
        pytest.skip("GDAL has no JP2OpenJPEG driver", allow_module_level=True)


def write_jp2(path, width, height, count=1, dtype="uint8", **options):
    rng = np.random.default_rng(width * 7 + height)
    top = 255 if dtype == "uint8" else 4095
    data = rng.integers(0, top, (count, height, width)).astype(dtype)
    profile = dict(
        driver="JP2OpenJPEG",
        width=width,
        height=height,
        count=count,
        dtype=dtype,
        crs="EPSG:32631",
        transform=from_origin(500000, 4600000, 10, 10),
    )
    with rasterio.open(path, "w", **profile, **options) as dst:
        dst.write(data)
    return str(path)


def assert_same_info(path):
    """terrio reports what GDAL reports: blocks, overviews and metadata of every domain
    `gdalinfo -json` lists, for the dataset and each band."""
    with rasterio.open(path) as g, trio.open(path) as t:
        assert t.block_shapes == g.block_shapes
        for b in range(1, g.count + 1):
            assert t.overviews(b) == g.overviews(b), b
            assert t.tags(b) == g.tags(b), b
            assert t.tags(b, ns="IMAGE_STRUCTURE") == g.tags(b, ns="IMAGE_STRUCTURE"), b
        assert t.tags() == g.tags()
        assert t.tags(ns="IMAGE_STRUCTURE") == g.tags(ns="IMAGE_STRUCTURE")


CASES = {
    "untiled-lossy": (512, 384, 1, "uint8", {}),
    "untiled-lossless": (512, 384, 1, "uint8", {"REVERSIBLE": "YES", "QUALITY": "100"}),
    "tiled-lossy": (1000, 700, 1, "uint8", {"BLOCKXSIZE": "256", "BLOCKYSIZE": "256"}),
    "tiled-lossless-rgb16": (
        600,
        520,
        3,
        "uint16",
        {"BLOCKXSIZE": "128", "BLOCKYSIZE": "256", "REVERSIBLE": "YES", "QUALITY": "100"},
    ),
    "nbits-12": (300, 200, 1, "uint16", {"NBITS": "12", "REVERSIBLE": "YES", "QUALITY": "100"}),
    "single-tile-past-1024": (1500, 1100, 1, "uint8", {"BLOCKXSIZE": "1500", "BLOCKYSIZE": "1100"}),
    "tile-wider-than-image": (300, 2000, 1, "uint8", {"BLOCKXSIZE": "512", "BLOCKYSIZE": "512"}),
    "few-resolutions": (800, 800, 1, "uint8", {"RESOLUTIONS": "2"}),
}


@pytest.mark.parametrize("case", CASES, ids=list(CASES))
def test_jp2_info_as_gdal(tmp_path, case):
    """Catches a JPEG 2000 dataset reporting one-row blocks, no `IMAGE_STRUCTURE` metadata or
    other overviews than GDAL's resolution levels (`gdalinfo -json`'s block, metadata and
    overview keys)."""
    w, h, n, dtype, options = CASES[case]
    assert_same_info(write_jp2(tmp_path / f"{case}.jp2", w, h, n, dtype, **options))


def test_jp2_info_of_terrio_written_file(tmp_path):
    """terrio writes lossless JPEG 2000 with GDAL's codestream comment: GDAL and terrio both
    read its reversibility as LOSSLESS."""
    src = write_jp2(tmp_path / "src.jp2", 200, 150, REVERSIBLE="YES", QUALITY="100")
    out = str(tmp_path / "terrio.jp2")
    terrio.write_raster(terrio.open_raster(src), out)
    assert_same_info(out)
    with rasterio.open(out) as g:
        assert g.tags(ns="IMAGE_STRUCTURE")["COMPRESSION_REVERSIBILITY"] == "LOSSLESS"


@pytest.mark.parametrize(
    "count,dtype,options",
    [
        (3, "uint8", {}),
        (4, "uint8", {}),
        (3, "uint16", {}),
        (2, "uint8", {}),
        (1, "uint16", {}),
        (3, "uint8", {"photometric": "RGB"}),
        (4, "uint8", {"photometric": "RGB", "alpha": "YES"}),
    ],
    ids=["rgb8", "rgba8", "rgb16", "two-bands", "gray16", "srgb", "srgb-alpha"],
)
def test_jp2_color_interpretation_as_gdal(tmp_path, count, dtype, options):
    """Catches JPEG 2000 bands interpreted unlike GDAL (gray for the first band whatever the
    colour space): from the `colr` and `cdef` boxes, as GDAL's driver reads them."""
    path = write_jp2(tmp_path / "c.jp2", 120, 80, count, dtype, **options)
    with rasterio.open(path) as g, trio.open(path) as t:
        assert t.colorinterp == g.colorinterp
