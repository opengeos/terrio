"""Complex GeoTIFFs (GDAL's CInt16, CFloat32 and CFloat64), read by `terrio.rio` as rasterio reads
them.

terrio typed CFloat32 bands `uint64` and failed to read them, and refused CInt16 and CFloat64 at
open. It now reads TIFF `SampleFormat` 5 and 6 as rasterio does: `complex_int16` (read as
complex64), `complex64` and `complex128`, whose values, windows and masks are rasterio's. GDAL
writes every file and is the oracle: no tolerance. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from rasterio.transform import from_origin  # noqa: E402
from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402

LAYOUTS = {
    "striped": dict(blockysize=5),
    "tiled": dict(tiled=True, blockxsize=16, blockysize=16),
}
COMPRESSIONS = ["NONE", "DEFLATE", "LZW", "PACKBITS", "ZSTD"]
# rasterio's names of GDAL's complex types, and the NumPy type of their values.
TYPES = {"complex_int16": np.complex64, "complex64": np.complex64, "complex128": np.complex128}


def _write(path, dtype, layout, compress, bands=2, interleave="pixel", nodata=None):
    """A GeoTIFF GDAL writes, and the values written."""
    rng = np.random.default_rng(len(dtype) + bands)
    height, width = 37, 41
    # Real parts from 0: only the nodata block has -5.
    values = rng.integers(0, 300, (bands, height, width)) + 1j * rng.integers(
        -300, 300, (bands, height, width)
    )
    values = values.astype(TYPES[dtype])
    if nodata is not None:
        values[:, :5, :7] = nodata + 3j
    profile = dict(
        driver="GTiff",
        width=width,
        height=height,
        count=bands,
        dtype=dtype,
        crs="EPSG:32631",
        transform=from_origin(500000, 4600000, 10, 10),
        compress=compress,
        interleave=interleave,
        nodata=nodata,
        **LAYOUTS[layout],
    )
    with rasterio.open(path, "w", **profile) as dst:
        dst.write(values)
    return values


@pytest.mark.parametrize("dtype", sorted(TYPES))
@pytest.mark.parametrize("layout", sorted(LAYOUTS))
@pytest.mark.parametrize("compress", COMPRESSIONS)
def test_complex_bands_read_as_rasterio_reads_them(tmp_path, dtype, layout, compress):
    path = tmp_path / "complex.tif"
    values = _write(path, dtype, layout, compress)
    with rasterio.open(path) as g, trio.open(path) as t:
        assert (t.count, t.dtypes) == (g.count, g.dtypes) == (2, (dtype, dtype))
        expected = g.read()
        assert expected.dtype == TYPES[dtype]
        np.testing.assert_array_equal(expected, values)
        got = t.read()
        assert got.dtype == expected.dtype
        np.testing.assert_array_equal(got, expected)
        # One band, and a window across blocks.
        np.testing.assert_array_equal(t.read(2), g.read(2))
        np.testing.assert_array_equal(
            t.read(1, window=TW(3, 4, 20, 25)), g.read(1, window=RW(3, 4, 20, 25))
        )
        assert t.block_shapes == g.block_shapes


@pytest.mark.parametrize("dtype", sorted(TYPES))
def test_band_interleaved_complex_bands_are_rasterios(tmp_path, dtype):
    path = tmp_path / "complex-band.tif"
    _write(path, dtype, "tiled", "DEFLATE", bands=3, interleave="band")
    with rasterio.open(path) as g, trio.open(path) as t:
        assert t.dtypes == g.dtypes
        np.testing.assert_array_equal(t.read([3, 1]), g.read([3, 1]))


@pytest.mark.parametrize("dtype", sorted(TYPES))
def test_masks_of_complex_bands_compare_their_real_parts(tmp_path, dtype):
    # GDAL's nodata mask reads a complex band as its real parts: -5 + 3j is nodata for -5.
    path = tmp_path / "complex-nodata.tif"
    _write(path, dtype, "striped", "NONE", bands=1, nodata=-5)
    with rasterio.open(path) as g, trio.open(path) as t:
        # The bytes of a block of one band: 4 per CInt16 pixel.
        assert t.block_size(1, 0, 0) == g.block_size(1, 0, 0)
        assert (t.nodata, t.nodatavals) == (g.nodata, g.nodatavals)
        np.testing.assert_array_equal(t.read_masks(1), g.read_masks(1))
        assert int((g.read_masks(1) == 0).sum()) == 35
        np.testing.assert_array_equal(
            np.ma.getmaskarray(t.read(masked=True)), np.ma.getmaskarray(g.read(masked=True))
        )
