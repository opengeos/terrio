"""Opening rasters as xarray objects: `open_rasterio` and the lazy array behind it.

The output follows rioxarray's `open_rasterio` (Apache-2.0, Copyright (c) 2019-2023 rioxarray
Contributors, itself adapted from xarray's former rasterio backend): the same dimensions,
coordinates, attributes and encoding. Pixels are read by window through terrio: GeoTIFFs and
COGs, local or remote, are read lazily, decoding (and for remote files fetching) only the tiles a
window needs.
"""

from __future__ import annotations

import contextlib
import copy
import os
import re
import threading
import warnings
from collections import defaultdict
from collections.abc import Hashable, Iterable, Iterator, Sequence
from dataclasses import dataclass, field
from typing import Any

import numpy as np
import xarray
from affine import Affine
from xarray.backends.common import BackendArray
from xarray.coding import times, variables
from xarray.core import indexing

from terrio import _terrio
from terrio.rio import _tiff
from terrio.rio import open as rio_open
from terrio.rio.crs import CRS
from terrio.rio.enums import MaskFlags
from terrio.rio.io import DatasetReader, _terrio_path
from terrio.xarray._spatial import (
    FILL_VALUE_NAMES,
    UNWANTED_RIO_ATTRS,
    generate_spatial_coords,
    index_variable,
)
from terrio.xarray.exceptions import RioXarrayError

_maybe_promote: Any
try:  # xarray's promotion rules for masked arrays
    from xarray.core.dtypes import maybe_promote as _maybe_promote
except ImportError:  # pragma: no cover
    _maybe_promote = None

# Helpers xarray's coding module does not export.
_pop_to: Any = getattr(variables, "pop_to")  # noqa: B009
_unpack_for_decoding: Any = getattr(variables, "unpack_for_decoding")  # noqa: B009


def maybe_promote(dtype: np.dtype[Any]) -> tuple[np.dtype[Any], Any]:
    """The data type and fill value of a masked array, as xarray promotes them.

    Args:
        dtype: The data type of the pixels.

    Returns:
        tuple: `(dtype, fill_value)`.
    """
    if _maybe_promote is not None:
        dt, fill = _maybe_promote(dtype)
        return np.dtype(dt), fill
    if dtype.kind == "f":  # pragma: no cover
        return dtype, np.nan
    if dtype.kind in "iu" and dtype.itemsize <= 2:  # pragma: no cover
        return np.dtype("float32"), np.nan
    return np.dtype("float64"), np.nan  # pragma: no cover


def _get_unsigned_dtype(unsigned: bool | None, dtype: Any) -> np.dtype[Any] | None:
    """The data type an `_Unsigned` attribute turns integers into.

    Args:
        unsigned: The attribute's value.
        dtype: The stored data type.

    Returns:
        numpy.dtype | None: The data type, or None when it does not change.
    """
    dtype = np.dtype(dtype)
    if unsigned is True and dtype.kind == "i":
        return np.dtype(f"u{dtype.itemsize}")
    if unsigned is False and dtype.kind == "u":
        return np.dtype(f"i{dtype.itemsize}")
    return None


class _RasterPool:
    """terrio rasters to read a file with, one per concurrent reader.

    terrio rasters read by one thread at a time, so dask threads each borrow a raster of their
    own: a lazily read file (GeoTIFFs, COGs) is opened again for each concurrent reader (only its
    headers are read), up to `max_rasters`. Each raster caches the byte ranges it fetched, so a
    single raster (`max_rasters=1`) fetches tiles shared by chunks (the bands of a
    pixel-interleaved file) once, at the cost of reading one chunk at a time. A file read into
    memory when opened is shared, behind a lock.
    """

    def __init__(self, path: str, first: Any = None, max_rasters: int | None = None) -> None:
        """Creates the pool.

        Args:
            path: The path terrio opens.
            first: An open `terrio.Raster` of the file to start with.
            max_rasters: The most rasters open at once (None: one per concurrent reader).
        """
        self.path = path
        self.max_rasters = max_rasters
        self._cond = threading.Condition()
        self._free: list[Any] = []
        self._open_count = 0
        self._shared: Any = None
        self._shared_lock = threading.Lock()
        if first is not None:
            self._add(first)

    def _add(self, raster: Any) -> None:
        """Keeps a raster for later reads.

        Args:
            raster: The `terrio.Raster`.
        """
        if raster.is_lazy:
            self._free.append(raster)
            self._open_count += 1
        else:
            self._shared = raster

    @contextlib.contextmanager
    def acquire(self) -> Iterator[Any]:
        """Borrows a raster.

        Yields:
            terrio.Raster: A raster no other thread reads while it is borrowed.
        """
        raster = None
        with self._cond:
            while self._shared is None:
                if self._free:
                    raster = self._free.pop()
                    break
                if self.max_rasters is None or self._open_count < self.max_rasters:
                    self._open_count += 1
                    break
                self._cond.wait()
            shared = self._shared
        if shared is not None and raster is None:
            with self._shared_lock:
                yield shared
            return
        if raster is None:
            try:
                raster = _terrio.open_raster(self.path, lazy=True)
            except BaseException:
                with self._cond:
                    self._open_count -= 1
                    self._cond.notify()
                raise
            if not raster.is_lazy:
                with self._cond:
                    self._open_count -= 1
                    if self._shared is None:
                        self._shared = raster
                    shared = self._shared
                    self._cond.notify_all()
                with self._shared_lock:
                    yield shared
                return
        try:
            yield raster
        finally:
            with self._cond:
                self._free.append(raster)
                self._cond.notify()

    def close(self) -> None:
        """Releases the rasters."""
        with self._cond:
            self._open_count -= len(self._free)
            self._free.clear()
            self._shared = None

    def __getstate__(self) -> dict[str, Any]:
        """The state to pickle: the path and the limit.

        Returns:
            dict: The state.
        """
        return {"path": self.path, "max_rasters": self.max_rasters}

    def __setstate__(self, state: dict[str, Any]) -> None:
        """Restores a pickled pool.

        Args:
            state: The state.
        """
        self.__init__(state["path"], max_rasters=state.get("max_rasters"))  # type: ignore[misc]


