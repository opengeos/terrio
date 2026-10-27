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
        with patch.object(comparison.subprocess, "run", side_effect=subprocess.TimeoutExpired("ogrlineref", 1)):
            with self.assertRaisesRegex(RuntimeError, "ogrlineref.*timed out after 1s"):
                comparison.run(["ogrlineref", "-get_coord"], timeout=1)

    def test_failed_command_keeps_stderr(self):
        result = subprocess.CompletedProcess(["ogrlineref"], 1, "", "bad parts layer")
        with patch.object(comparison.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(RuntimeError, "bad parts layer"):
                comparison.run(["ogrlineref", "-get_coord"])


if __name__ == "__main__":
    unittest.main()
