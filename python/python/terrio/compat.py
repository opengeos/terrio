"""Run code written for rasterio, rioxarray and pyogrio on terrio, in a process without GDAL.

`install()` makes `import rasterio` give `terrio.rio` and `import rasterio.crs` give
`terrio.rio.crs`, for every submodule; `import rioxarray` give `terrio.xarray`, with its accessor
registered as `.rio`; and `import pyogrio` give `terrio.pyogrio`, when terrio has it. The modules
are terrio's own objects, not copies, so `rasterio.crs.CRS is terrio.rio.crs.CRS` and an
exception terrio raises is the one `except rasterio.errors.RasterioIOError` catches. Nothing is
imported until code imports it. A library whose terrio module cannot be imported (`rioxarray`
without xarray) is not stood in for: it is found, or not found, as without terrio. Once `rasterio`
is terrio's, each HTTP request terrio retries is logged as rasterio logs GDAL's warning for it
(`CPLE_AppDefined in HTTP error code: 0 - URL. Retrying again in 0.5 secs`, on `rasterio._env`),
and so is each warp kernel run that moves warped values off the output nodata value
(`CPLE_AppDefined:Value 255 in the source dataset has been changed to 254 ...`, on `rasterio._err`).

An environment without GDAL runs it from a `.pth` file in its `site-packages`, before any user
code (see `PTH_LINE`)::

    import terrio.compat; terrio.compat.install()
"""

from __future__ import annotations

import importlib
import importlib.abc
import importlib.machinery
import importlib.util
import logging
import sys
import threading
from collections.abc import Sequence
from types import ModuleType

#: The libraries `install` stands in for, and the terrio module that stands in for each.
TARGETS: dict[str, str] = {
    "rasterio": "terrio.rio",
    "rioxarray": "terrio.xarray",
    "pyogrio": "terrio.pyogrio",
}

#: The `rasterio` submodules an application imports, from an inventory of its GDAL use and
#: of its `import` statements. Any other submodule terrio.rio has is aliased too, when something
#: imports it.
RASTERIO_SUBMODULES: tuple[str, ...] = (
    "control",
    "coords",
    "crs",
    "dtypes",
    "enums",
    "env",
    "errors",
    "features",
    "io",
    "mask",
    "merge",
    "plot",
    "profiles",
    "rio",
    "rio.helpers",
    "rio.overview",
    "shutil",
    "transform",
    "vrt",
    "warp",
    "windows",
)

#: The line of the `.pth` file that runs `install` at interpreter start.
PTH_LINE = "import terrio.compat; terrio.compat.install()\n"


class AlreadyImportedError(RuntimeError):
    """A library `install` would stand in for was imported first, so two would coexist."""


def _log_http_retry(message: str) -> None:
    """Logs a retried HTTP request as rasterio logs GDAL's warning for it.

    GDAL warns `HTTP error code: 0 - URL. Retrying again in 0.5 secs` (`CPLE_AppDefined`; code 0
    for a connection error) before each retry, and rasterio's error handler logs a warning at
    WARNING on the `rasterio._env` logger, as `"%s in %s"` of the error class and the message.

    Args:
        message: The warning, worded as GDAL's.
    """
    logging.getLogger("rasterio._env").log(logging.WARNING, "%s in %s", "CPLE_AppDefined", message)


def _log_warp_nodata(message: str) -> None:
    """Logs a kernel run that moved values off the output nodata as rasterio logs GDAL's warning.

    GDAL's warper warns `Value 255 in the source dataset has been changed to 254 in the destination
    dataset to avoid being treated as NoData. ...` (`CPLE_AppDefined`) once per kernel run, and
    rasterio's error handler for its warps and reads logs it at WARNING on the `rasterio._err`
    logger, as `"%s:%s"` of the error class and the message. (That handler is the calling thread's:
    with `num_threads` > 1, GDAL's worker threads print it to stderr instead; terrio logs it.)

    Args:
        message: The warning, worded as GDAL's.
    """
    logging.getLogger("rasterio._err").log(logging.WARNING, "%s:%s", "CPLE_AppDefined", message)


