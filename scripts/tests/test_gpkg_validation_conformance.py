"""Execution errors and missing requirement diagnostics cannot pass validation comparisons."""

import contextlib
import io
import json
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
try:
    import compare_gpkg_validation_with_gdal as comparison
except ModuleNotFoundError:
    comparison = None


@unittest.skipIf(comparison is None, "needs osgeo")
class GpkgValidationConformanceTests(unittest.TestCase):
    def compare(self, reference, requirements):
        report = dict(valid=False, complete=True, errors=[dict(requirement=n) for n in requirements])
        ours = subprocess.CompletedProcess([], 1, stdout=json.dumps(report), stderr="")
        output = io.StringIO()
        with (
            patch.object(sys, "argv", ["compare", "unused", "--json"]),
            patch.object(comparison, "fixtures", return_value=[("bad fixture", Path("bad.gpkg"), False)]),
            patch.object(comparison.subprocess, "run", side_effect=[reference, ours, reference, ours]),
            contextlib.redirect_stdout(output),
        ):
            code = comparison.main()
        return code, json.loads(output.getvalue())

    def test_reference_execution_error_is_not_a_matching_invalid_verdict(self):
        reference = subprocess.CompletedProcess([], 1, stdout="", stderr="cannot open input")
        code, rows = self.compare(reference, [19])
        self.assertEqual(code, 0)
        self.assertTrue(all(row["status"] == "not compared" for row in rows))

    def test_matching_invalid_verdict_requires_reference_requirement(self):
        reference = subprocess.CompletedProcess([], 1, stdout="Req 19: bad payload\nValidation failed", stderr="")
        code, rows = self.compare(reference, [20])
        self.assertEqual(code, 1)
        self.assertTrue(all(row["status"] == "differs" for row in rows))
        code, rows = self.compare(reference, [19, 20])
        self.assertEqual(code, 0)
        self.assertTrue(all(row["status"] == "identical" for row in rows))


if __name__ == "__main__":
    unittest.main()
