"""Stored overview selection must survive paths, lazy objects and repeated inputs."""

import numpy as np
import pytest

import terrio
import terrio.rio
from terrio.rio.control import GroundControlPoint
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.warp import reproject

GT = (0.0, 1.0, 0.0, 32.0, 0.0, -1.0)


@pytest.fixture
def pyramid(tmp_path):
    y, x = np.mgrid[:32, :32]
    values = (y * y + x).astype("float32")
    path = tmp_path / "pyramid.tif"
    terrio.Raster.from_array(values, GT, crs="EPSG:3857").write(
        str(path), overviews=[2, 4, 8], overview_resampling="average"
    )
    return path


@pytest.mark.parametrize("mode", [None, "AUTO", 1, "1"])
def test_overviews_for_paths_objects_mixed_and_repeated_sources(pyramid, mode):
    lazy = terrio.open_raster(str(pyramid), lazy=True)
    expected = lazy.read(1, overview=1)
    for src in [
        str(pyramid),
        pyramid,
        lazy,
        [lazy, lazy],
        [str(pyramid), lazy],
        [lazy, str(pyramid)],
    ]:
        out = terrio.warp(src, ts=(8, 8), overview=mode)
        np.testing.assert_array_equal(out.read(1), expected)
        assert out.transform == (0.0, 4.0, 0.0, 32.0, 0.0, -4.0)
    full = terrio.warp(lazy, ts=(8, 8), overview="NONE")
    assert np.any(full.read(1) != expected)


def test_finer_auto_and_explicit_bounds(pyramid):
    lazy = terrio.open_raster(str(pyramid), lazy=True)
    fine = terrio.warp(lazy, ts=(8, 8), overview="AUTO-1")
    explicit = terrio.warp(lazy, ts=(8, 8), overview=0)
    np.testing.assert_array_equal(fine.read(1), explicit.read(1))
    cropped = terrio.warp(lazy, te=(4, 4, 20, 20), ts=(4, 4), overview="AUTO")
    expected = lazy.read(1, window=(1, 3, 4, 4), overview=1)
    np.testing.assert_array_equal(cropped.read(1), expected)
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.warp(lazy, overview="invalid")


@pytest.mark.parametrize(
    ("mode", "total"), [(None, 16.0), ("AUTO", 16.0), ("NONE", 16.0), (0, 16.0)]
)
def test_a_sum_counts_each_overview_pixel_for_the_source_pixels_it_stands_for(
    tmp_path, mode, total
):
    # The overview has one pixel per 2-by-2 block, which counts for the four pixels of its block.
    path = tmp_path / "ones.tif"
    terrio.Raster.from_array(
        np.ones((4, 4), "float32"), (0.0, 1.0, 0.0, 4.0, 0.0, -1.0), crs="EPSG:3857"
    ).write(str(path), overviews=[2], overview_resampling="average")
    lazy = terrio.open_raster(str(path), lazy=True)
    for src in [str(path), lazy]:
        out = terrio.warp(src, ts=(2, 2), resampling="sum", ot="Float64", overview=mode)
        assert out.read(1).sum() == total


def test_an_automatic_sum_reads_the_overview_the_choice_gives(tmp_path):
    # A ramp whose overview holds one pixel of each 2-by-2 block: the automatic choice sums the
    # overview's pixels, each counted for its block, where NONE sums the 16 pixels.
    path = str(tmp_path / "ramp.tif")
    values = np.add.outer(np.arange(4) * 4, np.arange(4)).astype("float32")
    terrio.Raster.from_array(values, (0.0, 1.0, 0.0, 4.0, 0.0, -1.0), crs="EPSG:3857").write(
        path, overviews=[2], overview_resampling="nearest"
    )
    sampled = terrio.open_raster(path, lazy=True).read(1, overview=0)

    def warped(mode):
        return terrio.warp(path, ts=(2, 2), resampling="sum", ot="Float64", overview=mode).read(1)

    np.testing.assert_array_equal(warped("AUTO"), sampled * 4.0)
    np.testing.assert_array_equal(warped(None), warped(0))
    assert warped("NONE").sum() == values.sum() == 120.0
    assert warped("AUTO").sum() != 120.0


