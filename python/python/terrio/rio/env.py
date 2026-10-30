# Written to match the behavior of:
#   rasterio 1.5.2:
#     rasterio/
# No code is translated from these sources.

"""`Env`: a context for configuration options and credentials.

terrio reads its network settings and cloud credentials from options named as GDAL's
configuration options (`AWS_ACCESS_KEY_ID`, `AWS_NO_SIGN_REQUEST`, `GDAL_HTTP_TIMEOUT`,
`GDAL_HTTP_HEADERS`, `AZURE_STORAGE_ACCESS_TOKEN`, `CPL_VSIL_CURL_CHUNK_SIZE`...): the options set
for the calling thread, else the environment variables of the same names. `Env` sets the options it
is given for the calling thread only while it is active, as rasterio's `Env` sets GDAL's
thread-local configuration options: another thread does not see them, so two threads can read
with different credentials. Options terrio has no use for (such as `GDAL_CACHEMAX`) are accepted
and have no effect.
"""

from __future__ import annotations

import os
import threading
from collections.abc import Callable
from functools import wraps
from typing import Any

from terrio.rio.session import DummySession, Session

_local = threading.local()


def _stack() -> list[Env]:
    """The active environments of this thread.

    Returns:
        list: The environments, innermost last.
    """
    if not hasattr(_local, "stack"):
        _local.stack = []
    stack: list[Env] = _local.stack
    return stack


class Env:
    """Configuration options and credentials, active within a `with` block.

    Example::

        with terrio.rio.Env(AWS_NO_SIGN_REQUEST="YES"):
            with terrio.rio.open("s3://bucket/cog.tif") as src:
                ...
    """

    def __init__(
        self,
        session: Session | None = None,
        aws_unsigned: bool = False,
        profile_name: str | None = None,
        session_class: Any = None,
        **options: Any,
    ) -> None:
        """Creates the environment.

        Args:
            session: A session whose credentials to use.
            aws_unsigned: Make unsigned requests to S3.
            profile_name: An AWS profile.
            session_class: Unused.
            **options: GDAL configuration options.
        """
        if aws_unsigned:
            options.setdefault("AWS_NO_SIGN_REQUEST", "YES")
        if profile_name:
            options.setdefault("AWS_PROFILE", profile_name)
        if session is not None and not hasattr(session, "get_credential_options"):
            session = Session.from_foreign_session(session)
        self.session: Session = session or DummySession()
        self.options = {k: v for k, v in options.items()}

    @classmethod
    def from_defaults(cls, *args: Any, **kwargs: Any) -> Env:
        """An environment with rasterio's default options.

        Args:
            *args: Arguments of `Env`.
            **kwargs: Options of `Env`.

        Returns:
            Env: The environment.
        """
        opts: dict[str, Any] = {"CHECK_WITH_INVERT_PROJ": True, "GTIFF_IMPLICIT_JPEG_OVR": False}
        opts.update(kwargs)
        return cls(*args, **opts)

    @staticmethod
    def _value(v: Any) -> str:
        """A configuration value as text.

        Args:
            v: The value.

        Returns:
            str: `ON`/`OFF` for booleans, else the value as text.
        """
        if isinstance(v, bool):
            return "ON" if v else "OFF"
        return str(v)

    def credentialize(self) -> None:
        """Sets the session's credentials as options."""
        for k, v in self.session.get_credential_options().items():
            self.options.setdefault(k, v)

    def drivers(self) -> dict[str, str]:
        """The raster drivers terrio has.

        Returns:
            dict: Driver short names and long names.
        """
        from terrio import _terrio

        return {d["name"]: d.get("long_name", d["name"]) for d in _terrio.drivers()}

    def _scope(self) -> dict[str, str | None]:
        """The options as the calling thread's configuration scope.

        Returns:
            dict: Option names and text values.
        """
        return {k: None if v is None else self._value(v) for k, v in self.options.items()}

    def __enter__(self) -> Env:
        """Activates the options for the calling thread.

        Returns:
            Env: The environment.
        """
        from terrio import _terrio

        self.credentialize()
        _terrio.config_push(self._scope())
        _stack().append(self)
        return self

    def __exit__(self, *args: object) -> None:
        """Deactivates the options.

        Args:
            *args: Exception details.
        """
        from terrio import _terrio

        stack = _stack()
        if stack and stack[-1] is self:
            stack.pop()
            _terrio.config_pop()


def getenv() -> dict[str, Any]:
    """The options of the innermost active environment.

    Returns:
        dict: The options.

    Raises:
        EnvError: If no environment is active.
    """
    from terrio.rio.errors import EnvError

    stack = _stack()
    if not stack:
        raise EnvError("No GDAL environment exists")
    return dict(stack[-1].options)


def hasenv() -> bool:
    """Whether an environment is active.

    Returns:
        bool: True inside an `Env` block.
    """
    return bool(_stack())


def setenv(**options: Any) -> None:
    """Sets options in the active environment (or the process environment).

    Args:
        **options: GDAL configuration options.
    """
    from terrio import _terrio

    stack = _stack()
    if stack:
        # Replace the innermost scope with one holding the new options too.
        env = stack[-1]
        env.options.update(options)
        _terrio.config_pop()
        _terrio.config_push(env._scope())
        return
    for k, v in options.items():
        os.environ[k] = Env._value(v)


def delenv() -> None:
    """Restores the environment of the innermost active `Env`."""
    stack = _stack()
    if stack:
        stack[-1].__exit__()


def ensure_env(f: Callable[..., Any]) -> Callable[..., Any]:
    """Runs a function in an `Env` when none is active.

    Args:
        f: The function.

    Returns:
        callable: The wrapped function.
    """

    @wraps(f)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        if hasenv():
            return f(*args, **kwargs)
        with Env.from_defaults():
            return f(*args, **kwargs)

    return wrapper


ensure_env_with_credentials = ensure_env


class GDALVersion:
    """A version number, for code that checks rasterio's GDAL version."""

    def __init__(self, major: int = 0, minor: int = 0) -> None:
        """Creates a version.

        Args:
            major: The major version.
            minor: The minor version.
        """
        self.major, self.minor = major, minor

    @classmethod
    def runtime(cls) -> GDALVersion:
        """The GDAL version terrio's behaviour follows.

        Returns:
            GDALVersion: 3.12.
        """
        return cls(3, 12)

    def at_least(self, other: Any) -> bool:
        """Whether this version is at least another.

        Args:
            other: A version or a `"major.minor"` string.

        Returns:
            bool: True when not older.
        """
        if isinstance(other, str):
            parts = [int(p) for p in other.split(".")[:2]] + [0]
            other = GDALVersion(parts[0], parts[1])
        return (self.major, self.minor) >= (other.major, other.minor)


__all__ = ["Env", "GDALVersion", "delenv", "ensure_env", "getenv", "hasenv", "setenv"]
