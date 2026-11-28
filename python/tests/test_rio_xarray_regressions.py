"""Regression tests for terrio.rio, terrio.xarray, terrio.pyogrio, and binding bounds."""

from __future__ import annotations

import json
import os
import sys

import numpy as np
import pytest
from affine import Affine

import terrio
import terrio.rio as rio
from terrio.rio import features as rio_features
from terrio.rio.env import setenv
from terrio.rio.merge import merge as rio_merge
from terrio.rio.stack import stack as rio_stack
from terrio.rio.vrt import _Grid

xarray = pytest.importorskip("xarray")
shapely_geometry = pytest.importorskip("shapely.geometry")


def _write_gtiff(path, data: np.ndarray, transform: Affine, *, crs="EPSG:4326", nodata=None):
    if data.ndim == 2:
        data = data[np.newaxis, :, :]
    count, height, width = data.shape
    with rio.open(
        path,
        "w",
        driver="GTiff",
        height=height,
        width=width,
        count=count,
        dtype=data.dtype,
        crs=crs,
        transform=transform,
        nodata=nodata,
    ) as dst:
        dst.write(data)


def test_xarray_merge_arrays_2d_windowed_preserves_window_origin():
    """Clipping the top/left of a 2-D DataArray in merge_arrays must not sample from (0, 0)."""
    import terrio.xarray as txr

    # 4x4 grid covering x=[0, 4], y=[0, 4] with distinct pixel values 0..15.
    arr = np.arange(16, dtype=np.float32).reshape(4, 4)
    da = xarray.DataArray(
        arr,
        dims=("y", "x"),
        coords={"y": [3.5, 2.5, 1.5, 0.5], "x": [0.5, 1.5, 2.5, 3.5]},
    )
    da.terrio.write_crs("EPSG:4326", inplace=True)
    da.terrio.write_transform(Affine(1.0, 0.0, 0.0, 0.0, -1.0, 4.0), inplace=True)

    # Request only the bottom-right 2x2 sub-region: x=[2, 4], y=[0, 2] -> rows 2..4, cols 2..4.
    merged = txr.merge_arrays([da], bounds=(2.0, 0.0, 4.0, 2.0))
    assert merged.shape == (2, 2)
    np.testing.assert_array_equal(merged.values, arr[2:4, 2:4])


def test_xarray_merge_arrays_2d_single_row_and_col():
    """Merging 2-D DataArrays resulting in 1-row or 1-col output must preserve 2-D dims."""
    import terrio.xarray as txr

    arr = np.array([[10.0, 20.0, 30.0]], dtype=np.float32)
    da = xarray.DataArray(
        arr,
        dims=("y", "x"),
        coords={"y": [0.5], "x": [0.5, 1.5, 2.5]},
    )
    da.terrio.write_crs("EPSG:4326", inplace=True)
    da.terrio.write_transform(Affine(1.0, 0.0, 0.0, 0.0, -1.0, 1.0), inplace=True)

    merged = txr.merge_arrays([da])
    assert merged.dims == ("y", "x")
    assert merged.shape == (1, 3)
    np.testing.assert_array_equal(merged.values, arr)


def test_xarray_band_as_variable_preserves_single_row_dim(tmp_path):
    """open_rasterio(..., band_as_variable=True) must not drop length-1 y/x dims."""
    import terrio.xarray as txr

    path = tmp_path / "one_row.tif"
    data = np.array([[[1.0, 2.0, 3.0]], [[4.0, 5.0, 6.0]]], dtype=np.float32)
    _write_gtiff(path, data, Affine(1.0, 0.0, 0.0, 0.0, -1.0, 1.0))

    ds = txr.open_rasterio(path, band_as_variable=True)
    assert ds["band_1"].dims == ("y", "x")
    assert ds["band_1"].shape == (1, 3)
    assert ds["band_2"].dims == ("y", "x")
    assert ds["band_2"].shape == (1, 3)


def test_warped_vrt_grid_rescaled_preserves_rotated_geotransform():
    """_Grid.rescaled must scale dX/drow (g[2]) by fy and dY/dcol (g[4]) by fx."""
    # Rotated transform: Affine(a, b, c, d, e, f) -> GDAL (c, a, b, f, d, e).
    a = Affine(2.0, 0.5, 100.0, -0.25, -3.0, 200.0)
    grid = _Grid(None, a.to_gdal(), 100, 100)
    rescaled = grid.rescaled(50, 25)  # fx = 2.0, fy = 4.0
    expected = (a * Affine.scale(2.0, 4.0)).to_gdal()
    np.testing.assert_allclose(rescaled.gt, expected, rtol=0.0, atol=1e-12)


