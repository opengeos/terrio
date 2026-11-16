"""terrio's raster operations on DataArrays: terrain, sieve, proximity, zonal statistics...

Each operation converts the array to an in-memory `terrio.Raster` (a 2-D array as one band, a
3-D one band by band), calls the terrio function of the same name and returns its result: a
DataArray on the input's grid, or a `terrio.Layer` for vector results. Dask arrays are computed.
"""

from __future__ import annotations

from collections.abc import Callable, Hashable, Sequence
from typing import Any, NamedTuple

import numpy as np
import xarray
from affine import Affine

from terrio import _terrio
from terrio.rio.crs import CRS
from terrio.xarray.exceptions import TooManyDimensions


class _Grid(NamedTuple):
    """What the conversions need from the input, resolved once.

    Arrays derived from it (`transpose`, `isel`) get a new accessor, which forgets
    `set_spatial_dims` and `_source`.
    """

    y: Hashable
    x: Hashable
    crs: CRS | None
    transform: Affine  # north-up, as terrio holds rasters
    ascending: bool  # rows stored from south to north
    reversed_x: bool  # columns stored from east to west
    nodata: Any = None
    encoded: Any = None


def _grid(obj: xarray.DataArray | xarray.Dataset) -> _Grid:
    acc = obj.terrio
    y = obj[acc.y_dim].values
    x = obj[acc.x_dim].values
    ascending = bool(y.size > 1 and y[1] > y[0])
    reversed_x = bool(x.size > 1 and x[1] < x[0])
    transform = acc.transform(recalc=True)
    if ascending:
        transform = transform @ Affine.translation(0, y.size) @ Affine.scale(1, -1)
    if reversed_x:
        transform = transform @ Affine.translation(x.size, 0) @ Affine.scale(-1, 1)
    nodata = acc.nodata if isinstance(obj, xarray.DataArray) else None
    encoded = acc.encoded_nodata if isinstance(obj, xarray.DataArray) else None
    return _Grid(acc.y_dim, acc.x_dim, acc.crs, transform, ascending, reversed_x, nodata, encoded)


def _band_dims(da: xarray.DataArray, g: _Grid) -> list[Hashable]:
    extra = [d for d in da.dims if d not in (g.y, g.x)]
    if len(extra) > 1:
        raise TooManyDimensions(
            f"Only 2D and 3D data arrays supported. Data variable: {da.name} has {da.ndim} dims."
        )
    return extra


def to_raster(da: xarray.DataArray, g: _Grid | None = None) -> _terrio.Raster:
    """`da` (2-D, or 3-D with bands along its other dimension) as an in-memory Raster.

    NaN is nodata for floating-point arrays without a nodata value.
    """
    g = g or _grid(da)
    values = da.transpose(*_band_dims(da, g), g.y, g.x).values
    if g.ascending:
        values = values[..., ::-1, :]
    if g.reversed_x:
        values = values[..., ::-1]
    values = np.ascontiguousarray(values)
    nodata = g.nodata
    if g.encoded is not None and not np.isnan(g.encoded):
        # masked on read: back to the file's nodata value, which some operations need finite
        values = np.where(np.isnan(values), values.dtype.type(g.encoded), values)
        nodata = g.encoded
    elif nodata is None and values.dtype.kind == "f":
        nodata = np.nan
    return _terrio.Raster.from_array(
        values,
        g.transform.to_gdal(),
        crs=g.crs.to_wkt() if g.crs is not None else None,
        nodata=None if nodata is None else float(nodata),
    )