def test_a_reduced_read_of_a_whole_sum_warped_vrt_counts_its_overview_for_the_source(tmp_path):
    # The read takes the implicit overview the read's size gives, warped from the source's own
    # overview, each of whose pixels counts for the source pixels it stands for.
    gt = (0.0, 1.0, 0.0, 8.0, 0.0, -1.0)
    ones, ramp = tmp_path / "ones.tif", tmp_path / "ramp.tif"
    terrio.Raster.from_array(np.ones((8, 8), "float32"), gt, crs="EPSG:3857").write(
        str(ones), overviews=[2, 4], overview_resampling="average"
    )
    values = np.add.outer(np.arange(8) * 8, np.arange(8)).astype("float32")
    terrio.Raster.from_array(values, gt, crs="EPSG:3857").write(
        str(ramp), overviews=[2, 4], overview_resampling="nearest"
    )
    with terrio.rio.open(str(ones)) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        np.testing.assert_array_equal(vrt.read(1), np.ones((8, 8)))
        np.testing.assert_array_equal(vrt.read(1, out_shape=(4, 4)), np.full((4, 4), 4.0))
        np.testing.assert_array_equal(vrt.read(1, out_shape=(2, 2)), np.full((2, 2), 16.0))
        np.testing.assert_array_equal(vrt.read(out_shape=(1, 2, 2)), np.full((1, 2, 2), 16.0))
    # The ramp's overview at 4 holds one pixel of each 4-by-4 block: a read into 2 x 2 sums those,
    # each counted for its 16 pixels, rather than the 64 pixels of the full resolution.
    with terrio.rio.open(str(ramp), overview_level=1) as overview:
        sampled = overview.read(1)
    with terrio.rio.open(str(ramp)) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        np.testing.assert_array_equal(vrt.read(1, out_shape=(2, 2)), sampled * 16.0)
        assert vrt.read(1).sum() == values.sum()
    # `average` reads the implicit overview, warped from the source's own overview, unscaled.
    with terrio.rio.open(str(ramp)) as src, WarpedVRT(src, resampling=Resampling.average) as vrt:
        reduced = vrt.read(1, out_shape=(2, 2))
    np.testing.assert_array_equal(reduced, sampled)


def test_a_reduced_sum_read_of_a_warped_vrt_reads_less_of_a_remote_cog(tmp_path):
    # A remote 512 x 512 Float32 COG of 128-pixel tiles with average overviews at 2, 4 and 8, read
    # through a `sum` WarpedVRT into 64 x 64: the read takes the overview at 8 (one tile) where the
    # full resolution is 16 tiles, and the totals agree to rounding.
    from test_remote_reads import Server

    path = tmp_path / "cog.tif"
    values = (np.random.default_rng(7).random((512, 512), dtype="float32") * 100).astype("float32")
    terrio.Raster.from_array(values, (0.0, 1.0, 0.0, 512.0, 0.0, -1.0), crs="EPSG:3857").write(
        str(path),
        driver="COG",
        compress="DEFLATE",
        tile_size=128,
        options={"OVERVIEW_COUNT": "3"},
        overview_resampling="average",
    )
    assert terrio.open_raster(str(path), lazy=True).overviews == [(256, 256), (128, 128), (64, 64)]
    exact = values.astype("float64").sum()
    server = Server(tmp_path)
    sent = {}
    try:
        for name, shape in [("reduced", (64, 64)), ("full", None)]:
            terrio.clear_http_cache()
            start = server.bytes_sent()
            with terrio.rio.open(f"{server.url}/cog.tif") as src:
                with WarpedVRT(src, resampling=Resampling.sum) as vrt:
                    read = vrt.read(1, out_shape=shape)
            sent[name] = server.bytes_sent() - start
            assert read.astype("float64").sum() == pytest.approx(exact, rel=1e-6), name
    finally:
        server.close()
        terrio.clear_http_cache()
    assert sent["reduced"] * 8 < sent["full"], sent


