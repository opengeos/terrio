"""Failed commands and missing evidence must not pass VSI comparisons."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_vsi_with_gdal as comparison
import conformance


class VsiConformanceTests(unittest.TestCase):
    def test_empty_killed_comparison_is_an_error(self):
        context = SimpleNamespace(data="data", terrio="terrio", remote=False)
        with patch.object(conformance.shutil, "which", return_value="gdal"), patch.object(conformance, "run", return_value=(-9, "", "")):
            suite = conformance.suite_vsi(context)
        self.assertIn("-9", suite.error)
        self.assertFalse(suite.cases)

    def test_zip_transfer_always_records_missing_or_failed_evidence(self):
        for code, stderr, status in [
            (1, "4 HTTP request(s), 32000 bytes", "differs"),
            (0, "no statistics", "differs"),
            (0, "4 HTTP request(s), 32000 bytes", "within tolerance"),
            (0, "4 HTTP request(s), 200000 bytes", "differs"),
        ]:
            with self.subTest(code=code, stderr=stderr), patch.object(comparison, "RESULTS", []) as results:
                comparison.record_zip_transfer(subprocess.CompletedProcess([], code, stderr=stderr))
                self.assertEqual(len(results), 1)
                self.assertEqual(results[0]["status"], status)

    def check_sync_failure(self, exit_code, existing_synced):
        store = comparison.S3()
        for bucket in ("gdal", "terrio"):
            store.objects[f"{bucket}/data/file"] = (b"data", 0)
            if existing_synced:
                store.objects[f"{bucket}/synced/file"] = (b"previous result", 0)

        def injected_command(command, env=None):
            is_sync = "sync" in command
            return subprocess.CompletedProcess(command, exit_code if is_sync else 0, stdout="[]", stderr="injected sync failure")

        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source"
            source.mkdir()
            (source / "file").write_bytes(b"actual source")
            (source / "second").write_bytes(b"second source")
            with (
                patch.object(comparison, "S3", return_value=store),
                patch.object(comparison, "run", side_effect=injected_command),
                patch.object(comparison, "listing", return_value=[]),
                patch.object(comparison, "compare_listing"),
                patch.object(comparison, "compare_trees"),
                patch.object(comparison, "RESULTS", []) as results,
            ):
                comparison.s3_cases("fake-terrio", source, Path(directory))
                sync = next(row for row in results if row["name"] == "S3 sync (timestamp), then again")
                self.assertEqual(sync["status"], "differs")

    def test_failed_sync_cannot_pass_with_matching_old_objects(self):
        self.check_sync_failure(exit_code=1, existing_synced=True)

    def test_success_exit_cannot_pass_with_two_empty_prefixes(self):
        self.check_sync_failure(exit_code=0, existing_synced=False)


if __name__ == "__main__":
    unittest.main()
