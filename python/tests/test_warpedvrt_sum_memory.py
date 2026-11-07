"""A `sum` WarpedVRT warps a region a part at a time, each part from its own source window."""

import tracemalloc

import numpy as np
import pytest

import terrio
import terrio.rio
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT

# A `warp_mem_limit` (MB) under which the reads below warp a part at a time, and one under which
# they warp each region from one window.
PARTS, WHOLE = 1, 10000


def cog(path, values, gt, crs="EPSG:3857", nodata=None):
    """Writes `values` as a COG of 256 x 256 tiles without overviews, and returns its path."""
    terrio.Raster.from_array(values, gt, crs=crs, nodata=nodata).write(
        str(path), driver="COG", tile_size=256, options={"OVERVIEW_COUNT": "0"}
    )
    return str(path)


def recorded(src):
    """Records the windows the warp reads from `src`, in the list it returns."""
    windows = []
    read = src._read_level

    def spy(bands, window, level):
        windows.append(window)
        return read(bands, window, level)

    src._read_level = spy
    return windows


def bits(values):
    """The values' bits, to compare floating-point arrays bit for bit (NaN included)."""
    values = np.ascontiguousarray(values)
    return values.view(f"u{values.dtype.itemsize}")


def tile(path, limit, shape, **kw):
    """A reduced read of the whole `sum` VRT of `path`, and the windows it read."""
    with terrio.rio.open(path) as src:
        windows = recorded(src)
        with WarpedVRT(src, resampling=Resampling.sum, warp_mem_limit=limit, **kw) as vrt:
            return vrt.read(out_shape=shape), windows


def square(n, values, tmp_path, name, nodata=None):
    """`values` (n x n) as a COG of 1-unit pixels, and the WarpedVRT options of its own grid."""
    path = cog(tmp_path / name, values, (0.0, 1.0, 0.0, float(n), 0.0, -1.0), nodata=nodata)
    grid = {"crs": "EPSG:3857", "transform": from_origin(0, n, 1, 1), "width": n, "height": n}
    return path, grid


def test_a_sum_tile_reads_its_source_a_part_at_a_time(tmp_path):
    # 4096 x 4096 uint16 without overviews, summed into a 256 x 256 tile in float64 under a 1 MB
    # `warp_mem_limit`: each window the warp reads holds at most 1 MB in the working type, where a
    # block of the tile read whole held 4096 x 2050 pixels of the source, 64 MB, at once.
    n = 4096
    values = (np.arange(n * n, dtype="uint64") % 1009).astype("uint16").reshape(n, n)
    path, grid = square(n, values, tmp_path, "u16.tif")
    read, windows = tile(path, PARTS, (1, 256, 256), dtype="float64", **grid)
    largest = max(windows, key=lambda w: w[2] * w[3])
    assert largest[2] * largest[3] * 8 <= 1 << 20, (largest, len(windows))
    exact = values.reshape(256, 16, 256, 16).sum(axis=(1, 3), dtype="float64")
    np.testing.assert_array_equal(read[0], exact)