def test_a_reduced_sum_read_counts_a_partly_nodata_block_whole(tmp_path):
    # 2-by-2 blocks of 2s with one nodata pixel, 1s, nodata and 3s, with an average overview at 2:
    # its pixel of the first block, 2, counts for the whole block; the nodata block stays nodata.
    path = str(tmp_path / "holes.tif")
    values = np.array(
        [[2, 2, 1, 1], [2, -9999, 1, 1], [-9999, -9999, 3, 3], [-9999, -9999, 3, 3]], "float32"
    )
    terrio.Raster.from_array(
        values, (0.0, 1.0, 0.0, 4.0, 0.0, -1.0), crs="EPSG:3857", nodata=-9999.0
    ).write(path, overviews=[2], overview_resampling="average")
    expected = np.array([[8.0, 4.0], [-9999.0, 12.0]])
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        np.testing.assert_array_equal(vrt.read(1, out_shape=(2, 2)), expected)
    np.testing.assert_array_equal(terrio.warp(path, ts=(2, 2), resampling="sum").read(1), expected)
    exact = terrio.warp(path, ts=(2, 2), resampling="sum", overview="NONE").read(1)
    np.testing.assert_array_equal(exact, [[6.0, 4.0], [-9999.0, 12.0]])


def test_a_sum_counts_an_overview_whose_factor_does_not_divide_the_size(tmp_path):
    # 10 x 10 ones with an overview at 4 of 3 x 3 pixels: each pixel stands for 10/3 x 10/3 ones.
    path = str(tmp_path / "odd.tif")
    terrio.Raster.from_array(
        np.ones((10, 10), "float32"), (0.0, 1.0, 0.0, 10.0, 0.0, -1.0), crs="EPSG:3857"
    ).write(path, overviews=[4], overview_resampling="average")
    assert terrio.open_raster(path, lazy=True).overviews == [(3, 3)]
    for mode in [None, 0]:
        out = terrio.warp(path, ts=(2, 2), resampling="sum", ot="Float64", overview=mode)
        assert out.read(1).sum() == pytest.approx(100.0, rel=1e-12), mode
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        reduced = vrt.read(1, out_shape=(2, 2)).astype("float64")
    assert reduced.sum() == pytest.approx(100.0, rel=1e-6)


def test_a_sum_counts_each_axis_of_an_overview_by_its_own_ratio(tmp_path):
    # A 9 x 5 raster has an overview of 5 x 3 at 2, whose pixels stand for 9/5 by 5/3 pixels, 3 in
    # all, where the ratio of one axis squared would give 3.24 or 2.78. Every `terrio.rio` path
    # that counts an overview's pixels: a reduced read of a whole `WarpedVRT`, and a dataset opened
    # at the overview level, read through a `WarpedVRT` and reprojected.
    path = str(tmp_path / "oblong.tif")
    values = (np.add.outer(np.arange(5) * 7, np.arange(9) * 3) % 11 + 0.5).astype("float32")
    terrio.Raster.from_array(values, (0.0, 1.0, 0.0, 5.0, 0.0, -1.0), crs="EPSG:3857").write(
        path, overviews=[2], overview_resampling="average"
    )
    assert terrio.open_raster(path, lazy=True).overviews == [(5, 3)]
    exact = values.astype("float64").sum()
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        reduced = vrt.read(1, out_shape=(3, 5)).astype("float64").sum()
    one = {"dst_transform": from_origin(0, 5, 9, 5), "dst_crs": "EPSG:3857"}
    reprojected = np.zeros((1, 1))
    with terrio.rio.open(path, overview_level=0) as src:
        with WarpedVRT(src, resampling=Resampling.sum) as vrt:
            level = vrt.read(1).astype("float64").sum()
        reproject(terrio.rio.band(src, 1), reprojected, resampling=Resampling.sum, **one)
    for name, total in [("reduced", reduced), ("level", level), ("reproject", reprojected.sum())]:
        assert total == pytest.approx(exact, rel=1e-6), name


