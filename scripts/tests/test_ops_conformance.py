"""Known area-weight differences remain visible failures in conformance reports."""

import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance


class OpsDifferenceTests(unittest.TestCase):
    def result(self, name, **changes):
        result = dict(
            name=name,
            same_size=True,
            same_type=True,
            same_crs=True,
            valid_mismatch=0,
            differing=5,
            exact=0.5,
            within_tol=0.8,
            max_diff=3.0,
        )
        result.update(changes)
        return result

    def report(self, results):
        with patch.object(
            conformance, "run", return_value=(0, json.dumps(results), "")
        ):
            return conformance.suite_ops(
                SimpleNamespace(data="unused", terrio="unused")
            )

    def test_known_pixel_differences_keep_their_failing_status(self):
        names = [
            "warp DEM NAD83->UTM average",
            "warp DEM NAD83->UTM rms",
            "warp DEM ->3857 tr 100 tap average (-ovr NONE)",
        ]
        for case in self.report([self.result(name) for name in names]).cases:
            self.assertEqual(case.status, conformance.DIFFERS)
            self.assertIn("#703", case.detail)
            self.assertIn("5 pixels differ", case.detail)
            self.assertIn("max difference 3", case.detail)

    def test_other_failures_are_not_labelled_as_edge_weight_differences(self):
        name = "warp DEM NAD83->UTM average"
        cases = self.report(
            [
                self.result(name, same_type=False),
                self.result(name, valid_mismatch=1),
                self.result("warp unrelated average"),
            ]
        ).cases
        for case in cases:
            self.assertEqual(case.status, conformance.DIFFERS)
            self.assertNotIn("#703", case.detail)

    def test_passing_comparisons_keep_their_status(self):
        name = "warp DEM NAD83->UTM average"
        cases = self.report(
            [self.result(name, differing=0), self.result(name, within_tol=1.0)]
        ).cases
        self.assertEqual(
            [case.status for case in cases],
            [conformance.IDENTICAL, conformance.TOLERANCE],
        )
        self.assertTrue(all("#703" not in case.detail for case in cases))


if __name__ == "__main__":
    unittest.main()
