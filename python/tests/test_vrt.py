"""Serialized VRT inputs preserve lazy windows, source maps and georeferencing."""

import os

import numpy as np
import pytest

import terrio


def test_vrt_source_map_and_warp_output(tmp_path):
    path = tmp_path / "source.tif"
    source = terrio.Raster.from_array(
        np.arange(25, dtype="float32").reshape(5, 5), transform=(0, 1, 0, 5, 0, -1), crs="EPSG:3857"
    )
    source.write(str(path))
    xml = (
        '<VRTDataset rasterXSize="5" rasterYSize="5"><SRS>EPSG:3857</SRS>'
        "<GeoTransform>0,1,0,5,0,-1</GeoTransform>"
        '<VRTRasterBand dataType="Float32" subClass="VRTDerivedRasterBand">'
        '<PixelFunctionType>sum</PixelFunctionType><PixelFunctionArguments k="2"/>'
        "<SimpleSource><SourceFilename>upload.tif</SourceFilename>"
        "<SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
    )
    for value in [path, path.read_bytes()]:
        result = terrio.open_vrt(xml, {"upload.tif": value})
        np.testing.assert_array_equal(result.read(1, window=(1, 2, 2, 2)), [[13, 14], [18, 19]])
    destination = tmp_path / "warped.vrt"
    warped = terrio.warp(str(path), str(destination), ts=(3, 3))
    assert "VRTWarpedDataset" in destination.read_text()
    expected = terrio.warp(str(path), ts=(3, 3))
    np.testing.assert_array_equal(warped.read(), expected.read())
    sequenced = terrio.warp([str(path)], str(tmp_path / "sequenced.vrt"), ts=(3, 3))
    np.testing.assert_array_equal(sequenced.read(), expected.read())
    x_path, y_path = tmp_path / "x.tif", tmp_path / "y.tif"
    for coordinate_path, values in [
        (x_path, np.tile(np.linspace(10, 14, 5), (5, 1))),
        (y_path, np.tile(np.linspace(50, 46, 5)[:, None], (1, 5))),
    ]:
        terrio.Raster.from_array(values, transform=(0, 1, 0, 5, 0, -1)).write(str(coordinate_path))
    with pytest.raises(terrio.UnsupportedError, match="serialized geolocation"):
        terrio.warp(
            str(path),
            str(tmp_path / "geolocation.vrt"),
            geolocation={"x": str(x_path), "y": str(y_path), "srs": "EPSG:4326"},
        )
    with pytest.raises(terrio.UnsupportedError):
        terrio.open_vrt(
            xml.replace("sum", "external_custom_function"), {"upload.tif": path.read_bytes()}
        )


def test_vrt_mixed_band_types_keep_fractional_precision(tmp_path):
    path = tmp_path / "fractional.tif"
    values = np.array([[1.250000000001, 2.75]], dtype="float64")
    terrio.Raster.from_array(values, transform=(0, 1, 0, 1, 0, -1)).write(str(path))
    bands = "".join(
        f'<VRTRasterBand dataType="{dtype}"><SimpleSource>'
        "<SourceFilename>source.tif</SourceFilename><SourceBand>1</SourceBand>"
        "</SimpleSource></VRTRasterBand>"
        for dtype in ["Byte", "Float32", "Float64"]
    )
    result = terrio.open_vrt(
        f'<VRTDataset rasterXSize="2" rasterYSize="1">{bands}</VRTDataset>',
        {"source.tif": path.read_bytes()},
    )
    assert result.read().dtype == np.dtype("float64")
    np.testing.assert_array_equal(result.read(1), [[1, 3]])
    np.testing.assert_array_equal(result.read(2), values.astype("float32"))
    np.testing.assert_array_equal(result.read(3), values)


def test_vrt_preserves_filename_whitespace_in_native_and_uploaded_sources(tmp_path):
    values = {"source.tif": 1, "source.tif ": 9, " source.tif ": 7}
    uploaded = {
        name: terrio.Raster.from_array(
            np.array([[value]], dtype="uint8"), transform=(0, 1, 0, 1, 0, -1)
        ).to_bytes()
        for name, value in values.items()
    }
    source_maps = [uploaded]
    if os.name == "posix":
        native = {}
        for name, data in uploaded.items():
            path = tmp_path / name
            path.write_bytes(data)
            native[name] = path
        source_maps.append(native)
    for text, expected in [
        ("source.tif ", 9),
        (" source.tif", 1),
        ("&#32;source.tif ", 7),
        ("<![CDATA[ source.tif ]]>", 7),
        ("<![CDATA[source.tif]]> ", 1),
    ]:
        xml = (
            '<VRTDataset rasterXSize="1" rasterYSize="1">'
            '<VRTRasterBand dataType="Byte" band="1"><SimpleSource>'
            f"<SourceFilename>{text}</SourceFilename><SourceBand>1</SourceBand>"
            "</SimpleSource></VRTRasterBand></VRTDataset>"
        )
        for sources in source_maps:
            np.testing.assert_array_equal(terrio.open_vrt(xml, sources).read(1), [[expected]])
