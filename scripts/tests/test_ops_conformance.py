"""Known area-weight differences remain visible failures in conformance reports."""

import importlib.util
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


class VectorOpsReportTests(unittest.TestCase):
    """Keep failed or interrupted vector comparisons visible in reports."""

    def test_subprocess_failure_cannot_hide_behind_completed_cases(self):
        """Distinguish unexpected exits, reported differences and successful runs."""
        for code, status, stderr, completed, expected_error in [
            (1, "OK", "unexpected comparison failure", False, True),
            (1, "DIFF CRS differs", "unexpected comparison failure", False, True),
            (1, "DIFF CRS differs", "", True, False),
            (0, "OK", "", True, False),
            (0, "OK", "", False, True),
            (1, "OK", "Warning 1: ignored warning", False, True),
        ]:
            with self.subTest(code=code, status=status):
                output = f"## rasterize background vs gdal_rasterize\ncase  {status}\n"
                if completed:
                    output += "# vector comparisons complete\n"
                with patch.object(conformance, "run", return_value=(code, output, stderr)):
                    result = conformance.suite_vector_ops(SimpleNamespace(data="unused", terrio="unused"))
                self.assertEqual(len(result.cases), 1)
                self.assertEqual(bool(result.error), expected_error)
                if expected_error and not stderr.startswith("Warning 1:"):
                    self.assertIn(stderr, result.error)
                if stderr.startswith("Warning 1:"):
                    self.assertEqual(result.error, "Vector comparison subprocess did not complete")

    @unittest.skipUnless(importlib.util.find_spec("osgeo"), "GDAL Python bindings are unavailable")
    def test_missing_output_crs_is_reported_as_a_difference(self):
        """Compare real GDAL datasets with matching values but a missing output CRS."""
        import compare_vector_ops_with_gdal as comparison
        from osgeo import gdal, osr

        driver = gdal.GetDriverByName("MEM")
        background, expected, actual = [driver.Create("", 1, 1, 1, gdal.GDT_Byte) for _ in range(3)]
        crs = osr.SpatialReference()
        crs.ImportFromEPSG(5070)
        for dataset in (background, expected, actual):
            dataset.SetGeoTransform((0, 1, 0, 1, 0, -1))
            dataset.GetRasterBand(1).Fill(1 if dataset is background else 2)
        background.SetSpatialRef(crs)
        expected.SetSpatialRef(crs)
        with (
            patch.object(comparison, "run"),
            patch.object(comparison.shutil, "copy"),
            patch.object(comparison.gdal, "Open", side_effect=[expected, actual, background] * 3),
        ):
            rows = comparison.compare_rasterize_background(Path("unused"), Path("unused"), Path("terrio"))
        self.assertEqual(len(rows), 3)
        self.assertTrue(all("DIFF grid or CRS differs" in row for row in rows), rows)


if __name__ == "__main__":
    unittest.main()
