"""GML names or refuses retained CRS definitions; CSV explicitly reports their loss."""

import subprocess
import sys
import warnings

import pytest
from test_geojson_write_crs import CUSTOM, LOCAL

import terrio

DEFINITIONS = ["EPSG:32615", "ESRI:102003", "IAU_2015:49900", CUSTOM, LOCAL]


def points(definition):
    """Build a layer whose small coordinates expose accidental WGS 84 inference.

    Args:
        definition: Original coordinate reference system.

    Returns:
        terrio.Layer: One point near the coordinate-system origin.
    """
    return terrio.Layer.from_geojson(
        {
            "type": "Feature",
            "properties": {"id": 1},
            "geometry": {"type": "Point", "coordinates": [12, 34]},
        },
        name="points",
        crs=definition,
    )


@pytest.mark.parametrize("definition", DEFINITIONS)
@pytest.mark.parametrize("extension", ["gml", "csv"])
@pytest.mark.parametrize("surface", ["layer", "bytes", "convert", "cli"])
def test_writers_do_not_silently_drop_crs(tmp_path, definition, extension, surface):
    """Preserve a nameable CRS or explicitly report that the format will lose it.

    Args:
        tmp_path: Temporary dataset directory.
        definition: Source CRS.
        extension: Output format.
        surface: Public write entry point.
    """
    layer = points(definition)
    source = tmp_path / "source.gpkg"
    layer.write(str(source))
    target = tmp_path / f"points.{extension}"
    named = definition.startswith(("EPSG:", "ESRI:", "IAU_2015:"))
    refused = extension == "gml" and not named

    def write():
        """Write through the chosen API and return CLI diagnostics if applicable."""
        if surface == "layer":
            layer.write(str(target))
        elif surface == "bytes":
            target.write_bytes(layer.to_bytes(target.name))
        elif surface == "convert":
            terrio.convert(str(source), str(target))
        else:
            result = subprocess.run(
                [sys.executable, "-m", "terrio", "convert", str(source), str(target)],
                capture_output=True,
                text=True,
                timeout=30,
            )
            assert (result.returncode != 0) == refused, result.stderr
            return result.stderr
        return ""

    if refused:
        if surface == "cli":
            assert "GML cannot name" in write()
        else:
            with pytest.raises(terrio.UnsupportedError, match="GML cannot name"):
                write()
        assert not target.exists()
        assert not target.with_suffix(".xsd").exists()
        return
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        diagnostics = write()
    if extension == "csv":
        if surface == "cli":
            assert diagnostics.count("CSV does not store this layer's CRS") == 1
        else:
            assert sum("CSV does not store this layer's CRS" in str(w.message) for w in caught) == 1
        assert not target.with_suffix(".prj").exists()
    else:
        assert not caught
        authority, code = definition.split(":")
        assert f'srsName="urn:ogc:def:crs:{authority}::{code}"' in target.read_text()
    back = terrio.open_vector(str(target))[0]
    assert back.to_geojson()["features"][0]["geometry"]["coordinates"] == [12, 34]
    if extension == "gml":
        assert back.crs == definition


@pytest.mark.parametrize("surface", ["dataset", "function"])
def test_additional_csv_writers_forward_crs_warning(tmp_path, surface):
    """Forward driver warnings through dataset and standalone writer bindings.

    Args:
        tmp_path: Temporary dataset directory.
        surface: Public writer binding.
    """
    layer = points(LOCAL)
    target = tmp_path / "points.csv"
    source = tmp_path / "source.gpkg"
    layer.write(str(source))
    with pytest.warns(UserWarning, match="CSV does not store this layer's CRS"):
        if surface == "dataset":
            terrio.open_vector(str(source)).write(str(target))
        else:
            terrio.write_vector(layer, str(target))


def test_matching_csv_prj_restores_local_coordinates(tmp_path):
    """The warning's matching-sidecar remedy prevents false degree-coordinate inference.

    Args:
        tmp_path: Temporary dataset directory.
    """
    layer = points(LOCAL)
    path = tmp_path / "points.csv"
    with pytest.warns(UserWarning, match="supply a matching .prj"):
        layer.write(str(path))
    assert terrio.open_vector(str(path))[0].crs == "EPSG:4326"
    path.with_suffix(".prj").write_text(layer.crs_wkt)
    assert terrio.open_vector(str(path))[0].crs_wkt == layer.crs_wkt


@pytest.mark.parametrize("definition", ["EPSG:6708", "ESRI:102003", "IAU_2015:49900"])
def test_named_gml_coordinates_agree_with_gdal(tmp_path, definition):
    """Compare authority-order output with GDAL 3.12.1's GML reader.

    Args:
        tmp_path: Temporary dataset directory.
        definition: CRS with projected or geographic authority axes.
    """
    ogr = pytest.importorskip("osgeo.ogr")
    path = tmp_path / "points.gml"
    points(definition).write(str(path))
    with ogr.ExceptionMgr():
        dataset = ogr.Open(str(path))
        try:
            feature = dataset.GetLayer(0).GetNextFeature()
            geometry = feature.GetGeometryRef()
            assert (geometry.GetX(), geometry.GetY()) == (12, 34)
        finally:
            dataset = None


def test_unnamed_compound_gml_is_refused(tmp_path):
    """Refuse unnamed compound definitions before creating either output file.

    Args:
        tmp_path: Temporary dataset directory.
    """
    path = tmp_path / "compound.gml"
    with pytest.raises(terrio.UnsupportedError, match="GML cannot name"):
        points("EPSG:6341+5773").write(str(path))
    assert not path.exists()
    assert not path.with_suffix(".xsd").exists()


@pytest.mark.parametrize("altered", [False, True])
def test_declared_esri_parameters_are_checked_before_naming(tmp_path, altered):
    """Use an ESRI name only when retained projection parameters agree with it.

    Args:
        tmp_path: Temporary dataset directory.
        altered: Whether the stored central meridian contradicts its authority.
    """
    definition = points("ESRI:102003").crs_wkt
    if altered:
        changed = definition.replace(
            'PARAMETER["central_meridian",-96]', 'PARAMETER["central_meridian",-95]'
        )
        assert changed != definition
        definition = changed
    layer = points(definition)
    path = tmp_path / "declared.gml"
    if altered:
        with pytest.raises(terrio.UnsupportedError, match="GML cannot name"):
            layer.write(str(path))
        assert not path.exists()
        assert not path.with_suffix(".xsd").exists()
    else:
        layer.write(str(path))
        assert "urn:ogc:def:crs:ESRI::102003" in path.read_text()
