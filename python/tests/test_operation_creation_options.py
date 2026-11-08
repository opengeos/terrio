"""Creation options reach raster writers without changing the operation's result.

Local storage and metadata readback was validated with GDAL 3.13.3. In CI, GDAL
readback tests run in the real-data job with GDAL_REFERENCE_VERSION from ci.yml.
These readback tests skip when osgeo is unavailable, including in the default Python job.
"""

import numpy as np
import pytest

import terrio


@pytest.fixture
def gdal():
    """Load GDAL only for tests that inspect its readback."""
    module = pytest.importorskip("osgeo.gdal")
    module.UseExceptions()
    return module


CREATION = {
    "TILED": "YES",
    "BLOCKXSIZE": "16",
    "BLOCKYSIZE": "16",
    "COMPRESS": "DEFLATE",
}
TRANSFORM = (500000.0, 10.0, 0.0, 4600000.0, 0.0, -10.0)
OPERATIONS = [
    "translate",
    "warp",
    "warp_path",
    "mosaic",
    "dem",
    "dem_path",
    "calc",
    "proximity",
    "sieve",
    "viewshed",
    "fill_nodata",
    "pansharpen",
    "pansharpen_path",
    "unscale",
    "nodata_to_alpha",
    "reclassify",
    "blend",
    "neighbors",
    "nearblack",
    "clean_collar",
    "enhance",
    "rasterize",
    "vector_grid",
    "create_raster",
    "rgb_to_palette",
    "palette_to_rgb",
    "attach_palette",
    "write_raster",
]


@pytest.fixture
def inputs(tmp_path):
    """Build projected rasters and points for storage-option comparisons.

    Args:
        tmp_path: The test's isolated output directory.

    Returns:
        A dictionary of raster, path and vector inputs.
    """
    ramp = np.add.outer(np.arange(40, dtype="float32"), np.arange(40, dtype="float32")) + 10
    ramp[10:12, 10:12] = -9999
    dem = terrio.Raster.from_array(ramp, TRANSFORM, crs="EPSG:32610", nodata=-9999)
    category = np.zeros((40, 40), dtype="uint8")
    category[8:32, 8:32] = 1
    categorical = terrio.Raster.from_array(category, TRANSFORM, crs="EPSG:32610")
    rgb = terrio.Raster.from_array(
        np.stack([category * 80, category * 100, category * 120]),
        TRANSFORM,
        crs="EPSG:32610",
    )
    palette = [(0, 0, 0, 0), (80, 100, 120, 255)]
    indexed = terrio.attach_palette(categorical, palette)
    dem_path, rgb_path = tmp_path / "dem.tif", tmp_path / "rgb.tif"
    dem.write(str(dem_path))
    rgb.write(str(rgb_path))
    points = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {"v": value},
                    "geometry": {"type": "Point", "coordinates": [x, y]},
                }
                for x, y, value in [
                    (500050, 4599650, 1.0),
                    (500350, 4599650, 2.0),
                    (500050, 4599950, 3.0),
                    (500350, 4599950, 4.0),
                ]
            ],
        },
        crs="EPSG:32610",
    )
    return {
        "dem": dem,
        "cat": categorical,
        "rgb": rgb,
        "indexed": indexed,
        "palette": palette,
        "points": points,
        "dem_path": str(dem_path),
        "rgb_path": str(rgb_path),
    }


