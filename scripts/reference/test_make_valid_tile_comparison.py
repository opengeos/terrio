"""Controls for strict tile comparisons, including invalid collinear-ring ordering."""

import importlib.util
from pathlib import Path
import unittest

from shapely.geometry import MultiPolygon, Polygon

spec = importlib.util.spec_from_file_location("tile_comparison", Path(__file__).resolve().parents[1] / "compare_make_valid_tiles.py")
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)

WORLD = 20037508.342789244
ZOOM = 1
STEP = 2 * WORLD / (4096 * 2 ** ZOOM)


def ring(points):
    return [(x * STEP - WORLD, WORLD - y * STEP) for x, y in points]


A = ring([(10, 10), (20, 10), (20, 20), (10, 20), (10, 10)])
B = ring([(30, 10), (40, 10), (40, 20), (30, 20), (30, 10)])
LINE = ring([(12, 12), (13, 12), (14, 12), (12, 12)])
HOLE = ring([(15, 15), (17, 15), (17, 17), (15, 17), (15, 15)])


class TileComparisonTests(unittest.TestCase):
    def setUp(self):
        self.reference = MultiPolygon([Polygon(A, [LINE]), Polygon(B)])
        self.reordered = MultiPolygon([Polygon(A), Polygon(B, [LINE])])

    def classified(self, actual):
        return comparison.only_collinear_order_differs([self.reference], [actual], ZOOM)

    def test_reports_only_collinear_association_change(self):
        self.assertFalse(self.reference.is_valid)
        self.assertFalse(self.reordered.is_valid)
        self.assertTrue(comparison.compare({"a": [self.reference]}, {"a": [self.reordered]})[0])
        self.assertEqual(self.classified(self.reordered), 1)

    def test_rejects_missing_or_duplicated_ring(self):
        for actual in [MultiPolygon([Polygon(A), Polygon(B)]), MultiPolygon([Polygon(A), Polygon(B, [LINE, LINE])])]:
            self.assertIsNone(self.classified(actual))

    def test_rejects_changed_winding(self):
        actual = MultiPolygon([Polygon(A), Polygon(B, [LINE[::-1]])])
        self.assertIsNone(self.classified(actual))

    def test_rejects_shifted_ring(self):
        moved = [(x, y + STEP) for x, y in LINE]
        self.assertIsNone(self.classified(MultiPolygon([Polygon(A), Polygon(B, [moved])])))

    def test_rejects_changed_nonzero_hole_association(self):
        reference = MultiPolygon([Polygon(A, [LINE, HOLE]), Polygon(B)])
        actual = MultiPolygon([Polygon(A), Polygon(B, [LINE, HOLE])])
        self.assertIsNone(comparison.only_collinear_order_differs([reference], [actual], ZOOM))

    def test_rejects_non_grid_coordinates(self):
        moved = [(x + STEP / 100, y) for x, y in LINE]
        self.assertIsNone(self.classified(MultiPolygon([Polygon(A), Polygon(B, [moved])])))

    def test_rejects_feature_omission_or_duplication(self):
        self.assertIsNone(comparison.only_collinear_order_differs([self.reference], [], ZOOM))
        self.assertIsNone(comparison.only_collinear_order_differs([self.reference], [self.reordered, self.reordered], ZOOM))

    def test_valid_reference_cannot_use_exception(self):
        self.assertIsNone(comparison.invalid_collinear_order_signature(Polygon(A), ZOOM))

    def test_standard_geometry_controls(self):
        original = Polygon(A)
        shifted = Polygon([(x + STEP, y) for x, y in A])
        self.assertFalse(comparison.compare({"a": [original]}, {"a": [original]})[0])
        self.assertTrue(comparison.compare({"a": [original]}, {"a": [shifted]})[0])
        self.assertTrue(comparison.compare({"a": [original]}, {})[0])
        self.assertTrue(comparison.compare({"a": [original]}, {"a": [Polygon(A, [HOLE])]})[0])


if __name__ == "__main__":
    unittest.main()
