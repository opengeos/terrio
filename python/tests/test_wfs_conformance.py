# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""The WFS oracle ignores generated FIDs and keeps checks active under python -O."""

import importlib.util
import json
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

ogr = pytest.importorskip("osgeo.ogr")
SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "compare_wfs_with_gdal.py"
spec = importlib.util.spec_from_file_location("wfs_comparison", SCRIPT)
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


def layer(rows, *, keyed=False):
    """Keep the datasource alive alongside its OGR layer."""
    owner = ogr.GetDriverByName("Memory").CreateDataSource("")
    result = owner.CreateLayer("features", geom_type=ogr.wkbPoint)
    result.CreateField(ogr.FieldDefn("label", ogr.OFTString))
    if keyed:
        result.CreateField(ogr.FieldDefn("gml_id", ogr.OFTString))
    for fid, label, x, source_id in rows:
        feature = ogr.Feature(result.GetLayerDefn())
        feature.SetFID(fid)
        feature.SetField("label", label)
        if keyed:
            feature.SetField("gml_id", source_id)
        feature.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({x} 0)"))
        result.CreateFeature(feature)
    return owner, result


def test_unkeyed_features_ignore_fids_and_order_with_duplicate_multiplicity():
    reference = layer([(100, "a", 1, None), (200, "a", 1, None), (300, "b", 2, None)])
    actual = layer([(0, "b", 2, None), (1, "a", 1, None), (2, "a", 1, None)])
    comparison.compare_layer(comparison.snapshot(reference[1]), actual[1])


def test_unkeyed_matching_cannot_reuse_a_duplicate():
    reference = layer([(100, "a", 1, None), (200, "a", 1, None), (300, "b", 2, None)])
    actual = layer([(0, "a", 1, None), (1, "b", 2, None), (2, "b", 2, None)])
    with pytest.raises(AssertionError, match="no matching"):
        comparison.compare_layer(comparison.snapshot(reference[1]), actual[1])


def test_keyed_layers_still_require_matching_unique_source_ids():
    reference = layer([(100, "a", 1, "source.1")], keyed=True)
    actual = layer([(0, "a", 1, "source.2")], keyed=True)
    with pytest.raises(AssertionError, match="source IDs differ"):
        comparison.compare_layer(comparison.snapshot(reference[1]), actual[1])
    duplicate = layer([(100, "a", 1, "same"), (200, "a", 1, "same")], keyed=True)
    with pytest.raises(AssertionError, match="not unique"):
        comparison.snapshot(duplicate[1])


def _must_reject_mismatch():
    reference = layer([(100, "a", 1, None)])
    actual = layer([(0, "wrong", 1, None)])
    try:
        comparison.compare_layer(comparison.snapshot(reference[1]), actual[1])
    except AssertionError:
        return
    raise RuntimeError("optimized mode disabled the conformance checks")


def test_optimized_python_still_rejects_mismatches():
    path = str(Path(__file__).resolve())
    code = f"import runpy; runpy.run_path({path!r})['_must_reject_mismatch']()"
    result = subprocess.run(
        [sys.executable, "-O", "-c", code], capture_output=True, text=True, check=False
    )
    assert result.returncode == 0, result.stdout + result.stderr


def test_report_keeps_failed_comparisons_visible(monkeypatch):
    report_path = SCRIPT.with_name("conformance.py")
    spec = importlib.util.spec_from_file_location("wfs_report", report_path)
    report = importlib.util.module_from_spec(spec)
    monkeypatch.setitem(sys.modules, spec.name, report)
    spec.loader.exec_module(report)
    monkeypatch.setattr(report, "missing_modules", lambda _: False)
    monkeypatch.setattr(
        report, "_pytest_module_conformance", lambda *_args, **_kwargs: (True, "3 controls passed")
    )
    row = dict(name="service features", status="differs", detail="attribute mismatch")
    monkeypatch.setattr(report, "run", lambda _: (1, json.dumps([row]), ""))
    context = SimpleNamespace(terrio="terrio", remote=True, data=Path("data"), max_details=10)
    suite = report.suite_wfs(context)
    assert not suite.error
    assert report.tally(suite.cases)[-1] == 1
    rendered = report.markdown([suite], context, [])
    assert "attribute mismatch" in rendered
    assert "The suite could not run" not in rendered
    monkeypatch.setattr(report, "run", lambda _: (1, "not JSON", "bad response"))
    broken = report.suite_wfs(context)
    assert broken.error and "bad response" in broken.error
