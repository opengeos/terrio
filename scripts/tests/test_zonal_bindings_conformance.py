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
