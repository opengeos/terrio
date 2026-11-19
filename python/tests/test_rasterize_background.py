"""Edit DEM elevations with shapefile attributes, compared with GDAL 3.13.3."""

import subprocess
import sys

import numpy as np
import pytest

import terrio


@pytest.mark.parametrize(
    "geometry", ["LINESTRING (1 5, 5 1, 1 5)", "POLYGON ((1 1, 5 1, 5 5, 1 5, 1 1))"]
)
@pytest.mark.parametrize("add", [False, True])
@pytest.mark.parametrize("all_touched", [False, True])
def test_shapefile_elevation_preserves_dem(tmp_path, geometry, add, all_touched):
    """Compare signed attribute edits, untouched pixels, masks and metadata with GDAL.

    Args:
        tmp_path: Scratch directory.
        geometry: Line or polygon WKT.
        add: Add offsets instead of replacing elevations.
        all_touched: Include every pixel touched by a shape.
    """
    gdal = pytest.importorskip("osgeo.gdal")
    ogr = pytest.importorskip("osgeo.ogr")
    osr = pytest.importorskip("osgeo.osr")
    osr.UseExceptions()
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(32610)
    shp = tmp_path / "edits.shp"
    vectors = ogr.GetDriverByName("ESRI Shapefile").CreateDataSource(str(shp))
    layer = vectors.CreateLayer("edits", crs, geom_type=ogr.wkbUnknown)
    layer.CreateField(ogr.FieldDefn("Elevation", ogr.OFTReal))
    for value in (2.5, -1.25):
        feature = ogr.Feature(layer.GetLayerDefn())
        feature.SetField("Elevation", value)
        feature.SetGeometry(ogr.CreateGeometryFromWkt(geometry))
        layer.CreateFeature(feature)
    vectors = None
    values = np.arange(64, dtype="float32").reshape(8, 8) + 100
    values[0, 0] = -9999
    values[4, 2] = -9999
    transform = (0, 1, 0, 8, 0, -1)
    background = tmp_path / "dem.tif"
    source = gdal.GetDriverByName("GTiff").Create(str(background), 8, 8, 1, gdal.GDT_Float32)
    source.SetGeoTransform(transform)
    source.SetProjection(crs.ExportToWkt())
    source.SetMetadataItem("purpose", "elevation edits")
    source.GetRasterBand(1).SetNoDataValue(-9999)
    source.GetRasterBand(1).WriteArray(values)
    source = None
    expected = gdal.GetDriverByName("MEM").CreateCopy("", gdal.Open(str(background)))
    gdal.Rasterize(
        expected,
        str(shp),
        options=gdal.RasterizeOptions(attribute="Elevation", add=add, allTouched=all_touched),
    )
    output = tmp_path / "edited.tif"
    actual = terrio.rasterize(
        str(shp),
        str(output),
        background=str(background),
        attribute="Elevation",
        add=add,
        all_touched=all_touched,
    )
    np.testing.assert_array_equal(actual.read(1), expected.ReadAsArray())
    np.testing.assert_array_equal(terrio.open_raster(str(background)).read(1), values)
    assert np.count_nonzero(actual.read(1) != values) > 0
    written = gdal.Open(str(output))
    np.testing.assert_array_equal(written.ReadAsArray(), expected.ReadAsArray())
    np.testing.assert_array_equal(
        written.GetRasterBand(1).GetMaskBand().ReadAsArray(),
        expected.GetRasterBand(1).GetMaskBand().ReadAsArray(),
    )
    assert written.GetGeoTransform() == transform
    assert written.GetSpatialRef().IsSame(crs)
    assert written.GetRasterBand(1).DataType == gdal.GDT_Float32
    assert written.GetRasterBand(1).GetNoDataValue() == -9999
    assert written.GetMetadataItem("purpose") == "elevation edits"
    cli_output = tmp_path / "cli-edited.tif"
    command = [
        sys.executable,
        "-m",
        "terrio",
        "rasterize",
        str(shp),
        str(cli_output),
        "--background",
        str(background),
        "--attribute",
        "Elevation",
    ]
    if add:
        command.append("--add")
    if all_touched:
        command.append("--at")
    subprocess.run(command, check=True, capture_output=True, text=True)
    np.testing.assert_array_equal(gdal.Open(str(cli_output)).ReadAsArray(), expected.ReadAsArray())


def test_background_object_and_conflicting_options():
    """Keep in-memory inputs intact and refuse incompatible creation options."""
    values = np.full((2, 6, 6), 10, dtype="int16")
    background = terrio.Raster.from_array(values, (0, 1, 0, 6, 0, -1))
    points = {"type": "Point", "coordinates": [1.5, 1.5]}
    result = terrio.rasterize(points, background=background, burn=[3, 4])
    expected = values.copy()
    expected[:, 4, 1] = [3, 4]
    np.testing.assert_array_equal(result.read(), expected)
    np.testing.assert_array_equal(background.read(), values)
    for options in (
        {"like": background},
        {"tr": (1, 1)},
        {"ts": (6, 6)},
        {"te": (0, 0, 6, 6)},
        {"tap": True},
        {"ot": "Float32"},
        {"init": 0},
        {"nodata": -1},
        {"a_srs": "EPSG:4326"},
    ):
        with pytest.raises(terrio.InvalidArgumentError, match="background cannot"):
            terrio.rasterize(points, background=background, burn=3, **options)