def field(bands, nodata, seed=0):
    """Float32 values on a 1200 x 800 grid of 0.05 units, each band with its own nodata holes."""
    rng = np.random.default_rng(seed)
    values = (rng.random((bands, 800, 1200), dtype="float32") * 100).astype("float32")
    if nodata is not None:
        rows, cols = np.mgrid[0:800, 0:1200]
        for b in range(bands):
            # A third of the 37 x 37 blocks, others for each band.
            values[b][(rows // 37 + cols // 37 + b) % 3 == 0] = nodata
    return values, (-120.0, 0.05, 0.0, 70.0, 0.0, -0.05)


CASES = {
    # A decimation in the source's CRS.
    "same crs": ({}, {}),
    # From longitudes and latitudes (60 x 40 degrees) to a polar stereographic grid, along whose
    # rows they vary far from linearly, with the corners interpolated, transformed exactly, and
    # interpolated within 4 pixels.
    "reprojected": ({"crs": "EPSG:4326"}, {"crs": "EPSG:3413"}),
    "reprojected exactly": ({"crs": "EPSG:4326"}, {"crs": "EPSG:3413", "tolerance": 0}),
    "reprojected coarsely": ({"crs": "EPSG:4326"}, {"crs": "EPSG:3413", "tolerance": 4}),
    # Blocks of nodata, which stay nodata where they cover a pixel's footprint.
    "nodata blocks": ({"nodata": -9999.0}, {}),
    "nodata blocks reprojected": ({"crs": "EPSG:4326", "nodata": -9999.0}, {"crs": "EPSG:3413"}),
    # Three bands, each with its own holes, without an output nodata value.
    "three bands": ({"bands": 3, "nodata": -9999.0}, {"nodata": None}),
}


@pytest.mark.parametrize("case", CASES)
def test_a_sum_warped_a_part_at_a_time_is_the_same_bit_for_bit(tmp_path, case):
    # The same reduced `sum` read, warped from one window per region and a part at a time from
    # many smaller ones: the same pixels, bit for bit.
    source, kw = CASES[case]
    nodata = source.get("nodata")
    values, gt = field(source.get("bands", 1), nodata)
    path = cog(tmp_path / "src.tif", values, gt, source.get("crs", "EPSG:3857"), nodata)
    shape = (values.shape[0], 64, 64)
    whole, few = tile(path, WHOLE, shape, **kw)
    parts, many = tile(path, PARTS, shape, **kw)
    assert len(many) > len(few), (len(many), len(few))
    assert (whole > 0).any()
    np.testing.assert_array_equal(bits(parts), bits(whole))


def test_a_sum_warped_a_part_at_a_time_is_the_full_resolution_total(tmp_path):
    # 1024 x 1024 uint16 summed into 64 x 64 float64 a part at a time: each pixel is the exact sum
    # of its 16 x 16 source pixels, as `overview="NONE"` sums the full resolution.
    n = 1024
    values = (np.arange(n * n, dtype="uint64") * 7 % 65521).astype("uint16").reshape(n, n)
    path, grid = square(n, values, tmp_path, "u16.tif")
    parts, windows = tile(path, PARTS, (1, 64, 64), dtype="float64", **grid)
    assert len(windows) > 1
    exact = values.reshape(64, 16, 64, 16).sum(axis=(1, 3), dtype="float64")
    np.testing.assert_array_equal(parts[0], exact)
    full = terrio.warp(path, ts=(64, 64), resampling="sum", ot="Float64", overview="NONE")
    np.testing.assert_array_equal(bits(parts[0]), bits(full.read(1)))


def test_a_sum_of_bands_with_output_nodata_is_warped_whole(tmp_path):
    # Three uint16 bands with a corner of nodata (65535), the first summing past 65535. With
    # several bands, whether a sum stored on the output nodata value is moved off it depends on
    # whether its window holds a nodata pixel, so such a region is warped from one window: a part
    # away from the corner would move the first band's 65535 to 65534.
    n = 512
    values = np.ones((3, n, n), "uint16")
    values[0] = 2000
    values[:, :16, :16] = 65535
    path, grid = square(n, values, tmp_path, "rgb.tif", nodata=65535)
    whole, _ = tile(path, WHOLE, (3, 64, 64), **grid)
    parts, _ = tile(path, PARTS, (3, 64, 64), **grid)
    assert whole[0, 5, 5] == 65535 and whole[1, 5, 5] == 64
    np.testing.assert_array_equal(parts, whole)


def test_the_sum_budget_applies_to_each_part(tmp_path, monkeypatch):
    # 1024 x 1024 float32 summed into 64 x 64: in one window, the read would hold 4 bytes per
    # pixel and 8 for the shares, 12,582,912 bytes. Under a 4,000,000-byte budget it is refused
    # before anything is read, and a part at a time (1 MB windows) it is not. A 1 x 1 read, one
    # pixel whose window is the whole source, cannot be split and is refused either way.
    n = 1024
    values = np.random.default_rng(3).random((n, n), dtype="float32")
    path, grid = square(n, values, tmp_path, "f32.tif")
    expected, _ = tile(path, WHOLE, (1, 64, 64), **grid)
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "4000000")
    refused = r"about 12582912 bytes .*TERRIO_WARP_SUM_MAX_BYTES=4000000"
    with pytest.raises(terrio.ResourceLimitError, match=refused):
        tile(path, WHOLE, (1, 64, 64), **grid)
    parts, _ = tile(path, PARTS, (1, 64, 64), **grid)
    np.testing.assert_array_equal(bits(parts), bits(expected))
    with pytest.raises(terrio.ResourceLimitError, match=refused):
        tile(path, PARTS, (1, 1, 1), **grid)


def test_the_sum_budget_counts_the_largest_part(tmp_path, monkeypatch):
    # 1024 x 1024 float32 filling the right half of a grid twice as wide, summed into 128 x 64
    # under a 4 MB `warp_mem_limit`: the first part, the left half, reads a sliver of the source
    # (its padding, 2 x 1024 pixels) and each of the right half's parts 514 x 1024 pixels, about
    # 6.3 MB with the shares. Under a 1,000,000-byte budget the read is refused for its largest
    # part, whichever part comes first.
    n = 1024
    path = cog(tmp_path / "f32.tif", np.ones((n, n), "float32"), (0.0, 1.0, 0.0, n, 0.0, -1.0))
    grid = {"crs": "EPSG:3857", "transform": from_origin(-n, n, 1, 1), "width": 2 * n, "height": n}
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "1000000")
    with pytest.raises(terrio.ResourceLimitError, match=r"about 6316032 bytes"):
        tile(path, 4, (1, 64, 128), **grid)