@pytest.mark.parametrize("overviews", [[2, 4], None])
def test_a_reduced_read_of_any_bands_of_a_sum_warped_vrt_is_warped_into_its_buffer(
    tmp_path, overviews
):
    # A reduced read of the whole grid is warped into a grid of the buffer's size whichever bands
    # it reads: one band, a repeated band or every band, with or without overviews.
    path = str(tmp_path / "ones.tif")
    raster = terrio.Raster.from_array(
        np.ones((2, 8, 8), "float32"), (0.0, 1.0, 0.0, 8.0, 0.0, -1.0), crs="EPSG:3857"
    )
    if overviews:
        raster.write(path, overviews=overviews, overview_resampling="average")
    else:
        raster.write(path)
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        for bands in ([1], [1, 1], [2], [2, 1]):
            read = vrt.read(bands, out_shape=(len(bands), 4, 4))
            np.testing.assert_array_equal(read, np.full((len(bands), 4, 4), 4.0), str(bands))


def test_a_reduced_read_of_a_gcp_sum_warped_vrt_keeps_the_total(tmp_path):
    """A `sum` VRT georeferenced by GCPs keeps its total when its whole grid is read reduced.

    The total of the read is that of the full-resolution read, up to the rounding of each output
    pixel to 32 bits (a relative 1e-6).
    """
    width, height = 64, 48
    path = str(tmp_path / "gcps.tif")
    gcps = [
        GroundControlPoint(row=r, col=c, x=500000 + c * 10, y=4100000 - r * 10)
        for r in (0, height)
        for c in (0, width)
    ]
    profile = dict(width=width, height=height, count=1, dtype="float32", gcps=gcps)
    with terrio.rio.open(path, "w", driver="GTiff", crs="EPSG:32611", **profile) as dst:
        dst.write(np.add.outer(np.arange(height) * width, np.arange(width)).astype("float32")[None])
    with terrio.rio.open(path, "r+") as dst:
        dst.build_overviews([2, 4], Resampling.average)
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        full = vrt.read(1).astype("float64").sum()
        reduced = vrt.read(1, out_shape=(vrt.height // 4, vrt.width // 4)).astype("float64").sum()
    assert reduced == pytest.approx(full, rel=1e-6)


def test_a_sum_over_its_memory_budget_is_refused_before_its_source_is_read(tmp_path, monkeypatch):
    # 32 x 32 Float32 ones with an overview at 4. A `sum` holds the 4 bytes of each source pixel:
    # 4,096 bytes at full resolution, 256 for the overview; two sources, both whole, 8,192 at full
    # resolution.
    path = str(tmp_path / "ones.tif")
    terrio.Raster.from_array(np.ones((32, 32), "float32"), GT, crs="EPSG:3857").write(
        path, overviews=[4], overview_resampling="average"
    )
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "4000")
    refused = (
        r"TERRIO_WARP_SUM_MAX_BYTES=4000: sum an overview level instead, with `--overview n` "
        r"\(`overview=n` for `terrio\.warp`, or a source opened with `overview_level=n` in "
        r"`terrio\.rio`\)"
    )
    with pytest.raises(terrio.ResourceLimitError, match=refused):
        terrio.warp(path, ts=(1, 1), resampling="sum", overview="NONE")
    with pytest.raises(terrio.ResourceLimitError, match=refused):
        terrio.warp([path, path], ts=(1, 1), resampling="sum", overview="NONE")
    # A 1 x 1 VRT read at its size: its pixel's source window is the full resolution.
    pixel = {"crs": "EPSG:3857", "transform": from_origin(0, 32, 32, 32), "width": 1, "height": 1}
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum, **pixel) as vrt:
        with pytest.raises(terrio.ResourceLimitError, match=refused):
            vrt.read(1)
    one = {"dst_transform": from_origin(0, 32, 32, 32), "dst_crs": "EPSG:3857"}
    with terrio.rio.open(path) as src:
        with pytest.raises(terrio.ResourceLimitError, match=refused):
            reproject(terrio.rio.band(src, 1), np.zeros((1, 1)), resampling=Resampling.sum, **one)
    # The levels the message names: the overview's 64 pixels, each counted for its 16 pixels.
    reprojected_level = np.zeros((1, 1))
    with terrio.rio.open(path, overview_level=0) as src:
        with WarpedVRT(src, resampling=Resampling.sum) as vrt:
            assert vrt.read(1, out_shape=(1, 1)).sum() == 1024.0
        with WarpedVRT(src, resampling=Resampling.sum, **pixel) as vrt:
            assert vrt.read(1).sum() == 1024.0
        reproject(terrio.rio.band(src, 1), reprojected_level, resampling=Resampling.sum, **one)
    assert reprojected_level.sum() == 1024.0
    level = terrio.warp(path, ts=(1, 1), resampling="sum", ot="Float64", overview=0)
    assert level.read(1).sum() == 1024.0
    average = terrio.warp(path, ts=(1, 1), resampling="average", overview="NONE")
    assert average.read(1).sum() == 1.0
    # The automatic choice reads the overview, within the budget.
    auto = terrio.warp(path, ts=(1, 1), resampling="sum", ot="Float64")
    assert auto.read(1).sum() == 1024.0
    mosaic = terrio.warp([path, path], ts=(1, 1), resampling="sum", ot="Float64")
    assert mosaic.read(1).sum() == 1024.0
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        assert vrt.read(1, out_shape=(1, 1)).sum() == 1024.0
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "20000")
    full = terrio.warp(path, ts=(1, 1), resampling="sum", ot="Float64", overview="NONE")
    assert full.read(1).sum() == 1024.0
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum, **pixel) as vrt:
        assert vrt.read(1).sum() == 1024.0
    reprojected = np.zeros((1, 1))
    with terrio.rio.open(path) as src:
        reproject(terrio.rio.band(src, 1), reprojected, resampling=Resampling.sum, **one)
    assert reprojected.sum() == 1024.0