@dataclass
class _Source:
    """What `open_rasterio` knows of a raster: its metadata, and the rasters to read it with."""

    name: str
    path: str
    driver: str
    width: int
    height: int
    count: int
    dtypes: tuple[str, ...]
    nodatavals: tuple[float | None, ...]
    scales: tuple[float, ...]
    offsets: tuple[float, ...]
    descriptions: tuple[str | None, ...]
    units: tuple[str | None, ...]
    tags: dict[str, str]
    band_tags: list[dict[str, str]]
    namespace_tags: dict[str, dict[str, str]]
    transform: Affine
    crs: CRS | None
    block_shapes: list[tuple[int, int]]
    mask_flags: tuple[int, ...]
    alpha_band: int | None
    profile: dict[str, Any]
    overview: int | None
    pool: _RasterPool = field(repr=False)
    subdatasets: list[str] = field(default_factory=list)
    gcps: tuple[list[Any], CRS | None] = field(default_factory=lambda: ([], None))
    rpcs: Any = None

    @property
    def nodata(self) -> float | None:
        """The nodata value of the first band.

        Returns:
            float | None: The value.
        """
        return self.nodatavals[0] if self.nodatavals else None

    def read(self, bands: Sequence[int], window: tuple[tuple[int, int], tuple[int, int]]) -> Any:
        """Reads bands of a window.

        Args:
            bands: The bands, from 1.
            window: `((row_start, row_stop), (col_start, col_stop))`.

        Returns:
            numpy.ndarray: `(bands, rows, cols)`.
        """
        (r0, r1), (c0, c1) = window
        with self.pool.acquire() as raster:
            data = raster.read(
                [int(b) for b in bands], window=(c0, r0, c1 - c0, r1 - r0), overview=self.overview
            )
        return np.asarray(data)

    def invalid(
        self,
        data: np.ndarray,
        bands: Sequence[int],
        window: tuple[tuple[int, int], tuple[int, int]],
    ) -> np.ndarray | None:
        """Where pixels are masked, as GDAL's masks (nodata, alpha band) mask them.

        Args:
            data: The pixels, `(bands, rows, cols)`.
            bands: Their bands, from 1.
            window: Their window.

        Returns:
            numpy.ndarray | None: True where masked, or None when nothing is.
        """
        out: np.ndarray | None = None
        alpha: np.ndarray | None = None
        for k, b in enumerate(bands):
            flags = self.mask_flags[b - 1]
            bad: np.ndarray | None = None
            if flags & MaskFlags.nodata.value:
                nd = self.nodatavals[b - 1]
                vals = data[k]
                if nd is None:
                    continue
                if np.isnan(nd):
                    bad = np.isnan(vals) if vals.dtype.kind == "f" else None
                else:
                    ndv = np.array(nd).astype(vals.dtype) if vals.dtype.kind == "f" else nd
                    bad = vals == ndv
            elif flags & MaskFlags.alpha.value and self.alpha_band is not None:
                if alpha is None:
                    a = self.read([self.alpha_band], window)[0]
                    alpha = a == 0 if a.dtype == np.uint8 or a.dtype.kind in "iu" else a < 1
                bad = alpha
            if bad is None or not bad.any():
                continue
            if out is None:
                out = np.zeros(data.shape, dtype=bool)
            out[k] = bad
        return out


def _overview_band_metadata(riods: DatasetReader, raster: Any, level: int) -> dict[str, Any]:
    """The band metadata of an overview, as GDAL's overview datasets report it.

    The bands of an overview have the metadata of the overview's own TIFF directory (usually
    none): no description, scale or offset, whatever the full-resolution bands have.

    Args:
        riods: The dataset.
        raster: Its `terrio.Raster`.
        level: The overview.

    Returns:
        dict: `scales`, `offsets`, `descriptions`, `units` and `band_tags`.
    """
    s = _tiff.default_structure(raster)
    if riods.driver == "GTiff":
        try:
            ifds = _terrio.tiff_tags(_terrio_path(riods.name)) or []
        except Exception:  # noqa: BLE001 - no metadata then
            ifds = []
        # Reduced-resolution images that are not masks, in file order.
        overviews = [t for t in ifds[1:] if (_first_int(t.get(254)) & 5) == 1]
        if level < len(overviews):
            xml = overviews[level].get(_tiff.GDAL_METADATA)
            if isinstance(xml, str):
                _tiff._apply_gdal_metadata(s, xml, {})
    return {
        "scales": tuple(s.scales),
        "offsets": tuple(s.offsets),
        "descriptions": tuple(s.descriptions),
        "units": tuple(s.units),
        "band_tags": [dict(t) for t in s.band_tags],
    }


def _first_int(value: Any) -> int:
    """The first value of a TIFF tag as an int.

    Args:
        value: The tag's value (a list, an int or None).

    Returns:
        int: The value, or 0.
    """
    if isinstance(value, list):
        return int(value[0]) if value else 0
    return int(value) if isinstance(value, (int, float)) else 0


