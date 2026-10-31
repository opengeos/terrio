"""Ensure analysis comparison tolerances cannot conceal API output regressions."""

import sys
from pathlib import Path

from osgeo import osr
import pytest
from shapely.geometry import LineString, box, mapping

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_analysis_bindings_with_gdal import compare


def case(operation="buffer", geometry=None, fields=None):
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(3857)
    return dict(
        operation=operation,
        expected=[
            (
                geometry if geometry is not None else box(0, 0, 10, 10),
                fields if fields is not None else {"source_id": 1},
            )
        ],
        crs=crs,
        tolerance=1e-7,
        keys=["source_id"] if operation == "buffer" else [],
        options={"with_z": True} if operation == "contour" else {},
    )


def output(geometry, fields, crs="EPSG:3857"):
    return dict(
        crs=crs,
        geojson={
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": mapping(geometry),
                    "properties": fields,
                }
            ],
        },
    )


def test_polygon_comparison_accepts_matching_geometry_attributes_and_crs():
    assert (
        compare(case(), output(box(0, 0, 10, 10), {"source_id": 1}))[0] == "identical"
    )


@pytest.mark.parametrize("change", ["position", "attributes", "crs", "empty"])
def test_polygon_comparison_rejects_regressions(change):
    result = output(
        box(1, 0, 11, 10) if change == "position" else box(0, 0, 10, 10),
        {"source_id": 2 if change == "attributes" else 1},
        "EPSG:4326" if change == "crs" else "EPSG:3857",
    )
    if change == "empty":
        result["geojson"]["features"] = []
    with pytest.raises(AssertionError):
        compare(case(), result)


@pytest.mark.parametrize("change", ["lost-z", "wrong-z", "position", "elevation"])
def test_contour_comparison_checks_z_levels_and_positions(change):
    expected = LineString([(0, 0, 25), (10, 0, 25)])
    actual = {
        "lost-z": LineString([(0, 0), (10, 0)]),
        "wrong-z": LineString([(0, 0, 26), (10, 0, 26)]),
        "position": LineString([(0, 1, 25), (10, 1, 25)]),
        "elevation": expected,
    }[change]
    with pytest.raises(AssertionError):
        compare(
            case("contour", expected, {"ELEV": 25}),
            output(actual, {"ELEV": 26 if change == "elevation" else 25}),
        )


def test_contour_comparison_accepts_matching_z_lines():
    geometry = LineString([(0, 0, 25), (10, 0, 25)])
    status, detail = compare(
        case("contour", geometry, {"ELEV": 25}), output(geometry, {"ELEV": 25})
    )
    assert status == "tolerance" and "matching levels/counts/Z" in detail
