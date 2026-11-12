# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Missing real-data comparisons must remain visible beside synthetic successes."""

import json
import sys
import pytest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance  # noqa: E402


def test_missing_worldcover_is_not_reported_as_a_difference(monkeypatch):
    """Keep every synthetic result and the unavailable optional palette comparison."""
    rows = [dict(name=f"{driver} {bits}-bit", status="identical", detail="exact")
            for driver in ("GTiff", "COG") for bits in (1, 4, 7, 12, 24, 31)]
    rows.append(dict(name="COG 4-bit CUBIC bright edge", status="identical", detail="independent GDAL pyramid"))
    rows.extend(dict(name=name, status="identical", detail="source control") for name in
                ("JP2 15-bit AUTO to GTiff", "JP2 15-bit AUTO to COG", "COG overview-only source masks"))
    rows.append(dict(name="WorldCover palette", status="not compared", detail="fixture unavailable"))
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setattr(conformance, "run", lambda *args: (0, json.dumps(rows), ""))
    suite = conformance.suite_packed_tiff(SimpleNamespace(data="missing", terrio="terrio"))
    assert not suite.error
    assert [case.status for case in suite.cases] == [conformance.IDENTICAL] * 16 + [conformance.NOT_COMPARED]


def test_missing_cubic_reference_is_an_incomplete_suite(monkeypatch):
    """Do not report success when the independent generated-overview control is absent."""
    rows = [dict(name=f"{driver} {bits}-bit", status="identical", detail="exact")
            for driver in ("GTiff", "COG") for bits in (1, 4, 7, 12, 24, 31)]
    rows.extend(dict(name=name, status="identical", detail="source control") for name in
                ("JP2 15-bit AUTO to GTiff", "JP2 15-bit AUTO to COG", "COG overview-only source masks"))
    rows.append(dict(name="WorldCover palette", status="not compared", detail="fixture unavailable"))
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setattr(conformance, "run", lambda *args: (0, json.dumps(rows), ""))
    suite = conformance.suite_packed_tiff(SimpleNamespace(data="missing", terrio="terrio"))
    assert suite.error


def test_missing_synthetic_case_is_an_incomplete_suite(monkeypatch):
    """An unrelated extra row cannot substitute for a required packed-width case."""
    names = [f"{driver} {bits}-bit" for driver in ("GTiff", "COG") for bits in (1, 4, 7, 12, 24, 31)]
    names.remove("GTiff 1-bit")
    names.extend(("COG 4-bit CUBIC bright edge", "WorldCover palette", "JP2 15-bit AUTO to GTiff",
                  "JP2 15-bit AUTO to COG", "COG overview-only source masks", "unrelated"))
    rows = [dict(name=name, status="identical", detail="exact") for name in names]
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setattr(conformance, "run", lambda *args: (0, json.dumps(rows), ""))
    assert conformance.suite_packed_tiff(SimpleNamespace(data="missing", terrio="terrio")).error


@pytest.mark.parametrize("missing", ["JP2 15-bit AUTO to GTiff", "JP2 15-bit AUTO to COG", "COG overview-only source masks"])
def test_missing_source_preservation_control_is_incomplete(monkeypatch, missing):
    """Verify missing source preservation control is incomplete."""
    names = [f"{driver} {bits}-bit" for driver in ("GTiff", "COG") for bits in (1, 4, 7, 12, 24, 31)]
    names.extend(("COG 4-bit CUBIC bright edge", "WorldCover palette", "JP2 15-bit AUTO to GTiff",
                  "JP2 15-bit AUTO to COG", "COG overview-only source masks"))
    rows = [dict(name=name, status="identical", detail="exact") for name in names if name != missing]
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setattr(conformance, "run", lambda *args: (0, json.dumps(rows), ""))
    assert conformance.suite_packed_tiff(SimpleNamespace(data="missing", terrio="terrio")).error
