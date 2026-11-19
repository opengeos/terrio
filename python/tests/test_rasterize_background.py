"""Edit DEMs with vector attributes and compare with the installed GDAL.

Local reference: GDAL 3.12.1; CI's conformance environment uses GDAL 3.12.2.
The shared rasterizer's algorithm reference is GDAL 3.13.3.
"""

import json
import subprocess
import sys

import numpy as np
import pytest

import terrio


@pytest.mark.parametrize("polygon", [False, True])
@pytest.mark.parametrize("add", [False, True])
def test_cli_background_without_gdal(tmp_path, polygon, add):
    """Check CLI edits and full-extent retention without optional dependencies.

    Args:
        tmp_path: Scratch directory.
        polygon: Burn a polygon instead of a line.
        add: Add a signed offset instead of replacing elevations.
    """
    values = np.arange(36, dtype="float32").reshape(6, 6) + 100
    background = tmp_path / "dem.tif"
    terrio.Raster.from_array(values, (0, 1, 0, 6, 0, -1)).write(str(background))
    geometry = (
        {"type": "Polygon", "coordinates": [[[1, 1], [3, 1], [3, 3], [1, 3], [1, 1]]]}
        if polygon
        else {"type": "LineString", "coordinates": [[1.5, 1.5], [3.5, 1.5]]}
    )
    source = tmp_path / "edits.geojson"
    source.write_text(
        json.dumps({"type": "Feature", "properties": {"Elevation": -2.5}, "geometry": geometry})
    )
    output = tmp_path / "edited.tif"
    command = [
        sys.executable,
        "-m",
        "terrio",
        "rasterize",
        str(source),
        str(output),
        "--background",
        str(background),
        "--attribute",
        "Elevation",
    ]
    if add:
        command.append("--add")
    subprocess.run(command, check=True, capture_output=True, text=True)
    expected = values.copy()
    affected = np.s_[3:5, 1:3] if polygon else np.s_[4, 1:4]
    expected[affected] = values[affected] - 2.5 if add else -2.5
    result = terrio.open_raster(str(output))
    np.testing.assert_array_equal(result.read(1), expected)
    np.testing.assert_array_equal(terrio.open_raster(str(background)).read(1), values)
    assert (result.width, result.height, result.dtype) == (6, 6, "float32")
    assert result.transform == (0, 1, 0, 6, 0, -1)


@pytest.mark.parametrize("format", ["GTiff", "COG"])
@pytest.mark.parametrize(
    "dtype,bits,burn",
    [("uint8", 1, 2), ("uint8", 4, 7), ("uint16", 12, 4096), ("uint32", 20, 1048576)],
)
def test_background_packing_preserves_edited_values(tmp_path, format, dtype, bits, burn):
    """Retain compatible NBITS and widen storage when an edit exceeds it.

    Args:
        tmp_path: Scratch directory.
        format: Output format.
        dtype: Background sample type.
        bits: Background stored width.
        burn: Edited pixel value.
    """
    from terrio import rio

    values = np.ones((8, 8), dtype=dtype)
    background = tmp_path / "packed.tif"
    terrio.Raster.from_array(values, (0, 1, 0, 8, 0, -1)).write(
        str(background), options={"NBITS": str(bits)}
    )
    output = tmp_path / "edited.tif"
    terrio.rasterize(
        {"type": "Point", "coordinates": [1.5, 1.5]},
        str(output),
        background=str(background),
        burn=burn,
        format=format,
    )
    expected = values.copy()
    expected[6, 1] = burn
    with rio.open(str(output)) as result:
        np.testing.assert_array_equal(result.read(1), expected)
        assert result.dtypes == (dtype,)
        assert result.tags(1, ns="IMAGE_STRUCTURE").get("NBITS") == (
            str(bits) if burn < 2**bits else None
        )


@pytest.mark.parametrize(
    "geometry",
    [
        "LINESTRING (1 5, 5 1, 1 5)",
        "POLYGON ((1 1, 5 1, 5 5, 1 5, 1 1))",
        "MULTIPOINT ((1.1 5.1), (1.7 5.7))",
    ],
)
@pytest.mark.parametrize("add", [False, True])
@pytest.mark.parametrize("all_touched", [False, True])
def test_shapefile_elevation_preserves_dem(tmp_path, geometry, add, all_touched):
    """Compare signed attribute edits, untouched pixels, masks and metadata with GDAL.

    Args:
        tmp_path: Scratch directory.
        geometry: Line, polygon or coincident-pixel MultiPoint WKT.
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
    shape = ogr.CreateGeometryFromWkt(geometry)
    layer = vectors.CreateLayer("edits", crs, geom_type=shape.GetGeometryType())
    layer.CreateField(ogr.FieldDefn("Elevation", ogr.OFTReal))
    for value in (2.5, -1.25):
        feature = ogr.Feature(layer.GetLayerDefn())
        feature.SetField("Elevation", value)
        feature.SetGeometry(shape)
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


@pytest.mark.parametrize("features", [1, 2])
def test_background_multipoint_adds_per_coordinate(features):
    """Add each coordinate within a MultiPoint and accumulate independent features.

    Args:
        features: Number of coincident MultiPoint features.
    """
    background = terrio.Raster.from_array(np.array([[10.0]]), (0, 1, 0, 1, 0, -1))
    source = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {},
                "geometry": {"type": "MultiPoint", "coordinates": [[0.2, 0.2], [0.7, 0.7]]},
            }
        ]
        * features,
    }
    result = terrio.rasterize(source, background=background, burn=-2.5, add=True)
    np.testing.assert_array_equal(result.read(1), [[10.0 - 5.0 * features]])
    np.testing.assert_array_equal(background.read(1), [[10.0]])


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
