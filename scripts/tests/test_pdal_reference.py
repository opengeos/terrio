"""Diagnostics and optional-codec policy for point cloud reference comparisons."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import pdal_reference


class PdalReferenceTests(unittest.TestCase):
    def setUp(self):
        pdal_reference.ept_codec_available.cache_clear()

    def test_invalid_utf8_preserves_process_failure_and_diagnostic(self):
        result = subprocess.CompletedProcess(["pdal"], 7, b"", b"Unrecognized EPT dataType: \x8e\n")
        with patch.object(pdal_reference.subprocess, "run", return_value=result):
            with self.assertRaises(subprocess.CalledProcessError) as caught:
                pdal_reference.run_pipeline("pdal", [])
        self.assertEqual(caught.exception.returncode, 7)
        self.assertIn("Unrecognized EPT dataType", caught.exception.stderr)
        self.assertIn("\ufffd", caught.exception.stderr)
        self.assertIn("Unrecognized EPT dataType", str(caught.exception))

    def test_only_known_fixture_unsupported_zstandard_is_skipped(self):
        with tempfile.TemporaryDirectory() as directory:
            reference = Path(directory) / "ept.json"
            reference.write_text("{}")
            unsupported = subprocess.CalledProcessError(1, ["pdal"], stderr="Unrecognized EPT dataType")
            with patch.object(pdal_reference, "run_pipeline", side_effect=unsupported) as run:
                self.assertFalse(pdal_reference.ept_codec_available("pdal", reference, "zstandard"))
                self.assertFalse(pdal_reference.ept_codec_available("pdal", reference, "zstandard"))
                self.assertEqual(run.call_count, 1)
                self.assertTrue(pdal_reference.ept_codec_available("pdal", reference, "binary"))
                self.assertTrue(pdal_reference.ept_codec_available("pdal", reference, "laszip"))
                self.assertEqual(run.call_count, 1)

    def test_corrupt_reference_is_not_mislabeled_unsupported(self):
        with tempfile.TemporaryDirectory() as directory:
            reference = Path(directory) / "ept.json"
            reference.write_text("{}")
            corrupt = subprocess.CalledProcessError(1, ["pdal"], stderr="corrupt Zstandard tile")
            with patch.object(pdal_reference, "run_pipeline", side_effect=corrupt):
                with self.assertRaises(subprocess.CalledProcessError):
                    pdal_reference.ept_codec_available("pdal", reference, "zstandard")
        with self.assertRaises(FileNotFoundError):
            pdal_reference.ept_codec_available("pdal", Path("missing-ept-fixture.json"), "zstandard")

    def test_successful_reference_enables_output_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            reference = Path(directory) / "ept.json"
            reference.write_text("{}")
            with patch.object(pdal_reference, "run_pipeline", return_value=subprocess.CompletedProcess([], 0)):
                self.assertTrue(pdal_reference.ept_codec_available("pdal", reference, "zstandard"))


if __name__ == "__main__":
    unittest.main()
