"""Global options of `terrio.xarray`, as rioxarray's `set_options` defines them."""

from __future__ import annotations

from typing import Any

EXPORT_GRID_MAPPING = "export_grid_mapping"
SKIP_MISSING_SPATIAL_DIMS = "skip_missing_spatial_dims"

OPTIONS: dict[str, Any] = {EXPORT_GRID_MAPPING: True, SKIP_MISSING_SPATIAL_DIMS: False}


def get_option(key: str) -> Any:
    """The value of a global option.

    Args:
        key: The option's name.

    Returns:
        Any: Its value.
    """
    return OPTIONS[key]


class set_options:  # noqa: N801 (rioxarray's name)
    """Sets global options, for good or within a `with` block.

    Options:
        export_grid_mapping (bool, default True): write the CF grid mapping attributes of the CRS
            (else only `crs_wkt` and `spatial_ref`).
        skip_missing_spatial_dims (bool, default False): skip the variables of a Dataset that
            have no spatial dimensions in spatial operations.

    Usage::

        with terrio.xarray.set_options(export_grid_mapping=False):
            da = terrio.xarray.open_rasterio("dem.tif")
    """

    def __init__(self, **kwargs: Any) -> None:
        """Sets options.

        Args:
            **kwargs: Option values.

        Raises:
            ValueError: If an option is unknown or its value invalid.
        """
        self.old = OPTIONS.copy()
        for key, value in kwargs.items():
            if key not in OPTIONS:
                raise ValueError(
                    f"argument name {key} is not in the set of valid options {set(OPTIONS)}."
                )
            if not isinstance(value, bool):
                raise ValueError(f"option {key!r} gave an invalid value: {value!r}.")
            OPTIONS[key] = value

    def __enter__(self) -> None:
        """Enters a `with` block."""

    def __exit__(self, *args: object) -> None:
        """Restores the previous options.

        Args:
            *args: Exception information (unused).
        """
        OPTIONS.clear()
        OPTIONS.update(self.old)
