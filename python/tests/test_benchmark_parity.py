"""Binding regressions for the georeferencing loss found while investigating #253."""

import numpy as np

import terrio


def test_gcp_only_cog_copies_keep_coordinates(data_dir, gdal, tmp_path):
    source = tmp_path / "gcps.tif"
    original = gdal.Open(str(data_dir / "srtm90.tif"))
    gt = original.GetGeoTransform()
    points = [
        gdal.GCP(
            gt[0] + (x + 0.5) * gt[1],
            gt[3] + (y + 0.5) * gt[5],
            0,
            x + 0.5,
            y + 0.5,
        )
        for y in range(39)
        for x in range(41)
    ]
    gdal.Translate(
        str(source),
        original,
        srcWin=[0, 0, 64, 64],
        GCPs=points,
        outputSRS=original.GetProjection(),
    )
    original = gdal.Open(str(source))
    expected = original.ReadAsArray()
    rasters = [terrio.open_raster(str(source)), terrio.open_bytes(source.read_bytes(), "gcps.tif")]
    for i, raster in enumerate(rasters):
        target = tmp_path / f"cog-{i}.tif"
        raster.write(str(target), driver="COG")
        actual = gdal.Open(str(target))
        assert actual.GetGCPCount() == 1599
        assert actual.GetGeoTransform(can_return_null=True) is None
        assert actual.GetGCPSpatialRef().IsSame(original.GetGCPSpatialRef())
        assert [(g.GCPPixel, g.GCPLine, g.GCPX, g.GCPY, g.GCPZ) for g in actual.GetGCPs()] == [
            (g.GCPPixel, g.GCPLine, g.GCPX, g.GCPY, g.GCPZ) for g in original.GetGCPs()
        ]
        np.testing.assert_array_equal(actual.ReadAsArray(), expected)


def test_versioned_terrain_and_webp_options(tmp_path):
    data = np.array(
        [942.315, 946.702, 951.43, 945.132, 943.95, 959.37, 948.15, 954.318, 960.01],
        dtype=np.float32,
    ).reshape(3, 3)
    raster = terrio.Raster.from_array(data, [500000, 3.5, 0, 4100000, 0, -3.5], "EPSG:32610")
    slope = terrio.dem(raster, "slope", kernel_version="3.13").read()[0, 1, 1]
    assert abs(float(slope) - 64.0974) < 1e-5
    rgb = (np.arange(3 * 20 * 12).reshape(3, 12, 20) * 37 % 251).astype(np.uint8)
    image = terrio.Raster.from_array(rgb, [0, 1, 0, 12, 0, -1])
    path = tmp_path / "lossless.tif"
    image.write(str(path), compress="WEBP", options={"WEBP_LOSSLESS": "YES"})
    np.testing.assert_array_equal(terrio.open_bytes(path.read_bytes(), "webp.tif").read(), rgb)
    low, high = [tmp_path / f"quality-{quality}.tif" for quality in [10, 95]]
    for path, quality in [(low, 10), (high, 95)]:
        image.write(str(path), compress="WEBP", options={"WEBP_LEVEL": str(quality)})
    assert low.read_bytes() != high.read_bytes()
    assert not np.array_equal(
        terrio.open_raster(str(low)).read(), terrio.open_raster(str(high)).read()
    )
