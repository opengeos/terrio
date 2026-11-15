"""terrio's raster operations on DataArrays: terrain, sieve, proximity, zonal statistics...

Each operation converts the array to an in-memory `terrio.Raster` (a 2-D array as one band, a
3-D one band by band), calls the terrio function of the same name and returns its result: a
DataArray on the input's grid, or a `terrio.Layer` for vector results. Dask arrays are computed.
"""

from __future__ import annotations

from collections.abc import Callable, Hashable, Sequence
from typing import Any

import numpy as np
import xarray

from terrio import _terrio
from terrio.xarray.exceptions import TooManyDimensions


def _north_up(da: xarray.DataArray) -> xarray.DataArray:
    """`da` with its rows from north to south, as terrio holds rasters."""
    y = da[da.terrio.y_dim].values
    if y.size > 1 and y[1] > y[0]:
        return da.isel({da.terrio.y_dim: slice(None, None, -1)})
    return da


def _band_dims(da: xarray.DataArray) -> list[Hashable]:
    spatial = (da.terrio.y_dim, da.terrio.x_dim)
    extra = [d for d in da.dims if d not in spatial]
    if len(extra) > 1:
        raise TooManyDimensions(
            f"Only 2D and 3D data arrays supported. Data variable: {da.name} has {da.ndim} dims."
        )
    return extra


def to_raster(da: xarray.DataArray) -> _terrio.Raster:
    """`da` (2-D, or 3-D with bands along its other dimension) as an in-memory Raster.

    NaN is nodata for floating-point arrays without a nodata value.
    """
    extra = _band_dims(da)
    da = _north_up(da.transpose(*extra, da.terrio.y_dim, da.terrio.x_dim))
    values = np.ascontiguousarray(da.values)
    nodata = da.terrio.nodata
    encoded = da.terrio.encoded_nodata
    if encoded is not None and not np.isnan(encoded):
        # masked on read: back to the file's nodata value, which some operations need finite
        values = np.where(np.isnan(values), values.dtype.type(encoded), values)
        nodata = encoded
    elif nodata is None and da.dtype.kind == "f":
        nodata = np.nan
    crs = da.terrio.crs
    return _terrio.Raster.from_array(
        values,
        da.terrio.transform(recalc=True).to_gdal(),
        crs=crs.to_wkt() if crs is not None else None,
        nodata=None if nodata is None else float(nodata),
    )


def from_raster(raster: _terrio.Raster, like: xarray.DataArray) -> xarray.DataArray:
    """`raster` on the grid of the 2-D `like`: 2-D for one band, else along a `band` dimension.

    Floating-point results are masked (nodata as NaN, its value in the encoding), as
    `open_rasterio(masked=True)` reads them; integer results keep `_FillValue` in the attributes.
    """
    values = raster.read()
    nodata = raster.nodata
    if nodata is not None and values.dtype.kind == "f" and not np.isnan(nodata):
        values[values == nodata] = np.nan
    y_dim, x_dim = like.terrio.y_dim, like.terrio.x_dim
    if _north_up(like) is not like:
        values = values[:, ::-1]
    if raster.count == 1:
        out = xarray.DataArray(values[0], dims=(y_dim, x_dim), coords=like.coords, name=like.name)
        out = out.transpose(*like.dims)
    else:
        coords = {k: v for k, v in like.coords.items() if k != "band"}
        coords["band"] = np.arange(1, raster.count + 1)
        out = xarray.DataArray(values, dims=("band", y_dim, x_dim), coords=coords, name=like.name)
    if "grid_mapping" in like.encoding:
        out.encoding["grid_mapping"] = like.encoding["grid_mapping"]
    if nodata is not None:
        out.terrio.write_nodata(nodata, encoded=out.dtype.kind == "f", inplace=True)
    return out


def raster_op(
    da: xarray.DataArray, func: Callable[..., _terrio.Raster], *args: Any, **kwargs: Any
) -> xarray.DataArray:
    """`func(raster, *args, **kwargs)` on `da`, band by band: a DataArray on the same grid."""
    extra = _band_dims(da)
    if not extra:
        return from_raster(func(to_raster(da), *args, **kwargs), da)
    bands = [
        from_raster(func(to_raster(band), *args, **kwargs), band)
        for band in (da.isel({extra[0]: i}) for i in range(da.sizes[extra[0]]))
    ]
    return xarray.concat(bands, dim=extra[0]).transpose(*da.dims)


def rasterize(src: Any, like: xarray.DataArray | xarray.Dataset, **kwargs: Any) -> xarray.DataArray:
    """`terrio.rasterize` of `src` on the grid of `like`, as a DataArray.

    A single band comes back 2-D, several along a `band` dimension.
    """
    y_dim, x_dim = like.terrio.y_dim, like.terrio.x_dim
    grid = xarray.DataArray(
        np.zeros((like.sizes[y_dim], like.sizes[x_dim]), np.uint8),
        dims=(y_dim, x_dim),
        coords={y_dim: like[y_dim], x_dim: like[x_dim]},
    )
    if like.terrio.crs is not None:
        grid = grid.terrio.write_crs(like.terrio.crs)
    raster = _terrio.rasterize(src, like=to_raster(grid), **kwargs)
    out = from_raster(raster, grid)
    out.name = None
    return out


def zonal_stats(
    rasters: Sequence[xarray.DataArray], zones: Any, prefixes: Sequence[str] | None, **kwargs: Any
) -> _terrio.Layer:
    """`terrio.zonal_stats` of `zones` over `rasters`."""
    return _terrio.zonal_stats(
        zones, [to_raster(da) for da in rasters], prefixes=prefixes, **kwargs
    )
