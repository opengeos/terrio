"""WhiteIsZero GeoTIFFs (`PHOTOMETRIC=MINISWHITE`), read by `terrio.rio` as GDAL reads them.

GDAL (libtiff) returns WhiteIsZero samples as stored ("Pixel values remains unchanged", GDAL 3.12.1
`GTiffDataset::OpenOffset`) and gives an image of at most 16 bits an inverted gray color table. The
`tiff` crate inverts such samples (or refuses signed, Float16 and multi-sample ones) under no
compression, LZW, Deflate and PackBits, which terrio read so while its own ZSTD decoder read them as
stored: two compressions of one file disagreed. terrio now decodes them itself, as stored, under
every compression. GDAL writes every file and is the oracle: no tolerance. Skipped without
rasterio.
"""

import struct

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402

WIDTH, HEIGHT = 70, 45
LAYOUTS = {
    "striped": dict(blockysize=7),
    "tiled": dict(tiled=True, blockxsize=32, blockysize=16),
}
# (compression, predictor): GDAL writes no predictor with NONE and PACKBITS, and refuses one for
# samples of fewer than 8 bits.
CODINGS = [("NONE", 1), ("PACKBITS", 1)]
CODINGS += [(c, p) for c in ("LZW", "DEFLATE", "ZSTD", "LZMA") for p in (1, 2)]
# (dtype, bands, interleave, NBITS)
IMAGES = [
    ("uint8", 1, "band", None),
    ("uint16", 1, "band", None),
    ("float32", 1, "band", None),
    ("uint8", 1, "band", 1),
    ("int8", 1, "band", None),
    ("int16", 1, "band", None),
    ("uint32", 1, "band", None),
    ("float16", 1, "band", None),
    ("float64", 1, "band", None),
    ("uint8", 2, "pixel", None),
    ("uint16", 2, "band", None),
]


def _name(image):
    dtype, bands, interleave, nbits = image
    return f"{dtype}{f'-nbits{nbits}' if nbits else ''}-{bands}{interleave}"


def _values(dtype, bands, nbits):
    rng = np.random.default_rng(len(dtype) * 10 + bands)
    shape = (bands, HEIGHT, WIDTH)
    if nbits:
        return rng.integers(0, 2**nbits, shape).astype(dtype)
    if np.dtype(dtype).kind == "f":
        v = (rng.standard_normal(shape) * 1000).astype(dtype)
        specials = [0.0, -0.0, 0.5, 1.0, np.nan, np.inf, -np.inf, np.finfo(dtype).max]
        v.flat[: len(specials)] = np.array(specials, dtype=dtype)
        return v
    info = np.iinfo(dtype)
    v = rng.integers(info.min, int(info.max) + 1, shape, dtype=dtype)
    v.flat[:2] = [info.min, info.max]
    return v


def _colormap(ds):
    try:
        return {k: tuple(v) for k, v in ds.colormap(1).items()}
    except ValueError:
        return None


def _cases():
    for image in IMAGES:
        dtype, _, _, nbits = image
        for compress, predictor in CODINGS:
            if nbits and predictor != 1:
                continue
            # terrio reads no Float16 samples under LZMA, whatever the photometric interpretation.
            if dtype == "float16" and compress == "LZMA":
                continue
            yield pytest.param(
                image, compress, predictor, id=f"{_name(image)}-{compress}-p{predictor}"
            )
        if dtype in ("float32", "float64"):
            for compress in ("LZW", "DEFLATE", "ZSTD"):
                yield pytest.param(image, compress, 3, id=f"{_name(image)}-{compress}-p3")


@pytest.mark.parametrize("layout", list(LAYOUTS))
@pytest.mark.parametrize("image,compress,predictor", list(_cases()))
def test_white_is_zero_samples_read_as_stored(tmp_path, image, compress, predictor, layout):
    """Every sample, plain and windowed, bit for bit; and GDAL's color table and interpretation."""
    dtype, bands, interleave, nbits = image
    path = str(tmp_path / "miniswhite.tif")
    values = _values(dtype, bands, nbits)
    profile = dict(
        driver="GTiff",
        width=WIDTH,
        height=HEIGHT,
        count=bands,
        dtype=dtype,
        photometric="MINISWHITE",
        compress=compress,
        interleave=interleave,
        **LAYOUTS[layout],
    )
    if nbits:
        profile["NBITS"] = nbits
    if predictor != 1:
        profile["predictor"] = predictor
    with rasterio.open(path, "w", **profile) as dst:
        dst.write(values)
    window = (5, 3, 50, 30)
    with rasterio.open(path) as g, trio.open(path) as t:
        assert g.tags(ns="IMAGE_STRUCTURE").get("MINISWHITE") == "YES"
        want, want_window = g.read(), g.read(window=RW(*window))
        # GDAL reads back the values written, as stored: the comparison below is against a sound
        # oracle.
        assert want.tobytes() == values.tobytes()
        got, got_window = t.read(), t.read(window=TW(*window))
        if dtype == "float16":
            # terrio reads Float16 samples as Float32 of the same values.
            assert got.dtype == np.float32
            want, want_window = want.astype(np.float32), want_window.astype(np.float32)
        assert got.dtype == want.dtype
        assert got.tobytes() == want.tobytes()
        assert got_window.tobytes() == want_window.tobytes()
        assert [c.name for c in t.colorinterp] == [c.name for c in g.colorinterp]
        assert _colormap(t) == _colormap(g)


def _without_gdal_metadata(path):
    """Renumbers the GDAL_METADATA tag (42112) of a little-endian TIFF to a private tag GDAL
    ignores: the file as other software writes it, without GDAL's band color interpretation."""
    with open(path, "r+b") as f:
        b = f.read()
        (offset,) = struct.unpack("<I", b[4:8])
        (count,) = struct.unpack("<H", b[offset : offset + 2])
        for i in range(count):
            at = offset + 2 + 12 * i
            if struct.unpack("<H", b[at : at + 2]) == (42112,):
                f.seek(at)
                f.write(struct.pack("<H", 65000))
                return True
    return False


@pytest.mark.parametrize(
    "image",
    # Float16 aside (read as Float32), and 2- and 4-bit samples.
    [i for i in IMAGES if i[0] != "float16"] + [("uint8", 1, "band", n) for n in (2, 4)],
    ids=_name,
)
def test_white_is_zero_colors_without_gdal_metadata(tmp_path, image):
    """Without GDAL's metadata, a WhiteIsZero band of at most 16 bits is a palette, its color table
    an inverted gray ramp, and a deeper one is undefined, as GDAL has them."""
    dtype, bands, interleave, nbits = image
    path = str(tmp_path / "miniswhite.tif")
    profile = dict(
        driver="GTiff", width=16, height=8, count=bands, dtype=dtype, interleave=interleave
    )
    if nbits:
        profile["NBITS"] = nbits
    with rasterio.open(path, "w", photometric="MINISWHITE", **profile) as dst:
        dst.write(_values(dtype, bands, nbits)[:, :8, :16])
    assert _without_gdal_metadata(path)
    with rasterio.open(path) as g, trio.open(path) as t:
        assert [c.name for c in t.colorinterp] == [c.name for c in g.colorinterp]
        assert _colormap(t) == _colormap(g)
        assert t.read().tobytes() == g.read().tobytes()
