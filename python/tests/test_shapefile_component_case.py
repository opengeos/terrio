"""Shapefile replacement and discovery preserve attributes and CRS across extension case."""

import os
import shutil
import subprocess
import sys

import pytest

import terrio
import terrio.pyogrio as tpg


def point(value, crs="EPSG:3857"):
    """Create a point with distinguishable geometry and attributes.

    Args:
        value: Coordinate and property value.
        crs: Coordinate reference system.

    Returns:
        terrio.Layer: One point with a text property.
    """
    return terrio.Layer.from_geojson(
        {
            "type": "Feature",
            "properties": {"id": value, "name": "café"},
            "geometry": {"type": "Point", "coordinates": [value, value]},
        },
        crs=crs,
    )


@pytest.mark.parametrize("surface", ["layer", "convert", "cli"])
@pytest.mark.parametrize("new_crs", [None, "EPSG:32633"])
@pytest.mark.parametrize(
    "filename,case,duplicates",
    [
        ("roads.shp", "lower", False),
        ("ROADS.SHP", "upper", False),
        ("ROADS.V2.SHP", "upper", False),
        ("ROADS.V2.Shp", "mixed", False),
        ("ROADS.SHP", "upper", True),
    ],
)
def test_replacement_removes_stale_component_cases(
    tmp_path, surface, new_crs, filename, case, duplicates
):
    """Replace every component and remove stale CRS definitions through shared writers.

    Args:
        tmp_path: Temporary dataset directory.
        surface: Layer, Python conversion or CLI writer.
        new_crs: Replacement CRS, including an explicitly absent definition.
        filename: Requested geometry filename.
        case: Existing component extension case.
        duplicates: Whether both uppercase and lowercase old components exist.
    """
    if duplicates and os.path.normcase("a") == os.path.normcase("A"):
        pytest.skip("duplicate case spellings need a case-sensitive filesystem")
    source = tmp_path / "replacement.shp"
    point(2, new_crs or "EPSG:4326").write(str(source))
    if new_crs is None:
        source.with_suffix(".prj").unlink()
    replacement = terrio.open_vector(str(source))[0]
    folder = tmp_path / "output"
    folder.mkdir()
    old = folder / "old.shp"
    point(1).write(str(old))
    target = folder / filename
    for extension in ["shp", "shx", "dbf", "prj", "cpg"]:
        spelling = (
            extension.upper()
            if case == "upper"
            else extension.title()
            if case == "mixed"
            else extension
        )
        existing = target.with_suffix("." + spelling)
        old.with_suffix("." + extension).rename(existing)
        if duplicates:
            shutil.copyfile(existing, target.with_suffix("." + extension))
    if surface == "layer":
        replacement.write(str(target))
    elif surface == "convert":
        terrio.convert(str(source), str(target))
    else:
        result = subprocess.run(
            [sys.executable, "-m", "terrio", "convert", str(source), str(target)],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, result.stderr
    expected = {target.name, *(target.with_suffix("." + ext).name for ext in ["shx", "dbf", "cpg"])}
    if new_crs:
        expected.add(target.with_suffix(".prj").name)
    actual = {path.name for path in folder.iterdir()}
    assert len(actual) == len(expected)
    assert {name.casefold() for name in actual} == {name.casefold() for name in expected}
    for layer in [terrio.open_vector(str(target))[0], terrio.open_stream(str(target)).read_all()]:
        assert layer.crs == new_crs
        feature = layer.to_geojson()["features"][0]
        assert feature["properties"] == {"id": 2, "name": "café"}
        assert feature["geometry"]["coordinates"] == [2, 2]
    info = tpg.read_info(str(target))
    assert info["crs"] == new_crs
    assert list(info["fields"]) == ["id", "name"]


def test_mixed_case_components_are_discovered(tmp_path):
    """Discover DBF, PRJ and CPG sidecars with mixed-case extensions.

    Args:
        tmp_path: Temporary dataset directory.
    """
    original = tmp_path / "roads.shp"
    point(3, "EPSG:32633").write(str(original))
    for extension in ["shp", "shx", "dbf", "prj", "cpg"]:
        original.with_suffix("." + extension).rename(original.with_suffix("." + extension.title()))
    target = original.with_suffix(".Shp")
    for layer in [terrio.open_vector(str(target))[0], terrio.open_stream(str(target)).read_all()]:
        assert layer.crs == "EPSG:32633"
        assert layer.to_geojson()["features"][0]["properties"] == {"id": 3, "name": "café"}
    assert tpg.read_info(str(target))["crs"] == "EPSG:32633"
