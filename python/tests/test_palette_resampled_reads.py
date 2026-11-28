"""Exact palette RasterIO comparisons against GDAL 3.13."""

from pathlib import Path

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.windows import Window


@pytest.fixture(scope="module")
def palette_sources(tmp_path_factory):
    """The issue's GeoTIFF, a UInt16 palette, and PNG transparency."""
    gdal = pytest.importorskip("osgeo.gdal")
    if int(gdal.VersionInfo("VERSION_NUM")) < 3130000:
        pytest.skip("uses GDAL 3.13 as the reference")
    paths = {}
    root = tmp_path_factory.mktemp("palette_resampling")
    values = (np.arange(97 * 83).reshape(83, 97) % 11).astype(np.uint8)
    for kind in (
        "issue",
        "nodata",
        "wide",
        "transparent",
        "paletted_overview",
        "external_overview",
    ):
        png = kind == "transparent"
        wide = kind == "wide"
        path = root / (f"{kind}.png" if png else f"{kind}.tif")
        driver = gdal.GetDriverByName("MEM" if png else "GTiff")
        with driver.Create(
            "" if png else str(path), 97, 83, 1, gdal.GDT_UInt16 if wide else gdal.GDT_Byte
        ) as ds:
            band = ds.GetRasterBand(1)
            colors = gdal.ColorTable()
            offset = 300 if wide else 0
            for i in range(11):
                rgb = (i * 20, 255 - i * 20, i * 10)
                alpha = {6: 0, 7: 0, 2: 127}.get(i, 255) if png else 255
                colors.SetColorEntry(i + offset, (*rgb, alpha))
            # Duplicate a color at a lower index and retain unused entries in the search.
            if kind != "issue":
                colors.SetColorEntry(offset, colors.GetColorEntry(offset + 1))
            band.SetRasterColorTable(colors)
            data = values.astype(np.uint16) + offset if wide else values.copy()
            nodata = {"nodata": 10, "wide": 310}.get(kind)
            if nodata is not None:
                band.SetNoDataValue(nodata)
                data[5:25, 3:23] = nodata
            if png:
                data[5:25, 3:23] = 7
            else:
                ds.SetGeoTransform((140.0, 0.01, 0.0, -30.0, 0.0, -0.01))
            band.WriteArray(data)
            if png:
                gdal.GetDriverByName("PNG").CreateCopy(str(path), ds).Close()
        if kind in ("paletted_overview", "external_overview"):
            # Reopen after writing the base directory so GDAL sees its palette when
            # it creates the overview.
            mode = gdal.GA_Update if kind == "paletted_overview" else gdal.GA_ReadOnly
            with gdal.Open(str(path), mode) as ds:
                ds.BuildOverviews("NEAREST", [2])
                assert ds.GetRasterBand(1).GetOverview(0).GetColorTable() is not None
        paths[kind] = path
    # This checked-in TIFF's internal overview stores indices without a color table.
    paths["plain_overview"] = (
        Path(__file__).resolve().parents[2]
        / "crates/terrio/tests/data/resampled_reads/paletted.tif"
    )
    return gdal, paths


@pytest.mark.parametrize(
    ("kind", "window", "size"),
    [
        ("issue", (3, 5, 61, 47), (17, 23)),
        # GDAL's Python API rounds its integer window, while Terrio truncates it.
        # Fractions below 0.5 give both the same integer companion window.
        ("issue", (3.25, 5.25, 61.25, 47.25), (17, 23)),
        ("nodata", (3, 5, 61, 47), (17, 23)),
        ("nodata", (3.25, 5.25, 61.25, 47.25), (17, 23)),
        ("wide", (3.25, 5.25, 61.25, 47.25), (17, 23)),
        ("transparent", (3, 5, 61, 47), (17, 23)),
        ("transparent", (3.25, 5.25, 61.25, 47.25), (17, 23)),
        ("paletted_overview", (0, 0, 97, 83), (24, 21)),
        ("external_overview", (0, 0, 97, 83), (24, 21)),
        ("plain_overview", (0, 0, 40, 30), (17, 13)),
    ],
)
@pytest.mark.parametrize("method", ["average", "rms"])
def test_resampled_palette_reads_match_gdal(palette_sources, kind, window, size, method):
    gdal, paths = palette_sources
    path = paths[kind]
    alg = {"average": gdal.GRIORA_Average, "rms": gdal.GRIORA_RMS}[method]
    with gdal.Open(str(path)) as ds:
        band = ds.GetRasterBand(1)
        expected = band.ReadAsArray(*window, buf_xsize=size[0], buf_ysize=size[1], resample_alg=alg)
        nodata = band.GetNoDataValue()
    source = terrio.open_raster(str(path), lazy=True)
    data, masks = source.read_resampled_with_masks(size, band=1, window=window, resampling=method)
    assert data.dtype == expected.dtype
    np.testing.assert_array_equal(data, expected)
    np.testing.assert_array_equal(masks, np.where(expected == nodata, 0, 255))
    with rio.open(path) as reader:
        got = reader.read(
            1, window=Window(*window), out_shape=size[::-1], resampling=Resampling[method]
        )
        np.testing.assert_array_equal(got, expected)


@pytest.mark.parametrize("kind", ["issue", "transparent"])
@pytest.mark.parametrize("method", ["average", "rms"])
def test_resized_translation_retains_palette(palette_sources, kind, method):
    gdal, paths = palette_sources
    path = str(paths[kind])
    window = (3, 5, 61, 47)
    with gdal.Translate(
        "", path, format="MEM", srcWin=window, width=17, height=23, resampleAlg=method
    ) as reference:
        expected = reference.ReadAsArray()
        table = reference.GetRasterBand(1).GetColorTable()
        colors = [table.GetColorEntry(i) for i in range(table.GetCount())]
    translated = terrio.translate(path, srcwin=window, outsize=(17, 23), resampling=method)
    np.testing.assert_array_equal(translated.read(band=1), expected)
    assert translated.color_table == colors
