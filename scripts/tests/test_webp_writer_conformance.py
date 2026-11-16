# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""A partial pytest run must never certify raster writers' complete conformance."""

import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance  # noqa: E402


@pytest.mark.parametrize("writer", ["webp", "jp2"])
@pytest.mark.parametrize("outcome", ["complete", "partial", "skip", "failure", "missing-report"])
def test_writer_conformance_requires_every_collected_case(monkeypatch, outcome, writer):
    """Check report completeness independently of pytest's successful exit status."""
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])

    def run(args, env=None):
        """Return pytest collection and report outcomes, including a false-success case."""
        if "-c" in args:
            return 0, "3.13.3", ""
        filename = next(Path(arg).name for arg in args if arg.endswith(".py"))
        expected = 17 if filename == "test_webp_lossy_damage.py" else 2
        if "--collect-only" in args:
            collected = "\n".join(f"python/tests/{filename}::test_case_{i}" for i in range(expected))
            return 0, f"{collected}\n\n{expected} tests collected", ""
        reports = [arg.removeprefix("--junitxml=") for arg in args if arg.startswith("--junitxml=")]
        reported = expected - 1 if outcome == "partial" else expected
        if reports and outcome != "missing-report":
            element = {"skip": "<skipped/>", "failure": "<failure/>"}.get(outcome, "")
            cases = "".join(
                f'<testcase name="case_{i}">{element if i == 0 else ""}</testcase>'
                for i in range(reported)
            )
            Path(reports[0]).write_text(f"<testsuite>{cases}</testsuite>")
        return 0, f"{reported} passed", ""

    monkeypatch.setattr(conformance, "run", run)
    suite = getattr(conformance, f"suite_{writer}_writer")(SimpleNamespace(data="data"))
    expected = [conformance.IDENTICAL]
    if writer == "webp":
        expected.append(conformance.TOLERANCE)
    assert [case.status for case in suite.cases] == (
        expected if outcome == "complete" else [conformance.DIFFERS] * len(expected)
    )


@pytest.mark.parametrize("writer", ["webp", "jp2"])
def test_writer_conformance_resolves_data_and_clears_selection(monkeypatch, tmp_path, writer):
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
    getattr(conformance, f"suite_{writer}_writer")(SimpleNamespace(data=Path("relative-data")))
    assert pytest_runs
    for args, env in pytest_runs:
        assert env["TERRIO_TEST_DATA"] == str(tmp_path / "relative-data")
        assert env["PYTEST_ADDOPTS"] == ""
        assert "addopts=" in args


@pytest.mark.parametrize("writer", ["webp", "jp2"])
def test_missing_writer_prerequisites_are_skipped(monkeypatch, writer):
    """Unavailable reference modules are not decoding differences."""
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: ["rasterio"])
    monkeypatch.setattr(conformance, "run", lambda *args, **kwargs: (1, "", "module unavailable"))
    suite = getattr(conformance, f"suite_{writer}_writer")(SimpleNamespace(data="data"))
    assert "rasterio" in suite.skipped
    assert not suite.cases
    assert not suite.error


@pytest.mark.parametrize("available", [False, True])
def test_jp2_colors_checks_optional_node_exports(monkeypatch, tmp_path, available):
    import shutil

    if shutil.which("node") is None:
        pytest.skip("optional Node runtime is absent")
    monkeypatch.setattr(conformance, "ROOT", tmp_path)
    package = tmp_path / "js/pkg-node/terrio.js"
    package.parent.mkdir(parents=True)
    package.write_text(
        "module.exports = {open() {}};" if available
        else 'module.exports = require("./missing_bg.wasm");'
    )
    if available:
        package.with_name("terrio_bg.wasm").write_bytes(b"\0asm\x01\0\0\0")
    monkeypatch.setattr(
        conformance, "suite_json_ops", lambda *args, **kwargs: kwargs["extra"]
    )
    extra = conformance.suite_jp2_colors(SimpleNamespace())
    assert extra == (("--node-package", str(package)) if available else ())
