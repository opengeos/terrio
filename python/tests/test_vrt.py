"""Serialized VRT inputs preserve lazy windows, source maps and georeferencing."""

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
    with pytest.raises(terrio.UnsupportedError):
        terrio.open_vrt(
            xml.replace("sum", "external_custom_function"), {"upload.tif": path.read_bytes()}
        )
