"""Conformance script arguments stay confined to the suite that requested them."""
import sys
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance


def test_dem_calc_forwards_skip_without_affecting_histograms(monkeypatch):
    calls = []

    def run(arguments):
        calls.append(arguments)
        return 0, "[]", ""

    monkeypatch.setattr(conformance, "run", run)
    context = SimpleNamespace(data="data", terrio="terrio")
    conformance.suite_dem_calc(context)
    assert calls[-1][-1] == "--skip-cumulative"
    conformance.suite_histograms(context)
    assert "--skip-cumulative" not in calls[-1]
    assert calls[-1][-1] == "--json"
