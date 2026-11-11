"""JPEG 2000 quality and transform options, independently decoded by GDAL.

Locally validated with GDAL 3.13.3; conformance reports rasterio's actual GDAL version.

QUALITY is a percentage of uncompressed size, not a PSNR target. Rate targets are
approximate, including container overhead. Encoder byte identity with GDAL is not claimed.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
from rasterio.io import MemoryFile as GdalMemoryFile  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

import terrio.rio as rio  # noqa: E402
from terrio.rio.errors import RasterioIOError  # noqa: E402
from terrio.rio.io import MemoryFile  # noqa: E402

TRANSFORM = from_origin(500000, 4600000, 10, 10)


def encode(data, **options):
    with MemoryFile() as mem:
        with mem.open(
            driver="JP2",
            width=data.shape[2],
            height=data.shape[1],
            count=len(data),
            dtype=data.dtype,
            transform=TRANSFORM,
            crs="EPSG:32631",
            **options,
        ) as dst:
            dst.write(data)
        return mem.read()


@pytest.mark.parametrize(
    "options,lossless",
    [
        ({}, True),
        ({"quality": 100}, True),
        ({"quality": "99.999999"}, False),
        ({"QUALITY": 10}, False),
        ({"QUALITY": 50, "REVERSIBLE": "NO"}, False),
        ({"REVERSIBLE": "NO"}, False),
        ({"REVERSIBLE": "NO", "QUALITY": 100}, False),
        ({"REVERSIBLE": "YES", "QUALITY": 10}, False),
        ({"quality": "10,100", "reversible": "yes"}, True),
    ],
)
def test_jp2_path_and_memory_options(tmp_path, options, lossless):
    data = np.random.default_rng(7).integers(0, 256, (3, 128, 128), dtype="uint8")
    payload = encode(data, **options)
    path = tmp_path / "written.jp2"
    with rio.open(
        path,
        "w",
        driver="JP2",
        width=128,
        height=128,
        count=3,
        dtype="uint8",
        transform=TRANSFORM,
        crs="EPSG:32631",
        **options,
    ) as dst:
        dst.write(data)
    assert path.read_bytes() == payload
    with GdalMemoryFile(payload) as mem, mem.open() as oracle, rio.open(path) as actual:
        expected = oracle.read()
        np.testing.assert_array_equal(actual.read(), expected)
        assert actual.transform == oracle.transform == TRANSFORM
        assert actual.crs.to_epsg() == oracle.crs.to_epsg() == 32631
        assert actual.dtypes == oracle.dtypes == ("uint8",) * 3
        expected_label = "LOSSLESS" if lossless else "LOSSY"
        assert actual.tags(ns="IMAGE_STRUCTURE")["COMPRESSION_REVERSIBILITY"] == expected_label
        assert oracle.tags(ns="IMAGE_STRUCTURE")["COMPRESSION_REVERSIBILITY"] == expected_label
    if lossless:
        np.testing.assert_array_equal(expected, data)
    else:
        assert np.any(expected != data)


@pytest.mark.parametrize(
    "quality", [0, -1, 101, "100.000001", "nan", "inf", "1e-40", "50,10", "25,25", ""]
)
def test_invalid_quality_is_refused(quality):
    with pytest.raises(RasterioIOError, match="QUALITY"):
        encode(np.zeros((3, 16, 16), dtype="uint8"), quality=quality)


def test_real_landsat_quality_controls_size(data_dir):
    with rasterio.open(data_dir / "landsat7.tif") as src:
        data = src.read([1, 2, 3], window=rasterio.windows.Window(500, 500, 128, 128))
    low, high, exact = (encode(data, quality=q) for q in (10, 50, 100))
    assert len(low) < len(high) < len(exact)
    for payload in (low, high, exact):
        with GdalMemoryFile(payload) as mem, mem.open() as oracle:
            expected = oracle.read()
        with MemoryFile(payload, ext=".jp2") as mem, mem.open() as actual:
            np.testing.assert_array_equal(actual.read(), expected)


@pytest.mark.parametrize("dtype", ["int16", "uint16"])
def test_lossy_integer_samples_decode_as_gdal(dtype):
    values = np.random.default_rng(11).integers(0, 20000, (1, 64, 64))
    if dtype == "int16":
        values -= 10000
    data = values.astype(dtype)
    payload = encode(data, QUALITY=25, REVERSIBLE="NO")
    with GdalMemoryFile(payload) as mem, mem.open() as oracle:
        expected = oracle.read()
        assert oracle.dtypes == (dtype,)
    with MemoryFile(payload, ext=".jp2") as mem, mem.open() as actual:
        assert actual.dtypes == (dtype,)
        np.testing.assert_array_equal(actual.read(), expected)