def write_operation(name, sources, destination, **output):
    """Run one operation with its ordinary arguments and shared writer keywords.

    Args:
        name: The operation or streaming-path case to run.
        sources: The prepared fixture inputs.
        destination: The output raster path.
        **output: Additional output options.

    Returns:
        The operation's normal return value.
    """
    dem, category, rgb = (sources[key] for key in ("dem", "cat", "rgb"))
    path = str(destination)
    if name in ("Raster.write", "write_raster") and "format" in output:
        output["driver"] = output.pop("format")
    if name == "Raster.write":
        return dem.write(path, **output)
    if name == "write_raster":
        return terrio.write_raster(dem, path, **output)
    if name == "create_raster":
        return terrio.create_raster(path, like=dem, **output)
    if name == "calc":
        return terrio.calc("A + 1", path, A=dem, **output)
    if name == "mosaic":
        return terrio.mosaic([dem], path, **output)
    if name in ("dem", "dem_path"):
        source = dem if name == "dem" else sources["dem_path"]
        return terrio.dem(source, "slope", path, **output)
    if name in ("warp", "warp_path"):
        source = dem if name == "warp" else sources["dem_path"]
        return terrio.warp(source, path, t_srs="EPSG:32610", **output)
    if name in ("pansharpen", "pansharpen_path"):
        pan = dem if name == "pansharpen" else sources["dem_path"]
        spectral = rgb if name == "pansharpen" else sources["rgb_path"]
        return terrio.pansharpen(pan, spectral, path, **output)
    if name == "viewshed":
        return terrio.viewshed(dem, 500200, 4599800, path, md=100, **output)
    if name in ("proximity", "sieve"):
        return getattr(terrio, name)(category, path, **output)
    if name == "reclassify":
        return terrio.reclassify(category, "0=2;1=3", path, **output)
    if name == "blend":
        return terrio.blend(rgb, rgb, path, **output)
    if name == "neighbors":
        return terrio.neighbors(dem, path, kernel="equal", size=3, **output)
    if name == "enhance":
        return terrio.enhance(dem, path, equalize=True, **output)
    if name in ("nearblack", "clean_collar"):
        return getattr(terrio, name)(rgb, path, add_mask=True, **output)
    if name == "rgb_to_palette":
        return terrio.rgb_to_palette(rgb, path, color_count=8, **output)
    if name == "palette_to_rgb":
        return terrio.palette_to_rgb(sources["indexed"], path, **output)
    if name == "attach_palette":
        return terrio.attach_palette(category, sources["palette"], path, **output)
    if name == "rasterize":
        return terrio.rasterize(sources["points"], path, burn=7, tr=(10, 10), **output)
    if name == "vector_grid":
        return terrio.vector_grid(
            sources["points"],
            path,
            algorithm="nearest",
            zfield="v",
            size=(40, 40),
            extent=(500000, 4599600, 500400, 4600000),
            **output,
        )
    return getattr(terrio, name)(dem, path, **output)


@pytest.mark.parametrize("operation", OPERATIONS)
def test_creation_options_reach_each_operation_writer(tmp_path, inputs, operation, gdal):
    """Check real TIFF storage tags and preserve all pixel and validity information.

    Args:
        tmp_path: The test's isolated output directory.
        inputs: The raster and vector fixture inputs.
        operation: The operation being verified.
    """
    reference, actual = tmp_path / "reference.tif", tmp_path / "actual.tif"
    write_operation(operation, inputs, reference)
    write_operation(operation, inputs, actual, options=CREATION)
    expected, got = gdal.Open(str(reference)), gdal.Open(str(actual))
    assert got.GetRasterBand(1).GetBlockSize() == [16, 16]
    assert got.GetMetadata("IMAGE_STRUCTURE")["COMPRESSION"] == "DEFLATE"
    assert got.GetGeoTransform() == expected.GetGeoTransform()
    assert got.GetSpatialRef().IsSame(expected.GetSpatialRef())
    assert got.RasterCount == expected.RasterCount
    for band in range(1, got.RasterCount + 1):
        wanted, value = expected.GetRasterBand(band), got.GetRasterBand(band)
        assert value.DataType == wanted.DataType
        np.testing.assert_equal(value.GetNoDataValue(), wanted.GetNoDataValue())
        np.testing.assert_array_equal(value.ReadAsArray(), wanted.ReadAsArray())
        np.testing.assert_array_equal(
            value.GetMaskBand().ReadAsArray(), wanted.GetMaskBand().ReadAsArray()
        )
        table, expected_table = value.GetColorTable(), wanted.GetColorTable()
        if expected_table is not None:
            assert table is not None and table.GetCount() == expected_table.GetCount()
            assert [table.GetColorEntry(i) for i in range(table.GetCount())] == [
                expected_table.GetColorEntry(i) for i in range(expected_table.GetCount())
            ]


@pytest.mark.parametrize("format_name", ["netcdf4", "zarr3"])
def test_format_specific_creation_options(tmp_path, inputs, format_name):
    """Choose the actual container version through the operation's output options.

    Args:
        tmp_path: The test's isolated output directory.
        inputs: The raster fixture inputs.
        format_name: The requested container version.
    """
    if format_name == "netcdf4":
        path = tmp_path / "output.nc"
        options = {"FORMAT": "NC4", "COMPRESS": "DEFLATE"}
    else:
        path = tmp_path / "output.zarr"
        options = {"FORMAT": "ZARR_V3"}
    terrio.translate(inputs["dem"], str(path), options=options)
    if format_name == "netcdf4":
        assert path.read_bytes().startswith(b"\x89HDF\r\n\x1a\n")
    else:
        assert (path / "zarr.json").is_file()
    result = terrio.open_raster(str(path))
    np.testing.assert_array_equal(result.read(), inputs["dem"].read())
    assert result.transform == inputs["dem"].transform


