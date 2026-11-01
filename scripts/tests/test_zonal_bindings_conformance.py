"""Ensure the binding comparison rejects incorrect statistical results."""

from copy import deepcopy
from pathlib import Path
import sys

from osgeo import osr
import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_zonal_bindings_with_gdal import compare


def example():
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(4326)
    geometry = {
        "type": "Polygon",
        "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 1], [0, 0]]],
    }
    original = [
        dict(properties={"name": "zone"}, geometry=geometry, crs=crs.ExportToWkt())
    ]
    expected = [{"r0_b1_count": 3, "r0_b1_mean": 2.5, "r0_b1_median": None}]
    result = dict(
        crs="EPSG:4326",
        geojson={
            "type": "FeatureCollection",
            "features": [
                dict(
                    type="Feature",
                    properties=original[0]["properties"] | expected[0],
                    geometry=deepcopy(geometry),
                )
            ],
        },
    )
    return result, expected, original


def test_matching_zonal_result_is_accepted():
    assert "counts/nulls" in compare(*example(), {})


@pytest.mark.parametrize(
    "change",
    [
        "count",
        "value",
        "null",
        "attribute",
        "crs",
        "missing-crs",
        "geometry",
        "geometry-null",
        "feature",
        "nan",
    ],
)
def test_zonal_comparison_rejects_regressions(change):
    result, expected, source = example()
    feature = result["geojson"]["features"][0]
    if change == "count":
        feature["properties"]["r0_b1_count"] = 2
    if change == "value":
        feature["properties"]["r0_b1_mean"] = 3
    if change == "null":
        feature["properties"]["r0_b1_median"] = 0
    if change == "attribute":
        feature["properties"]["name"] = "other"
    if change == "crs":
        result["crs"] = "EPSG:3857"
    if change == "missing-crs":
        del result["crs"]
    if change == "geometry-null":
        feature["geometry"] = None
    if change == "geometry":
        feature["geometry"]["coordinates"][0][1][0] = 1.01
    if change == "feature":
        result["geojson"]["features"] = []
    if change == "nan":
        feature["properties"]["r0_b1_mean"] = float("nan")
    with pytest.raises(AssertionError):
        compare(result, expected, source, {})


def test_polygon_boundary_cannot_replace_polygon():
    result, expected, source = example()
    polygon = result["geojson"]["features"][0]["geometry"]
    result["geojson"]["features"][0]["geometry"] = {
        "type": "LineString", "coordinates": polygon["coordinates"][0]
    }
    with pytest.raises(AssertionError, match="geometry type differs"):
        compare(result, expected, source, {})


def test_lost_polygon_hole_is_rejected():
    result, expected, source = example()
    hole = [[0.25, 0.25], [0.75, 0.25], [0.75, 0.75], [0.25, 0.75], [0.25, 0.25]]
    source[0]["geometry"]["coordinates"].append(hole)
    with pytest.raises(AssertionError, match="output geometry differs"):
        compare(result, expected, source, {})
    result["geojson"]["features"][0]["geometry"]["coordinates"].append(hole)
    compare(result, expected, source, {})


@pytest.mark.parametrize("field", ["r0_b1_count", "r0_b1_mean"])
def test_booleans_cannot_stand_in_for_numeric_statistics(field):
    result, expected, source = example()
    expected[0][field] = 1
    result["geojson"]["features"][0]["properties"][field] = True
    with pytest.raises(AssertionError):
        compare(result, expected, source, {})


def test_missing_data_skips_before_launching_comparison(monkeypatch, tmp_path):
    import conformance
    from types import SimpleNamespace
    from osgeo import gdal

    monkeypatch.setitem(
        sys.modules, "terrio", SimpleNamespace(zonal_stats=True, singlepart=True)
    )
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setattr(gdal, "VersionInfo", lambda *args: "3130300")
    suite = conformance.suite_zonal_bindings(SimpleNamespace(data=tmp_path))
    assert "missing zonal comparison data" in suite.skipped and not suite.cases


def test_missing_node_retains_python_comparisons(monkeypatch, tmp_path):
    import json
    import conformance
    from types import SimpleNamespace
    from osgeo import gdal

    monkeypatch.setitem(sys.modules, "terrio", SimpleNamespace(zonal_stats=True, singlepart=True))
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setattr(gdal, "VersionInfo", lambda *args: "3130300")
    for name in ["TN_Counties.geojson", "dem.tif", "nlcd_knoxville.tif", "landsat.tif", "countries.geojson", "countries.fgb"]:
        (tmp_path / name).touch()
    monkeypatch.setattr(conformance, "node_exports_available", lambda *args: False)
    results = [dict(name=str(i), status="identical", detail="reference matches") for i in range(6)]
    monkeypatch.setattr(conformance, "run", lambda args: (0, json.dumps(results), ""))
    suite = conformance.suite_zonal_bindings(SimpleNamespace(data=str(tmp_path)))
    assert not suite.error and not suite.skipped
    assert len(suite.cases) == 7
    assert suite.cases[0].status == conformance.NOT_COMPARED
    assert all(case.status == conformance.IDENTICAL for case in suite.cases[1:])


