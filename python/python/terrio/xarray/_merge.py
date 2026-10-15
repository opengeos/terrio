"""Merging DataArrays and Datasets, as rioxarray's `merge_arrays` and `merge_datasets` do.

They follow rioxarray's `merge` module (Apache-2.0, Copyright (c) 2019-2023 rioxarray
Contributors) with `terrio.rio.merge.merge` in place of rasterio's.
"""

from __future__ import annotations

from collections.abc import Callable, Hashable, Sequence
from typing import Any

import numpy as np
import xarray

from terrio.rio.merge import merge as _rio_merge
from terrio.xarray._spatial import get_nonspatial_coords, index_variable, make_coords


class RasterioDatasetDuck:
    """A DataArray looking like a `terrio.rio` dataset, for `terrio.rio.merge.merge`."""

    def __init__(self, xds: xarray.DataArray) -> None:
        """Wraps an array.

        Args:
            xds: The array.
        """
        acc = xds.terrio
        self._xds = xds
        self.crs = acc.crs
        self.bounds = acc.bounds(recalc=True)
        self.count = int(acc.count)
        self.dtypes = [xds.dtype]
        self.name = xds.name
        self.nodatavals = [acc.nodata]
        res = acc.resolution(recalc=True)
        self.res = (abs(res[0]), abs(res[1]))
        self.transform = acc.transform(recalc=True)
        self.profile: dict[str, Any] = {}

    def __enter__(self) -> RasterioDatasetDuck:
        """Enters a `with` block.

        Returns:
            RasterioDatasetDuck: Itself.
        """
        return self

    def __exit__(self, *args: object) -> None:
        """Leaves a `with` block.

        Args:
            *args: Exception information (unused).
        """

    def colormap(self, *args: Any, **kwargs: Any) -> None:
        """The color table (none).

        Args:
            *args: Unused.
            **kwargs: Unused.
        """
        return None

    def read(
        self, window: Any = None, out_shape: Any = None, *args: Any, **kwargs: Any
    ) -> np.ma.MaskedArray[Any, Any]:
        """Reads a window, masked with the nodata value.

        Args:
            window: The window.
            out_shape: The output shape.
            *args: Unused.
            **kwargs: Unused.

        Returns:
            numpy.ma.MaskedArray: The pixels.
        """
        data_window = self._xds.terrio.isel_window(window)
        if out_shape is not None and data_window.shape != tuple(out_shape):
            if len(out_shape) == 3:
                _, out_height, out_width = out_shape
            else:
                out_height, out_width = out_shape
            data_window = self._xds.terrio.reproject(
                self._xds.terrio.crs,
                transform=self.transform,
                shape=(out_height, out_width),
            )
        values = np.asarray(data_window.values)
        nodata = self.nodatavals[0]
        mask: Any = False
        fill_value = None
        if nodata is not None and np.isnan(nodata):
            mask = np.isnan(values)
        elif nodata is not None:
            mask = values == nodata
            fill_value = nodata
        if out_shape is not None and len(out_shape) == 3 and values.ndim == 2:
            values = values.reshape((1,) + values.shape)
            if not isinstance(mask, bool):
                mask = mask.reshape(values.shape)
        return np.ma.array(values, mask=mask, fill_value=fill_value, dtype=self.dtypes[0])


