"""The analytic Mollweide edge verifier respects false origins and projected units."""

import math
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_ob_tran_with_gdal import inverse_probes, mollweide_longitude, mollweide_roundtrip_error


class MollweideEdgeTests(unittest.TestCase):
    def test_inverse_probes_keep_duplicate_and_standalone_references(self):
        points = [[1, 2], [3, 4], [5, 6]]
        projected = [[10, 20], None, [10, 20]]
        case = {"inverse_samples": [([30, 40], [7, 8]), ([50, 60], None)]}
        self.assertEqual(inverse_probes(points, projected, case), [
            ([10, 20], [1, 2]), ([10, 20], [5, 6]),
            ([30, 40], [7, 8]), ([50, 60], None),
        ])

    def test_standalone_edge_roundtrip_accepts_either_image(self):
        case = {"target": "+proj=ob_tran +o_proj=moll +x_0=500", "metres_per_unit": 1000}
        point = [20.5, 10]
        self.assertEqual(mollweide_roundtrip_error(case, point, point), 0)
        self.assertEqual(mollweide_roundtrip_error(case, point, [-19.5, 10]), 0)
        self.assertGreater(mollweide_roundtrip_error(case, point, [-19.5, 11]), 0.001)

    def test_offsets_and_units_preserve_both_antimeridians(self):
        radius, x0, y0, theta = 6371000, 420123, -912345, 0.37
        for factor in [1, 1000, 1200 / 3937]:
            case = {
                "target": f"+proj=ob_tran +o_proj=moll +x_0={x0} +y_0={y0}",
                "radius_metres": radius,
                "metres_per_unit": factor,
                "false_northing_metres": y0,
            }
            for lam in [-math.pi, math.pi, math.pi + 1e-9]:
                point = (
                    (radius * 2 * math.sqrt(2) / math.pi * lam * math.cos(theta) + x0) / factor,
                    (radius * math.sqrt(2) * math.sin(theta) + y0) / factor,
                )
                self.assertAlmostEqual(mollweide_longitude(case, point), lam, delta=8 * math.ulp(math.pi))


if __name__ == "__main__":
    unittest.main()