def traced(path, limit, shape, **kw):
    """The peak of the arrays Python allocates during a reduced `sum` read, and the windows read.

    The windows the source reads arrive allocated by terrio, not traced: what is traced is what
    the read allocates in Python, a converted copy of a window among it.
    """
    with terrio.rio.open(path) as src:
        windows = recorded(src)
        with WarpedVRT(src, resampling=Resampling.sum, warp_mem_limit=limit, **kw) as vrt:
            tracemalloc.start()
            try:
                vrt.read(out_shape=shape)
                return tracemalloc.get_traced_memory()[1], windows
            finally:
                tracemalloc.stop()


def test_a_sum_takes_a_source_its_dtype_holds_as_it_is(tmp_path):
    # 2048 x 2048 uint16 summed into 128 x 128 a part at a time (16 MB). Float64 holds every
    # uint16 value, and the kernel reads each value as a float64 either way: the windows are
    # not converted, so no float64 copy of one is allocated, and the read allocates less than
    # half of one. Uint8 does not hold them: those windows are converted first, which allocates
    # at least a window's worth.
    n = 2048
    values = (np.arange(n * n, dtype="uint64") % 1009).astype("uint16").reshape(n, n)
    path, grid = square(n, values, tmp_path, "u16.tif")
    peak, windows = traced(path, 16, (1, 128, 128), dtype="float64", **grid)
    largest = max(w * h for _, _, w, h in windows)
    assert peak < largest * 8 / 2, (peak, largest)
    peak, windows = traced(path, 16, (1, 128, 128), dtype="uint8", **grid)
    largest = max(w * h for _, _, w, h in windows)
    assert peak >= largest, (peak, largest)


@pytest.mark.parametrize(
    "source, dtype", [("uint16", "float64"), ("float32", "float64"), ("float32", "uint16")]
)
def test_a_sum_in_another_dtype_sums_the_values_converted_to_it(tmp_path, source, dtype):
    # A `sum` through a WarpedVRT of another dtype gives the pixels of the same values converted
    # to that dtype first, bit for bit: uint16 and float32 in float64, which holds them, warped
    # as they are; float32 fractions in uint16, which does not hold them, rounded first (0.3
    # counts as 0), where adding them as they are would give other sums.
    n = 512
    rng = np.random.default_rng(11)
    if source == "uint16":
        values = rng.integers(0, 65535, (n, n), dtype="uint16")
    else:
        values = (rng.random((n, n), dtype="float32") * 3).astype("float32")
    converted = values.astype(dtype) if dtype == "float64" else np.floor(values + 0.5).astype(dtype)
    path, grid = square(n, values, tmp_path, "src.tif")
    same, _ = square(n, converted, tmp_path, "converted.tif")
    warped, _ = tile(path, PARTS, (1, 32, 32), dtype=dtype, **grid)
    expected, _ = tile(same, PARTS, (1, 32, 32), dtype=dtype, **grid)
    np.testing.assert_array_equal(bits(warped), bits(expected))