def merge_arrays(
    dataarrays: Sequence[xarray.DataArray],
    *,
    bounds: tuple[float, float, float, float] | None = None,
    res: tuple[float, float] | float | None = None,
    nodata: float | None = None,
    precision: float | None = None,
    method: str | Callable[..., Any] | None = None,
    crs: Any = None,
    parse_coordinates: bool = True,
) -> xarray.DataArray:
    """Merges DataArrays into one, as `rioxarray.merge.merge_arrays` does.

    Arrays in another CRS or resolution are reprojected to the first's first.

    Args:
        dataarrays: The arrays.
        bounds: The output bounds `(left, bottom, right, top)` (default: the union).
        res: The output resolution (default: the first array's).
        nodata: The output nodata value (default: the first array's).
        precision: Unused (deprecated by rasterio).
        method: `first`, `last`, `min`, `max`, `sum`, `count`, or a function.
        crs: The output CRS (default: the first array's).
        parse_coordinates: Generate x and y coordinates.

    Returns:
        xarray.DataArray: The merged array.
    """
    input_kwargs: dict[str, Any] = {
        "bounds": bounds,
        "res": res,
        "nodata": nodata,
        "method": method,
    }
    if crs is None:
        crs = dataarrays[0].terrio.crs
    if res is None:
        res = tuple(abs(v) for v in dataarrays[0].terrio.resolution())
    elif isinstance(res, (int, float)):
        res = (float(res), float(res))
    else:
        res = (res[0], res[0]) if len(res) == 1 else (res[0], res[1])
    ducks = []
    for da in dataarrays:
        da_res = tuple(abs(v) for v in da.terrio.resolution())
        if da_res != res or da.terrio.crs != crs:
            ducks.append(RasterioDatasetDuck(da.terrio.reproject(dst_crs=crs, resolution=res)))
        else:
            ducks.append(RasterioDatasetDuck(da))
    merged_data, merged_transform = _rio_merge(
        ducks, **{k: v for k, v in input_kwargs.items() if v is not None}
    )
    representative = ducks[0]._xds
    racc = representative.terrio
    coords: dict[Hashable, Any]
    if parse_coordinates:
        coords = make_coords(
            src_data_array=representative,
            dst_affine=merged_transform,
            dst_width=merged_data.shape[-1],
            dst_height=merged_data.shape[-2],
        )
        if racc.x_dim != "x" and "x" in coords and np.ndim(coords["x"]) == 1:
            coords[racc.x_dim] = index_variable(racc.x_dim, coords.pop("x"))
        if racc.y_dim != "y" and "y" in coords and np.ndim(coords["y"]) == 1:
            coords[racc.y_dim] = index_variable(racc.y_dim, coords.pop("y"))
    else:
        coords = dict(get_nonspatial_coords(representative))
    if merged_data.ndim == 3 and representative.ndim == 2:
        merged_data = merged_data.squeeze()
    xda = xarray.DataArray(
        name=representative.name,
        data=merged_data,
        coords=coords,
        dims=tuple(representative.dims),
        attrs=representative.attrs,
    )
    xda.encoding = representative.encoding.copy()
    acc = xda.terrio
    acc.write_nodata(nodata if nodata is not None else racc.nodata, inplace=True)
    acc.write_crs(racc.crs, grid_mapping_name=racc.grid_mapping, inplace=True)
    acc.write_transform(merged_transform, grid_mapping_name=racc.grid_mapping, inplace=True)
    return xda


def merge_datasets(
    datasets: Sequence[xarray.Dataset],
    *,
    bounds: tuple[float, float, float, float] | None = None,
    res: tuple[float, float] | float | None = None,
    nodata: float | None = None,
    precision: float | None = None,
    method: str | Callable[..., Any] | None = None,
    crs: Any = None,
) -> xarray.Dataset:
    """Merges Datasets variable by variable, as `rioxarray.merge.merge_datasets` does.

    Args:
        datasets: The Datasets.
        bounds: The output bounds.
        res: The output resolution.
        nodata: The output nodata value.
        precision: Unused.
        method: The merge method.
        crs: The output CRS.

    Returns:
        xarray.Dataset: The merged Dataset.
    """
    representative = datasets[0]
    merged: dict[Hashable, xarray.DataArray] = {}
    for i, var in enumerate(representative.data_vars):
        merged[var] = merge_arrays(
            [ds[var] for ds in datasets],
            bounds=bounds,
            res=res,
            nodata=nodata,
            precision=precision,
            method=method,
            crs=crs,
            parse_coordinates=i == 0,
        )
    first = list(representative.data_vars)[0]
    xds = xarray.Dataset(merged, attrs=representative.attrs)
    xds.terrio.write_crs(merged[first].terrio.crs, inplace=True)
    return xds
