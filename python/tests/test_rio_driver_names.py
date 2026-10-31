"""Datasets whose name points to another format than their content, opened by `terrio.rio` as
GDAL opens them: GDAL's drivers identify files by their content.

- A GeoTIFF named `.jp2` opens with `driver="GTiff"` (a tile server's test of a forced
  driver on a `.jp2` name); terrio refused it.
- A VRT named `.tif` opens as a VRT when no driver is forced; terrio refused it.

The forced driver still refuses what it would not open: a VRT or a JPEG 2000 named `.tif` with
`driver="GTiff"`, a GeoTIFF with `driver="JP2OpenJPEG"`. GDAL is the oracle. Skipped without
rasterio.
"""

import shutil

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
pytest.importorskip("rasterio.shutil")

from rasterio.errors import RasterioIOError as GdalIOError  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.errors import RasterioIOError  # noqa: E402


def _tiff(path):
    data = np.arange(60 * 40, dtype=np.uint32).reshape(1, 40, 60).astype("uint8")
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=60,
        height=40,
        count=1,
        dtype="uint8",
        crs="EPSG:32633",
        transform=from_origin(500000, 5000000, 10, 10),
    ) as dst:
        dst.write(data)
    return data


def _vrt(path, source):
    with open(path, "w") as f:
        f.write(
            '<VRTDataset rasterXSize="60" rasterYSize="40"><VRTRasterBand dataType="Byte" band="1">'
            f'<SimpleSource><SourceFilename relativeToVRT="0">{source}</SourceFilename>'
            "<SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
        )


@pytest.mark.parametrize("name", ["a.jp2", "a.png", "a.JP2", "a.tif"])
def test_a_gtiff_named_otherwise_opens_with_the_gtiff_driver(tmp_path, name):
    src = str(tmp_path / "src.tif")
    data = _tiff(src)
    path = str(tmp_path / name)
    if path != src:
        shutil.copy(src, path)
    with rasterio.open(path, driver="GTiff") as g, trio.open(path, driver="GTiff") as t:
        np.testing.assert_array_equal(g.read(), data)
        np.testing.assert_array_equal(t.read(), data)
        assert t.driver == g.driver == "GTiff"


def test_a_forced_driver_still_refuses_what_it_would_not_open(tmp_path):
    src = str(tmp_path / "src.tif")
    _tiff(src)
    tif_named_jp2 = str(tmp_path / "a.jp2")
    shutil.copy(src, tif_named_jp2)
    vrt_named_tif = str(tmp_path / "v.tif")
    _vrt(vrt_named_tif, src)
    jp2_named_tif = str(tmp_path / "j.tif")
    with rasterio.open(src) as ds:
        rasterio.shutil.copy(ds, str(tmp_path / "real.jp2"), driver="JP2OpenJPEG")
    shutil.copy(str(tmp_path / "real.jp2"), jp2_named_tif)
    for path, driver in (
        (tif_named_jp2, "JP2OpenJPEG"),
        (vrt_named_tif, "GTiff"),
        (jp2_named_tif, "GTiff"),
    ):
        with pytest.raises(GdalIOError):
            rasterio.open(path, driver=driver)
        with pytest.raises(RasterioIOError):
            trio.open(path, driver=driver)


@pytest.mark.parametrize("name", ["v.tif", "v.vrt", "v"])
def test_a_vrt_named_otherwise_opens_as_a_vrt(tmp_path, name):
    src = str(tmp_path / "src.tif")
    data = _tiff(src)
    path = str(tmp_path / name)
    _vrt(path, src)
    with rasterio.open(path) as g, trio.open(path) as t:
        np.testing.assert_array_equal(g.read(), data)
        np.testing.assert_array_equal(t.read(), data)