def test_a_sum_counts_the_bytes_of_every_band(tmp_path, monkeypatch):
    # 32 x 32 Float32 ones summed into 1 x 1. Each band holds 4 bytes per pixel: under 6,000 bytes
    # one band (4,096) fits and two (8,192) do not. Two sources are held whole: under 12,000, one
    # band each (8,192) fits and two (16,384) do not.
    one, two = str(tmp_path / "one.tif"), str(tmp_path / "two.tif")
    terrio.Raster.from_array(np.ones((32, 32), "float32"), GT, crs="EPSG:3857").write(one)
    terrio.Raster.from_array(np.ones((2, 32, 32), "float32"), GT, crs="EPSG:3857").write(two)
    pixel = {"dst_transform": from_origin(0, 32, 32, 32), "dst_crs": "EPSG:3857"}
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "6000")
    refused = r"about 8192 bytes .*TERRIO_WARP_SUM_MAX_BYTES=6000"
    with pytest.raises(terrio.ResourceLimitError, match=refused):
        terrio.warp(two, ts=(1, 1), resampling="sum")
    assert terrio.warp(one, ts=(1, 1), resampling="sum", ot="Float64").read(1).sum() == 1024.0
    with terrio.rio.open(two) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        with pytest.raises(terrio.ResourceLimitError, match=refused):
            vrt.read(out_shape=(2, 1, 1))
    with terrio.rio.open(one) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        assert vrt.read(1, out_shape=(1, 1)).sum() == 1024.0
    reprojected = np.zeros((1, 1))
    with terrio.rio.open(two) as src:
        with pytest.raises(terrio.ResourceLimitError, match=refused):
            reproject(
                terrio.rio.band(src, [1, 2]),
                np.zeros((2, 1, 1)),
                resampling=Resampling.sum,
                **pixel,
            )
        reproject(terrio.rio.band(src, 1), reprojected, resampling=Resampling.sum, **pixel)
    assert reprojected.sum() == 1024.0
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "12000")
    with pytest.raises(terrio.ResourceLimitError, match=r"about 16384 bytes"):
        terrio.warp([two, two], ts=(1, 1), resampling="sum")
    mosaic = terrio.warp([one, one], ts=(1, 1), resampling="sum", ot="Float64")
    assert mosaic.read(1).sum() == 1024.0


