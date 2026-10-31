"""GeoTIFFs of 1- to 7-bit samples (GDAL's `NBITS`, bilevel images), read by `terrio.rio` as GDAL
reads them.

The `tiff` crate returns such chunks packed, and terrio placed each packed byte as a pixel: 97,751
to 99,494 of 99,900 pixels were wrong, silently. terrio now unpacks them to a byte per
sample, as GDAL's `GTiffBitmapBand` and `GTiffOddBitsBand` do, keeps WhiteIsZero samples as stored,
and reports GDAL's color table (black and white for one-band bilevel images, an inverted gray ramp
for WhiteIsZero ones), palette color interpretation and band `NBITS` item. GDAL writes every file
and is the oracle: no tolerance. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.transform import from_origin  # noqa: E402
from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402

LAYOUTS = {
    "striped": dict(blockysize=7),
    "tiled": dict(tiled=True, blockxsize=128, blockysize=64),
}
COMPRESSIONS = ["NONE", "DEFLATE", "LZW", "PACKBITS", "ZSTD"]
# (bits, bands, interleave, photometric)
IMAGES = [(b, 1, "band", "MINISBLACK") for b in (1, 2, 3, 4, 7)]
IMAGES += [(b, 1, "band", "MINISWHITE") for b in (1, 2, 4)]
IMAGES += [(b, 3, "pixel", "MINISBLACK") for b in (1, 2, 4, 5)]
IMAGES += [(b, 3, "band", "MINISBLACK") for b in (1, 4)]


def _write(path, bits, bands, interleave, photometric, compress, layout, width=300, height=203):
    """A GeoTIFF GDAL writes with `NBITS=bits`, and the values written."""
    rng = np.random.default_rng(bits * 10 + bands)
    values = rng.integers(0, 2**bits, (bands, height, width)).astype("uint8")
    profile = dict(
        driver="GTiff",
        width=width,
        height=height,
        count=bands,
        dtype="uint8",
        crs="EPSG:32633",
        transform=from_origin(500000, 5000000, 10, 10),
        NBITS=bits,
        compress=compress,
        interleave=interleave,
        photometric=photometric,
        **LAYOUTS[layout],
    )
    with rasterio.open(path, "w", **profile) as dst:
        dst.write(values)
    return values


def _colormap(ds):
    try:
        return {k: tuple(v) for k, v in ds.colormap(1).items()}
    except ValueError:
        return None


def _name(image):
    bits, bands, interleave, photometric = image
    return f"nbits{bits}-{bands}{interleave}-{photometric}"


@pytest.mark.parametrize("layout", list(LAYOUTS))
@pytest.mark.parametrize("compress", COMPRESSIONS)
@pytest.mark.parametrize("image", IMAGES, ids=_name)
def test_sub_byte_samples_read_like_rasterio(tmp_path, image, compress, layout):
    """Every pixel, plain and windowed, and what GDAL reports about the band."""
    bits, bands, interleave, photometric = image
    path = str(tmp_path / "nbits.tif")
    values = _write(path, bits, bands, interleave, photometric, compress, layout)
    with rasterio.open(path) as g, trio.open(path) as t:
        want = g.read()
        # GDAL reads back the values written: the comparison below is against a sound oracle.
        np.testing.assert_array_equal(want, values)
        got = t.read()
        assert got.dtype == want.dtype == np.uint8
        np.testing.assert_array_equal(got, want)
        np.testing.assert_array_equal(
            t.read(window=TW(37, 11, 150, 101)), g.read(window=RW(37, 11, 150, 101))
        )
        assert [c.name for c in t.colorinterp] == [c.name for c in g.colorinterp]
        assert _colormap(t) == _colormap(g)
        for b in range(1, bands + 1):
            assert t.tags(b, ns="IMAGE_STRUCTURE").get("NBITS") == g.tags(
                b, ns="IMAGE_STRUCTURE"
            ).get("NBITS")


@pytest.mark.parametrize("layout", list(LAYOUTS))
@pytest.mark.parametrize("photometric", ["MINISBLACK", "MINISWHITE"])
def test_ccitt_fax4_bilevel_images_read_like_rasterio(tmp_path, photometric, layout):
    """CCITT Group 4, which only the `tiff` crate decodes: its rows unpacked, WhiteIsZero samples
    as stored (the crate inverts them)."""
    path = str(tmp_path / "fax4.tif")
    _write(path, 1, 1, "band", photometric, "CCITTFAX4", layout)
    with rasterio.open(path) as g, trio.open(path) as t:
        np.testing.assert_array_equal(t.read(), g.read())
        assert _colormap(t) == _colormap(g)


def test_the_reproducers_files_read_like_rasterio(tmp_path):
    """The files that showed the packed bytes, as their reproducers wrote them (333 x 300,
    128 x 128 tiles or default strips; and `u8-nbits1.tif`: 1100 x 900 bilevel strips)."""
    rng = np.random.default_rng(3)
    for bits in (1, 2, 4):
        values = rng.integers(0, 2**bits, size=(1, 300, 333)).astype("uint8")
        for extra in ({}, {"tiled": True, "blockxsize": 128, "blockysize": 128}):
            path = str(tmp_path / f"nbits{bits}-{len(extra)}.tif")
            with rasterio.open(
                path,
                "w",
                driver="GTiff",
                width=333,
                height=300,
                count=1,
                dtype="uint8",
                crs="EPSG:32633",
                transform=from_origin(500000, 5000000, 10, 10),
                NBITS=bits,
                **extra,
            ) as dst:
                dst.write(values)
            with trio.open(path) as t:
                assert np.count_nonzero(t.read(1) != values[0]) == 0
    base = rng.random((900, 1100))
    path = str(tmp_path / "u8-nbits1.tif")
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=1100,
        height=900,
        count=1,
        dtype="uint8",
        crs="EPSG:32633",
        transform=from_origin(500000, 5000000, 10, 10),
        NBITS=1,
    ) as dst:
        dst.write((base > 0.5).astype("uint8")[None])
    with rasterio.open(path) as g, trio.open(path) as t:
        np.testing.assert_array_equal(t.read(), g.read())
        assert sorted(np.unique(t.read()).tolist()) == [0, 1]
        assert [c.name for c in t.colorinterp] == ["palette"]
        assert _colormap(t) == _colormap(g) == {0: (0, 0, 0, 255), 1: (255, 255, 255, 255)}