class _AliasImporter(importlib.abc.MetaPathFinder, importlib.abc.Loader):
    """Resolves `rasterio`, `rasterio.<name>`... to the terrio module that stands in for each.

    The import system gets terrio's module object itself, so it is never executed twice under two
    names. A name terrio does not have is left to the other finders, which fail to find it as
    they would without terrio (or find the real library, when it is installed).
    """

    def __init__(self) -> None:
        self.targets: dict[str, str] = {}
        self._specs: dict[str, importlib.machinery.ModuleSpec | None] = {}
        self._lock = threading.Lock()

    def real_name(self, fullname: str) -> str | None:
        """The terrio module that stands in for `fullname`, or None.

        Args:
            fullname: A module name such as `rasterio.crs`.

        Returns:
            str | None: The terrio module name, such as `terrio.rio.crs`.
        """
        top, _, rest = fullname.partition(".")
        base = self.targets.get(top)
        if base is None:
            return None
        return f"{base}.{rest}" if rest else base

    def find_spec(
        self,
        fullname: str,
        path: Sequence[str] | None = None,
        target: ModuleType | None = None,
    ) -> importlib.machinery.ModuleSpec | None:
        """Finds the terrio module that stands in for `fullname`.

        Args:
            fullname: The module name being imported.
            path: The parent package's search path (unused).
            target: The module being reloaded (unused).

        Returns:
            importlib.machinery.ModuleSpec | None: A spec whose loader gives terrio's module, or
                None when terrio has no module for the name, or cannot import the one that stands
                in for a library (`terrio.xarray` without xarray), as the import system finds
                nothing for a library that is not installed.

        Raises:
            ImportError: When terrio's module for a submodule (`rasterio.<name>`) fails to import.
        """
        real = self.real_name(fullname)
        if real is None:
            return None
        try:
            if importlib.util.find_spec(real) is None:
                return None
        except ModuleNotFoundError:
            return None  # A parent terrio does not have (terrio.pyogrio before it exists).
        try:
            module = importlib.import_module(real)
        except ImportError:
            if "." in fullname:
                raise
            # terrio's module cannot be imported here (terrio.xarray without xarray), so there is
            # nothing to stand in with: the name is left to the other finders, which find the
            # real library when it is installed and nothing otherwise, as without terrio.
            # `importlib.util.find_spec("rioxarray")` then returns None instead of raising.
            return None
        if fullname == "rioxarray":
            # Before `rioxarray` is in sys.modules, so `.rio` becomes terrio's accessor.
            module.register_rio_accessor()
        if fullname == "rasterio":
            # rasterio logs GDAL's warnings for retried HTTP requests: terrio's are logged alike.
            extension = importlib.import_module("terrio._terrio")
            setter = getattr(extension, "_set_http_retry_logger", None)
            if setter is not None:
                setter(_log_http_retry)
            # So are its warper's warnings for values moved off the output nodata.
            setter = getattr(extension, "_set_warp_nodata_logger", None)
            if setter is not None:
                setter(_log_warp_nodata)
        with self._lock:
            self._specs[fullname] = module.__spec__
        return importlib.util.spec_from_loader(
            fullname, self, is_package=hasattr(module, "__path__")
        )

    def create_module(self, spec: importlib.machinery.ModuleSpec) -> ModuleType:
        """Gives the import system terrio's module itself.

        Args:
            spec: The spec `find_spec` returned.

        Returns:
            ModuleType: The terrio module.
        """
        real = self.real_name(spec.name)
        assert real is not None
        return sys.modules[real]

    def exec_module(self, module: ModuleType) -> None:
        """Gives the module back its own spec, which the import system replaced with the alias's.

        Args:
            module: The terrio module.
        """
        spec = module.__spec__
        if spec is not None:
            with self._lock:
                original = self._specs.pop(spec.name, spec)
            module.__spec__ = original


_IMPORTER = _AliasImporter()


def _is_alias(name: str, module: object) -> bool:
    """Whether `module`, found in sys.modules as `name`, is the terrio module standing in for it.

    Args:
        name: The name in sys.modules.
        module: The module there.

    Returns:
        bool: True for terrio's module, False for the real library's.
    """
    top, _, rest = name.partition(".")
    real = TARGETS[top] + (f".{rest}" if rest else "")
    return module is sys.modules.get(real)


def install(rasterio: bool = True, rioxarray: bool = True, pyogrio: bool = True) -> None:
    """Makes the chosen libraries' imports give terrio's modules, for the rest of the process.

    Calling it again adds libraries; it never removes one.

    Args:
        rasterio: Stand in for `rasterio` and all its submodules with `terrio.rio`.
        rioxarray: Stand in for `rioxarray` with `terrio.xarray`, whose accessor becomes `.rio`.
        pyogrio: Stand in for `pyogrio` with `terrio.pyogrio`, when terrio has it.

    Raises:
        AlreadyImportedError: When one of these libraries, the real one, is already imported.
    """
    wanted = [name for name, on in (("rasterio", rasterio), ("rioxarray", rioxarray)) if on]
    if pyogrio and importlib.util.find_spec(TARGETS["pyogrio"]) is not None:
        wanted.append("pyogrio")
    real = sorted(
        name
        for name, module in list(sys.modules.items())
        if name.partition(".")[0] in wanted and module is not None and not _is_alias(name, module)
    )
    if real:
        raise AlreadyImportedError(
            f"{', '.join(real)} imported before terrio.compat.install(): the real library and terrio "
            "would both be in use. Install terrio.compat first (its .pth file does at interpreter start)."
        )
    for name in wanted:
        _IMPORTER.targets[name] = TARGETS[name]
    if _IMPORTER not in sys.meta_path:
        sys.meta_path.insert(0, _IMPORTER)


def installed() -> dict[str, str]:
    """The libraries `install` stands in for, and the terrio module for each.

    Returns:
        dict[str, str]: Library name to terrio module name.
    """
    return dict(_IMPORTER.targets) if _IMPORTER in sys.meta_path else {}
