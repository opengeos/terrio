"""JPEG 2000 quality and transform options, independently decoded by GDAL.

Locally validated with GDAL 3.13.3; conformance reports rasterio's actual GDAL version.

QUALITY is a percentage of uncompressed size, not a PSNR target. Rate targets are
approximate, including container overhead. Encoder byte identity with GDAL is not claimed.
"""

import platform
import sys

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
from rasterio.io import MemoryFile as GdalMemoryFile  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

import terrio.rio as rio  # noqa: E402
from terrio.rio.errors import RasterioIOError  # noqa: E402
from terrio.rio.io import MemoryFile  # noqa: E402

TRANSFORM = from_origin(500000, 4600000, 10, 10)

MACOS_ARM = sys.platform == "darwin" and platform.machine() == "arm64"
# The irreversible 9/7 wavelet is computed in float32. Both decoders read the same payload, yet
# on macOS arm64 a few samples of some cases decode one step apart from GDAL's. That the OpenJPEG
# build in rasterio's wheel fuses multiply-adds is inferred, not measured. As with
# `FMA_ROUNDING_CASES` in `test_resampled_reads.py`, only the cases that differed get a limit.
# Measured on the macos-latest runner, with each limit's multiple of it:
#   uint8 QUALITY=10:        1 of 49152 samples (0.002%) -> limit 0.1% (49 samples, 49x)
#   uint8 REVERSIBLE=NO:     2 of 49152 samples (0.004%) -> limit 0.1% (49 samples, 25x)
#   int16 QUALITY=25 9/7:    3 of 4096 samples  (0.07%)  -> limit 0.5% (20 samples, 7x)
#   uint16 QUALITY=25 9/7:  41 of 4096 samples  (1.0%)   -> limit 2%   (81 samples, 2x)
# The uint8 limits are 49 samples rather than a few, so a new runner image does not flake on a
# handful of boundary samples. The limits are looser than that file's 1e-4, presumably because
# 9/7 output at these qualities sits on rounding boundaries more often; that is not measured.
# The seeds are fixed, so re-measure these counts when the rasterio wheel is upgraded.
# These limits apply only to the live GDAL comparison. `test_jp2_decode_reference.py` pins
# terrio's decode of frozen payloads for these four cases exactly on every platform, without
# depending on GDAL. Every other case, and the reversible 5/3 path, stays exact here everywhere.
DRIFT_UINT8 = 0.001


def assert_decodes_as_gdal(actual, expected, *, macos_drift=0.0, macos_arm=MACOS_ARM):
    """Assert terrio's decode equals GDAL's, exactly or within the macOS arm64 drift limit.

    Args:
        actual: Samples decoded by terrio.
        expected: Samples decoded by GDAL from the same payload.
        macos_drift: Largest fraction of samples that may differ by one step on macOS arm64.
        macos_arm: Whether the macOS arm64 limit applies; defaults to the running platform.
    """
    assert actual.dtype == expected.dtype, (actual.dtype, expected.dtype)
    assert actual.shape == expected.shape, (actual.shape, expected.shape)
    if not (macos_arm and macos_drift):
        np.testing.assert_array_equal(actual, expected)
        return
    difference = np.abs(actual.astype("int64") - expected.astype("int64"))
    assert difference.max(initial=0) <= 1, f"max difference {difference.max()}"
    fraction = np.mean(difference != 0)
    assert fraction <= macos_drift, f"{fraction:.4%} of samples differ, limit {macos_drift:.2%}"


@pytest.mark.parametrize(
    "changed,step,drift,passes",
    [
        (40, 1, 0.02, True),
        (120, 1, 0.02, False),
        (1, 2, 0.02, False),
        (1, 1, 0.0, False),
    ],
)
def test_one_step_tolerance_on_macos_arm(changed, step, drift, passes):
    """The macOS arm64 limit admits sparse one-step drift and nothing else."""
    expected = np.full(4096, 1000, dtype="uint16")
    actual = expected.copy()
    actual[:changed] += step
    if passes:
        assert_decodes_as_gdal(actual, expected, macos_drift=drift, macos_arm=True)
    else:
        with pytest.raises(AssertionError):
            assert_decodes_as_gdal(actual, expected, macos_drift=drift, macos_arm=True)


def test_decode_check_requires_matching_dtype():
    """A decode of the wrong type fails on every platform, even inside the limit."""
    expected = np.zeros(16, dtype="uint16")
    for macos_arm in (False, True):
        with pytest.raises(AssertionError):
            assert_decodes_as_gdal(
                expected.astype("int16"), expected, macos_drift=0.02, macos_arm=macos_arm
            )


def test_one_step_drift_fails_off_macos_arm():
    """Off macOS arm64 the drift limit is ignored: one-step differences fail."""
    expected = np.full(4096, 1000, dtype="uint16")
    actual = expected.copy()
    actual[:40] += 1
    with pytest.raises(AssertionError):
        assert_decodes_as_gdal(actual, expected, macos_drift=0.02, macos_arm=False)


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
    "options,lossless,macos_drift",
    [
        ({}, True, 0.0),
        ({"quality": 100}, True, 0.0),
        ({"quality": "99.999999"}, False, 0.0),
        ({"quality": "99.999999,99.9999995"}, False, 0.0),
        ({"QUALITY": "1e-30"}, False, 0.0),
        ({"QUALITY": 10}, False, DRIFT_UINT8),
        ({"QUALITY": 50, "REVERSIBLE": "NO"}, False, 0.0),
        ({"REVERSIBLE": "NO"}, False, DRIFT_UINT8),
        ({"REVERSIBLE": "NO", "QUALITY": 100}, False, 0.0),
        ({"REVERSIBLE": "YES", "QUALITY": 10}, False, 0.0),
        ({"quality": "10,100", "reversible": "yes"}, True, 0.0),
    ],
)
def test_jp2_path_and_memory_options(tmp_path, options, lossless, macos_drift):
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
        assert_decodes_as_gdal(actual.read(), expected, macos_drift=macos_drift)
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
    "quality", [0, -1, 101, "100.000001", "nan", "inf", "1e-40", "1e-36", "50,10", "25,25", ""]
)
def test_invalid_quality_is_refused(quality):
    with pytest.raises(RasterioIOError, match="QUALITY"):
        encode(np.zeros((3, 16, 16), dtype="uint8"), quality=quality)


def test_real_landsat_quality_controls_size(data_dir):
    with rasterio.open(data_dir / "landsat7.tif") as src:
        data = src.read([1, 2, 3], window=rasterio.windows.Window(500, 500, 128, 128))
    low, high, exact = (encode(data, quality=q) for q in (10, 50, 100))
    # The quality 10 and 50 payloads are 9/7 decodes too. Only the Linux real-data job sets
    # TERRIO_TEST_DATA, so this test skips on macOS and its macOS arm64 result is unmeasured.
    assert len(low) < len(high) < len(exact)
    for payload in (low, high, exact):
        with GdalMemoryFile(payload) as mem, mem.open() as oracle:
            expected = oracle.read()
        with MemoryFile(payload, ext=".jp2") as mem, mem.open() as actual:
            np.testing.assert_array_equal(actual.read(), expected)


@pytest.mark.parametrize("dtype,macos_drift", [("int16", 0.005), ("uint16", 0.02)])
def test_lossy_integer_samples_decode_as_gdal(dtype, macos_drift):
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
        assert_decodes_as_gdal(actual.read(), expected, macos_drift=macos_drift)