def test_a_sum_counts_the_working_type_of_a_warped_vrt(tmp_path, monkeypatch):
    # Two bands of 32 x 32 ones read through a `WarpedVRT` summed into 1 x 1, under 20,000
    # bytes. Float64 converted to a float32 working type, a window is held in both types at once:
    # 8 + 4 bytes per band and pixel, 24,576. Float32 in a float64 working type, which holds every
    # float32 value, is not converted: 4 bytes per band (8,192), and it fits, as in float32.
    path, f64 = str(tmp_path / "two.tif"), str(tmp_path / "f64.tif")
    terrio.Raster.from_array(np.ones((2, 32, 32), "float32"), GT, crs="EPSG:3857").write(path)
    terrio.Raster.from_array(np.ones((2, 32, 32), "float64"), GT, crs="EPSG:3857").write(f64)
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "20000")
    with terrio.rio.open(f64) as src:
        with WarpedVRT(src, resampling=Resampling.sum, dtype="float32") as vrt:
            with pytest.raises(terrio.ResourceLimitError, match=r"about 24576 bytes"):
                vrt.read(out_shape=(2, 1, 1))
    with terrio.rio.open(path) as src:
        with WarpedVRT(src, resampling=Resampling.sum, dtype="float64") as vrt:
            assert vrt.read(out_shape=(2, 1, 1)).sum(axis=(1, 2)).tolist() == [1024.0, 1024.0]
        with WarpedVRT(src, resampling=Resampling.sum) as vrt:
            assert vrt.read(out_shape=(2, 1, 1)).sum(axis=(1, 2)).tolist() == [1024.0, 1024.0]


@pytest.mark.parametrize("mode", [True, False])
def test_rejects_boolean_overview(pyramid, mode):
    with pytest.raises(terrio.InvalidArgumentError, match="boolean"):
        terrio.warp(pyramid, overview=mode)


def test_geolocation_coordinates_keep_full_resolution_pixel_scale(pyramid):
    lazy = terrio.open_raster(str(pyramid), lazy=True)
    x = terrio.Raster.from_array(
        np.array([[10, 42], [10, 42]], dtype="float64"), (0, 1, 0, 2, 0, -1)
    )
    y = terrio.Raster.from_array(
        np.array([[50, 50], [18, 18]], dtype="float64"), (0, 1, 0, 2, 0, -1)
    )
    result = terrio.warp(
        lazy,
        t_srs="EPSG:4326",
        te=(10, 18, 42, 50),
        ts=(8, 8),
        overview=1,
        geolocation={"x": x, "y": y, "pixel_step": 32, "line_step": 32},
    )
    np.testing.assert_array_equal(result.read(1), lazy.read(1, overview=1))


