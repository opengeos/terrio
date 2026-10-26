"""Georeferencing participates in pixel-command conformance results."""

import sys
import tempfile
import unittest
from pathlib import Path

from osgeo import gdal, ogr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance
from compare_raster_pixel_with_gdal import compare_vector, raster_extra


class PixelGeoreferencingTests(unittest.TestCase):
    def compare_transforms(self, expected, actual, allow_s5p=False):
        with tempfile.TemporaryDirectory() as directory:
            paths = [str(Path(directory) / name) for name in ("gdal.tif", "terrio.tif")]
            for path, transform in zip(paths, (expected, actual)):
                ds = gdal.GetDriverByName("GTiff").Create(path, 2, 3, 1)
                if transform is not None:
                    ds.SetGeoTransform(transform)
                ds = None
            result = {"same_size": True, "differing": 0}
            result.update(raster_extra(*paths, allow_s5p_default=allow_s5p))
            return result

    def test_identical_pixels_with_shifted_transform_fail(self):
        result = self.compare_transforms((100, 2, 0, 200, 0, -2), (102, 2, 0, 200, 0, -2))
        self.assertEqual(conformance.classify_raster(result)[0], conformance.DIFFERS)

    def test_matching_transforms_are_identical(self):
        transform = (100, 2, 0, 200, 0, -2)
        result = self.compare_transforms(transform, transform)
        self.assertEqual(conformance.classify_raster(result)[0], conformance.IDENTICAL)

    def test_known_default_grid_is_explicit_and_not_identical(self):
        result = self.compare_transforms(None, (0, 1, 0, 3, 0, -1), allow_s5p=True)
        status, detail = conformance.classify_raster(result)
        self.assertEqual(status, conformance.TOLERANCE)
        self.assertIn("Sentinel-5P", detail)
        self.assertFalse(result["same_georeferencing"])

    def test_zero_scale_is_not_confused_with_an_absent_scale(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = [str(Path(directory) / name) for name in ("gdal.tif", "terrio.tif")]
            for index, path in enumerate(paths):
                dataset = gdal.GetDriverByName("GTiff").Create(path, 2, 3, 1)
                if index == 0:
                    dataset.GetRasterBand(1).SetScale(0.0)
                dataset = None
            result = {"same_size": True, "differing": 0}
            result.update(raster_extra(*paths))
            self.assertFalse(result["same_metadata"])
            self.assertEqual(conformance.classify_raster(result)[0], conformance.DIFFERS)

    def test_equal_numeric_values_do_not_hide_different_ogr_field_types(self):
        for left, right in [(ogr.OFTInteger, ogr.OFTInteger64), (ogr.OFTInteger, ogr.OFTReal),
                            (ogr.OFTString, ogr.OFTReal)]:
            with self.subTest(left=left, right=right), tempfile.TemporaryDirectory() as directory:
                paths = [str(Path(directory) / name) for name in ("gdal.gpkg", "terrio.gpkg")]
                for path, kind in zip(paths, (left, right)):
                    dataset = ogr.GetDriverByName("GPKG").CreateDataSource(path)
                    layer = dataset.CreateLayer("pixels", geom_type=ogr.wkbPoint)
                    layer.CreateField(ogr.FieldDefn("VALUE", kind))
                    feature = ogr.Feature(layer.GetLayerDefn())
                    feature.SetField("VALUE", "1" if kind == ogr.OFTString else 1)
                    point = ogr.Geometry(ogr.wkbPoint)
                    point.AddPoint_2D(0, 0)
                    feature.SetGeometry(point)
                    layer.CreateFeature(feature)
                    feature = layer = dataset = None
                result = compare_vector(*paths)
                self.assertFalse(result["same_type"])
                self.assertEqual(conformance.classify_raster(result)[0], conformance.DIFFERS)

    def test_exception_does_not_hide_other_transforms_or_cases(self):
        for transform, allowed in (((0, 1, 0, 3, 0, -1), False), ((2, 1, 0, 3, 0, -1), True)):
            result = self.compare_transforms(None, transform, allow_s5p=allowed)
            self.assertEqual(conformance.classify_raster(result)[0], conformance.DIFFERS)


if __name__ == "__main__":
    unittest.main()
