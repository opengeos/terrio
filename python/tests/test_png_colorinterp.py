"""PNGs with an alpha band or RGB, read and converted to COG as GDAL reads and converts them.

GDAL's PNG driver gives each band a colour interpretation (`PNGRasterBand::GetColorInterpretation`
in `frmts/png/pngdataset.cpp`): gray and alpha, red, green and blue, and alpha after them. The
alpha band is then the mask of the other bands (`GetMaskFlags`: alpha, per dataset), and a COG of
the PNG is RGB with an alpha band that weights its overviews. terrio read a gray band then
undefined ones, so the masks of a gray and alpha PNG and of an RGBA one were all valid (205,000
and 615,000 mask pixels differed from GDAL's on 1100 x 900 images), and level 1 of their COGs
differed from GDAL's in 39,163 and 114,264 pixels. GDAL is the oracle. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
pytest.importorskip("rasterio.shutil")

from rasterio.enums import ColorInterp  # noqa: E402

import terrio  # noqa: E402
import terrio.rio  # noqa: E402
import terrio.rio.shutil  # noqa: E402

KINDS = {"gray-alpha": 2, "rgb": 3, "rgba": 4}


def _source(path, bands):
    """A smooth field written by GDAL; the alpha band transparent in a block, half in a strip."""
    height, width = 900, 1100
    y, x = np.mgrid[0:height, 0:width].astype("float64")
    planes = [
        np.clip((np.sin(x / (19 + 5 * b)) * 0.3 + np.cos(y / 23) * 0.2 + 0.5) * 255, 0, 255)
        for b in range(bands)
    ]
    if bands in (2, 4):
        alpha = np.full((height, width), 255.0)
        alpha[200:500, 300:800] = 0
        alpha[100:150, :] = 128
        planes[-1] = alpha
    with rasterio.open(
        path, "w", driver="PNG", width=width, height=height, count=bands, dtype="uint8"
    ) as dst:
        dst.write(np.stack(planes).astype("uint8"))
        if bands == 2:
            dst.colorinterp = [ColorInterp.gray, ColorInterp.alpha]
    return str(path)


def _about(ds):
    flags = [sorted(f.name for f in fl) for fl in ds.mask_flag_enums]
    return [c.name for c in ds.colorinterp], flags


def _levels(path):
    with rasterio.open(path) as ds:
        count = len(ds.overviews(1))
        about = _about(ds)
        out = [(ds.read(), ds.read_masks())]
    for i in range(count):
        with rasterio.open(path, overview_level=i) as ds:
            out.append((ds.read(), ds.read_masks()))
    return about, out


def _assert_same_cog(gdal, ours):
    (g_about, g_levels), (t_about, t_levels) = _levels(gdal), _levels(ours)
    assert t_about == g_about
    assert len(t_levels) == len(g_levels) == 3
    for i, ((gx, gm), (tx, tm)) in enumerate(zip(g_levels, t_levels, strict=True)):
        np.testing.assert_array_equal(tm, gm, err_msg=f"level {i} masks")
        np.testing.assert_array_equal(tx, gx, err_msg=f"level {i} pixels")


@pytest.mark.parametrize("kind", list(KINDS))
def test_a_png_reads_as_gdals(tmp_path, kind):
    src = _source(tmp_path / "src.png", KINDS[kind])
    with rasterio.open(src) as g, terrio.rio.open(src) as t:
        assert _about(t) == _about(g)
        np.testing.assert_array_equal(t.read_masks(), g.read_masks())
        np.testing.assert_array_equal(t.dataset_mask(), g.dataset_mask())


@pytest.mark.parametrize("kind", list(KINDS))
def test_the_cog_of_a_png_is_gdals(tmp_path, kind):
    """The source profile, driver COG and DEFLATE, through `shutil.copy`."""
    src = _source(tmp_path / "src.png", KINDS[kind])
    gdal, ours = str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif")
    for path, mod, shutil in (
        (gdal, rasterio, rasterio.shutil),
        (ours, terrio.rio, terrio.rio.shutil),
    ):
        with mod.open(src) as s:
            profile = dict(s.profile)
            profile.update(driver="COG", compress="deflate")
            shutil.copy(s, path, **profile)
    _assert_same_cog(gdal, ours)


@pytest.mark.parametrize("kind", list(KINDS))
def test_a_converted_png_is_gdals_cog(tmp_path, kind):
    src = _source(tmp_path / "src.png", KINDS[kind])
    gdal, ours = str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif")
    rasterio.shutil.copy(src, gdal, driver="COG", compress="DEFLATE")
    terrio.convert(src, ours, format="COG", compress="DEFLATE")
    _assert_same_cog(gdal, ours)