def _open_source(
    filename: Any, overview_level: int | None, max_rasters: int | None = None
) -> _Source:
    """Opens a raster and reads its metadata.

    Args:
        filename: A path, URL or `terrio.rio` dataset.
        overview_level: An overview to open instead of the full resolution.
        max_rasters: The most rasters reading the file at once (None: no limit).

    Returns:
        _Source: The raster.

    Raises:
        ValueError: If the overview does not exist.
    """
    if isinstance(filename, DatasetReader):
        name = filename.name
    else:
        name = os.fspath(filename) if not isinstance(filename, str) else filename
    with rio_open(name) as riods:
        first = riods._raster
        width, height = riods.width, riods.height
        transform = riods.transform
        block_shapes = list(riods.block_shapes)
        if overview_level is not None:
            sizes = list(first.overviews)
            if overview_level < 0 or overview_level >= len(sizes):
                raise ValueError(
                    f"overview_level {overview_level} does not exist: the raster has "
                    f"{len(sizes)} overview(s)"
                )
            ow, oh = sizes[overview_level]
            # As GDAL's overview datasets scale the geotransform.
            xr_, yr_ = width / ow, height / oh
            t = transform
            transform = Affine(t.a * xr_, t.b * yr_, t.c, t.d * xr_, t.e * yr_, t.f)
            width, height = ow, oh
        namespaces = {}
        if riods.driver == "ENVI":
            namespaces["ENVI"] = riods.tags(ns="ENVI")
        alpha = riods.structure.alpha_band() if riods.count else None
        band_meta: dict[str, Any] = {
            "scales": tuple(riods.scales),
            "offsets": tuple(riods.offsets),
            "descriptions": tuple(riods.descriptions),
            "units": tuple(riods.units),
            "band_tags": [dict(riods.tags(b)) for b in riods.indexes],
        }
        if overview_level is not None:
            band_meta = _overview_band_metadata(riods, first, overview_level)
        src = _Source(
            name=riods.name,
            path=_terrio_path(riods.name),
            driver=riods.driver,
            width=width,
            height=height,
            count=riods.count,
            dtypes=tuple(riods.dtypes),
            nodatavals=tuple(riods.nodatavals),
            scales=band_meta["scales"],
            offsets=band_meta["offsets"],
            descriptions=band_meta["descriptions"],
            units=band_meta["units"],
            tags=dict(riods.tags()),
            band_tags=band_meta["band_tags"],
            namespace_tags=namespaces,
            transform=transform,
            crs=riods.crs,
            block_shapes=block_shapes,
            mask_flags=tuple(int(sum(f.value for f in fl)) for fl in riods.mask_flag_enums),
            alpha_band=alpha,
            profile=dict(riods.profile),
            overview=overview_level,
            pool=_RasterPool(_terrio_path(riods.name), first, max_rasters),
            subdatasets=list(riods.subdatasets),
            gcps=riods.gcps,
            rpcs=riods.rpcs,
        )
    return src


