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
