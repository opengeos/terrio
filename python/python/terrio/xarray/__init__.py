# Written to match the behavior of:
#   rioxarray 0.23.0:
#     rioxarray/
# No code is translated from these sources.

"""xarray integration: rioxarray's API on terrio, without GDAL.

Open rasters lazily as DataArrays, with rioxarray's coordinates, attributes and encoding, and
work on them with the `.terrio` accessor, which has rioxarray's `.rio` methods::

    import xarray as xr
    import terrio.xarray as txr

    da = txr.open_rasterio("dem.tif", masked=True, chunks=True)  # or the xarray backend:
    ds = xr.open_dataset("dem.tif", engine="terrio")

    da.terrio.crs, da.terrio.transform(), da.terrio.bounds(), da.terrio.nodata
    sub = da.terrio.clip_box(-105.5, 39.5, -105.0, 40.0, crs="EPSG:4326")  # reads the window only
    utm = da.terrio.reproject(da.terrio.estimate_utm_crs(), resampling="bilinear")
    utm.terrio.to_raster("dem_utm.tif", driver="COG", compress="DEFLATE")
    utm.terrio.to_raster("dem_utm.zarr")  # GeoZarr

`register_rio_accessor()` also registers the accessor as `.rio` (when rioxarray is not
imported), so that code written for rioxarray runs unchanged.

Requires xarray (and dask for `chunks`). The API follows rioxarray (Apache-2.0, Copyright (c)
2019-2023 rioxarray Contributors).
"""

from __future__ import annotations

import sys
import warnings

try:
    import xarray as _xarray
except ImportError:  # pragma: no cover
    raise ImportError(
        "terrio.xarray needs xarray: pip install 'terrio[xarray]' (or pip install xarray)"
    ) from None

from terrio.xarray._accessor import TerrioArray, TerrioDataset, XRasterBase

with warnings.catch_warnings():
    # Registering again (on reload) replaces the accessor: that is intended.
    warnings.simplefilter("ignore", _xarray.core.extensions.AccessorRegistrationWarning)
    _xarray.register_dataarray_accessor("terrio")(TerrioArray)  # type: ignore[no-untyped-call]
    _xarray.register_dataset_accessor("terrio")(TerrioDataset)  # type: ignore[no-untyped-call]

from terrio.xarray import exceptions  # noqa: E402
from terrio.xarray._crs import crs_from_user_input, crs_to_cf  # noqa: E402
from terrio.xarray._io import TerrioArrayWrapper, open_rasterio  # noqa: E402
from terrio.xarray._merge import merge_arrays, merge_datasets  # noqa: E402
from terrio.xarray._options import set_options  # noqa: E402


def register_rio_accessor(force: bool = False) -> bool:
    """Registers the accessors as `.rio` too, for code written for rioxarray.

    Nothing is registered when rioxarray is imported (its `.rio` stays), unless `force` is True.

    Args:
        force: Replace rioxarray's `.rio` accessors.

    Returns:
        bool: True when the accessors were registered.
    """
    if "rioxarray" in sys.modules and not force:
        return False
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", _xarray.core.extensions.AccessorRegistrationWarning)
        _xarray.register_dataarray_accessor("rio")(_shared_accessor)  # type: ignore[no-untyped-call]
        _xarray.register_dataset_accessor("rio")(_shared_accessor)  # type: ignore[no-untyped-call]
    return True


def _shared_accessor(obj: _xarray.DataArray | _xarray.Dataset) -> TerrioArray | TerrioDataset:
    """The `.terrio` accessor of an object, shared with `.rio` so that both see the same state.

    Args:
        obj: The DataArray or Dataset.

    Returns:
        TerrioArray | TerrioDataset: Its accessor.
    """
    acc: TerrioArray | TerrioDataset = obj.terrio
    return acc


__all__ = [
    "TerrioArray",
    "TerrioArrayWrapper",
    "TerrioDataset",
    "XRasterBase",
    "crs_from_user_input",
    "crs_to_cf",
    "exceptions",
    "merge_arrays",
    "merge_datasets",
    "open_rasterio",
    "register_rio_accessor",
    "set_options",
]
