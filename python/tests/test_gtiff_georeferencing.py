"""GeoTIFF geotransforms read and written by `terrio.rio` as GDAL reads and writes them.

terrio gives a raster without georeferencing the geotransform `(0, 1, 0, height, 0, -1)`, and
`terrio.rio` reported that value as the identity, GDAL's transform for a dataset without one. A file
whose own transform is `(1, 0, 0, 0, -1, H)`, H its height (`from_origin(0, H, 1, 1)`, common in
tests), was therefore read as the identity, whoever wrote it (156 and 7 test failures in two
applications, `WindowError: Bounds and transform are inconsistent`). GDAL is the oracle, as
reader and as writer. Skipped without rasterio.
"""

import warnings

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from affine import Affine  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.transform import from_origin  # noqa: E402
from terrio.rio.windows import from_bounds  # noqa: E402

SIZES = [(256, 256), (300, 200), (64, 64)]


def _write(lib, path, width, height, crs, tiled, transform):
    profile = dict(driver="GTiff", width=width, height=height, count=1, dtype="int16", nodata=-9999)
    if crs is not None:
        profile["crs"] = crs
    if transform is not None:
        profile["transform"] = transform
    if tiled:
        profile.update(tiled=True, blockxsize=16, blockysize=16)
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        with lib.open(path, "w", **profile) as ds:
            ds.write(np.full((1, height, width), 20, dtype="int16"))


def _read(lib, path):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        with lib.open(path) as ds:
            return tuple(ds.transform)[:6], tuple(ds.bounds)


@pytest.mark.parametrize("size", SIZES, ids=lambda s: f"{s[0]}x{s[1]}")
@pytest.mark.parametrize("tiled", [True, False], ids=["tiled", "striped"])
@pytest.mark.parametrize("crs", ["EPSG:32650", "EPSG:4326", None], ids=str)
@pytest.mark.parametrize("writer", ["gdal", "terrio"])
def test_the_flipped_identity_round_trips(tmp_path, writer, crs, tiled, size):
    """`(1, 0, 0, 0, -1, H)` reads back as GDAL reads it, from either writer, by either reader."""
    width, height = size
    path = str(tmp_path / "flipped.tif")
    _write(
        rasterio if writer == "gdal" else trio,
        path,
        width,
        height,
        crs,
        tiled,
        from_origin(0, height, 1, 1),
    )
    want = (1.0, 0.0, 0.0, 0.0, -1.0, float(height))
    gdal_transform, gdal_bounds = _read(rasterio, path)
    terrio_transform, terrio_bounds = _read(trio, path)
    assert gdal_transform == want
    assert terrio_transform == want
    assert terrio_bounds == gdal_bounds == (0.0, 0.0, float(width), float(height))
    # A statistics service's call on the dataset's own bounds.
    with trio.open(path) as ds:
        w = from_bounds(*ds.bounds, transform=ds.transform)
        assert (w.col_off, w.row_off, w.width, w.height) == (0, 0, width, height)


@pytest.mark.parametrize("writer", ["gdal", "terrio"])
def test_a_tiff_without_georeferencing_reads_as_the_identity(tmp_path, writer):
    """The control: no geotransform is still the identity, as in GDAL."""
    path = str(tmp_path / "plain.tif")
    _write(rasterio if writer == "gdal" else trio, path, 64, 48, None, False, None)
    assert _read(rasterio, path)[0] == tuple(Affine.identity())[:6]
    assert _read(trio, path)[0] == tuple(Affine.identity())[:6]
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        with trio.open(path) as ds:
            assert not ds._georeferenced


@pytest.mark.parametrize(
    "transform",
    [
        from_origin(0, 255, 1, 1),
        from_origin(10, 256, 1, 1),
        from_origin(0, 256, 2, 2),
        Affine(1, 0, 0, 0, -1, 0),
        from_origin(500000, 4600000, 10, 10),
    ],
    ids=["origin-255", "x-10", "res-2", "flipped-0", "utm"],
)
def test_other_transforms_read_like_rasterio(tmp_path, transform):
    """Neighbours of the flipped identity read as GDAL reads them."""
    path = str(tmp_path / "other.tif")
    _write(rasterio, path, 256, 256, "EPSG:32650", False, transform)
    assert _read(trio, path) == _read(rasterio, path)
