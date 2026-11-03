# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Public-reference availability must not produce an empty successful comparison."""

import json
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

pytest.importorskip("osgeo")
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_oapif_with_gdal as comparison


def test_empty_reference_is_unavailable_instead_of_an_identical_comparison(tmp_path, monkeypatch):
    path = tmp_path / "empty.geojson"
    path.write_text(json.dumps({"type": "FeatureCollection", "features": []}))
    source = comparison.gdal.OpenEx(str(path), comparison.gdal.OF_VECTOR)
    monkeypatch.setattr(comparison, "open_reference", lambda *args: (source, source.GetLayer(0)))
    with pytest.raises(RuntimeError, match="reference returned no features"):
        comparison.reference_rows("unused", [], None)


def test_reference_timeout_is_set_before_opening_the_service(monkeypatch):
    previous = comparison.gdal.GetConfigOption("GDAL_HTTP_TIMEOUT")
    seen = []
    layer = SimpleNamespace()

    def open_service(*args, **kwargs):
        seen.append(comparison.gdal.GetConfigOption("GDAL_HTTP_TIMEOUT"))
        return SimpleNamespace(GetLayer=lambda index: layer)

    monkeypatch.setattr(comparison.gdal, "OpenEx", open_service)
    try:
        _, actual = comparison.open_reference("https://example.test/collections/a", [])
        assert actual is layer
        assert seen == ["60"]
    finally:
        comparison.gdal.SetConfigOption("GDAL_HTTP_TIMEOUT", previous)
