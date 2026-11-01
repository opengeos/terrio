"""Coverage simplification comparisons never waive declaration or coordinate failures."""

import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from osgeo import ogr, osr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_coverage_simplify_with_gdal as comparison
import conformance


class CoverageSimplificationComparisonTests(unittest.TestCase):
    def record(
        self, wkt="POLYGON ((0 0,1 0,1 1,0 0))", *, declared=0, fid=1, label="kept"
    ):
        geometry = ogr.CreateGeometryFromWkt(wkt)
        crs = osr.SpatialReference()
        crs.ImportFromEPSG(5070)
        kind = ogr.GT_Flatten(geometry.GetGeometryType())
        fingerprint = (
            sorted(comparison.polygon_edges(geometry))
            if kind in (ogr.wkbPolygon, ogr.wkbMultiPolygon)
            else bytes(geometry.ExportToIsoWkb())
        )
        return {
            "coverage": (
                [(fid, {"label": label}, kind, fingerprint)],
                [("label", 4, 0)],
                crs,
                declared,
            )
        }

    def compare(self, a, b):
        with patch.object(comparison, "read", side_effect=[a, b]):
            return comparison.compare("reference", "actual")

    def test_reversed_rings_keep_the_same_edge_multiset(self):
        self.assertIn(
            "exact retained edges",
            self.compare(self.record(), self.record("POLYGON ((0 0,1 1,1 0,0 0))")),
        )

    def test_unknown_declaration_cannot_silently_become_polygon(self):
        with self.assertRaisesRegex(AssertionError, "declared layer type differs"):
            self.compare(self.record(declared=0), self.record(declared=3))

    def test_optional_dimensional_flags_cannot_silently_become_required(self):
        with self.assertRaisesRegex(AssertionError, "declared layer type differs"):
            self.compare(
                self.record(declared=(0, ("GEOMETRY", 2, 0))),
                self.record(declared=(0, ("GEOMETRY", 1, 0))),
            )

    def test_measured_coordinates_are_checked(self):
        with self.assertRaises(AssertionError):
            self.compare(
                self.record("POLYGON M ((0 0 10,1 0 20,1 1 30,0 0 10))"),
                self.record("POLYGON M ((0 0 10,1 0 21,1 1 30,0 0 10))"),
            )

    def test_moved_vertices_and_changed_feature_geometry_types_fail(self):
        for other in [
            self.record("POLYGON ((0 0,1 0,1 1.00000001,0 0))"),
            self.record("MULTIPOLYGON (((0 0,1 0,1 1,0 0)))"),
        ]:
            with self.assertRaises(AssertionError):
                self.compare(self.record(), other)

    def test_attributes_and_ids_are_checked(self):
        for other in [self.record(label="wrong"), self.record(fid=2)]:
            with self.assertRaisesRegex(AssertionError, "ID/attributes"):
                self.compare(self.record(), other)

    def test_inactive_point_coordinates_are_checked(self):
        with self.assertRaisesRegex(AssertionError, "inactive geometry coordinates"):
            self.compare(
                self.record("POINT (1 2)", declared=1),
                self.record("POINT (1 2.0000001)", declared=1),
            )

    def test_unavailable_gdal_command_skips_before_data_access(self):
        with (
            patch.object(conformance, "missing_modules", return_value=False),
            patch.object(conformance.shutil, "which", return_value="gdal"),
            patch.object(conformance, "run", return_value=(1, "", "unknown command")),
        ):
            result = conformance.suite_coverage_simplify(
                SimpleNamespace(data="missing")
            )
        self.assertIn("simplify-coverage", result.skipped)
        self.assertFalse(result.cases)


if __name__ == "__main__":
    unittest.main()
