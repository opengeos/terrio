"""Ensure SQL conformance refuses value, schema and geometry regressions."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_sql_depth_with_gdal import compare
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


def test_original_polygons_must_match_wkb():
    a = ogr.CreateGeometryFromWkt("POLYGON ((0 0,1 0,1 1,0 1,0 0))")
    b = ogr.CreateGeometryFromWkt("POLYGON ((0 0,2 0,2 1,0 1,0 0))")
    with pytest.raises(AssertionError, match="geometry"):
        compare(([], None, [([], a)]), ([], None, [([], b)]))
