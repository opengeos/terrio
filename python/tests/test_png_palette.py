"""Paletted PNGs, read and converted to COG as GDAL reads and converts them.

GDAL's PNG driver reads a paletted PNG as one band with colour interpretation palette and the
PNG's colour table, each entry's alpha from the tRNS chunk; the index of the only fully
transparent entry is the band's nodata value (`PNGDataset::Open`). terrio read the indices as a
gray band without a colour table. Its COG of such a PNG, written with the source profile, driver
COG and DEFLATE through `shutil.copy`, was then gray, had no colormap, and its overviews resampled
the indices as continuous values with CUBIC: GDAL's COG driver takes NEAREST for paletted rasters
(`GetResampling` in `frmts/gtiff/cogdriver.cpp`). Level 1 of a 720 x 405 aerial image differed
from GDAL's in 95% of its pixels. GDAL is the oracle. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
pytest.importorskip("rasterio.shutil")

import terrio  # noqa: E402
import terrio.rio  # noqa: E402
import terrio.rio.shutil  # noqa: E402

# The alphas of some colour table entries, written by GDAL as a tRNS chunk.
ALPHAS = {
    "opaque": {},
    "one-transparent": {7: 0, 3: 128},
    "two-transparent": {7: 0, 9: 0, 3: 128},
}


def _source(path, alphas):
    """A paletted PNG written by GDAL: 1100 x 900 indices of a 200-entry palette."""
    height, width = 900, 1100
    y, x = np.mgrid[0:height, 0:width]
    rng = np.random.default_rng(5)
    indices = (x // 9 + 3 * (y // 7) + rng.integers(0, 4, (height, width))) % 200
    cmap = {i: (i, 255 - i, (37 * i) % 256, alphas.get(i, 255)) for i in range(200)}
    with rasterio.open(
        path, "w", driver="PNG", width=width, height=height, count=1, dtype="uint8"
    ) as dst:
        dst.write(indices.astype("uint8"), 1)
        dst.write_colormap(1, cmap)
    return str(path)


def _about(ds):
    try:
        cmap = ds.colormap(1)
    except ValueError:
        cmap = None
    flags = [sorted(f.name for f in fl) for fl in ds.mask_flag_enums]
    return [c.name for c in ds.colorinterp], cmap, ds.nodata, flags


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


@pytest.mark.parametrize("case", list(ALPHAS))
def test_a_paletted_png_reads_as_gdals(tmp_path, case):
    src = _source(tmp_path / "src.png", ALPHAS[case])
    with rasterio.open(src) as g, terrio.rio.open(src) as t:
        assert _about(t) == _about(g)
        assert g.colorinterp[0].name == "palette"
        assert len(g.colormap(1)) == 200
        assert g.nodata == (7.0 if case == "one-transparent" else None)
        np.testing.assert_array_equal(t.read(), g.read())
        np.testing.assert_array_equal(t.read_masks(), g.read_masks())


@pytest.mark.parametrize("case", list(ALPHAS))
def test_the_cog_of_a_paletted_png_is_gdals(tmp_path, case):
    """The source profile, driver COG and DEFLATE, through `shutil.copy`."""
    src = _source(tmp_path / "src.png", ALPHAS[case])
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


@pytest.mark.parametrize("case", list(ALPHAS))
def test_a_converted_paletted_png_is_gdals_cog(tmp_path, case):
    src = _source(tmp_path / "src.png", ALPHAS[case])
    gdal, ours = str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif")
    rasterio.shutil.copy(src, gdal, driver="COG", compress="DEFLATE")
    terrio.convert(src, ours, format="COG", compress="DEFLATE")
    _assert_same_cog(gdal, ours)
