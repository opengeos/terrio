"""Exceptions raised by terrio.

Every error derives from `TerrioError`. Errors about files and the network also derive from
`OSError`, and errors about arguments and data from `ValueError`, so that code catching the
built-in exceptions keeps working.
"""


class TerrioError(Exception):
    """Base class of the errors terrio raises."""


class TerrioIOError(TerrioError, OSError):
    """A file system or network error."""


class NotFoundError(TerrioIOError, FileNotFoundError):
    """A file or object does not exist."""


class InvalidArgumentError(TerrioError, ValueError):
    """An argument or option is invalid."""


class CRSError(TerrioError, ValueError):
    """A coordinate reference system cannot be parsed or used."""


class UnsupportedError(TerrioError, ValueError):
    """The format is not recognized, or the operation is not supported for it."""


class InvalidDataError(TerrioError, ValueError):
    """A dataset was found but its contents are not valid."""


class InternalError(TerrioError):
    """A bug in terrio that this input reached: a Rust panic, caught before it reached Python.

    Its message starts with "Rust panic in terrio:" and holds the panic's text. The process stays
    usable. (Uncaught, PyO3 would raise `PanicException`, a `BaseException` that
    `except Exception` does not catch.)
    """


__all__ = [
    "CRSError",
    "InternalError",
    "InvalidArgumentError",
    "InvalidDataError",
    "NotFoundError",
    "TerrioError",
    "TerrioIOError",
    "UnsupportedError",
]
