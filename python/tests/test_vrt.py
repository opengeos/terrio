"""Serialized VRT inputs preserve lazy windows, source maps and georeferencing."""

import socket

import numpy as np
import pytest

import terrio
import terrio.rio as trio
from terrio.rio.errors import RasterioIOError
from terrio.rio.windows import Window


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


def test_vrt_windows_over_reachable_sources_read_when_another_is_down(tmp_path):
    """A mosaic of a local tile (7) and a remote one (9) whose server is down, as GDAL reads it:
    a source is opened only by a read that reaches its destination rectangle
    (`VRTSimpleSource::GetSrcDstWindow()`), so windows over the local tile read its values and a
    window reaching the remote one fails, naming it (as GDAL does the first time)."""
    left = tmp_path / "left.tif"
    terrio.Raster.from_array(
        np.full((256, 256), 7, dtype="uint8"),
        transform=(500000, 10, 0, 4000000, 0, -10),
        crs="EPSG:32633",
    ).write(str(left))
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    remote = f"/vsicurl/http://127.0.0.1:{port}/right.tif"

    def source(name, relative, x):
        return (
            f'<SimpleSource><SourceFilename relativeToVRT="{relative}">{name}</SourceFilename>'
            '<SourceBand>1</SourceBand><SrcRect xOff="0" yOff="0" xSize="256" ySize="256"/>'
            f'<DstRect xOff="{x}" yOff="0" xSize="256" ySize="256"/></SimpleSource>'
        )

    path = tmp_path / "mosaic.vrt"
    path.write_text(
        '<VRTDataset rasterXSize="512" rasterYSize="256"><SRS>EPSG:32633</SRS>'
        "<GeoTransform>500000, 10, 0, 4000000, 0, -10</GeoTransform>"
        '<VRTRasterBand dataType="Byte" band="1"><NoDataValue>0</NoDataValue>'
        f"{source('left.tif', 1, 0)}{source(remote, 0, 256)}</VRTRasterBand></VRTDataset>"
    )
    with trio.open(str(path)) as ds:
        for window, shape in (((0, 0, 256, 256), (256, 256)), ((10, 10, 100, 50), (50, 100))):
            data = ds.read(1, window=Window(*window))
            assert data.shape == shape and np.unique(data).tolist() == [7], window
            assert np.unique(ds.read_masks(1, window=Window(*window))).tolist() == [255], window
        for window in ((200, 0, 100, 10), (0, 0, 257, 4), (300, 0, 10, 10), None):
            with pytest.raises(RasterioIOError, match=f"127.0.0.1:{port}/right.tif"):
                ds.read(1, window=None if window is None else Window(*window))
