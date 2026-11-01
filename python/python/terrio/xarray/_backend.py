# SPDX-FileCopyrightText: Copyright (c) 2019-2025, Corteva Agriscience™
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND Apache-2.0
#
# Derived from (adapted and modified):
#   rioxarray 0.23.0:
#     rioxarray/xarray_plugin.py

"""The `terrio` xarray backend: `xr.open_dataset(path, engine="terrio")`."""

from __future__ import annotations

import os
from collections.abc import Iterable
from typing import Any

import xarray
from xarray.backends import BackendEntrypoint

CAN_OPEN_EXTS = {
    "asc",
    "geotif",
    "geotiff",
    "img",
    "j2k",
    "jp2",
    "jpg",
    "jpeg",
    "png",
    "tif",
    "tiff",
    "vrt",
}


class TerrioBackendEntrypoint(BackendEntrypoint):
    """Opens rasters with terrio, as rioxarray's `rasterio` engine opens them with GDAL.

    Bands become a `band` dimension of the `band_data` variable (or variables `band_1`,
    `band_2`... with `band_as_variable=True`); pixels are read lazily by window.
    """

    description = "Open rasters (GeoTIFF, COG, Zarr, netCDF...) with terrio, without GDAL"
    url = "https://terrio.dev/xarray/"
    open_dataset_parameters = (
        "filename_or_obj",
        "drop_variables",
        "parse_coordinates",
        "lock",
        "masked",
        "mask_and_scale",
        "variable",
        "group",
        "default_name",
        "decode_coords",
        "decode_times",
        "decode_timedelta",
        "band_as_variable",
        "overview_level",
        "open_kwargs",
    )

    def open_dataset(
        self,
        filename_or_obj: Any,
        *,
        drop_variables: str | Iterable[str] | None = None,
        parse_coordinates: bool | None = None,
        lock: Any = None,
        masked: bool = False,
        mask_and_scale: bool = True,
        variable: Any = None,
        group: Any = None,
        default_name: str = "band_data",
        decode_coords: Any = "all",
        decode_times: bool = True,
        decode_timedelta: bool | None = None,
        band_as_variable: bool = False,
        overview_level: int | None = None,
        open_kwargs: dict[str, Any] | None = None,
    ) -> xarray.Dataset:
        """Opens a raster as a Dataset.

        Args:
            filename_or_obj: A path or URL.
            drop_variables: Variables to drop.
            parse_coordinates: Generate x and y coordinates.
            lock: A lock to hold while reading.
            masked: Mask nodata pixels with NaN.
            mask_and_scale: Mask, and apply scales and offsets (the default, as in rioxarray).
            variable: Unsupported (subdatasets).
            group: Unsupported (subdatasets).
            default_name: The variable's name.
            decode_coords: Only `"all"` is supported.
            decode_times: Decode CF times.
            decode_timedelta: Decode CF time deltas.
            band_as_variable: Open each band as a variable.
            overview_level: Open an overview instead of the full resolution.
            open_kwargs: Other options of `open_rasterio`.

        Returns:
            xarray.Dataset: The raster.

        Raises:
            RioXarrayError: If `decode_coords` is not `"all"`.
        """
        from terrio.xarray import open_rasterio
        from terrio.xarray.exceptions import RioXarrayError

        if decode_coords not in ("all", True):
            raise RioXarrayError("Only 'all' is supported for 'decode_coords'.")
        rds = open_rasterio(
            filename_or_obj,
            parse_coordinates=parse_coordinates,
            cache=False,
            lock=lock,
            masked=masked,
            mask_and_scale=mask_and_scale,
            variable=variable,
            group=group,
            default_name=default_name,
            decode_times=decode_times,
            decode_timedelta=decode_timedelta,
            band_as_variable=band_as_variable,
            overview_level=overview_level,
            **(open_kwargs or {}),
        )
        if isinstance(rds, list):
            raise RioXarrayError(
                "Multiple resolution sets found. Use 'variable' or 'group' to filter."
            )
        if isinstance(rds, xarray.DataArray):
            dataset = rds.to_dataset()
            dataset.set_close(rds._close)
            rds = dataset
        if drop_variables is not None:
            rds = rds.drop_vars(drop_variables)
        return rds

    def guess_can_open(self, filename_or_obj: Any) -> bool:
        """Whether a path looks like a raster terrio opens.

        Args:
            filename_or_obj: The path.

        Returns:
            bool: True for raster file extensions.
        """
        try:
            _, ext = os.path.splitext(filename_or_obj)
        except TypeError:
            return False
        return ext[1:].lower() in CAN_OPEN_EXTS
