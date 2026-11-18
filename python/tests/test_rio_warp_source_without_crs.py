"""A source dataset with no CRS warped to a destination CRS (rasterio not needed).

GDAL's image-projection transformer does not reproject a source that has no CRS: the source
geotransform maps straight into the destination CRS. `WarpedVRT` does this for a source
georeferenced by RPCs; the same holds for one georeferenced by a geotransform.
"""

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.control import GroundControlPoint
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
    # rasterio 1.5.0 (GDAL 3.12.1): the source grid at half the size.
    with WarpedVRT(source, crs="EPSG:4326", width=32, height=32) as vrt:
        assert vrt.transform.almost_equals(from_origin(10, 46, 1 / 32, 1 / 32))
        assert vrt.read(1).shape == (32, 32)


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


def test_reproject_into_a_band_takes_the_destination_crs(source, tmp_path):
    path = str(tmp_path / "dst.tif")
    profile = dict(driver="GTiff", width=64, height=64, count=1, dtype="uint8")
    with rio.open(path, "w", transform=TRANSFORM, crs="EPSG:4326", **profile) as dst:
        reproject(rio.band(source, 1), rio.band(dst, 1), resampling=Resampling.nearest)
    with rio.open(path) as dst:
        assert (dst.read(1) == DATA).all()


def test_a_dataset_with_no_geotransform_still_requires_a_crs(tmp_path):
    path = str(tmp_path / "bare.tif")
    profile = dict(driver="GTiff", width=64, height=64, count=1, dtype="uint8")
    with rio.open(path, "w", **profile) as dst:
        dst.write(DATA, 1)
    with rio.open(path) as src:
        with pytest.raises(CRSError, match="A source CRS is required"):
            reproject(
                rio.band(src, 1),
                np.zeros((64, 64), "uint8"),
                dst_transform=TRANSFORM,
                dst_crs="EPSG:4326",
            )
        for size in ({}, {"width": 32, "height": 32}):
            with pytest.raises(CRSError, match="A CRS is required"):
                WarpedVRT(src, crs="EPSG:4326", **size)
        # rasterio 1.5.0: a `src_transform` places it on a grid given in full, and only there.
        for size in ({}, {"width": 32, "height": 32}):
            with pytest.raises(CRSError, match="A CRS is required"):
                WarpedVRT(src, crs="EPSG:4326", src_transform=TRANSFORM, **size)
        grid = dict(transform=TRANSFORM, width=64, height=64)
        with WarpedVRT(src, crs="EPSG:4326", src_transform=TRANSFORM, **grid) as vrt:
            assert (vrt.read(1) == DATA).all()
        # Its bounds are pixels and lines.
        with pytest.raises(CRSError, match="A CRS is required"):
            calculate_default_transform(src.crs, "EPSG:4326", src.width, src.height, *src.bounds)


def test_default_transform_from_gcps_still_requires_a_crs():
    gcps = [
        GroundControlPoint(row=r, col=c, x=10 + c / 64, y=46 - r / 64)
        for r, c in ((0, 0), (0, 64), (64, 0), (64, 64))
    ]
    with pytest.raises(CRSError, match="A CRS is required"):
        calculate_default_transform(None, "EPSG:4326", 64, 64, gcps=gcps)