class TerrioArrayWrapper(BackendArray):
    """A lazy array of a raster's bands, read by window through terrio when indexed."""

    def __init__(
        self,
        source: _Source,
        bands: Sequence[int],
        *,
        lock: Any = None,
        masked: bool = False,
        mask_and_scale: bool = False,
        unsigned: bool | None = None,
        name: Hashable = None,
    ) -> None:
        """Creates the array.

        Args:
            source: The raster.
            bands: Its bands in the array, from 1.
            lock: A lock to hold while reading (none is needed: readers do not share rasters).
            masked: Mask nodata (or alpha) pixels with NaN.
            mask_and_scale: Also apply the bands' scales and offsets.
            unsigned: The `_Unsigned` attribute: read integers as unsigned (True) or signed.
            name: The variable's name, for warnings.

        Raises:
            ValueError: If the bands' data types differ.
        """
        self.source = source
        self.bands = [int(b) for b in bands]
        self.lock = lock if lock is not None else contextlib.nullcontext()
        self.masked = masked or mask_and_scale
        self.mask_and_scale = mask_and_scale
        dtypes = [source.dtypes[b - 1] for b in self.bands]
        if len(set(dtypes)) > 1:
            raise ValueError("All bands should have the same dtype")
        dtype = np.dtype(dtypes[0])
        self._fill_value: Any = source.nodatavals[self.bands[0] - 1]
        self._unsigned_dtype = None
        if mask_and_scale and unsigned is not None:
            self._unsigned_dtype = _get_unsigned_dtype(unsigned, dtype)
            if self._unsigned_dtype is not None and self._fill_value is not None:
                self._fill_value = self._unsigned_dtype.type(self._fill_value)
            if self._unsigned_dtype is None:
                warnings.warn(
                    f"variable {name!r} has _Unsigned attribute but is not of integer type. "
                    "Ignoring attribute.",
                    xarray.SerializationWarning,
                    stacklevel=3,
                )
        if self.masked:
            self._dtype, self._fill_value = maybe_promote(dtype)
        else:
            self._dtype = dtype
        self._shape = (len(self.bands), source.height, source.width)

    @property
    def dtype(self) -> np.dtype[Any]:
        """The data type of the array.

        Returns:
            numpy.dtype: The data type.
        """
        return self._dtype

    @property
    def shape(self) -> tuple[int, int, int]:
        """The shape of the array: `(bands, rows, cols)`.

        Returns:
            tuple: The shape.
        """
        return self._shape

    def __deepcopy__(self, memo: dict[int, Any]) -> TerrioArrayWrapper:
        """A copy sharing the raster and the lock (deep copies of arrays copy their wrappers).

        Args:
            memo: The `copy.deepcopy` memo.

        Returns:
            TerrioArrayWrapper: The copy.
        """
        new = copy.copy(self)
        memo[id(self)] = new
        return new

    @property
    def fill_value(self) -> Any:
        """The value of masked pixels.

        Returns:
            Any: The fill value.
        """
        return self._fill_value

    def _get_indexer(
        self, key: tuple[Any, ...]
    ) -> tuple[Any, tuple[tuple[int, int], ...], tuple[int, ...], tuple[Any, ...]]:
        """Splits an outer indexer into bands, a window and what remains to index in memory.

        Args:
            key: An outer indexer of three items.

        Returns:
            tuple: The band positions (a list, or an int for one band selected by a scalar), the
            window `((row_start, row_stop), (col_start, col_stop))`, the axes to squeeze and the
            indexer of the window's array.

        Raises:
            RioXarrayError: If the key does not have three items.
        """
        if len(key) != 3:
            raise RioXarrayError("rasterio datasets should always be 3D")
        band_key = key[0]
        np_inds: list[Any] = []
        if isinstance(band_key, slice):
            start, stop, step = band_key.indices(self.shape[0])
            band_key = np.arange(start, stop, step)
        band_key = np.asarray(band_key).tolist()
        if isinstance(band_key, list):
            np_inds.append(slice(None))
        window = []
        squeeze_axis = []
        for i, (ikey, size) in enumerate(zip(key[1:], self.shape[1:], strict=True)):
            if isinstance(ikey, slice):
                start, stop, step = ikey.indices(size)
                np_inds.append(slice(None, None, step))
            elif np.ndim(ikey) == 0:
                squeeze_axis.append(-(2 - i))
                start = int(ikey)
                stop = start + 1
            else:
                ikey = np.asarray(ikey)
                start, stop = int(np.min(ikey)), int(np.max(ikey)) + 1
                np_inds.append(ikey - start)
            window.append((start, stop))
        if isinstance(key[1], np.ndarray) and isinstance(key[2], np.ndarray):
            np_inds[-2:] = np.ix_(*np_inds[-2:])
        return band_key, tuple(window), tuple(squeeze_axis), tuple(np_inds)

    def _getitem(self, key: tuple[Any, ...]) -> np.ndarray:
        """Reads the pixels of an outer indexer.

        Args:
            key: The indexer.

        Returns:
            numpy.ndarray: The pixels.
        """
        band_key, window, squeeze_axis, np_inds = self._get_indexer(key)
        positions = band_key if isinstance(band_key, list) else [band_key]
        bands = [self.bands[p] for p in positions]
        win = (window[0], window[1])
        if not bands or any(start == stop for (start, stop) in window):
            shape = (len(bands),) + tuple(stop - start for (start, stop) in window)
            out: np.ndarray = np.zeros(shape, dtype=self.dtype)
        else:
            with self.lock:
                out = self.source.read(bands, win)
                invalid = self.source.invalid(out, bands, win) if self.masked else None
            if self._unsigned_dtype is not None:
                out = out.astype(self._unsigned_dtype)
            if self.masked:
                out = out.astype(self.dtype)
                if invalid is not None:
                    out[invalid] = self.fill_value
            if self.mask_and_scale:
                for i, b in enumerate(bands):
                    out[i] = out[i] * self.source.scales[b - 1] + self.source.offsets[b - 1]
        if not isinstance(band_key, list):
            out = out[0]
        if squeeze_axis:
            out = np.squeeze(out, axis=squeeze_axis)
        result: np.ndarray = out[np_inds]
        return result

    def __getitem__(self, key: indexing.ExplicitIndexer) -> Any:
        """Reads the pixels of an indexer.

        Args:
            key: An xarray explicit indexer.

        Returns:
            numpy.ndarray: The pixels.
        """
        return indexing.explicit_indexing_adapter(
            key, self.shape, indexing.IndexingSupport.OUTER, self._getitem
        )


# Metadata.


def _to_numeric(value: Any) -> Any:
    """A tag's value as a number when it is one.

    Args:
        value: The value.

    Returns:
        Any: An int, a float or the value.
    """
    try:
        return int(value)
    except (TypeError, ValueError):
        try:
            return float(value)
        except (TypeError, ValueError):
            return value


def _parse_tag(*, key: str, value: Any) -> tuple[str, Any]:
    """A tag as an attribute: `{a,b}` lists become arrays, numbers numbers.

    Args:
        key: The tag's name.
        value: Its value.

    Returns:
        tuple: The attribute's name and value.
    """
    key = key.split("NC_GLOBAL#")[-1]
    if value.startswith("{") and value.endswith("}"):
        try:
            new_val = np.array(
                [float(v) for v in value.strip("{}").split(",") if v.strip()], dtype="float"
            )
            value = new_val if len(new_val) else _to_numeric(value)
        except ValueError:
            value = _to_numeric(value)
    else:
        value = _to_numeric(value)
    return key, value


def _parse_tags(tags: dict[str, str]) -> dict[str, Any]:
    """Tags as attributes.

    Args:
        tags: The tags.

    Returns:
        dict: The attributes.
    """
    return dict(_parse_tag(key=k, value=v) for k, v in tags.items())


