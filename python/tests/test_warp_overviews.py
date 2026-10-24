"""Stored overview selection must survive paths, lazy objects and repeated inputs."""

import numpy as np
import pytest

import terrio

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
