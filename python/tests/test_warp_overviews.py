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
    ("mode", "total"), [(None, 16.0), ("AUTO", 16.0), ("NONE", 16.0), (0, 4.0)]
)
def test_sum_reads_full_resolution_unless_a_level_is_given(tmp_path, mode, total):
    # The overview has one pixel per 2-by-2 block: its sum is a quarter of the total.
    path = tmp_path / "ones.tif"
    terrio.Raster.from_array(
        np.ones((4, 4), "float32"), (0.0, 1.0, 0.0, 4.0, 0.0, -1.0), crs="EPSG:3857"
    ).write(str(path), overviews=[2], overview_resampling="average")
    lazy = terrio.open_raster(str(path), lazy=True)
    for src in [str(path), lazy]:
        out = terrio.warp(src, ts=(2, 2), resampling="sum", ot="Float64", overview=mode)
        assert out.read(1).sum() == total


def test_reduced_reads_of_a_whole_sum_warped_vrt_read_full_resolution(tmp_path):
    # An implicit overview of the VRT has one pixel per block: its sum would be a fraction.
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
    # `average` still reads the implicit overview, warped from the source's own overview.
    with terrio.rio.open(str(ramp)) as src, WarpedVRT(src, resampling=Resampling.average) as vrt:
        reduced = vrt.read(1, out_shape=(2, 2))
    with terrio.rio.open(str(ramp), overview_level=1) as overview:
        np.testing.assert_array_equal(reduced, overview.read(1))


@pytest.mark.parametrize("overviews", [[2, 4], None])
def test_a_reduced_read_of_any_bands_of_a_sum_warped_vrt_reads_full_resolution(tmp_path, overviews):
    # A reduced read of the whole grid is warped from the full resolution whichever bands it
    # reads: one band, a repeated band or every band, with or without overviews.
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
    # 32 x 32 Float32 ones with an overview at 4. A `sum` holds 4 bytes per source pixel and 8 of
    # shares: 12,288 bytes at full resolution; two sources, both whole and the shares, 16,384.
    path = str(tmp_path / "ones.tif")
    terrio.Raster.from_array(np.ones((32, 32), "float32"), GT, crs="EPSG:3857").write(
        path, overviews=[4], overview_resampling="average"
    )
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "10000")
    refused = r"TERRIO_WARP_SUM_MAX_BYTES=10000.*--overview n"
    with pytest.raises(terrio.ResourceLimitError, match=refused):
        terrio.warp(path, ts=(1, 1), resampling="sum")
    with pytest.raises(terrio.ResourceLimitError, match=refused):
        terrio.warp([path, path], ts=(1, 1), resampling="sum")
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        with pytest.raises(terrio.ResourceLimitError, match=refused):
            vrt.read(1, out_shape=(1, 1))
    one = {"dst_transform": from_origin(0, 32, 32, 32), "dst_crs": "EPSG:3857"}
    with terrio.rio.open(path) as src:
        with pytest.raises(terrio.ResourceLimitError, match=refused):
            reproject(terrio.rio.band(src, 1), np.zeros((1, 1)), resampling=Resampling.sum, **one)
    level = terrio.warp(path, ts=(1, 1), resampling="sum", ot="Float64", overview=0)
    assert level.read(1).sum() == 64.0
    average = terrio.warp(path, ts=(1, 1), resampling="average", overview="NONE")
    assert average.read(1).sum() == 1.0
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "20000")
    full = terrio.warp(path, ts=(1, 1), resampling="sum", ot="Float64")
    assert full.read(1).sum() == 1024.0
    with terrio.rio.open(path) as src, WarpedVRT(src, resampling=Resampling.sum) as vrt:
        assert vrt.read(1, out_shape=(1, 1)).sum() == 1024.0
    reprojected = np.zeros((1, 1))
    with terrio.rio.open(path) as src:
        reproject(terrio.rio.band(src, 1), reprojected, resampling=Resampling.sum, **one)
    assert reprojected.sum() == 1024.0


def test_a_sum_counts_the_bytes_of_every_band(tmp_path, monkeypatch):
    # 32 x 32 Float32 ones summed into 1 x 1. Each band holds 4 bytes per pixel and the shares 8:
    # under 14,000 bytes one band (12,288) fits and two (16,384) do not. Two sources are held
    # whole with the shares of one window: under 20,000, one band each (16,384) fits and two
    # (24,576) do not.
    one, two = str(tmp_path / "one.tif"), str(tmp_path / "two.tif")
    terrio.Raster.from_array(np.ones((32, 32), "float32"), GT, crs="EPSG:3857").write(one)
    terrio.Raster.from_array(np.ones((2, 32, 32), "float32"), GT, crs="EPSG:3857").write(two)
    pixel = {"dst_transform": from_origin(0, 32, 32, 32), "dst_crs": "EPSG:3857"}
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "14000")
    refused = r"about 16384 bytes .*TERRIO_WARP_SUM_MAX_BYTES=14000"
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
    monkeypatch.setenv("TERRIO_WARP_SUM_MAX_BYTES", "20000")
    with pytest.raises(terrio.ResourceLimitError, match=r"about 24576 bytes"):
        terrio.warp([two, two], ts=(1, 1), resampling="sum")
    mosaic = terrio.warp([one, one], ts=(1, 1), resampling="sum", ot="Float64")
    assert mosaic.read(1).sum() == 1024.0


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
