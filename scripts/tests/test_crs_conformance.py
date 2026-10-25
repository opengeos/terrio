"""Cargo failures must remain visible in the projection conformance report."""

import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance


class ProjectionProcessFailureTests(unittest.TestCase):
    def test_unlisted_failure_is_not_hidden_by_listed_successes(self):
        registry = {"sample": {"registered": "registered comparison"}}
        output = "test registered ... ok\ntest unregistered ... FAILED\n"
        suite = conformance.Suite("sample", "sample", "PROJ", "test")
        with (
            patch.object(conformance, "PROJ_CASES", registry),
            patch.object(conformance, "run", return_value=(101, output, "test failure")),
        ):
            conformance.proj_cases(suite)
        self.assertTrue(any(case.status == conformance.DIFFERS for case in suite.cases))
        self.assertIn("Cargo test process", suite.cases[-1].name)

    def test_standalone_comparisons_keep_terrio_projection_coverage(self):
        suite = conformance.Suite("sample", "sample", "PROJ", "test")
        with (
            patch.object(conformance, "PROJ_CASES", {"sample": {}, "geostationary": {}}),
            patch.object(conformance, "run", return_value=(0, "", "")) as run,
        ):
            conformance.proj_cases(suite)
        core_command = run.call_args_list[0].args[0]
        self.assertIn("wbprojection-fallback", core_command)
        self.assertIn("terrio-crs", core_command)
        terrio_command = run.call_args_list[1].args[0]
        self.assertIn("terrio", terrio_command)
        self.assertNotIn("wbprojection-fallback", terrio_command)


if __name__ == "__main__":
    unittest.main()
