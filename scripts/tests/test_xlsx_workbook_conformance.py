# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Incomplete workbook oracle runs must not produce a green conformance report."""
import json
import os
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance


@pytest.mark.parametrize("failure", [None, "missing", "empty", "malformed", "unknown", "exit", "differs"])
def test_workbook_oracle_reports(monkeypatch, tmp_path, failure):
    example = tmp_path / "examples" / ("xlsx_workbook.exe" if os.name == "nt" else "xlsx_workbook")
    example.parent.mkdir()
    example.touch()
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    calls = []

    def run(args):
        if "--json" not in args:
            return 0, "", ""
        calls.append(args[1])
        report = Path(args[-1])
        status = "unknown" if failure == "unknown" else "differs" if failure == "differs" else "identical"
        if failure != "missing":
            report.write_text("bad JSON" if failure == "malformed" else json.dumps({
                "cases": [] if failure == "empty" else [{"name": "control", "status": status}]
            }))
        return (1 if failure == "exit" else 0), "oracle output", ""

    monkeypatch.setattr(conformance, "run", run)
    suite = conformance.suite_xlsx_workbook(SimpleNamespace(terrio=str(tmp_path / "terrio")))
    if failure in ("missing", "empty", "malformed", "unknown", "exit"):
        assert suite.error
    else:
        assert not suite.error
        assert [c.group for c in suite.cases] == ["writer", "edit"]
        assert all(c.status == ("differs" if failure else "identical") for c in suite.cases)
        assert len(calls) == 2


def test_missing_workbook_example_is_an_error(monkeypatch, tmp_path):
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    suite = conformance.suite_xlsx_workbook(SimpleNamespace(terrio=str(tmp_path / "terrio")))
    assert suite.error
    assert not suite.skipped
