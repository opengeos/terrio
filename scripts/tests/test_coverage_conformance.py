"""Exact coverage-edge comparison ignores order but never coordinate or schema changes."""
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from osgeo import ogr, osr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_coverage_with_gdal as comparison
import conformance


class CoverageComparisonTests(unittest.TestCase):
    def record(self, wkt, *, schema=None, fid=1):
        crs = osr.SpatialReference()
        crs.ImportFromEPSG(5070)
        return ({fid: (fid, {}, ogr.CreateGeometryFromWkt(wkt))}, schema or [], crs, ogr.wkbMultiLineString, "invalid_edge")

    def compare(self, a, b):
        with patch.object(comparison, "read", side_effect=[a, b]):
            return comparison.compare("reference", "actual")

    def test_line_order_and_direction_do_not_change_the_edge_multiset(self):
        a = self.record("MULTILINESTRING ((0 0,1 1,2 2),(4 4,5 5))")
        b = self.record("MULTILINESTRING ((5 5,4 4),(2 2,1 1,0 0))")
        self.assertIn("exact edge sets", self.compare(a, b))

    def test_shifted_edges_and_duplicate_edges_are_rejected(self):
        a = self.record("MULTILINESTRING ((0 0,1 1))")
        for wkt in ["MULTILINESTRING ((0 0,1 1.00000001))", "MULTILINESTRING ((0 0,1 1),(0 0,1 1))"]:
            with self.assertRaisesRegex(AssertionError, "invalid edge set differs"):
                self.compare(a, self.record(wkt))

    def test_ids_and_schema_are_checked(self):
        a = self.record("MULTILINESTRING ((0 0,1 1))")
        for b in [self.record("MULTILINESTRING ((0 0,1 1))", fid=2), self.record("MULTILINESTRING ((0 0,1 1))", schema=[("extra", 0, 0)])]:
            with self.assertRaisesRegex(AssertionError, "schema/CRS/type/name differs"):
                self.compare(a, b)

    def test_old_gdal_command_skips_before_dataset_access(self):
        with (patch.object(conformance, "missing_modules", return_value=False),
              patch.object(conformance.shutil, "which", return_value="gdal"),
              patch.object(conformance, "run", return_value=(1, "", "unknown command"))):
            result = conformance.suite_coverage(SimpleNamespace(data="missing"))
        self.assertIn("check-coverage", result.skipped)
        self.assertFalse(result.cases)


if __name__ == "__main__":
    unittest.main()
