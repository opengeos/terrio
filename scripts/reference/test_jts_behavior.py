"""Controls for exact comparison of invalid executable-reference hull output."""

from pathlib import Path
import sys
from types import SimpleNamespace
import unittest

from shapely.geometry import GeometryCollection, MultiPolygon, Point, Polygon
from shapely import union_all

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_jts_behavior import compare  # noqa: E402
from compare_vector_geom_with_gdal import Geos, valid_str_order  # noqa: E402


class ReferenceHullComparison(unittest.TestCase):
    def setUp(self):
        # Captured GEOS non-tight contact behavior: the first shell covers
        # the other polygon. Its invalid component structure must be preserved.
        self.parts = [
            Polygon([(0, 0), (6, 0), (6, 2), (2, 6), (0, 6)]),
            Polygon([(2, 2), (4, 3), (3, 4)]),
        ]
        self.expected = MultiPolygon(self.parts)
        self.case = {"operation": "polygon_hull"}
        self.assertFalse(self.expected.is_valid)

    def status(self, actual):
        return compare(self.case, actual.wkb_hex, self.expected.wkb_hex)[0]

    def test_ring_direction_and_component_order_are_normalized(self):
        parts = []
        for polygon in reversed(self.parts):
            points = list(polygon.exterior.coords)[:-1]
            points = points[2:] + points[:2]
            parts.append(Polygon(points[::-1]))
        actual = MultiPolygon(parts)
        self.assertEqual(self.status(actual), "reference-invalid-exact")

    def test_geometrically_equal_union_is_not_component_equal(self):
        self.assertEqual(self.status(union_all(self.parts)), "differs")

    def test_missing_or_duplicate_component_is_rejected(self):
        for parts in [self.parts[:1], self.parts + self.parts[:1]]:
            with self.subTest(count=len(parts)):
                self.assertEqual(self.status(MultiPolygon(parts)), "differs")

    def test_coordinate_changes_have_no_tolerance(self):
        changed = Polygon([(2, 2), (4 + 1e-12, 3), (3, 4)])
        self.assertEqual(self.status(MultiPolygon([self.parts[0], changed])), "differs")

    def test_reconnected_vertices_are_rejected(self):
        changed = Polygon([(0, 0), (6, 2), (6, 0), (2, 6), (0, 6)])
        self.assertEqual(self.status(MultiPolygon([changed, self.parts[1]])), "differs")

    def test_null_reference_result_is_not_serialized(self):
        reference = Geos.__new__(Geos)
        reference.h = None
        reference.read = lambda _: 1
        reference.lib = SimpleNamespace(
            GEOSConcaveHullOfPolygons_r=lambda *_: None,
            GEOSGeom_destroy_r=lambda *_: None,
        )
        reference.write = lambda _: self.fail("must not serialize a null GEOS result")
        with self.assertRaisesRegex(RuntimeError, "returned no geometry"):
            reference.concave_hull_of_polygons(None, 0.3, True, True)


class StrOrderComparison(unittest.TestCase):
    def test_null_envelopes_must_follow_non_null_envelopes(self):
        envelopes = [None, (0, 0, 0, 0), (1, 1, 1, 1), None]
        self.assertTrue(valid_str_order([1, 2, 0, 3], envelopes))
        for order in [[0, 1, 2, 3], [1, 0, 2, 3]]:
            with self.subTest(order=order):
                self.assertFalse(valid_str_order(order, envelopes))
        self.assertTrue(valid_str_order([0, 3], envelopes))
        self.assertFalse(valid_str_order([0, 1, 3], envelopes))

    def test_reversed_extreme_x_slices_are_not_ties(self):
        points = [Point(1e308 + i * 1e306, i) for i in range(24)]
        case = {"operation": "strtree", "input": GeometryCollection(points).wkb_hex}
        expected = list(range(24))
        reversed_slices = expected[12:] + expected[:12]
        self.assertEqual(compare(case, reversed_slices, expected)[0], "differs")

    def test_reversed_extreme_y_order_is_rejected(self):
        envelopes = [(0, 0, 1e308 + i * 1e306, 1e308 + i * 1e306) for i in range(10)]
        self.assertTrue(valid_str_order(list(range(10)), envelopes))
        self.assertFalse(valid_str_order(list(reversed(range(10))), envelopes))

    def test_subnormal_x_slices_remain_distinct(self):
        tiny = float.fromhex("0x0.0000000000001p-1022")
        envelopes = [(x, x, 0, 0) for x in [0.] * 12 + [tiny] * 12]
        expected = list(range(24))
        self.assertTrue(valid_str_order(expected, envelopes))
        self.assertFalse(valid_str_order(expected[12:] + expected[:12], envelopes))


if __name__ == "__main__":
    unittest.main()