_NETCDF_DTYPE_MAP: dict[int, Any] = {
    0: object,
    1: np.byte,
    2: np.str_,
    3: np.short,
    4: np.int_,
    5: float,
    6: np.double,
    7: np.ubyte,
    8: np.ushort,
    9: np.uint,
    10: np.int64,
    11: np.uint64,
    12: object,
}


def _load_netcdf_attrs(*, tags: dict[str, str], data_array: xarray.DataArray) -> None:
    """Copies netCDF variable attributes (`variable#attribute` tags) to coordinates.

    Args:
        tags: The tags.
        data_array: The array (changed in place).
    """
    for key, value in tags.items():
        key, value = _parse_tag(key=key, value=value)
        parts = key.split("#")
        if len(parts) != 2:
            continue
        variable_name, attr_name = parts
        if variable_name in data_array.coords:
            data_array.coords[variable_name].attrs.update({attr_name: value})


def _parse_netcdf_attr_array(attr: Any, *, dtype: Any = None) -> np.ndarray:
    """A netCDF tag holding a list (`{2,6}` or `[2. 6.]`) as an array.

    Args:
        attr: The tag's value.
        dtype: The array's data type.

    Returns:
        numpy.ndarray: The values.
    """
    value: Any
    if isinstance(attr, str):
        value = attr.strip("{}").split(",") if attr.startswith("{") else attr.strip("[]").split()
    elif not isinstance(attr, Iterable):
        value = [attr]
    else:
        value = attr
    return np.array(value, dtype=dtype)


def _load_netcdf_1d_coords(tags: dict[str, Any]) -> dict[Hashable, Any]:
    """The extra dimensions of a netCDF variable (`NETCDF_DIM_*` tags) as coordinates.

    Args:
        tags: The attributes.

    Returns:
        dict: The coordinates.
    """
    dim_names = tags.get("NETCDF_DIM_EXTRA")
    if dim_names is None or (isinstance(dim_names, str) and not dim_names):
        return {}
    coords: dict[Hashable, Any] = {}
    for dim_name in _parse_netcdf_attr_array(dim_names):
        dim_def = tags.get(f"NETCDF_DIM_{dim_name}_DEF")
        if dim_def is None:
            continue
        _, dim_dtype = _parse_netcdf_attr_array(dim_def)
        _ = _NETCDF_DTYPE_MAP.get(int(float(dim_dtype)), object)
        dim_values = _parse_netcdf_attr_array(tags[f"NETCDF_DIM_{dim_name}_VALUES"])
        coords[str(dim_name)] = index_variable(str(dim_name), dim_values)
    return coords


def _get_attrs(src: _Source, bands: Sequence[int], band_only: bool) -> dict[str, Any]:
    """The attributes rioxarray gives a raster's bands.

    Args:
        src: The raster.
        bands: The bands, from 1.
        band_only: Use the band's tags only (for a band read as its own variable).

    Returns:
        dict: The attributes.
    """
    first = bands[0]
    if band_only:
        tags = dict(src.band_tags[first - 1])
    else:
        tags = {**src.tags, **src.band_tags[0]}
    attrs = _parse_tags(tags)
    for unwanted in FILL_VALUE_NAMES + UNWANTED_RIO_ATTRS:
        attrs.pop(unwanted, None)
    nodata = src.nodatavals[first - 1]
    if nodata is not None:
        attrs["_FillValue"] = nodata
    scales = tuple(src.scales[b - 1] for b in bands)
    offsets = tuple(src.offsets[b - 1] for b in bands)
    descriptions = tuple(src.descriptions[b - 1] for b in bands)
    units = tuple(src.units[b - 1] for b in bands)
    if len(set(scales)) > 1:
        attrs["scales"] = scales
        warnings.warn(
            "Scales differ across bands. The 'scale_factor' attribute will not be added. "
            "See the 'scales' attribute.",
            stacklevel=3,
        )
    else:
        attrs["scale_factor"] = scales[0]
    if len(set(offsets)) > 1:
        attrs["offsets"] = offsets
        warnings.warn(
            "Offsets differ across bands. The 'add_offset' attribute will not be added. "
            "See the 'offsets' attribute.",
            stacklevel=3,
        )
    else:
        attrs["add_offset"] = offsets[0]
    if any(descriptions):
        attrs["long_name"] = descriptions[0] if len(set(descriptions)) == 1 else descriptions
    if any(units):
        attrs["units"] = units[0] if len(units) == 1 else units
    return attrs


def _parse_envi(meta: dict[str, str]) -> dict[str, Any]:
    """ENVI header items as values (wavelengths as arrays).

    Args:
        meta: The `ENVI` metadata domain.

    Returns:
        dict: The items.
    """

    def parsevec(value: str) -> np.ndarray:
        """Parses a `{a, b}` list.

        Args:
            value: The text.

        Returns:
            numpy.ndarray: The values.
        """
        return np.array([float(v) for v in value.strip("{}").split(",") if v.strip()])

    def default(value: str) -> str:
        """Strips braces.

        Args:
            value: The text.

        Returns:
            str: The text.
        """
        return value.strip("{}")

    parse = {"wavelength": parsevec, "fwhm": parsevec}
    return {key: parse.get(key, default)(value) for key, value in meta.items()}


