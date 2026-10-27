"""Known integer subtype value differences must not hide other regressions."""

import copy
import sys
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from osgeo import gdal

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance
import compare_vector_dataset_with_gdal as comparison
from compare_vector_dataset_with_gdal import DIFFERS, IDENTICAL, compare_dumps


class IntegerSubtypeWaiverTests(unittest.TestCase):
    def setUp(self):
        self.reference = {
            "layer": {
                "fields": [("i", "Integer"), ("name", "String")],
                "crs": "4326",
                "features": [("POINT (0 0)", [100000, "kept"])],
            }
        }
        self.actual = copy.deepcopy(self.reference)
        self.actual["layer"]["features"][0][1][0] = 32767

    def compare(self):
        return compare_dumps(
            "subtype", self.actual, self.reference, ignore_field_values=("i",)
        )["status"]

    def test_only_expected_value_can_be_waived(self):
        self.assertEqual(self.compare(), IDENTICAL)

    def test_other_field_value_still_fails(self):
        self.actual["layer"]["features"][0][1][1] = "wrong"
        self.assertEqual(self.compare(), DIFFERS)

    def test_geometry_still_fails(self):
        self.actual["layer"]["features"][0] = ("POINT (1 0)", [32767, "kept"])
        self.assertEqual(self.compare(), DIFFERS)

    def test_crs_still_fails(self):
        self.actual["layer"]["crs"] = "3857"
        self.assertEqual(self.compare(), DIFFERS)

    def test_missing_feature_still_fails(self):
        self.actual["layer"]["features"].clear()
        self.assertEqual(self.compare(), DIFFERS)


class DatasetSuitePrerequisiteTests(unittest.TestCase):
    def test_old_gdal_reports_a_skip(self):
        with patch.object(gdal, "VersionInfo", return_value="3120200"):
            result = conformance.suite_vector_dataset(
                SimpleNamespace(data="unused", terrio="unused")
            )
        self.assertIn("GDAL >= 3.13", result.skipped)

    def test_missing_required_fixture_reports_a_skip(self):
        fixtures = (
            "countries.gpkg", "UScounties.gpkg", "multi.gpkg", "TN_Counties.geojson",
            "las_vegas_roads.geojson", "hike_gps_trace.csv",
            "ne_places/ne_10m_populated_places_simple.shp",
        )
        for missing in fixtures:
            with (
                self.subTest(missing=missing),
                tempfile.TemporaryDirectory() as directory,
                patch.object(gdal, "VersionInfo", return_value="3130300"),
                patch.object(conformance.shutil, "which", return_value="/test/tool"),
            ):
                for name in fixtures:
                    if name != missing:
                        fixture = Path(directory, name)
                        fixture.parent.mkdir(parents=True, exist_ok=True)
                        fixture.touch()
                result = conformance.suite_vector_dataset(
                    SimpleNamespace(data=directory, terrio="unused")
                )
            self.assertIn("vector test data", result.skipped)


class ChildTimeoutTests(unittest.TestCase):
    def test_command_timeout_is_a_failure(self):
        with patch.object(comparison.subprocess, "run", side_effect=subprocess.TimeoutExpired("terrio", 1)):
            with self.assertRaisesRegex(RuntimeError, "terrio.*timed out after 1s"):
                comparison.run(["terrio", "create"], timeout=1)

    def test_help_timeout_reports_command_unavailable(self):
        with (
            patch.object(comparison.shutil, "which", return_value="/test/gdal"),
            patch.object(comparison.subprocess, "run", side_effect=subprocess.TimeoutExpired("gdal", 60)),
        ):
            self.assertFalse(comparison.has_gdal_command("vector", "update"))


if __name__ == "__main__":
    unittest.main()
