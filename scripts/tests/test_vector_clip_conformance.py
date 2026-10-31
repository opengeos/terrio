"""Clipping tolerances stay relative for small features and older GDAL skips."""
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from osgeo import ogr, osr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_vector_clip_with_gdal as comparison
import conformance


class ClipToleranceTests(unittest.TestCase):
    def check_rejected(self, reference, actual, geometry_type):
        crs = osr.SpatialReference()
        crs.ImportFromEPSG(3857)
        def contents(wkt):
            return {1: ([], [ogr.CreateGeometryFromWkt(wkt)])}, [], crs, geometry_type
        with patch.object(comparison, "contents", side_effect=[contents(reference), contents(actual)]):
            with self.assertRaisesRegex(AssertionError, "relative geometry difference"):
                comparison.compare("reference", "actual")

    def test_small_displaced_polygon_at_large_offset_is_rejected(self):
        self.check_rejected(
            "POLYGON ((4000000 0,4000000.00001 0,4000000.00001 .00001,4000000 .00001,4000000 0))",
            "POLYGON ((4000000.00002 0,4000000.00003 0,4000000.00003 .00001,4000000.00002 .00001,4000000.00002 0))",
            ogr.wkbPolygon,
        )

    def test_small_line_length_change_is_rejected(self):
        self.check_rejected(
            "LINESTRING (4000000 0,4000000 .00001)",
            "LINESTRING (4000000 0,4000000 .0000100001)",
            ogr.wkbLineString,
        )

    def test_missing_clip_command_skips_before_dataset_access(self):
        with (patch.object(conformance, "missing_modules", return_value=False),
              patch.object(conformance.shutil, "which", return_value="gdal"),
              patch.object(conformance, "run", return_value=(1, "", "unknown command"))):
            result = conformance.suite_vector_clip(SimpleNamespace(data="missing"))
        self.assertIn("vector clip command", result.skipped)
        self.assertFalse(result.cases)


if __name__ == "__main__":
    unittest.main()