def _decode_datetime_cf(
    data_array: xarray.DataArray, decode_times: bool, decode_timedelta: bool | None
) -> xarray.DataArray:
    """Decodes CF times of coordinates.

    Args:
        data_array: The array.
        decode_times: Decode `units: ... since ...` coordinates as datetimes.
        decode_timedelta: Decode time-unit coordinates as timedeltas (default: decode_times).

    Returns:
        xarray.DataArray: The array.
    """
    if decode_timedelta is None:
        decode_timedelta = decode_times
    for coord in data_array.coords:
        time_var = None
        units = data_array[coord].attrs.get("units", "")
        if decode_times and isinstance(units, str) and "since" in units:
            time_var = times.CFDatetimeCoder(use_cftime=True).decode(
                xarray.Variable(
                    data_array[coord].dims, data_array[coord].values, data_array[coord].attrs
                ),
                name=coord,
            )
        elif decode_timedelta and units in times.TIME_UNITS:
            time_var = times.CFTimedeltaCoder().decode(
                xarray.Variable(
                    data_array[coord].dims, data_array[coord].values, data_array[coord].attrs
                ),
                name=coord,
            )
        if time_var is not None:
            dims, data, attributes, encoding = _unpack_for_decoding(time_var)
            data_array = data_array.assign_coords(
                {coord: index_variable(dims, data, attributes, encoding)}
            )
    return data_array


def _prepare_dask(
    *,
    result: xarray.DataArray,
    src: _Source,
    chunks: Any,
    bidx: int | None = None,
) -> xarray.DataArray:
    """Chunks an array with dask.

    Args:
        result: The array.
        src: Its raster.
        chunks: The chunks: an int, a tuple, a dict, True or `"auto"`.
        bidx: The band, when it is read as its own variable.

    Returns:
        xarray.DataArray: The chunked array.
    """
    from dask.base import tokenize

    try:
        mtime: float | None = os.path.getmtime(src.name)
    except (TypeError, OSError):
        mtime = None
    if chunks is True or chunks == "auto":
        from dask.array.core import normalize_chunks

        block_shape = (1,) + tuple(src.block_shapes[0])
        norm = normalize_chunks(  # type: ignore[no-untyped-call]
            chunks=(1, "auto", "auto"),
            shape=result.shape,
            dtype=np.dtype(src.dtypes[0]),
            previous_chunks=block_shape,
        )
        chunks = dict(zip(result.dims, norm, strict=True))
    token_name = src.name if bidx is None else f"{src.name}-{bidx}"
    token = tokenize(token_name, mtime, chunks, src.overview)
    return result.chunk(chunks, name_prefix=f"open_rasterio-{token}", token=token)


def _handle_encoding(
    *,
    result: xarray.DataArray,
    mask_and_scale: bool,
    masked: bool,
    da_name: Hashable,
    unsigned: bool | None,
) -> None:
    """Moves encoding attributes (`_FillValue`, `scale_factor`...) to the encoding.

    Args:
        result: The array (changed in place).
        mask_and_scale: Whether scales and offsets were applied.
        masked: Whether nodata was masked.
        da_name: The array's name.
        unsigned: The `_Unsigned` attribute.
    """
    if "grid_mapping" in result.attrs:
        _pop_to(result.attrs, result.encoding, "grid_mapping", name=da_name)
    if mask_and_scale:
        for key in ("scale_factor", "scales", "add_offset", "offsets"):
            if key in result.attrs:
                _pop_to(result.attrs, result.encoding, key, name=da_name)
    if masked:
        for key in ("_FillValue", "missing_value"):
            if key in result.attrs:
                _pop_to(result.attrs, result.encoding, key, name=da_name)
    if mask_and_scale and unsigned is not None and "_FillValue" in result.encoding:
        unsigned_dtype = _get_unsigned_dtype(unsigned, result.encoding["dtype"])
        if unsigned_dtype is not None:
            result.encoding["_FillValue"] = unsigned_dtype.type(result.encoding["_FillValue"])


