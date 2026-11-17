"""A source dataset with no CRS warped to a destination CRS (rasterio not needed).

GDAL's image-projection transformer does not reproject a source that has no CRS: the source
geotransform maps straight into the destination CRS. `WarpedVRT` does this for a source
georeferenced by RPCs; the same holds for one georeferenced by a geotransform.
"""

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.errors import CRSError
from terrio.rio.transform import from_origin
from terrio.rio.vrt import WarpedVRT
from terrio.rio.warp import calculate_default_transform, reproject

DATA = (np.arange(64 * 64) % 251 + 1).astype("uint8").reshape(64, 64)
TRANSFORM = from_origin(10, 46, 1 / 64, 1 / 64)


@pytest.fixture
def source(tmp_path):
    path = str(tmp_path / "no-crs.tif")
    profile = dict(driver="GTiff", width=64, height=64, count=1, dtype="uint8")
    with rio.open(path, "w", transform=TRANSFORM, **profile) as dst:
        dst.write(DATA, 1)
    with rio.open(path) as src:
        assert src.crs is None
        yield src


def test_default_transform_is_the_source_grid(source):
    transform, width, height = calculate_default_transform(
        source.crs, "EPSG:4326", source.width, source.height, *source.bounds
    )
    assert (width, height) == (64, 64)
    assert transform.almost_equals(TRANSFORM)


def test_warped_vrt_reads_the_source_pixels(source):
    with WarpedVRT(source, crs="EPSG:4326") as vrt:
        assert vrt.src_crs is None
        assert vrt.transform.almost_equals(TRANSFORM)
        assert (vrt.read(1) == DATA).all()


def test_reproject_from_a_band_copies_the_pixels(source):
    out = np.zeros((64, 64), "uint8")
    reproject(
        rio.band(source, 1),
        out,
        dst_transform=source.transform,
        dst_crs="EPSG:4326",
        resampling=Resampling.nearest,
    )
    assert (out == DATA).all()


def test_an_array_with_no_crs_still_requires_one():
    with pytest.raises(CRSError, match="A source CRS is required"):
        reproject(
            DATA,
            np.zeros((64, 64), "uint8"),
            src_transform=TRANSFORM,
            dst_transform=TRANSFORM,
            dst_crs="EPSG:4326",
        )
