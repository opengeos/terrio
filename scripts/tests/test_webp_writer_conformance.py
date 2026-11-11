# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""A partial pytest run must never certify the WebP writer's complete conformance."""

import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance  # noqa: E402


@pytest.mark.parametrize("outcome", ["complete", "partial", "skip", "failure", "missing-report"])
def test_webp_conformance_requires_every_collected_case(monkeypatch, outcome):
    """Check report completeness independently of pytest's successful exit status."""
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])

    def run(args, env=None):
        """Return pytest collection and report outcomes, including a false-success case."""
        if "-c" in args:
            return 0, "3.13.3", ""
        if "--collect-only" in args:
            return 0, "python/tests/test_webp_writer.py::test_lossless\npython/tests/test_webp_writer.py::test_lossy\n\n2 tests collected", ""
        reports = [arg.removeprefix("--junitxml=") for arg in args if arg.startswith("--junitxml=")]
        if reports and outcome != "missing-report":
            element = {"skip": "<skipped/>", "failure": "<failure/>"}.get(outcome, "")
            cases = f'<testcase name="lossless">{element}</testcase>'
            if outcome != "partial":
                cases += '<testcase name="lossy"/>'
            Path(reports[0]).write_text(f"<testsuite>{cases}</testsuite>")
        return 0, "1 passed" if outcome == "partial" else "2 passed", ""

    monkeypatch.setattr(conformance, "run", run)
    suite = conformance.suite_webp_writer(SimpleNamespace(data="data"))
    assert len(suite.cases) == 1
    assert suite.cases[0].status == (conformance.IDENTICAL if outcome == "complete" else conformance.DIFFERS)


def test_webp_conformance_resolves_data_and_clears_selection(monkeypatch, tmp_path):
    """A caller's cwd and PYTEST_ADDOPTS must not change which inputs/tests run."""
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("PYTEST_ADDOPTS", "-k defaults")
    pytest_runs = []

    def run(args, env=None):
        """Record the environment passed to the pytest subprocess."""
        if "-c" in args:
            return 0, "3.13.3", ""
        pytest_runs.append((args, env))
        return 1, "", "collection failed"

    monkeypatch.setattr(conformance, "run", run)
    conformance.suite_webp_writer(SimpleNamespace(data=Path("relative-data")))
    assert pytest_runs
    for args, env in pytest_runs:
        assert env["TERRIO_TEST_DATA"] == str(tmp_path / "relative-data")
        assert env["PYTEST_ADDOPTS"] == ""
        assert "addopts=" in args
