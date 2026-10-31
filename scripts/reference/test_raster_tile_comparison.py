"""Value differences cannot hide missing tiles or malformed tile structure."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import numpy as np
from osgeo import gdal, osr

spec = importlib.util.spec_from_file_location("raster_tile_comparison", Path(__file__).resolve().parents[1] / "compare_raster_tiles_with_gdal.py")
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


class RasterTileComparisonTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.reference, self.actual = (Path(self.temp.name) / name for name in ['reference', 'actual'])
        self.reference.mkdir()
        self.actual.mkdir()
        self.write(self.reference / 'a.tif', value=1)
        self.write(self.actual / 'a.tif', value=2)

    def test_pyramid_reports_exact_matches_and_only_waives_value_differences(self):
        for root in [self.reference, self.actual]:
            gdal.Translate(str(root / 'a.png'), str(self.reference / 'a.tif'), format='PNG')
        with patch.object(comparison, 'compare_direct_pyramid', return_value='direct source verified'):
            self.assertEqual(comparison.compare_pyramid(None, self.reference, self.actual, None)[0], 'identical')
            gdal.Translate(str(self.actual / 'a.png'), str(self.actual / 'a.tif'), format='PNG')
            self.assertEqual(comparison.compare_pyramid(None, self.reference, self.actual, None)[0], 'tolerance')
            (self.actual / 'a.png').unlink()
            with self.assertRaisesRegex(AssertionError, 'tile names differ'):
                comparison.compare_pyramid(None, self.reference, self.actual, None)

    def write(self, path, value=1, width=2, dtype=gdal.GDT_Byte, nodata=None):
        ds = gdal.GetDriverByName('GTiff').Create(str(path), width, 2, 1, dtype)
        ds.SetGeoTransform([0, 1, 0, 2, 0, -1])
        crs = osr.SpatialReference()
        crs.ImportFromEPSG(4326)
        ds.SetProjection(crs.ExportToWkt())
        ds.GetRasterBand(1).WriteArray(np.full((2, width), value))
        if nodata is not None:
            ds.GetRasterBand(1).SetNoDataValue(nodata)
        ds = None

    def assert_structural_error(self):
        with self.assertRaises(AssertionError) as caught:
            comparison.compare_folders(self.reference, self.actual, 'tif')
        self.assertNotIsInstance(caught.exception, comparison.PixelDifference)

    def test_only_pixel_mismatches_have_the_policy_exception(self):
        with self.assertRaises(comparison.PixelDifference):
            comparison.compare_folders(self.reference, self.actual, 'tif')

    def test_missing_tiles_are_never_value_policy_differences(self):
        self.write(self.reference / 'b.tif')
        self.assert_structural_error()

    def test_later_shape_failure_is_not_hidden_by_an_earlier_pixel_difference(self):
        self.write(self.reference / 'b.tif')
        self.write(self.actual / 'b.tif', width=3)
        self.assert_structural_error()

    def test_dataset_mask_failure_is_not_hidden_by_a_pixel_difference(self):
        ds = gdal.Open(str(self.actual / 'a.tif'), gdal.GA_Update)
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        ds.GetRasterBand(1).GetMaskBand().Fill(0)
        ds = None
        self.assert_structural_error()

    def test_alpha_value_difference_does_not_hide_an_unrelated_derived_mask_error(self):
        for root, alpha in [(self.reference, 128), (self.actual, 255)]:
            ds = gdal.GetDriverByName('GTiff').Create(str(root / 'a.tif'), 2, 2, 2, gdal.GDT_Byte)
            ds.SetGeoTransform([0, 1, 0, 2, 0, -1])
            crs = osr.SpatialReference()
            crs.ImportFromEPSG(4326)
            ds.SetProjection(crs.ExportToWkt())
            ds.GetRasterBand(1).Fill(7)
            ds.GetRasterBand(2).Fill(alpha)
            ds.GetRasterBand(2).SetColorInterpretation(gdal.GCI_AlphaBand)
            ds = None
        original_open = gdal.Open

        class CorruptMask:
            def ReadAsArray(self):
                return np.zeros((2, 2), dtype=np.uint8)

        class BandProxy:
            def __init__(self, band):
                self.band = band
            def __getattr__(self, name):
                return getattr(self.band, name)
            def GetMaskBand(self):
                return CorruptMask()

        class DatasetProxy:
            def __init__(self, ds):
                self.ds = ds
            def __getattr__(self, name):
                return getattr(self.ds, name)
            def GetRasterBand(self, i):
                band = self.ds.GetRasterBand(i)
                return BandProxy(band) if i == 1 else band

        def open_with_corrupt_derived_mask(path):
            ds = original_open(path)
            return DatasetProxy(ds) if Path(path).parent == self.actual else ds

        with patch.object(comparison.gdal, 'Open', side_effect=open_with_corrupt_derived_mask):
            self.assert_structural_error()

    def test_matching_nan_nodata_is_equal(self):
        self.write(self.reference / 'a.tif', dtype=gdal.GDT_Float32, nodata=float('nan'))
        self.write(self.actual / 'a.tif', dtype=gdal.GDT_Float32, nodata=float('nan'))
        comparison.compare_folders(self.reference, self.actual, 'tif')

    def test_type_and_nodata_failures_are_never_value_policy_differences(self):
        for dtype, nodata in [(gdal.GDT_UInt16, None), (gdal.GDT_Byte, 200)]:
            with self.subTest(dtype=dtype, nodata=nodata):
                self.write(self.actual / 'a.tif', value=2, dtype=dtype, nodata=nodata)
                self.assert_structural_error()


if __name__ == '__main__':
    unittest.main()
