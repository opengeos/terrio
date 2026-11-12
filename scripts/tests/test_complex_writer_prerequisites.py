# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Unavailable GDAL array bindings or comparison data are not pixel differences."""

import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance  # noqa: E402


@pytest.mark.parametrize("missing", ["gdal_array", "landsat"])
def test_complex_comparisons_report_missing_prerequisites(monkeypatch, tmp_path, missing):
    """Require both GDAL's array API and the complete module's real-data fixture."""
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    calls = []

    def run(args, env=None):
        calls.append(args)
        assert "-c" in args, "pytest must not run with missing prerequisites"
        assert "gdal_array" in args[-1]
        return (1, "", "missing gdal_array") if missing == "gdal_array" else (0, "3.13.3", "")

    monkeypatch.setattr(conformance, "run", run)
    suite = conformance.suite_complex_writes(SimpleNamespace(data=tmp_path))
    assert calls
    assert suite.skipped
    assert not suite.error
    assert not suite.cases