def _build(
    src: _Source,
    bands: Sequence[int],
    *,
    band_only: bool,
    parse_coordinates: bool,
    chunks: Any,
    cache: bool | None,
    lock: Any,
    masked: bool,
    mask_and_scale: bool,
    default_name: Hashable,
    decode_times: bool,
    decode_timedelta: bool | None,
) -> xarray.DataArray:
    """The DataArray of bands of a raster.

    Args:
        src: The raster.
        bands: The bands, from 1.
        band_only: Whether a single band is read as its own variable.
        parse_coordinates: Generate x and y coordinates.
        chunks: Dask chunks, or None.
        cache: Cache pixels in memory once read.
        lock: A lock to hold while reading.
        masked: Mask nodata pixels with NaN.
        mask_and_scale: Mask, and apply scales and offsets.
        default_name: The array's name.
        decode_times: Decode CF times of coordinates.
        decode_timedelta: Decode CF time deltas of coordinates.

    Returns:
        xarray.DataArray: The array, `(band, y, x)`.
    """
    masked = masked or mask_and_scale
    if cache is None:
        cache = chunks is None
    attrs = _get_attrs(src, bands, band_only)
    coords = _load_netcdf_1d_coords(attrs)
    if src.driver == "ENVI" and not band_only:
        for key, value in _parse_envi(src.namespace_tags.get("ENVI", {})).items():
            if isinstance(value, (list, np.ndarray)) and len(value) == src.count:
                coords[key] = ("band", np.asarray(value))
            else:
                attrs[key] = value
    coord_name: Hashable = "band"
    for coord in coords:
        if f"NETCDF_DIM_{coord}" in attrs:
            coord_name = coord
            attrs.pop(f"NETCDF_DIM_{coord}")
            break
        if f"NETCDF_DIM_{coord}_VALUES" in attrs:
            coord_name = coord
            attrs.pop(f"NETCDF_DIM_{coord}_VALUES")
            attrs.pop(f"NETCDF_DIM_{coord}_DEF", None)
            attrs.pop("NETCDF_DIM_EXTRA", None)
            break
    else:
        coords[coord_name] = np.asarray(list(bands))
    # Georeferenced by ground control points or RPCs: no coordinates from the transform.
    if src.gcps[0] or src.rpcs:
        parse_coordinates = False
    if parse_coordinates:
        coords.update(
            generate_spatial_coords(affine=src.transform, width=src.width, height=src.height)
        )
    unsigned = None
    encoding: dict[Hashable, Any] = {}
    if mask_and_scale and "_Unsigned" in attrs:
        unsigned = _pop_to(attrs, encoding, "_Unsigned") == "true"
    if masked:
        encoding["dtype"] = str(np.dtype(src.dtypes[bands[0] - 1]))
    da_name = attrs.pop("NETCDF_VARNAME", default_name)
    data: Any = indexing.LazilyIndexedArray(
        TerrioArrayWrapper(
            src,
            bands,
            lock=lock,
            masked=masked,
            mask_and_scale=mask_and_scale,
            unsigned=unsigned,
            name=da_name,
        )
    )
    data = indexing.CopyOnWriteArray(data)
    if cache and chunks is None:
        data = indexing.MemoryCachedArray(data)  # type: ignore[no-untyped-call]
    result = xarray.DataArray(
        data=data, dims=(coord_name, "y", "x"), coords=coords, attrs=attrs, name=da_name
    )
    result.encoding = encoding
    tags = {**src.tags, **src.band_tags[0]} if not band_only else src.band_tags[bands[0] - 1]
    _load_netcdf_attrs(tags=tags, data_array=result)
    result = _decode_datetime_cf(
        result, decode_times=decode_times, decode_timedelta=decode_timedelta
    )
    if "_FillValue" in result.attrs:
        result.attrs["_FillValue"] = result.dtype.type(result.attrs["_FillValue"])
    _handle_encoding(
        result=result,
        mask_and_scale=mask_and_scale,
        masked=masked,
        da_name=da_name,
        unsigned=unsigned,
    )
    acc = result.terrio
    acc.write_transform(src.transform, inplace=True)
    if src.crs:
        acc.write_crs(src.crs, inplace=True)
    if src.gcps[0]:
        acc.write_gcps(*src.gcps, inplace=True)
    if src.rpcs:
        acc.write_rpcs(src.rpcs, inplace=True)
    if chunks is not None:
        result = _prepare_dask(
            result=result, src=src, chunks=chunks, bidx=bands[0] if band_only else None
        )
    else:
        result.encoding["preferred_chunks"] = {
            "y": src.block_shapes[0][0],
            "x": src.block_shapes[0][1],
            coord_name: 1,
        }
    result.encoding["source"] = src.name
    result.encoding["rasterio_dtype"] = str(src.dtypes[0])
    for coord in result.coords:
        result.attrs = {
            attr: value
            for attr, value in result.attrs.items()
            if not str(attr).startswith(f"{coord}#")
        }
    if result.name:
        result.attrs = {
            attr: value
            for attr, value in result.attrs.items()
            if not str(attr).startswith(f"{result.name}#")
        }
    result.set_close(src.pool.close)
    result.terrio._source = src
    return result


def _subdataset_filter(
    group: str | Sequence[str] | None, variable: str | Sequence[str] | None
) -> re.Pattern[str]:
    """The pattern of the subdataset names of groups and variables, as rioxarray matches them.

    Args:
        group: A group name or names (None: any).
        variable: A variable name or names (None: any).

    Returns:
        re.Pattern: The pattern.
    """

    def alternatives(names: str | Sequence[str]) -> str:
        names = [names] if isinstance(names, str) else list(names)
        return rf"(?:{'|'.join(re.escape(n) for n in names)})"

    var = r"\w+" if variable is None else alternatives(variable)
    if group is None:
        return re.compile(rf".*(?:\:/|\:)(/+)?{var}$")
    return re.compile(rf".*(?:\:/|\:)(/+)?{alternatives(group)}[:/](/+)?{var}$")


def _load_subdatasets(
    src: _Source,
    *,
    group: str | Sequence[str] | None,
    variable: str | Sequence[str] | None,
    parse_coordinates: bool,
    **kwargs: Any,
) -> xarray.Dataset | list[xarray.Dataset]:
    """Opens the subdatasets of a file, as rioxarray does.

    Each subdataset (filtered by `group` and `variable`) opens as a DataArray named after its
    variable; those of one shape make a Dataset, with the file's tags as its attributes.

    Args:
        src: The file.
        group: Groups to keep.
        variable: Variables to keep.
        parse_coordinates: Generate x and y coordinates.
        **kwargs: Other options of `open_rasterio`.

    Returns:
        xarray.Dataset | list[xarray.Dataset]: A Dataset, or one per shape.
    """
    pattern = (
        _subdataset_filter(group, variable) if group is not None or variable is not None else None
    )
    groups: dict[tuple[int, int], dict[Hashable, xarray.DataArray]] = defaultdict(dict)
    for name in src.subdatasets:
        if pattern is not None and not pattern.match(name):
            continue
        with rio_open(name) as sub:
            shape = (sub.height, sub.width)
        da = open_rasterio(
            name,
            parse_coordinates=shape not in groups and parse_coordinates,
            default_name=name.split(":")[-1].lstrip("/").replace("/", "_").strip('"'),
            **kwargs,
        )
        assert isinstance(da, xarray.DataArray)
        groups[shape][da.name] = da
    attrs = _parse_tags(src.tags)
    datasets = []
    for arrays in groups.values():
        ds = xarray.Dataset(arrays, attrs=attrs)
        # Global attributes are the Dataset's, not each variable's.
        for var in ds.variables:
            ds[var].attrs = {
                k: v
                for k, v in ds[var].attrs.items()
                if k not in ds.attrs and not any(str(k).startswith(f"{c}#") for c in ds.coords)
            }

        def close(arrays: dict[Hashable, xarray.DataArray] = arrays) -> None:
            """Closes the subdatasets' rasters."""
            for da in arrays.values():
                da.close()

        ds.set_close(close)
        datasets.append(ds)
    if not datasets:
        return xarray.Dataset(attrs=attrs)
    return datasets[0] if len(datasets) == 1 else datasets


