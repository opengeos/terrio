"""Georeferencing participates in pixel-command conformance results."""

import sys
import tempfile
import unittest
from pathlib import Path

from osgeo import gdal

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance
from compare_raster_pixel_with_gdal import raster_extra


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

    def test_exception_does_not_hide_other_transforms_or_cases(self):
        for transform, allowed in (((0, 1, 0, 3, 0, -1), False), ((2, 1, 0, 3, 0, -1), True)):
            result = self.compare_transforms(None, transform, allow_s5p=allowed)
            self.assertEqual(conformance.classify_raster(result)[0], conformance.DIFFERS)


if __name__ == "__main__":
    unittest.main()
