"""`terrio.rio.open(path, overview_level=N)` opens overview N as the dataset, as rasterio does.

rasterio passes `overview_level` to GDAL as the `OVERVIEW_LEVEL` open option
(`GDALCreateOverviewDataset`): the dataset is the overview, its geotransform rescaled, its
overviews the smaller levels, its masks the overview's. terrio ignored the option and returned the
full resolution at every level (`cog_u16_utm.tif` `(4096, 4096)` x3 where GDAL gives
`(2048, 2048), (1024, 1024), (512, 512)`). GDAL is the oracle. Skipped without rasterio.
"""

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
pytest.importorskip("rasterio.shutil")

from rasterio.errors import RasterioIOError as GdalIOError  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402
from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.errors import RasterioIOError  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402


def _cog(tmp_path, kind):
    """A COG GDAL writes from a 1001 x 777 source: gray with nodata, or RGB with a mask."""
    src = str(tmp_path / f"{kind}-src.tif")
    rng = np.random.default_rng(7)
    y, x = np.mgrid[0:777, 0:1001]
    bands = 3 if kind == "mask" else 1
    data = np.stack(
        [(np.sin(x / 17 + b) * 100 + 120 + rng.normal(0, 5, x.shape)) for b in range(bands)]
    )
    data = data.astype("uint16" if kind == "nodata" else "uint8")
    profile = dict(
        driver="GTiff",
        width=1001,
        height=777,
        count=bands,
        dtype=data.dtype.name,
        crs="EPSG:32633",
        transform=from_origin(500000, 5000000, 10, 10),
    )
    if kind == "nodata":
        data[:, 100:200, 300:500] = 0
        profile["nodata"] = 0
    with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
        with rasterio.open(src, "w", **profile) as dst:
            dst.write(data)
            if kind == "mask":
                dst.write_mask(
                    np.where((x - 500) ** 2 + (y - 400) ** 2 < 300**2, 255, 0).astype("uint8")
                )
    out = str(tmp_path / f"{kind}.tif")
    rasterio.shutil.copy(src, out, driver="COG", blocksize=128, compress="deflate")
    return out


@pytest.mark.parametrize("kind", ["nodata", "mask"])
def test_overview_levels_open_as_in_rasterio(tmp_path, kind):
    path = _cog(tmp_path, kind)
    with rasterio.open(path) as g:
        count = len(g.overviews(1))
    assert count >= 2
    for level in range(count):
        with (
            rasterio.open(path, overview_level=level) as g,
            trio.open(path, overview_level=level) as t,
        ):
            assert (t.width, t.height) == (g.width, g.height)
            assert t.transform.almost_equals(g.transform, precision=1e-9)
            assert t.bounds == pytest.approx(g.bounds)
            assert t.overviews(1) == g.overviews(1)
            np.testing.assert_array_equal(t.read(), g.read())
            np.testing.assert_array_equal(t.read_masks(), g.read_masks())
            np.testing.assert_array_equal(t.dataset_mask(), g.dataset_mask())
            w = (RW(3, 5, 60, 40), TW(3, 5, 60, 40))
            np.testing.assert_array_equal(t.read(1, window=w[1]), g.read(1, window=w[0]))
            np.testing.assert_array_equal(
                t.read(1, out_shape=(t.height // 3, t.width // 3)),
                g.read(1, out_shape=(g.height // 3, g.width // 3)),
            )


def test_a_missing_overview_level_is_refused(tmp_path):
    path = _cog(tmp_path, "nodata")
    with rasterio.open(path) as g:
        count = len(g.overviews(1))
    for level in (count, count + 3):
        with pytest.raises(GdalIOError):
            rasterio.open(path, overview_level=level)
        with pytest.raises(RasterioIOError, match="overview level"):
            trio.open(path, overview_level=level)