@pytest.mark.parametrize("wanted, got", [(1, True), (True, 1), (0, False), (False, 0)])
def test_boolean_and_numeric_source_attributes_are_distinct(wanted, got):
    result, expected, source = example()
    source[0]["properties"]["original_flag"] = wanted
    result["geojson"]["features"][0]["properties"]["original_flag"] = wanted
    compare(result, expected, source, {})
    result["geojson"]["features"][0]["properties"]["original_flag"] = got
    with pytest.raises(AssertionError, match="attribute/count/null differs"):
        compare(result, expected, source, {})


def test_omitted_target_crs_preserves_projected_zone_crs():
    result, expected, source = example()
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(3857)
    source[0]["crs"] = crs.ExportToWkt()
    result["crs"] = "EPSG:3857"
    assert "counts/nulls" in compare(result, expected, source, {})


def test_explicit_unchanged_crs_keeps_the_tight_geometry_bound():
    result, expected, source = example()
    result["geojson"]["features"][0]["geometry"]["coordinates"][0][1][0] += 5e-6
    with pytest.raises(AssertionError, match="output geometry differs"):
        compare(result, expected, source, {"t_srs": "EPSG:4326"})


def install_binding_failure_fixture(monkeypatch, tmp_path, failing_operation=None):
    """Inject one binding failure while preserving independent comparison cases."""
    import compare_zonal_bindings_with_gdal as harness
    from types import SimpleNamespace

    layer = SimpleNamespace(to_geojson=lambda: {}, crs="EPSG:4326", write=lambda path: None)

    def zonal(zones, rasters, **options):
        if failing_operation == "zonal" and zones == "bad":
            raise ValueError("injected zonal failure")
        return layer

    def singlepart(zones):
        if failing_operation == "singlepart" and str(zones).endswith("countries.geojson"):
            raise ValueError("injected split failure")
        return layer

    requests = [dict(name=name, zones=name, rasters=[], options={}) for name in ["bad", "good"]]
    monkeypatch.setitem(sys.modules, "terrio", SimpleNamespace(zonal_stats=zonal, singlepart=singlepart))
    monkeypatch.setattr(harness, "prepare", lambda *args: (deepcopy(requests), {name: (None, None, {}) for name in ["bad", "good"]}))
    monkeypatch.setattr(harness, "compare", lambda *args: "independent statistics match")
    monkeypatch.setattr(harness, "compare_singlepart", lambda *args: "independent parts match")
    return harness


@pytest.mark.parametrize("operation", ["zonal", "singlepart"])
def test_binding_failure_is_reported_without_losing_other_cases(monkeypatch, tmp_path, capsys, operation):
    import json

    harness = install_binding_failure_fixture(monkeypatch, tmp_path, operation)
    monkeypatch.setattr(sys, "argv", ["compare", str(tmp_path), "--json"])
    assert harness.main() == 1
    results = json.loads(capsys.readouterr().out)
    assert len(results) == 4
    failures = [row for row in results if row["status"] == "differs"]
    assert len(failures) == 1 and "injected" in failures[0]["detail"]
    assert all(row["name"].startswith("python/") for row in results)
    assert any(row["name"] == "python/good" and row["status"] == "tolerance" for row in results)


@pytest.mark.parametrize("failure", ["exit", "timeout", "invalid_json"])
def test_node_backend_failures_retain_all_python_results(monkeypatch, tmp_path, capsys, failure):
    import json
    import subprocess
    from types import SimpleNamespace

    harness = install_binding_failure_fixture(monkeypatch, tmp_path)
    monkeypatch.setattr(sys, "argv", ["compare", str(tmp_path), "--js", str(tmp_path / "module.js"), "--json"])

    def failed_node(arguments, **options):
        assert options["timeout"] == 600
        if failure == "timeout":
            raise subprocess.TimeoutExpired(arguments, 600)
        return SimpleNamespace(returncode=7 if failure == "exit" else 0, stdout="invalid JSON", stderr="injected Node failure")

    monkeypatch.setattr(harness.subprocess, "run", failed_node)
    assert harness.main() == 1
    results = json.loads(capsys.readouterr().out)
    python = [row for row in results if row["name"].startswith("python/")]
    node = [row for row in results if row["name"].startswith("js/")]
    assert len(python) == len(node) == 4
    assert all(row["status"] != "differs" for row in python)
    assert all(row["status"] == "differs" and row["detail"] for row in node)


def test_node_runner_isolates_one_binding_exception(tmp_path):
    import json
    import shutil
    import subprocess

    if shutil.which("node") is None:
        pytest.skip("Node is unavailable")
    module = tmp_path / "module.cjs"
    module.write_text("module.exports={zonalStats(zone){if(zone.filename==='bad.geojson')throw Error('injected');return {toGeoJSON(){return {}},crs:'EPSG:4326'}}}")
    requests = []
    for name in ["bad", "good"]:
        zone = tmp_path / (name + ".geojson")
        zone.write_text("{}")
        requests.append(dict(name=name, zones=str(zone), rasters=[], options={}))
    request_path = tmp_path / "requests.json"
    request_path.write_text(json.dumps(requests))
    response = subprocess.run(["node", str(Path(__file__).resolve().parents[1] / "run_zonal_bindings.mjs"), str(module), str(request_path)], capture_output=True, text=True, timeout=30, check=True)
    rows = json.loads(response.stdout)
    assert len(rows) == 2 and "injected" in rows[0]["error"]
    assert rows[1] == dict(name="good", geojson={}, crs="EPSG:4326")
