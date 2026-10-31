"""The ordering verifier rejects wrong greedy owners, gaps, leaks and overlaps."""

import sys
import tempfile
import unittest
from pathlib import Path

from osgeo import ogr, osr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_coverage_clean_with_gdal as comparison
from compare_coverage_with_gdal import rectangle, run, write


class CoverageCleanComparisonTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.crs = osr.SpatialReference()
        self.crs.ImportFromEPSG(5070)
        self.source = self.root / "input.gpkg"
        write(self.source, [rectangle(0, 0, 3, 2), rectangle(2, 0, 4, 2)], self.crs)

    def output(self, shapes):
        path = self.root / "actual.gpkg"
        path.unlink(missing_ok=True)
        write(path, shapes, self.crs)
        return path

    def test_gdal_result_has_a_complete_legal_order(self):
        output = self.root / "gdal.geojson"
        run(["gdal", "vector", "clean-coverage", self.source, output])
        self.assertEqual(
            len(comparison.ordering_witness(self.source, output, "longest-border")), 1
        )

    def test_a_valid_coverage_with_the_wrong_strategy_is_rejected(self):
        output = self.output([rectangle(0, 0, 2, 2), rectangle(2, 0, 4, 2)])
        # Smaller-area merging assigns this overlap east, but the longest-border
        # tie must choose the west input. Merely being a valid partition is insufficient.
        self.assertEqual(
            len(comparison.ordering_witness(self.source, output, "min-area")), 1
        )
        with self.assertRaisesRegex(AssertionError, "no legal progressive merge order"):
            comparison.ordering_witness(self.source, output, "longest-border")

    def test_missing_area_and_new_area_are_rejected(self):
        for west, east in [
            (rectangle(0, 0, 1, 2), rectangle(2, 0, 4, 2)),
            (rectangle(0, 0, 2, 2), rectangle(2, 0, 5, 2)),
        ]:
            with self.assertRaisesRegex(AssertionError, "changed the input XY union"):
                comparison.ordering_witness(
                    self.source, self.output([west, east]), "longest-border"
                )

    def test_overlaps_and_non_parent_owners_are_rejected(self):
        output = self.output([rectangle(0, 0, 3, 2), rectangle(2, 0, 4, 2)])
        with self.assertRaisesRegex(AssertionError, "no unique output owner"):
            comparison.ordering_witness(self.source, output, "longest-border")
        output = self.output([ogr.Geometry(ogr.wkbPolygon), rectangle(0, 0, 4, 2)])
        with self.assertRaisesRegex(AssertionError, "outside its input parents"):
            comparison.ordering_witness(self.source, output, "longest-border")


if __name__ == "__main__":
    unittest.main()
