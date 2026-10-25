"""CAD comparison prerequisites must be skipped without launching a broken subprocess."""

import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance


class CadPrerequisiteTests(unittest.TestCase):
    def test_missing_optional_modules_skip_comparison(self):
        for module in ("osgeo", "numpy", "shapely"):
            with (
                self.subTest(module=module),
                patch.object(conformance, "missing_modules", return_value=[module]),
                patch.object(conformance, "run") as run,
            ):
                suite = conformance.suite_cad(
                    SimpleNamespace(data="unused", terrio="unused")
                )
                self.assertIn(module, suite.skipped)
                self.assertFalse(suite.error)
                run.assert_not_called()

    def make_fixtures(self, root):
        paths = [root / "las_vegas_roads.geojson"] + [
            root / "cad" / name
            for name in (
                "3dface.dxf",
                "LWPOLYLINE-OCS.dxf",
                "circle.dxf",
                "closed_polyline_with_bulge.dxf",
                "solid-vertex-ordering.dxf",
                "hatch_as_multipolygon.dxf",
                "bin_dxf_r2000.dxf",
                "block-basepoint.dxf",
                "Line.dwg",
                "Point.dwg",
            )
        ]
        (root / "cad").mkdir()
        for path in paths:
            path.touch()
        return paths

    def test_each_missing_fixture_skips_comparison(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(conformance, "missing_modules", return_value=[]),
            patch.object(conformance, "run") as run,
        ):
            paths = self.make_fixtures(Path(directory))
            for path in paths:
                with self.subTest(missing=path.name):
                    path.unlink()
                    suite = conformance.suite_cad(
                        SimpleNamespace(data=directory, terrio="unused")
                    )
                    self.assertIn("fetch_test_data.sh", suite.skipped)
                    self.assertFalse(suite.error)
                    run.assert_not_called()
                    path.touch()

    def test_available_prerequisites_run_comparison(self):
        payload = [{"name": "DXF sample", "status": "identical", "detail": ""}]
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(conformance, "missing_modules", return_value=[]),
            patch.object(
                conformance, "run", return_value=(0, json.dumps(payload), "")
            ) as run,
        ):
            self.make_fixtures(Path(directory))
            suite = conformance.suite_cad(
                SimpleNamespace(data=directory, terrio="terrio")
            )
            self.assertFalse(suite.skipped)
            self.assertFalse(suite.error)
            self.assertEqual(len(suite.cases), 1)
            run.assert_called_once()


if __name__ == "__main__":
    unittest.main()
