"""Ensure the algebra comparer rejects lost geometry, values and dimensions."""

import sys
import json
from types import SimpleNamespace
from unittest.mock import patch
import tempfile
import subprocess
import unittest
from pathlib import Path

from osgeo import ogr, osr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_layer_algebra_with_gdal import compare
import conformance


class LayerAlgebraComparisonTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def write(self, name, wkt, value=1):
        path = self.root / name
        crs = osr.SpatialReference()
        crs.ImportFromEPSG(5070)
        ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(path))
        layer = ds.CreateLayer("result", crs, geom_type=ogr.wkbUnknown)
        layer.CreateField(ogr.FieldDefn("value", ogr.OFTInteger64))
        f = ogr.Feature(layer.GetLayerDefn())
        f["value"] = value
        f.SetGeometry(ogr.CreateGeometryFromWkt(wkt))
        layer.CreateFeature(f)
        ds = None
        return path

    def test_identical_geometry_and_reordered_components(self):
        a = self.write("a.gpkg", "MULTIPOINT ((1 1),(2 2))")
        b = self.write("b.gpkg", "MULTIPOINT ((2 2),(1 1))")
        self.assertEqual(compare(a, b), (True, 1))

    def test_wrong_attributes_are_rejected(self):
        a = self.write("a.gpkg", "POINT (1 1)", 1)
        b = self.write("b.gpkg", "POINT (1 1)", 2)
        with self.assertRaisesRegex(AssertionError, "attributes differ"):
            compare(a, b)

    def test_missing_collection_line_is_not_hidden_by_area_agreement(self):
        a = self.write(
            "a.gpkg", "GEOMETRYCOLLECTION (POLYGON ((0 0,2 0,2 2,0 2,0 0)),POINT (4 4))"
        )
        b = self.write(
            "b.gpkg",
            "GEOMETRYCOLLECTION (POLYGON ((0 0,2 0,2 2,0 2,0 0)),LINESTRING (3 0,3 2),POINT (4 4))",
        )
        with self.assertRaisesRegex(AssertionError, "lost dimension 1"):
            compare(a, b)

    def test_missing_line_length_and_wrong_point_are_rejected(self):
        for i, (a_wkt, b_wkt, message) in enumerate(
            [
                ("LINESTRING (0 0,1 1)", "LINESTRING (0 0,2 2)", "length difference"),
                ("POINT (0 0)", "POINT (1 1)", "point position"),
            ]
        ):
            a = self.write(f"a{i}.gpkg", a_wkt)
            b = self.write(f"b{i}.gpkg", b_wkt)
            with self.assertRaisesRegex(AssertionError, message):
                compare(a, b)

    def test_tiny_disjoint_polygons_are_rejected(self):
        a = self.write(
            "tiny-a.gpkg", "POLYGON ((0 0,0.0001 0,0.0001 0.0001,0 0.0001,0 0))"
        )
        b = self.write(
            "tiny-b.gpkg", "POLYGON ((1 1,1.0001 1,1.0001 1.0001,1 1.0001,1 1))"
        )
        with self.assertRaisesRegex(AssertionError, "polygon area difference"):
            compare(a, b)

    def test_equal_length_networks_with_the_same_vertices_are_rejected(self):
        a = self.write("network-a.gpkg", "MULTILINESTRING ((0 0,1 0),(0 1,1 1))")
        b = self.write("network-b.gpkg", "MULTILINESTRING ((0 0,0 1),(1 0,1 1))")
        with self.assertRaisesRegex(AssertionError, "line edges differ"):
            compare(a, b)

    def test_optimized_python_still_rejects_wrong_attributes(self):
        a = self.write("optimized-a.gpkg", "POINT (1 1)", 1)
        b = self.write("optimized-b.gpkg", "POINT (1 1)", 2)
        script = "from compare_layer_algebra_with_gdal import compare; import sys; compare(sys.argv[1], sys.argv[2])"
        process = subprocess.run(
            [sys.executable, "-O", "-c", script, str(a), str(b)],
            cwd=Path(__file__).resolve().parents[1],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("attributes differ", process.stderr)

    def test_missing_geos_skips_the_reference(self):
        ctx = SimpleNamespace(data=str(self.root), terrio="terrio")
        with (
            patch.object(conformance, "missing_modules", return_value=[]),
            patch.object(ogr.Geometry, "Buffer", return_value=None),
        ):
            suite = conformance.suite_layer_algebra(ctx)
        self.assertEqual(suite.skipped, "needs GDAL with GEOS")
        self.assertEqual(suite.cases, [])

    def test_reported_comparison_failures_are_counted_as_differences(self):
        ctx = SimpleNamespace(data=str(self.root), terrio="terrio")
        rows = [
            {"name": "lost geometry", "status": "differs", "detail": "missing polygon"}
        ]
        with (
            patch.object(conformance, "missing_modules", return_value=[]),
            patch.object(conformance.os.path, "isfile", return_value=True),
            patch.object(conformance, "run", return_value=(1, json.dumps(rows), "")),
        ):
            suite = conformance.suite_layer_algebra(ctx)
        self.assertEqual(suite.error, "")
        self.assertEqual(len(suite.cases), 1)
        self.assertEqual(suite.cases[0].status, conformance.DIFFERS)


if __name__ == "__main__":
    unittest.main()
