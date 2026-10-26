"""Known integer subtype value differences must not hide other regressions."""

import copy
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from osgeo import gdal

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance
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

    def test_missing_roads_reports_a_skip(self):
        with tempfile.TemporaryDirectory() as directory:
            for name in [
                "countries.gpkg",
                "UScounties.gpkg",
                "multi.gpkg",
                "TN_Counties.geojson",
            ]:
                Path(directory, name).touch()
            result = conformance.suite_vector_dataset(
                SimpleNamespace(data=directory, terrio="unused")
            )
        self.assertIn("vector test data", result.skipped)


if __name__ == "__main__":
    unittest.main()