@pytest.mark.parametrize("options", [{}, {"COMPRESS": "DEFLATE"}])
def test_create_raster_creation_options_require_destination(options):
    """Reject file-output options instead of silently ignoring them in memory."""
    with pytest.raises(ValueError, match="file-output options require dst"):
        terrio.create_raster(size=(2, 2), options=options)


@pytest.mark.parametrize(
    "operation", ["translate", "write_raster", "create_raster", "Raster.write"]
)
@pytest.mark.parametrize("format_name", ["GTiff", "COG"])
def test_compression_keyword_overrides_creation_option(
    tmp_path, inputs, operation, format_name, gdal
):
    """Read the actual storage selected when the two compression inputs conflict."""
    path = tmp_path / "compression.tif"
    write_operation(
        operation, inputs, path, format=format_name, compress="LZW", options={"COMPRESS": "DEFLATE"}
    )
    result = gdal.Open(str(path))
    assert result.GetMetadata("IMAGE_STRUCTURE")["COMPRESSION"] == "LZW"


@pytest.mark.parametrize(
    "operation", ["translate", "write_raster", "create_raster", "Raster.write"]
)
@pytest.mark.parametrize("format_name", ["GTiff", "COG"])
def test_bigtiff_keyword_overrides_creation_option(tmp_path, inputs, operation, format_name):
    """Check the on-disk TIFF version when the keyword and creation option conflict."""
    path = tmp_path / "large-offsets.tif"
    write_operation(
        operation, inputs, path, format=format_name, bigtiff=True, options={"BIGTIFF": "NO"}
    )
    assert path.read_bytes()[:4] in (b"II+\x00", b"MM\x00+")


@pytest.mark.parametrize(
    "operation", ["translate", "write_raster", "create_raster", "Raster.write"]
)
@pytest.mark.parametrize(
    ("format_name", "options", "block_size"),
    [
        ("GTiff", {"TILED": "YES", "BLOCKXSIZE": "16", "BLOCKYSIZE": "32"}, [16, 32]),
        ("COG", {"BLOCKSIZE": "32"}, [32, 32]),
    ],
)
def test_creation_block_sizes_override_tile_size(
    tmp_path, inputs, operation, format_name, options, block_size, gdal
):
    """Read actual tile dimensions when explicit creation options override tile_size."""
    path = tmp_path / "blocks.tif"
    write_operation(operation, inputs, path, format=format_name, tile_size=64, options=options)
    result = gdal.Open(str(path))
    assert result.GetRasterBand(1).GetBlockSize() == block_size


@pytest.mark.parametrize("options", [{}, {"COMPRESS": "DEFLATE"}])
@pytest.mark.parametrize("explicit_format", [False, True])
def test_pansharpen_vrt_rejects_creation_options(tmp_path, inputs, options, explicit_format):
    """Reject VRT creation options before publishing an output instead of ignoring them."""
    path = tmp_path / ("output.xml" if explicit_format else "output.vrt")
    output = {"format": "VRT"} if explicit_format else {}
    with pytest.raises(ValueError, match="creation options do not apply to a VRT output"):
        terrio.pansharpen(
            inputs["dem_path"], inputs["rgb_path"], str(path), options=options, **output
        )
    assert not path.exists()


@pytest.mark.parametrize("operation", ["mosaic", "warp", "color_relief"])
@pytest.mark.parametrize("options", [{}, {"COMPRESS": "DEFLATE"}])
@pytest.mark.parametrize("explicit_format", [False, True])
def test_virtual_outputs_reject_creation_options(
    tmp_path, inputs, operation, options, explicit_format
):
    """Virtual outputs refuse writer mappings without publishing an XML file."""
    extension = "xml" if explicit_format and operation != "mosaic" else "vrt"
    path = tmp_path / f"output.{extension}"
    output = {"format": "VRT"} if explicit_format else {}
    with pytest.raises(ValueError, match="creation options do not apply to a VRT output"):
        if operation == "mosaic":
            terrio.mosaic([inputs["dem_path"]], str(path), options=options, **output)
        elif operation == "warp":
            terrio.warp(inputs["dem_path"], str(path), options=options, **output)
        else:
            colors = tmp_path / "colors.txt"
            colors.write_text("0 0 0 0\n100 255 255 255\nnv 0 0 0 0\n")
            terrio.dem(
                inputs["dem_path"],
                "color-relief",
                str(path),
                color_file=str(colors),
                options=options,
                **output,
            )
    assert not path.exists()