@pytest.mark.parametrize("georef", ["gcp", "rpc"])
def test_mixed_path_retains_file_georeferencing(gdal, tmp_path, georef):
    from osgeo import osr

    values = np.arange(32 * 32, dtype="float32").reshape(32, 32)
    path = str(tmp_path / f"{georef}.tif")
    file = gdal.GetDriverByName("GTiff").Create(path, 32, 32, 1, gdal.GDT_Float32)
    file.GetRasterBand(1).WriteArray(values)
    geographic = osr.SpatialReference()
    geographic.ImportFromEPSG(4326)
    if georef == "gcp":
        file.SetGCPs(
            [
                gdal.GCP(10 + x / 1000, 45 - y / 1000, 0, x, y)
                for x, y in [(0, 0), (32, 0), (0, 32), (32, 32)]
            ],
            geographic.ExportToWkt(),
        )
        flags = {"polynomialOrder": 1}
    else:

        def coefficients(position, value):
            return " ".join(str(value if i == position else 0) for i in range(20))

        file.SetMetadata(
            {
                "LINE_OFF": "15.5",
                "SAMP_OFF": "15.5",
                "LAT_OFF": "44.984",
                "LONG_OFF": "10.016",
                "HEIGHT_OFF": "0",
                "LINE_SCALE": "16",
                "SAMP_SCALE": "16",
                "LAT_SCALE": "0.016",
                "LONG_SCALE": "0.016",
                "HEIGHT_SCALE": "1",
                "LINE_NUM_COEFF": coefficients(2, -1),
                "SAMP_NUM_COEFF": coefficients(1, 1),
                "LINE_DEN_COEFF": coefficients(0, 1),
                "SAMP_DEN_COEFF": coefficients(0, 1),
            },
            "RPC",
        )
        flags = {"rpc": True}
    file.BuildOverviews("AVERAGE", [2, 4])
    file = None
    expected = gdal.Warp(
        "",
        path,
        format="MEM",
        dstSRS="EPSG:4326",
        outputBounds=[10, 44.968, 10.032, 45],
        width=8,
        height=8,
        overviewLevel=1,
        errorThreshold=0,
        **flags,
    )
    memory = terrio.Raster.from_array(values, GT)
    lazy = terrio.open_raster(path, lazy=True)
    # A first path must use its file GCPs/RPCs even when another input is a Raster.
    for source in [path, [path, lazy]]:
        result = terrio.warp(
            source, t_srs="EPSG:4326", te=(10, 44.968, 10.032, 45), ts=(8, 8), overview=1, et=0
        )
        np.testing.assert_array_equal(result.read(1), expected.ReadAsArray())
    # Loaded sources have no stored overviews, but must use the same file transform.
    expected_full = gdal.Warp(
        "",
        path,
        format="MEM",
        dstSRS="EPSG:4326",
        outputBounds=[10, 44.968, 10.032, 45],
        width=8,
        height=8,
        overviewLevel="NONE",
        errorThreshold=0,
        **flags,
    )
    result = terrio.warp(
        [path, memory],
        t_srs="EPSG:4326",
        te=(10, 44.968, 10.032, 45),
        ts=(8, 8),
        overview="NONE",
        et=0,
    )
    np.testing.assert_array_equal(result.read(1), expected_full.ReadAsArray())


def test_loaded_sources_warn_once_for_unavailable_level():
    memory = terrio.Raster.from_array(np.arange(32 * 32, dtype="float32").reshape(32, 32), GT)
    expected = terrio.warp(memory, ts=(8, 8), overview="NONE")
    with pytest.warns(UserWarning, match="cannot get overview level") as warnings:
        result = terrio.warp([memory, memory], ts=(8, 8), overview=0)
    assert len(warnings) == 1
    np.testing.assert_array_equal(result.read(1), expected.read(1))


def test_cropped_sum_can_differ_from_full_resolution_total(tmp_path):
    """A partial overview cell loses the distribution within its averaged source block."""
    path = str(tmp_path / "contrast.tif")
    terrio.Raster.from_array(
        np.array([[100, 0], [0, 0]], dtype="float64"),
        (0, 1, 0, 2, 0, -1),
        crs="EPSG:3857",
    ).write(path, overviews=[2], overview_resampling="average")
    options = dict(te=(0, 1, 1, 2), ts=(1, 1), resampling="sum", ot="Float64")
    estimated = terrio.warp(path, overview=0, **options)
    exact = terrio.warp(path, overview="NONE", **options)
    np.testing.assert_array_equal(estimated.read(1), [[25.0]])
    np.testing.assert_array_equal(exact.read(1), [[100.0]])
    assert estimated.transform == exact.transform == (0.0, 1.0, 0.0, 2.0, 0.0, -1.0)


def test_implicit_vrt_level_keeps_the_opened_source_scale(tmp_path):
    """An implicit VRT level stays comparable to its already-scaled base pixels."""
    from terrio.rio.windows import Window

    path = str(tmp_path / "opened-level.tif")
    terrio.Raster.from_array(
        np.ones((8, 8), dtype="float32"),
        (0, 1, 0, 8, 0, -1),
        crs="EPSG:3857",
    ).write(path, overviews=[2, 4], overview_resampling="average")
    with terrio.rio.open(path, overview_level=0) as source:
        with WarpedVRT(source, resampling=Resampling.sum, dtype="float64") as vrt:
            np.testing.assert_array_equal(vrt.read(1), np.full((4, 4), 4.0))
            reduced = vrt.read(1, window=Window(0, 0, 2, 2), out_shape=(1, 1))
            np.testing.assert_array_equal(reduced, [[4.0]])
