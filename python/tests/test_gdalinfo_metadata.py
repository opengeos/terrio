"""TIFF metadata `gdalinfo -json` shows, as GDAL reads it: JPEG quality, NBITS, 1-bit tables.

A pipeline's `raster_info` filter reproduces `gdalinfo -json` with `terrio.rio`; these are the
items of a TIFF it takes from terrio: the `JPEG_QUALITY` and `JPEGTABLESMODE` GDAL guesses from
the `JPEGTables` tag, the bands' `NBITS` when the bits per sample are not the data type's, and
the black and white color table of a 1-bit band. Each file is written by GDAL (rasterio) and
read by both; skipped without rasterio.
"""

import numpy as np
import pytest

import terrio.rio as trio

rasterio = pytest.importorskip("rasterio")
from rasterio.transform import from_origin  # noqa: E402

BASE = dict(
    driver="GTiff",
    width=64,
    height=48,
    crs="EPSG:32631",
    transform=from_origin(500000, 4600000, 10, 10),
)


@pytest.mark.parametrize(
    ("bands", "photometric", "quality"),
    [(3, "YCBCR", 85), (3, "YCBCR", 75), (3, "RGB", 90), (1, None, 60), (1, None, 100)]
    # The layouts GDAL finds the quality of by writing a file at every quality.
    + [(2, None, 75), (2, None, 31), (4, "RGB", 88), (3, "MINISBLACK", 50)],
)
def test_jpeg_quality_is_gdals(tmp_path, bands, photometric, quality):
    path = tmp_path / "jpeg.tif"
    extra = {"photometric": photometric} if photometric else {}
    data = np.random.default_rng(quality).integers(0, 255, (bands, 48, 64), dtype="uint8")
    with rasterio.open(
        path,
        "w",
        **BASE,
        count=bands,
        dtype="uint8",
        compress="JPEG",
        jpeg_quality=quality,
        tiled=True,
        blockxsize=32,
        blockysize=32,
        **extra,
    ) as dst:
        dst.write(data)
    with rasterio.open(path) as g, trio.open(path) as t:
        expected = g.tags(ns="IMAGE_STRUCTURE")
        assert expected["JPEG_QUALITY"] == str(quality)
        assert t.tags(ns="IMAGE_STRUCTURE") == expected


@pytest.mark.parametrize(
    ("nbits", "dtype", "photometric"),
    [(1, "uint8", None), (2, "uint8", None), (4, "uint8", None), (12, "uint16", None)],
)
def test_nbits_and_one_bit_color_table_are_gdals(tmp_path, nbits, dtype, photometric):
    path = tmp_path / "nbits.tif"
    extra = {"photometric": photometric} if photometric else {}
    data = (np.arange(48 * 64).reshape(1, 48, 64) % (1 << nbits)).astype(dtype)
    with rasterio.open(path, "w", **BASE, count=1, dtype=dtype, nbits=nbits, **extra) as dst:
        dst.write(data)
    with rasterio.open(path) as g, trio.open(path) as t:
        assert g.tags(1, ns="IMAGE_STRUCTURE") == {"NBITS": str(nbits)}
        assert t.tags(1, ns="IMAGE_STRUCTURE") == g.tags(1, ns="IMAGE_STRUCTURE")
        assert [c.name for c in t.colorinterp] == [c.name for c in g.colorinterp]
        if g.colorinterp[0].name == "palette":
            assert t.colormap(1) == g.colormap(1)


def test_miniswhite_is_gdals(tmp_path):
    path = tmp_path / "miniswhite.tif"
    data = (np.arange(48 * 64).reshape(1, 48, 64) % 2).astype("uint8")
    with rasterio.open(path, "w", **BASE, count=1, dtype="uint8", nbits=1, photometric="MINISWHITE") as dst:
        dst.write(data)
    with rasterio.open(path) as g, trio.open(path) as t:
        assert g.tags(ns="IMAGE_STRUCTURE")["MINISWHITE"] == "YES"
        assert t.tags(ns="IMAGE_STRUCTURE") == g.tags(ns="IMAGE_STRUCTURE")


@pytest.mark.parametrize("crs", ["EPSG:4326+5773", "EPSG:32631+5773"])
def test_compound_crs_code_and_band_unit_are_gdals(tmp_path, crs):
    # GDAL names the registry's compound CRS of a GeoTIFF's two codes (EPSG:9707 for the first,
    # none for the second), and gives the bands the unit of the heights.
    path = tmp_path / "compound.tif"
    profile = dict(BASE, crs=rasterio.crs.CRS.from_string(crs))
    with rasterio.open(path, "w", **profile, count=1, dtype="float32") as dst:
        dst.write(np.zeros((1, 48, 64), "float32"))
    with rasterio.open(path) as g, trio.open(path) as t:
        assert g.units == ("metre",)
        assert (t.units, t.crs.to_epsg()) == (g.units, g.crs.to_epsg())
        assert t.crs.to_wkt() == g.crs.to_wkt()


def test_esri_crs_of_a_geotiff_is_gdals(tmp_path):
    # An ESRI CRS goes into an ESRI PE citation, which GDAL reads without the ESRI authority.
    path = tmp_path / "mollweide.tif"
    profile = dict(BASE, crs=rasterio.crs.CRS.from_string("ESRI:54009"))
    with rasterio.open(path, "w", **profile, count=1, dtype="uint8") as dst:
        dst.write(np.zeros((1, 48, 64), "uint8"))
    with rasterio.open(path) as g, trio.open(path) as t:
        assert "AUTHORITY" not in g.crs.to_wkt()[-30:]
        assert t.crs.to_wkt() == g.crs.to_wkt()
        assert (t.crs.to_epsg(), t.crs.to_authority()) == (g.crs.to_epsg(), g.crs.to_authority())
