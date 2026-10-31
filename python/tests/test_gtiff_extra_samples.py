"""Pixel-interleaved GeoTIFFs with extra samples, read by `terrio.rio` as GDAL reads them.

The `tiff` crate keeps only the samples of its color type: an RGB image with an unspecified fourth
sample, or with more than one extra sample, came back with fewer samples per pixel than the file
has, and terrio spread them over the bands with the file's count (99% of the pixels wrong). terrio
decodes such chunks itself. This covers every compression terrio decodes for GeoTIFF, predictors 2
and 3, tiled and striped images, plain, windowed and decimated reads and the masks. GDAL is
deterministic here: no tolerance. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from affine import Affine  # noqa: E402
from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402

LAYOUTS = {"tiled": dict(tiled=True, blockxsize=128, blockysize=64), "striped": dict(blockysize=7)}

# (photometric, bands, alpha creation option, dtype, compression, predictor)
IMAGES = [("RGB", 4, None, "uint8", c, 1) for c in ("NONE", "DEFLATE", "PACKBITS", "ZSTD", "WEBP")]
IMAGES += [
    ("RGB", 4, None, "uint8", "LZW", 2),
    ("RGB", 4, None, "uint8", "DEFLATE", 2),
    ("RGB", 4, None, "uint16", "ZSTD", 2),
    ("RGB", 4, None, "uint16", "NONE", 1),
    ("RGB", 4, None, "float32", "DEFLATE", 3),
    ("RGB", 4, None, "float32", "PACKBITS", 1),
    ("RGB", 4, "UNSPECIFIED", "uint8", "ZSTD", 1),
    ("RGB", 4, "YES", "uint8", "DEFLATE", 2),
    ("RGB", 4, "PREMULTIPLIED", "uint8", "NONE", 1),
    ("RGB", 5, None, "uint8", "DEFLATE", 2),
    ("RGB", 5, "YES", "uint16", "PACKBITS", 1),
    ("RGB", 5, None, "float32", "ZSTD", 3),
    ("MINISBLACK", 4, None, "uint16", "DEFLATE", 2),
    ("MINISBLACK", 5, "PREMULTIPLIED", "uint8", "ZSTD", 1),
]


def _name(image):
    photometric, bands, alpha, dtype, compress, predictor = image
    return f"{photometric}{bands}-{alpha or 'noalpha'}-{dtype}-{compress}-p{predictor}"


@pytest.mark.parametrize("layout", list(LAYOUTS))
@pytest.mark.parametrize("image", IMAGES, ids=_name)
def test_extra_samples_read_like_rasterio(tmp_path, image, layout):
    """Every sample of every pixel, as GDAL reads it, in plain, windowed and decimated reads."""
    photometric, bands, alpha, dtype, compress, predictor = image
    rng = np.random.default_rng(bands * 100 + len(dtype))
    width, height = 300, 203
    if dtype == "float32":
        v = rng.normal(100, 50, (bands, height, width)).astype(dtype)
    else:
        top = np.iinfo(dtype).max
        v = rng.integers(0, top, (bands, height, width), endpoint=True).astype(dtype)
    p = str(tmp_path / f"{_name(image)}-{layout}.tif")
    kw = dict(LAYOUTS[layout], photometric=photometric, compress=compress)
    if alpha:
        kw["alpha"] = alpha
    if predictor != 1:
        kw["predictor"] = predictor
    with rasterio.open(
        p,
        "w",
        driver="GTiff",
        width=width,
        height=height,
        count=bands,
        dtype=dtype,
        crs="EPSG:3857",
        transform=Affine(1, 0, 0, 0, -1, 0),
        **kw,
    ) as dst:
        dst.write(v)
    win, shape = (37, 21, 150, 90), (bands, 50, 77)
    with rasterio.open(p) as s:
        expected = [s.read(), s.read(window=RW(*win)), s.read(out_shape=shape), s.read_masks()]
        expected.append(s.dataset_mask())
        ci = s.colorinterp
    with trio.open(p) as s:
        got = [s.read(), s.read(window=TW(*win)), s.read(out_shape=shape), s.read_masks()]
        got.append(s.dataset_mask())
        assert [c.name for c in s.colorinterp] == [c.name for c in ci]
    names = ["plain", "window", "out_shape", "masks", "dataset mask"]
    for what, a, b in zip(names, expected, got, strict=True):
        assert a.shape == b.shape, what
        assert np.array_equal(a, b), f"{what}: {(a != b).mean():.3%} differ"
