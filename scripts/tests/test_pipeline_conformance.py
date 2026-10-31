"""The pipeline verifier must reject relative errors in small geometries."""

import sys
from pathlib import Path

import pytest
from osgeo import ogr

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_pipeline_with_gdal import compare_vector


def write(path, wkt):
    ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(path))
    layer = ds.CreateLayer("result", geom_type=ogr.wkbUnknown)
    feature = ogr.Feature(layer.GetLayerDefn())
    feature.SetGeometry(ogr.CreateGeometryFromWkt(wkt))
    layer.CreateFeature(feature)
    ds = None
    return path


@pytest.mark.parametrize(
    "reference,actual,error",
    [
        (
            "POLYGON ((0 0,0.001 0,0.001 0.001,0 0.001,0 0))",
            "POLYGON ((0 0,0.0011 0,0.0011 0.001,0 0.001,0 0))",
            "relative polygon",
        ),
        (
            "LINESTRING (0 0,0.000001 0)",
            "LINESTRING (0 0,0.000001001 0)",
            "relative line",
        ),
    ],
)
def test_small_geometry_relative_error_is_not_hidden_by_unit_floor(
    tmp_path, reference, actual, error
):
    expected = write(tmp_path / "reference.gpkg", reference)
    output = write(tmp_path / "actual.gpkg", actual)
    with pytest.raises(AssertionError, match=error):
        compare_vector(output, expected)


def test_missing_gdal_executable_skips_instead_of_reporting_differences(monkeypatch):
    import conformance

    monkeypatch.setattr(conformance, "missing_modules", lambda *args: False)
    monkeypatch.setattr(conformance.shutil, "which", lambda name: None)
    suite = conformance.suite_pipeline(None)
    assert "pipeline command" in suite.skipped
    assert not suite.cases
