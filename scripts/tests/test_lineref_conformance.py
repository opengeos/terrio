"""Child failures retain useful linear-reference comparison diagnostics."""

import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_lineref_with_gdal as comparison


class LinerefCommandTests(unittest.TestCase):
    def test_timeout_becomes_named_runtime_failure(self):
        with patch.object(
            comparison.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired("ogrlineref", 1),
        ):
            with self.assertRaisesRegex(RuntimeError, "ogrlineref.*timed out after 1s"):
                comparison.run(["ogrlineref", "-get_coord"], timeout=1)

    def test_failed_command_keeps_stderr(self):
        result = subprocess.CompletedProcess(["ogrlineref"], 1, "", "bad parts layer")
        with patch.object(comparison.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(RuntimeError, "bad parts layer"):
                comparison.run(["ogrlineref", "-get_coord"])

    def test_path_arguments_keep_failure_diagnostics(self):
        result = subprocess.CompletedProcess(
            [Path("ogrlineref")], 1, "", "bad parts layer"
        )
        with patch.object(comparison.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(RuntimeError, "ogrlineref input.gpkg"):
                comparison.run([Path("ogrlineref"), Path("input.gpkg")])

    def test_nonfinite_fields_and_vertices_cannot_pass_as_finite(self):
        for a, b, expected in [
            (float("nan"), 1.0, "differs"),
            (1.0, float("nan"), "differs"),
            (float("nan"), None, "differs"),
            (float("inf"), float("-inf"), "differs"),
            (float("nan"), float("nan"), "identical"),
            (float("inf"), float("inf"), "identical"),
        ]:
            for field in [True, False]:
                left = [({"value": a}, [(0.0, 0.0)])] if field else [({}, [(a, 0.0)])]
                right = [({"value": b}, [(0.0, 0.0)])] if field else [({}, [(b, 0.0)])]
                with (
                    self.subTest(a=a, b=b, field=field),
                    patch.object(comparison, "features", side_effect=[left, right]),
                ):
                    self.assertEqual(
                        comparison.compare_layers("gdal", "terrio", 1e-9)[0], expected
                    )


if __name__ == "__main__":
    unittest.main()
