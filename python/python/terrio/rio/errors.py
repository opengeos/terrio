# Written to match the behavior of:
#   rasterio 1.5.2:
#     rasterio/
# No code is translated from these sources.

"""Errors and warnings, named as rasterio names them.

Errors about files derive from `OSError` and errors about arguments from `ValueError`, as in
rasterio, so that code catching rasterio's exceptions (or the built-in ones) keeps working.
"""

from __future__ import annotations


class RasterioError(Exception):
    """Base class of the errors raised by `terrio.rio`."""


class InvalidArrayError(RasterioError):
    """An array is not valid for an operation."""


class WindowError(RasterioError):
    """A window is not valid."""


class CRSError(ValueError):
    """A coordinate reference system is not valid or cannot be used."""


class EnvError(RasterioError):
    """The environment cannot be set up."""


class DriverCapabilityError(RasterioError, ValueError):
    """The driver cannot do the operation (such as writing)."""


class DriverRegistrationError(ValueError):
    """The driver is unknown."""


class FileOverwriteError(RasterioError):
    """A file would be overwritten."""


class RasterioIOError(RasterioError, OSError):
    """A dataset cannot be opened, read or written."""


class NodataShadowWarning(UserWarning):
    """The nodata value shadows the alpha band."""

    def __str__(self) -> str:
        """The warning's message.

        Returns:
            str: The message.
        """
        return (
            "The dataset's nodata attribute is shadowing the alpha band. All masks will be "
            "determined by the nodata attribute"
        )


class NotGeoreferencedWarning(UserWarning):
    """The dataset has no georeferencing."""


class TransformWarning(UserWarning):
    """A transform could not be computed exactly."""


class RPCError(ValueError):
    """Rational polynomial coefficients are not valid."""


class InvalidShapeError(RasterioError, ValueError):
    """A geometry is not valid."""


class ShapeSkipWarning(UserWarning):
    """A geometry was skipped."""


class GDALBehaviorChangeException(RuntimeError):
    """Raised by rasterio when GDAL behaviour differs between versions; kept for compatibility."""


class GDALOptionNotImplementedError(RasterioError):
    """An option is not implemented."""


class GDALVersionError(RasterioError):
    """Raised by rasterio for unsupported GDAL versions; kept for compatibility."""


class WindowEvaluationError(ValueError):
    """A window cannot be evaluated."""


class RasterioDeprecationWarning(FutureWarning):
    """A deprecated feature was used."""


class RasterBlockError(RasterioError):
    """A block size is not valid."""


class BandOverviewError(UserWarning):
    """A band overview does not exist."""


class WarpOptionsError(RasterioError):
    """Warp options are not valid."""


class UnsupportedOperation(RasterioError):
    """The operation is not supported in the dataset's mode."""


class OverviewCreationError(RasterioError):
    """Overviews cannot be created."""


class DatasetAttributeError(RasterioError, NotImplementedError):
    """A dataset attribute cannot be set."""


class PathError(RasterioError):
    """A path cannot be parsed."""


class ResamplingAlgorithmError(RasterioError):
    """A resampling method is not valid for the operation."""


class TransformError(RasterioError):
    """Coordinates cannot be transformed."""


class WarpedVRTError(RasterioError):
    """A warped VRT cannot be created."""


class DatasetIOShapeError(RasterioError):
    """Array and dataset shapes do not match."""


class WarpOperationError(RasterioError):
    """A warp failed."""


class StatisticsError(RasterioError):
    """Statistics cannot be computed."""


class OpenerRegistrationError(RasterioError):
    """A file opener cannot be registered."""


class MergeError(RasterioError):
    """Datasets cannot be merged."""


class StackError(RasterioError):
    """Datasets cannot be stacked."""


__all__ = [
    "BandOverviewError",
    "CRSError",
    "DatasetAttributeError",
    "DatasetIOShapeError",
    "DriverCapabilityError",
    "DriverRegistrationError",
    "EnvError",
    "FileOverwriteError",
    "GDALBehaviorChangeException",
    "GDALOptionNotImplementedError",
    "GDALVersionError",
    "InvalidArrayError",
    "InvalidShapeError",
    "MergeError",
    "NodataShadowWarning",
    "NotGeoreferencedWarning",
    "OpenerRegistrationError",
    "OverviewCreationError",
    "PathError",
    "RPCError",
    "RasterBlockError",
    "RasterioDeprecationWarning",
    "RasterioError",
    "RasterioIOError",
    "ResamplingAlgorithmError",
    "ShapeSkipWarning",
    "StackError",
    "StatisticsError",
    "TransformError",
    "TransformWarning",
    "UnsupportedOperation",
    "WarpOperationError",
    "WarpOptionsError",
    "WarpedVRTError",
    "WindowError",
    "WindowEvaluationError",
]
