# Written to match the behavior of:
#   rioxarray 0.23.0:
#     rioxarray/
# No code is translated from these sources.

"""Exceptions of `terrio.xarray`, with rioxarray's names and hierarchy."""

from __future__ import annotations


class RioXarrayError(RuntimeError):
    """The base exception of the xarray extension."""


class NoDataInBounds(RioXarrayError):
    """There is no data in the bounds a raster is clipped to."""


class SingleVariableDataset(RioXarrayError):
    """A dataset has a single variable."""


class DimensionError(RioXarrayError):
    """There are more dimensions than a method supports."""


class MissingSpatialDimensionError(DimensionError):
    """A spatial dimension cannot be found."""


class TooManyDimensions(DimensionError):
    """There are more dimensions than a method supports."""


class InvalidDimensionOrder(DimensionError):
    """The dimensions are not ordered as a method expects."""


class OneDimensionalRaster(DimensionError):
    """A raster has a single row or column."""


class DimensionMissingCoordinateError(RioXarrayError):
    """A dimension has no coordinate."""


class MissingCRS(RioXarrayError):
    """The dataset has no CRS."""


__all__ = [
    "DimensionError",
    "DimensionMissingCoordinateError",
    "InvalidDimensionOrder",
    "MissingCRS",
    "MissingSpatialDimensionError",
    "NoDataInBounds",
    "OneDimensionalRaster",
    "RioXarrayError",
    "SingleVariableDataset",
    "TooManyDimensions",
]
