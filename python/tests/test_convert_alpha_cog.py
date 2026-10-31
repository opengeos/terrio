"""`terrio.convert(..., format="COG")` of a GeoTIFF with an alpha band, as GDAL's COG of it.

GDAL's `CreateCopy` takes the source bands' colour interpretations: an RGB source with an alpha
band gives an RGB COG with an unassociated alpha band, which weights the overviews and masks every
band. terrio's whole-file reader kept no colour interpretation, so the converted COG was gray with
undefined bands, its overviews unweighted: levels 1 to 3 differed from GDAL's in 22.5, 33.4 and
38.0% of their pixels and 7% of their masks. GDAL is the oracle. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
pytest.importorskip("rasterio.shutil")

from rasterio.transform import from_origin  # noqa: E402

import terrio  # noqa: E402

KINDS = {
    # bands, photometric, alpha
    "rgba": (4, "RGB", "YES"),
    "gray-alpha": (2, "MINISBLACK", "YES"),
    "rgb": (3, "RGB", None),
    "rgba-unspecified": (4, "RGB", "UNSPECIFIED"),
}


def _source(path, kind):
    """A smooth field; the alpha band transparent in a block, with soft edges and holes."""
    bands, photometric, alpha = KINDS[kind]
    width, height = 1100, 900
    rng = np.random.default_rng(bands)
    y, x = np.mgrid[0:height, 0:width].astype("float64")
    colors = bands - 1 if alpha == "YES" else bands
    planes = [
        np.clip(
            np.sin(x / (19 + 5 * b)) * 0.3
            + np.cos(y / 23) * 0.2
            + 0.5
            + rng.normal(0, 0.04, x.shape),
            0,
            1,
        )
        * 250
        for b in range(colors)
    ]
    if alpha == "YES":
        a = np.full((height, width), 255.0)
        a[: height // 3, : width // 4] = 0
        edge = (x > width // 2) & (x < width // 2 + 40)
        a[edge] = (x[edge] - width // 2) * 6
        a[rng.random(a.shape) < 0.02] = 0
        planes.append(a)
    profile = dict(
        driver="GTiff",
        width=width,
        height=height,
        count=bands,
        dtype="uint8",
        photometric=photometric,
        crs="EPSG:32633",
        transform=from_origin(500000, 5000000, 10, 10),
    )
    if alpha:
        profile["alpha"] = alpha
    with rasterio.open(path, "w", **profile) as dst:
        dst.write(np.stack(planes).astype("uint8"))
    return str(path)


def _levels(path):
    with rasterio.open(path) as ds:
        count = len(ds.overviews(1))
        out = [(ds.read(), ds.read_masks())]
        about = (
            [c.name for c in ds.colorinterp],
            [[f.name for f in fl] for fl in ds.mask_flag_enums],
        )
    for i in range(count):
        with rasterio.open(path, overview_level=i) as ds:
            out.append((ds.read(), ds.read_masks()))
    return about, out


@pytest.mark.parametrize("kind", list(KINDS))
def test_a_converted_cog_is_gdals(tmp_path, kind):
    src = _source(tmp_path / "src.tif", kind)
    gdal, ours = str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif")
    rasterio.shutil.copy(src, gdal, driver="COG", compress="DEFLATE")
    terrio.convert(src, ours, format="COG", compress="DEFLATE")
    (g_about, g_levels), (t_about, t_levels) = _levels(gdal), _levels(ours)
    assert t_about == g_about
    assert len(t_levels) == len(g_levels) > 2
    for i, ((gx, gm), (tx, tm)) in enumerate(zip(g_levels, t_levels, strict=True)):
        np.testing.assert_array_equal(tm, gm, err_msg=f"level {i} masks")
        np.testing.assert_array_equal(tx, gx, err_msg=f"level {i} pixels")
