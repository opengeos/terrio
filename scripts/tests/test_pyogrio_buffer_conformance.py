# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Distinguish comparison failures from pytest setup errors in buffer conformance."""

import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance  # noqa: E402


def test_buffer_oracle_refuses_terrio_as_its_own_reference(monkeypatch):
    """A compatibility module is an unavailable reference, not a broken comparison.

    Args:
        monkeypatch: Scoped dependency replacement.
    """
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setitem(
        sys.modules, "pyogrio", SimpleNamespace(__terrio_version__="test")
    )
    suite = conformance.suite_pyogrio_buffers(SimpleNamespace())
    assert suite.skipped
    assert not suite.error
    assert not suite.cases


@pytest.mark.parametrize(
    "outcome", ["match", "difference", "fixture-error", "skip", "malformed"]
)
def test_buffer_oracle_reports_comparisons_and_setup_errors_separately(
    monkeypatch, outcome
):
    """Fixture errors must not be presented as differences in vector data.

    Args:
        monkeypatch: Scoped module and runner replacements.
        outcome: Pytest report outcome to exercise.
    """
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setitem(
        sys.modules,
        "pyogrio",
        SimpleNamespace(__gdal_version__=(3, 13, 3), __version__="reference"),
    )

    def run(args):
        """Write a pytest-style JUnit report from the external comparison runner.

        Args:
            args: Pytest command including its report destination.

        Returns:
            tuple: Process status, standard output and standard error.
        """
        report = Path(
            next(
                arg.removeprefix("--junitxml=")
                for arg in args
                if arg.startswith("--junitxml=")
            )
        )
        element = {
            "match": "",
            "difference": '<failure message="values differ">row mismatch</failure>',
            "fixture-error": '<error message="missing fixture">fixture setup failed</error>',
            "skip": '<skipped message="reference driver unavailable"/>',
            "malformed": "",
        }[outcome]
        report.write_text(
            "bad XML"
            if outcome == "malformed"
            else f'<testsuite><testcase name="buffer">{element}</testcase></testsuite>'
        )
        return (
            (0 if outcome in ("match", "skip") else 1),
            "oracle output",
            "oracle diagnostic",
        )

    monkeypatch.setattr(conformance, "run", run)
    suite = conformance.suite_pyogrio_buffers(SimpleNamespace())
    if outcome in ("fixture-error", "malformed"):
        assert suite.error
        assert not suite.cases
    else:
        assert not suite.error
        expected = {
            "match": conformance.IDENTICAL,
            "difference": conformance.DIFFERS,
            "skip": conformance.NOT_COMPARED,
        }[outcome]
        assert [case.status for case in suite.cases] == [expected]
