"""Ensure SQL conformance refuses value, schema and geometry regressions."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_grouped_spatial_sql_with_gdal as validation
from compare_grouped_spatial_sql_with_gdal import compare
from osgeo import ogr


def test_empty_schema_policy_cannot_hide_populated_or_wrong_count_columns():
    expected = ([("CONTINENT", "String"), ("n", "String")], None, [])
    actual = ([("CONTINENT", "String"), ("n", "Integer")], None, [])
    assert compare(expected, actual, schema_policy="empty groups") == "within tolerance"
    with pytest.raises(AssertionError, match="schema differs"):
        compare(expected, actual)
    with pytest.raises(AssertionError, match="zero rows"):
        compare(expected, (actual[0], None, [(["Asia", 1], None)]), schema_policy="empty groups")
    with pytest.raises(AssertionError, match="schema differs"):
        compare(
            expected,
            ([("CONTINENT", "String"), ("n", "Real")], None, []),
            schema_policy="empty groups",
        )


def test_null_schema_policy_cannot_hide_nonnull_values():
    ef = [("area", "String"), ("length", "String"), ("intersects", "Integer")]
    af = [("area", "Real"), ("length", "Real"), ("intersects", "Integer")]
    expected = (ef, None, [([None, None, -1], None)])
    with pytest.raises(AssertionError, match="all-null"):
        compare(
            expected, (af, None, [([2.0, None, -1], None)]), schema_policy="null spatial values"
        )
    with pytest.raises(AssertionError, match="attribute differs"):
        compare(
            expected, (af, None, [([None, None, 0], None)]), schema_policy="null spatial values"
        )


def test_buffer_overlap_is_not_sufficient_for_equivalence():
    a = ogr.CreateGeometryFromWkt("POLYGON ((0 0,1 0,1 1,0 1,0 0))")
    b = ogr.CreateGeometryFromWkt("POLYGON ((0.5 0,1.5 0,1.5 1,0.5 1,0.5 0))")
    with pytest.raises(AssertionError, match="symmetric difference"):
        compare(([], None, [([], a)]), ([], None, [([], b)]), buffer=True)


def test_non_point_geometry_differences_are_rejected():
    a = ogr.CreateGeometryFromWkt("POLYGON ((0 0,1 0,1 1,0 1,0 0))")
    b = ogr.CreateGeometryFromWkt("POLYGON ((0 0,0 1,1 1,1 0,0 0))")
    assert a.SymmetricDifference(b).IsEmpty()
    assert a.ExportToWkb() != b.ExportToWkb()
    assert compare(([], None, [([], a)]), ([], None, [([], a.Clone())])) == "identical"
    with pytest.raises(AssertionError, match="constructed geometry exceeds 1e-7"):
        compare(([], None, [([], a)]), ([], None, [([], b)]))


def test_unordered_comparison_preserves_group_values_and_multiplicity():
    fields = [("key", "String"), ("n", "Integer")]
    expected = (fields, None, [(["A", 1], None), (["B", 2], None)])
    actual = (fields, None, [(["B", 2], None), (["A", 1], None)])
    assert compare(expected, actual, unordered=True) == "identical"
    with pytest.raises(AssertionError, match="attribute differs"):
        compare(expected, actual)
    with pytest.raises(AssertionError, match="attribute differs"):
        compare(expected, (fields, None, [(["B", 3], None), (["A", 1], None)]), unordered=True)
    with pytest.raises(AssertionError, match="row counts"):
        compare(expected, (fields, None, [(["A", 1], None)]), unordered=True)


def test_constructed_point_displacement_is_bounded():
    source = ogr.CreateGeometryFromWkt("POINT (0 0)")
    near = ogr.CreateGeometryFromWkt("POINT (0.00000001 0)")
    far = ogr.CreateGeometryFromWkt("POINT (0.000001 0)")
    assert compare(([], None, [([], source)]), ([], None, [([], near)])) == "within tolerance"
    with pytest.raises(AssertionError, match="constructed geometry exceeds 1e-7"):
        compare(([], None, [([], source)]), ([], None, [([], far)]))


def test_optional_node_surface_is_reported_and_required_surface_cannot_skip(tmp_path, monkeypatch):
    package = tmp_path / "terrio.js"
    status = validation.node_surface_status(package)
    assert status["status"] == "not compared"
    with pytest.raises(ValueError, match="package missing"):
        validation.node_surface_status(package, required=True)
    package.write_text("")
    monkeypatch.setattr(validation.shutil, "which", lambda name: None)
    assert validation.node_surface_status(package)["status"] == "not compared"
    with pytest.raises(ValueError, match="runtime unavailable"):
        validation.node_surface_status(package, required=True)
    monkeypatch.setattr(validation.shutil, "which", lambda name: "/node")
    assert validation.node_surface_status(package, required=True) is None
