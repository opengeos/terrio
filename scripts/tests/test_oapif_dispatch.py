"""OAPIF report statuses cannot silently bypass strict comparison failures."""
import json
import sys
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance


def test_oapif_maps_comparison_statuses_and_checks_node_exports(monkeypatch):
    monkeypatch.setitem(sys.modules, "osgeo", SimpleNamespace(gdal=SimpleNamespace(VersionInfo=lambda _: "3130300")))
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    exports = []

    def available(package, required):
        exports.append(required)
        return False

    monkeypatch.setattr(conformance, "node_exports_available", available)
    rows = [dict(name=status, status=status, detail="fixture") for status in ["identical", "tolerance", "differs", "unknown"]]
    monkeypatch.setattr(conformance, "run", lambda args: (0, json.dumps(rows), ""))
    suite = conformance.suite_oapif(SimpleNamespace(terrio="terrio", remote=False))
    assert [c.status for c in suite.cases] == [
        conformance.IDENTICAL, conformance.TOLERANCE,
        conformance.DIFFERS, conformance.DIFFERS, conformance.NOT_COMPARED,
    ]
    assert exports == [["openOapif"]]
