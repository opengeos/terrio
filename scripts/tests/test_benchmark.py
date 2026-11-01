"""Regression tests for benchmark scoring; run with unittest discover -s scripts/tests."""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import benchmark


class BenchmarkTests(unittest.TestCase):
    def test_warp_commands_match_reference_kernel(self):
        with (
            patch.object(benchmark, "gdal_kernel_version", return_value="3.13"),
            patch.object(benchmark.os.path, "exists", return_value=True),
        ):
            cases = benchmark.cases("data", "out", 2) + benchmark.large_cases(
                "large", "out", 2
            )
        warps = [t for _, _, t, _ in cases if "warp" in t]
        self.assertTrue(warps)
        for command in warps:
            self.assertEqual(command[command.index("--kernel-version") + 1], "3.13")

    def test_terrain_and_cog_commands_match_reference_version(self):
        for version in ["3.12", "3.13"]:
            with patch.object(benchmark, "gdal_kernel_version", return_value=version):
                self.assertEqual(benchmark.terrio_dem("terrio", "slope")[-1], version)
                self.assertEqual("COG_OVERVIEW_POLICY=GDAL_3_13" in benchmark.terrio_cog("terrio"), version == "3.13")
                self.assertIn("RESAMPLING=AVERAGE", benchmark.terrio_cog("terrio"))

    def test_successful_but_wrong_output_is_not_scored(self):
        with tempfile.TemporaryDirectory() as root:

            def commands(out, threads):
                return tuple(
                    [
                        sys.executable,
                        "-c",
                        f"from pathlib import Path; Path({str(Path(out) / 'value')!r}).write_text({v!r})",
                    ]
                    for v in ("right", "wrong")
                )

            def validate(a, b):
                return [] if (Path(a) / "value").read_text() == (Path(b) / "value").read_text() else ["values differ"]

            result = benchmark.measure_pair(commands, root, [1], 3, 1, os.environ.copy(), validate)
            self.assertIsNone(result["ratio"])
            self.assertIn("values differ", result["validation"])
            self.assertEqual(len(result["samples"][0]), 3)

    def test_interleaves_and_cleans_each_run(self):
        with tempfile.TemporaryDirectory() as root:
            log = Path(root) / "order"

            def commands(out, threads):
                # A stale output would fail even though the command normally succeeds.
                code = "from pathlib import Path; p=Path({!r}); assert not p.exists(); p.write_text('ok'); open({!r}, 'a').write({!r})"
                return tuple(
                    [
                        sys.executable,
                        "-c",
                        code.format(str(Path(out) / "value"), str(log), label),
                    ]
                    for label in ("T", "G")
                )

            benchmark.measure_pair(commands, root, [1], 4, 1, os.environ.copy(), lambda a, b: [])
            # tuning warm-up + one sample, then independent warm-up + four samples
            self.assertEqual(log.read_text(), "TGGT" + "TG" + "TGGT TGGT".replace(" ", ""))

    def test_each_tool_selects_its_own_best_budget(self):
        def fake_run(cmd, env, stdout, metrics):
            budget = int(env["TERRIO_BENCH_CPU_LIMIT"])
            # Terrio prefers four CPUs; GDAL prefers one.
            elapsed = (5 / budget) if cmd[0] == "T" else budget
            return elapsed, 1, 0, ""

        with (
            tempfile.TemporaryDirectory() as root,
            patch.object(benchmark, "run_once", fake_run),
        ):
            result = benchmark.measure_pair(lambda out, n: (["T"], ["G"]), root, [1, 4], 3, 2, {}, lambda a, b: [])
        self.assertEqual(result["budgets"], [4, 1])
        self.assertEqual(result["ratio"], 0.8)

    def test_background_load_suppresses_ratio_even_for_equal_outputs(self):
        def fake_run(cmd, env, stdout, metrics):
            metrics["background_cpus"] = 6.0
            return 1.0, 1, 0, ""

        with (
            tempfile.TemporaryDirectory() as root,
            patch.object(benchmark, "run_once", fake_run),
        ):
            result = benchmark.measure_pair(lambda out, n: (["T"], ["G"]), root, [1], 3, 2, {}, lambda a, b: [])
        self.assertIsNone(result["ratio"])
        self.assertEqual(result["validation"], [])
        self.assertIn("background CPU", result["measurement_issues"][0])

    def test_short_command_tick_noise_is_averaged_with_its_pair(self):
        def fake_run(cmd, env, stdout, metrics):
            # Tick quantization makes the short sample look busy; the longer pair
            # resolves a background estimate below one CPU for the series.
            short = cmd[0] == "T"
            metrics["background_cpus"] = 1.4 if short else 0.1
            return 0.015 if short else 0.1, 1, 0, ""

        with tempfile.TemporaryDirectory() as root, patch.object(benchmark, "run_once", fake_run):
            result = benchmark.measure_pair(lambda out, n: (["T"], ["G"]), root, [1], 3, 2, {}, lambda a, b: [])
        self.assertIsNotNone(result["ratio"])
        self.assertLess(result["background_series_averages"]["measurement"], 1)

    def test_outlier_is_annotated_without_changing_the_median_ratio(self):
        calls = {"T": 0, "G": 0}

        def fake_run(cmd, env, stdout, metrics):
            calls[cmd[0]] += 1
            # T's sixth call is the final measurement after tuning and warm-ups.
            elapsed = 4.0 if cmd[0] == "T" and calls["T"] == 6 else (1.0 if cmd[0] == "T" else 2.0)
            return elapsed, 1, 0, ""

        with tempfile.TemporaryDirectory() as root, patch.object(benchmark, "run_once", fake_run):
            result = benchmark.measure_pair(lambda out, n: (["T"], ["G"]), root, [1], 3, 1, {}, lambda a, b: [])
        self.assertEqual(result["samples"][0], [1, 1, 4])
        self.assertEqual(result["timing_notes"], ["wide timing range"])
        self.assertEqual(result["ratio"], 2)

    @unittest.skipUnless(hasattr(os, "sched_getaffinity"), "Linux affinity")
    def test_affinity_is_inherited_by_non_rayon_child(self):
        with tempfile.TemporaryFile() as stdout:
            code = "import os,subprocess,sys,json; print(json.dumps([len(os.sched_getaffinity(0)),int(subprocess.check_output([sys.executable,'-c','import os; print(len(os.sched_getaffinity(0)))']))]))"
            result = benchmark.run_once(
                [sys.executable, "-c", code],
                dict(os.environ, TERRIO_BENCH_CPU_LIMIT="1"),
                stdout,
            )
            self.assertEqual(result[2], 0)
            stdout.seek(0)
            self.assertEqual(json.load(stdout), [1, 1])

    def test_missing_taskset_is_reported_before_running_commands(self):
        stderr = io.StringIO()
        with (
            patch.object(sys, "argv", ["benchmark.py", "fixtures"]),
            patch.object(benchmark.os, "sched_getaffinity", return_value={0}, create=True),
            patch.object(benchmark.shutil, "which", return_value=None),
            contextlib.redirect_stderr(stderr),
            self.assertRaises(SystemExit) as error,
        ):
            benchmark.main()
        self.assertEqual(error.exception.code, 2)
        self.assertIn("requires taskset", stderr.getvalue())

    def test_cpu_budgets_above_available_affinity_are_rejected(self):
        for args, message in [(["--threads", "2"], "only 1 CPUs available"), (["--thread-counts", "2"], "tuning budgets must not exceed")]:
            with self.subTest(args=args):
                stderr = io.StringIO()
                with (
                    patch.object(sys, "argv", ["benchmark.py", "fixtures", *args]),
                    patch.object(benchmark.os, "sched_getaffinity", return_value={0}, create=True),
                    patch.object(benchmark.shutil, "which", return_value="/bin/taskset"),
                    contextlib.redirect_stderr(stderr),
                    self.assertRaises(SystemExit) as error,
                ):
                    benchmark.main()
                self.assertEqual(error.exception.code, 2)
                self.assertIn(message, stderr.getvalue())

    def test_main_caches_cases_and_marks_failed_duration(self):
        from osgeo import gdal

        original_config = {
            name: gdal.GetConfigOption(name)
            for name in ["GDAL_PAM_ENABLED", "GDAL_NUM_THREADS"]
        }

        def large_cases(data, out, budget):
            return [("test", "cached case", ["terrio", str(Path(out) / "out.tif")], ["gdal", str(Path(out) / "out.tif")])]

        output = io.StringIO()

        def fake_run(command, env, stdout, metrics):
            return 1.0, None, 2 if command[0] == "gdal" else 0, "test failure"

        actual_output = subprocess.check_output

        def version_or_git(command, **kwargs):
            return actual_output(command, **kwargs) if command[0] == "git" else "test"

        with (
            tempfile.TemporaryDirectory() as root,
            patch.object(sys, "argv", ["benchmark.py", "--large", root, "--thread-counts", "1", "--large-runs", "1", "--tune-runs", "1"]),
            patch.object(benchmark, "large_cases", side_effect=large_cases) as factory,
            patch.object(benchmark, "gdal_version", return_value="GDAL " + gdal.VersionInfo("RELEASE_NAME")),
            patch.object(benchmark.subprocess, "check_output", side_effect=version_or_git),
            patch.object(benchmark, "run_once", side_effect=fake_run),
            patch("benchmark_validation.validate_case", return_value=[]),
            contextlib.redirect_stdout(output),
        ):
            original_cwd = os.getcwd()
            try:
                os.chdir(root)  # An invocation from outside the harness's Git checkout.
                benchmark.main()
            finally:
                os.chdir(original_cwd)
                for name, value in original_config.items():
                    gdal.SetConfigOption(name, value)
        # One metadata list, then one list for each tool's output directory.
        self.assertEqual(factory.call_count, 3)
        self.assertIn("| failed |", output.getvalue())

    def test_rejects_nonpositive_counts(self):
        self.assertRaises(ValueError, benchmark.positive_int, "0")
        self.assertRaises(ValueError, benchmark.positive_int, "-1")


if __name__ == "__main__":
    unittest.main()