def test_rio_merge_integer_indexes_and_nan_nodata(tmp_path):
    """rio.merge supports integer indexes >= 2 and masks NaN nodata when masked=True."""
    p1 = tmp_path / "s1.tif"
    d1 = np.array(
        [
            [[1.0, 2.0], [3.0, 4.0]],
            [[10.0, np.nan], [30.0, 40.0]],
        ],
        dtype=np.float32,
    )
    _write_gtiff(p1, d1, Affine(1.0, 0.0, 0.0, 0.0, -1.0, 2.0), nodata=float("nan"))

    out, _ = rio_merge([p1], indexes=2, masked=True)
    assert out.shape == (1, 2, 2)
    assert bool(out.mask[0, 0, 1]) is True
    assert bool(out.mask[0, 0, 0]) is False
    assert float(out.data[0, 0, 0]) == 10.0
    assert float(out.data[0, 1, 1]) == 40.0


def test_rio_stack_keeps_caller_writer_open_and_masks_nan(tmp_path):
    """rio.stack masks NaN nodata and does not close a caller-owned DatasetWriter."""
    p1 = tmp_path / "a.tif"
    d1 = np.array([[[1.0, np.nan], [3.0, 4.0]]], dtype=np.float32)
    tf = Affine(1.0, 0.0, 0.0, 0.0, -1.0, 2.0)
    _write_gtiff(p1, d1, tf, nodata=float("nan"))

    stacked, _ = rio_stack([p1], masked=True)
    assert bool(stacked.mask[0, 0, 1]) is True
    assert bool(stacked.mask[0, 0, 0]) is False

    dst_path = tmp_path / "out.tif"
    with rio.open(
        dst_path,
        "w",
        driver="GTiff",
        height=2,
        width=2,
        count=1,
        dtype="float32",
        crs="EPSG:4326",
        transform=tf,
        nodata=float("nan"),
    ) as writer:
        rio_stack([p1], dst_path=writer)
        assert not writer.closed


def test_rio_setenv_none_unsets_os_environ(monkeypatch):
    """setenv(KEY=None) outside an active Env unsets os.environ[KEY]."""
    key = "TERRIO_TEST_SETENV_UNSET_VAR"
    monkeypatch.setenv(key, "1")
    setenv(**{key: None})
    assert key not in os.environ


def test_rio_features_rasterize_accepts_linear_ring_north_up():
    """rio.features.rasterize burns LinearRing as a LineString boundary on north-up transforms."""
    ring = {
        "type": "LinearRing",
        "coordinates": [(0.5, 0.5), (3.5, 0.5), (3.5, 3.5), (0.5, 3.5), (0.5, 0.5)],
    }
    out = rio_features.rasterize(
        [(ring, 7)],
        out_shape=(4, 4),
        transform=Affine(1.0, 0.0, 0.0, 0.0, -1.0, 4.0),
        fill=0,
        dtype="uint8",
    )
    expected = np.array(
        [
            [7, 7, 7, 7],
            [7, 0, 0, 7],
            [7, 0, 0, 7],
            [7, 7, 7, 7],
        ],
        dtype=np.uint8,
    )
    np.testing.assert_array_equal(out, expected)


def test_pyogrio_write_linear_ring_and_multipoint_with_empty_member():
    """pyogrio._write serializes LinearRing as LineString and skips empty points in MultiPoint."""
    import shapely

    from terrio.pyogrio._write import _geometry

    ring = shapely_geometry.LinearRing([(0, 0), (1, 0), (1, 1), (0, 0)])
    parsed_ring = json.loads(_geometry(ring))
    assert parsed_ring["type"] == "LineString"
    assert len(parsed_ring["coordinates"]) == 4

    mp = shapely.from_wkt("MULTIPOINT (EMPTY, (1 2))")
    assert any(p.is_empty for p in mp.geoms)
    parsed_mp = json.loads(_geometry(mp))
    assert parsed_mp["type"] == "MultiPoint"
    assert parsed_mp["coordinates"] == [[1, 2]]


def test_binding_dimension_overflow_raises_value_error(tmp_path):
    """Overflowing band counts or resampled regions return ValueError instead of panicking."""
    from terrio import _terrio

    usize_bits = sys.maxsize.bit_length() + 1
    over_half_usize = (1 << (usize_bits - 1)) + 1
    gt = [0.0, 1.0, 0.0, 1.0, 0.0, -1.0]
    with pytest.raises(ValueError, match="overflow"):
        terrio.GeoTiffWriter(
            str(tmp_path / "ovf1.tif"),
            width=1,
            height=1,
            count=over_half_usize,
            dtype="complex64",
            transform=gt,
        )
    with pytest.raises(ValueError, match="overflow"):
        _terrio.CogWriter(
            str(tmp_path / "ovf2.tif"),
            width=1,
            height=1,
            count=over_half_usize,
            dtype="complex64",
            transform=gt,
        )
    arr = np.zeros((1, 2, 2), dtype=np.float32)
    with pytest.raises(ValueError, match="overflow"):
        _terrio.resampled_read(
            arr,
            (0, 0, over_half_usize, 2),
            "nearest",
            (2, 2),
            (0, 0, 2, 2),
            (0.0, 0.0, 2.0, 2.0),
            (2, 2),
            2,
            None,
            None,
            False,
        )