def from_raster(raster: _terrio.Raster, like: xarray.DataArray, g: _Grid) -> xarray.DataArray:
    """`raster` as a DataArray: 2-D for one band, else along a `band` dimension.

    On the grid of the 2-D `like` when it has its shape, else (a viewshed cropped to its
    distance) on its own grid. Floating-point results are masked (nodata as NaN, its value in
    the encoding), as `open_rasterio(masked=True)` reads them; integer results keep `_FillValue`
    in the attributes.
    """
    values = raster.read()
    nodata = raster.nodata
    if nodata is not None and values.dtype.kind == "f" and not np.isnan(nodata):
        values[values == nodata] = np.nan
    coords = dict(like.coords)
    if (raster.height, raster.width) != (like.sizes[g.y], like.sizes[g.x]):
        t = Affine.from_gdal(*raster.transform)
        # not the grid mapping either: its GeoTransform is the input's (write_crs adds it back)
        coords = {
            k: v
            for k, v in coords.items()
            if not {g.y, g.x} & set(v.dims) and not {"crs_wkt", "spatial_ref"} & set(v.attrs)
        }
        coords[g.x] = t.c + t.a * (np.arange(raster.width) + 0.5)
        coords[g.y] = t.f + t.e * (np.arange(raster.height) + 0.5)
        if g.ascending:
            coords[g.y] = coords[g.y][::-1]
        if g.reversed_x:
            coords[g.x] = coords[g.x][::-1]
    if g.ascending:
        values = values[:, ::-1]
    if g.reversed_x:
        values = values[..., ::-1]
    if raster.count == 1:
        out = xarray.DataArray(values[0], dims=(g.y, g.x), coords=coords, name=like.name)
        out = out.transpose(*(d for d in like.dims if d in (g.y, g.x)))
    else:
        coords = {k: v for k, v in coords.items() if k != "band"}
        coords["band"] = np.arange(1, raster.count + 1)
        out = xarray.DataArray(values, dims=("band", g.y, g.x), coords=coords, name=like.name)
    if "grid_mapping" in like.encoding:
        out.encoding["grid_mapping"] = like.encoding["grid_mapping"]
    out.terrio.set_spatial_dims(x_dim=g.x, y_dim=g.y, inplace=True)
    if g.crs is not None and out.terrio.crs is None:
        out = out.terrio.write_crs(g.crs)
        out.terrio.set_spatial_dims(x_dim=g.x, y_dim=g.y, inplace=True)
    if nodata is not None:
        out.terrio.write_nodata(nodata, encoded=out.dtype.kind == "f", inplace=True)
    return out


def raster_op(
    da: xarray.DataArray, func: Callable[..., _terrio.Raster], *args: Any, **kwargs: Any
) -> xarray.DataArray:
    """`func(raster, *args, **kwargs)` on `da`, band by band: a DataArray on the same grid.

    On a 3-D array, `band=` picks the band (1-based) to run on, and the result is 2-D.
    """
    g = _grid(da)
    extra = _band_dims(da, g)
    if extra and "band" in kwargs:
        da, extra = da.isel({extra[0]: int(kwargs.pop("band")) - 1}), []
    if not extra:
        return from_raster(func(to_raster(da, g), *args, **kwargs), da, g)
    bands = []
    for i in range(da.sizes[extra[0]]):
        band = da.isel({extra[0]: i})
        raster = func(to_raster(band, g), *args, **kwargs)
        if raster.count > 1:
            raise ValueError(
                f"{getattr(func, '__name__', 'the operation')} returns {raster.count} bands per "
                f"band; run it on each 2-D slice of {extra[0]!r}"
            )
        bands.append(from_raster(raster, band, g))
    out = xarray.concat(bands, dim=extra[0]).transpose(*da.dims)
    out.terrio.set_spatial_dims(x_dim=g.x, y_dim=g.y, inplace=True)
    return out


def rasterize(src: Any, like: xarray.DataArray | xarray.Dataset, **kwargs: Any) -> xarray.DataArray:
    """`terrio.rasterize` of `src` on the grid of `like`, as a DataArray.

    A single band comes back 2-D, several along a `band` dimension.
    """
    g = _grid(like)._replace(nodata=None, encoded=None)
    grid = xarray.DataArray(
        np.zeros((like.sizes[g.y], like.sizes[g.x]), np.uint8),
        dims=(g.y, g.x),
        coords={g.y: like[g.y], g.x: like[g.x]},
    )
    raster = _terrio.rasterize(src, like=to_raster(grid, g), **kwargs)
    out = from_raster(raster, grid, g)
    out.name = None
    return out


def zonal_stats(
    rasters: Sequence[xarray.DataArray], zones: Any, prefixes: Sequence[str] | None, **kwargs: Any
) -> _terrio.Layer:
    """`terrio.zonal_stats` of `zones` over `rasters`."""
    return _terrio.zonal_stats(
        zones, [to_raster(da) for da in rasters], prefixes=prefixes, **kwargs
    )