def open_rasterio(
    filename: str | os.PathLike[str] | DatasetReader,
    *,
    parse_coordinates: bool | None = None,
    chunks: int | tuple[Any, ...] | dict[Hashable, Any] | bool | str | None = None,
    cache: bool | None = None,
    lock: Any = None,
    masked: bool = False,
    mask_and_scale: bool = False,
    variable: str | list[str] | tuple[str, ...] | None = None,
    group: str | list[str] | tuple[str, ...] | None = None,
    default_name: str | None = None,
    decode_times: bool = True,
    decode_timedelta: bool | None = None,
    band_as_variable: bool = False,
    overview_level: int | None = None,
    **open_kwargs: Any,
) -> xarray.Dataset | xarray.DataArray | list[xarray.Dataset]:
    """Opens a raster as an xarray DataArray, as `rioxarray.open_rasterio` does.

    The pixels are read lazily, by window, when they are indexed or computed: GeoTIFFs and COGs
    (local or remote) are read tile by tile, and only the tiles needed are fetched. x and y
    coordinates are the pixel centres.

    Args:
        filename: A path or URL (`https://`, `s3://`, `gs://`, `az://`, GDAL `/vsicurl/`), or a
            `terrio.rio` dataset.
        parse_coordinates: Generate x and y coordinates from the transform (default True).
        chunks: Dask chunks: an int, a tuple, a dict by dimension, or True or `"auto"` for chunks
            aligned on the raster's blocks. None reads into NumPy arrays.
        cache: Keep pixels in memory once read (default: True without chunks).
        lock: None or False (the default) reads chunks in parallel, each concurrent reader with
            a raster of its own; True reads one chunk at a time through a single raster, which
            fetches tiles shared by chunks (the bands of a pixel-interleaved COG) once; a lock
            object is held while reading.
        masked: Mask nodata (and alpha) pixels with NaN; the nodata value goes to the encoding.
        mask_and_scale: Mask, and apply the bands' scales and offsets.
        variable: For a file of subdatasets (netCDF, HDF5, Zarr...), the variable or variables to
            open (by default all).
        group: For a file of subdatasets, the group or groups to open them from.
        default_name: The array's name.
        decode_times: Decode CF times of netCDF coordinates.
        decode_timedelta: Decode CF time deltas of coordinates (default: decode_times).
        band_as_variable: Open each band as a variable of a Dataset (`band_1`, `band_2`...).
        overview_level: Open an overview (0 is the largest) instead of the full resolution.
        **open_kwargs: Ignored GDAL open options (`sharing`...).

    Returns:
        xarray.DataArray | xarray.Dataset | list[xarray.Dataset]: The raster; for a file of
        subdatasets, as rioxarray opens it, a Dataset of them (a list of Datasets, one per shape,
        when they differ in shape).

    Raises:
        ValueError: If the raster has no bands or the overview does not exist.
    """
    open_kwargs.pop("sharing", None)
    if "overview_level" in open_kwargs:  # pragma: no cover
        overview_level = open_kwargs.pop("overview_level")
    parse_coordinates = True if parse_coordinates is None else parse_coordinates
    masked = masked or mask_and_scale
    max_rasters = 1 if lock is True else None
    if lock is True or lock is False:
        lock = None
    src = _open_source(filename, overview_level, max_rasters)
    if src.subdatasets:
        src.pool.close()
        return _load_subdatasets(
            src,
            group=group,
            variable=variable,
            parse_coordinates=parse_coordinates,
            chunks=chunks,
            cache=cache,
            lock=lock,
            masked=masked,
            mask_and_scale=mask_and_scale,
            decode_times=decode_times,
            decode_timedelta=decode_timedelta,
        )
    if src.count < 1:
        raise ValueError("Unknown dims")
    common = {
        "parse_coordinates": parse_coordinates,
        "chunks": chunks,
        "cache": cache,
        "lock": lock,
        "masked": masked,
        "mask_and_scale": mask_and_scale,
        "decode_times": decode_times,
        "decode_timedelta": decode_timedelta,
    }
    if band_as_variable:
        data_vars = {}
        for band in range(1, src.count + 1):
            name = f"band_{band}"
            opts = dict(common, parse_coordinates=band == 1 and parse_coordinates)
            da = _build(src, [band], band_only=True, default_name=name, **opts)
            data_vars[name] = da.squeeze().drop_vars("band")
        dataset = xarray.Dataset(data_vars, attrs=_parse_tags(src.tags))
        dataset.set_close(src.pool.close)
        return dataset
    return _build(
        src,
        list(range(1, src.count + 1)),
        band_only=False,
        default_name=default_name,
        **common,
    )
