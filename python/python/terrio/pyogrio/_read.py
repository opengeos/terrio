# SPDX-FileCopyrightText: Copyright 2007, Mateusz Loskot
# SPDX-FileCopyrightText: Copyright 2008-2024, Even Rouault <even.rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 2007, Mateusz Loskot
# SPDX-FileCopyrightText: Copyright (c) 2008-2017, Even Rouault <even dot rouault at spatialys dot com>
# SPDX-FileCopyrightText: Copyright (c) 2020-2024 Brendan C. Ward and pyogrio contributors
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (translated to Python and modified):
#   GDAL v3.12.1:
#     ogr/ogrlibjsonutils.cpp
#     ogr/ogrsf_frmts/geojson/ogrgeojsonreader.cpp
#   pyogrio 0.13.0:
#     pyogrio/raw.py

"""Reading vector data the way pyogrio 0.13 reads it through GDAL 3.12, on terrio's drivers.

Every dataset is opened with `terrio.open_vector`'s path (through `_terrio.pyogrio_read`), so the
driver allow-list (`terrio.set_allowed_drivers`) applies to paths, directories, archives and bytes
alike: bytes and file objects are written to a private temporary file first, as pyogrio writes them
to GDAL's `/vsimem/`.
"""

from __future__ import annotations

import datetime as _dt
import json
import os
import re
import tempfile
import warnings
import zipfile
from pathlib import Path
from typing import Any

import numpy as np

from terrio import _terrio
from terrio import errors as _terrio_errors
from terrio._crs import dataframe_crs as _crs
from terrio.pyogrio.errors import DataLayerError, DataSourceError

_INT32 = (-(2**31), 2**31 - 1)


# ----------------------------------------------------------------------------------------------
# Locating the dataset
# ----------------------------------------------------------------------------------------------


_NOT_RECOGNIZED = (
    "'{}' not recognized as being in a supported file format. It might help to specify the correct "
    "driver explicitly by prefixing the file path with '<DRIVER>:', e.g. 'CSV:path'."
)


# A dataset terrio opens, and the shapefiles of a directory or a zip archive of shapefiles:
# `(path, None)`, or `(path of the first shapefile, (directory or archive, [(name, path), ...]))`.
_Located = tuple[str, tuple[str, list[tuple[str, str]]] | None]


def _shapefiles(container: str, names: list[str], path) -> _Located:
    """The shapefiles `names` of a directory or an archive: a layer each, named by its stem."""
    layers = [(Path(n).stem, path(n)) for n in names]
    return layers[0][1], (container, layers)


def _vsizip(archive: str, member: str | None) -> _Located:
    """`/vsizip/<archive>/<member>`; without a member, what GDAL opens for `/vsizip/<archive>`.

    Use the only file of a single-file archive, or its top-level shapefiles in archive order.
    With multiple entries and no top-level shapefile, let the core choose the main dataset
    and its sidecars from the whole archive.
    """
    if member is None:
        try:
            with zipfile.ZipFile(archive) as z:
                names = [n for n in z.namelist() if not n.endswith("/")]
        except (OSError, zipfile.BadZipFile) as e:
            raise DataSourceError(f"{archive}: {e}") from None
        if len(names) == 1:
            member = names[0]
        else:
            shapefiles = [n for n in names if "/" not in n and n.lower().endswith(".shp")]
            if not shapefiles:
                # The core chooses the main dataset and its sidecars. Other files such as a
                # README do not turn an otherwise readable archive into a shapefile container.
                return f"/vsizip/{archive}", None
            return _shapefiles(archive, shapefiles, lambda n: f"/vsizip/{archive}/{n}")
    return f"/vsizip/{archive}/{member.lstrip('/')}", None


def _directory(directory: str) -> _Located:
    """The shapefiles of a directory, a layer each.

    Sorted by name here; GDAL takes the order the file system lists them in.
    """
    names = sorted(n for n in os.listdir(directory) if n.lower().endswith(".shp"))
    if not names:
        raise DataSourceError(_NOT_RECOGNIZED.format(directory))
    return _shapefiles(directory, names, lambda n: os.path.join(directory, n))


def _local_path(path: str) -> _Located:
    """The path terrio opens for a path as pyogrio's `vsi_path` and GDAL's open read it."""
    if path.startswith("zip://"):
        archive, _, member = path[len("zip://") :].partition("!")
        return _vsizip(archive, member or None)
    if path.startswith("/vsizip/"):
        rest = path[len("/vsizip/") :]
        low = rest.lower()
        end = low.find(".zip/")
        if end < 0 and low.endswith(".zip"):
            return _vsizip(rest, None)
        if end >= 0:
            return _vsizip(rest[: end + 4], rest[end + 5 :] or None)
        return path, None
    if path.split("!")[0].lower().endswith(".zip") and "://" not in path:
        archive, _, member = path.partition("!")
        return _vsizip(archive, member or None)
    if os.path.isdir(path):
        return _directory(path)
    return path, None


def _spill_name(data: bytes, filename: str | None = None) -> str:
    """A file name telling terrio the format of bytes, from their content.

    Args:
        data: Uploaded bytes, identified without opening a file or decoding a dataset.
        filename: Optional file-object basename, used for unsigned formats and layer names.

    Returns:
        str: A filename derived from the core's shared vector identifiers.

    Raises:
        DataSourceError: Content needs a filename or exceeds archive resource limits.
    """
    try:
        extension = _terrio.vector_byte_extension(data)
    except _terrio_errors.TerrioError as error:
        raise DataSourceError(str(error)) from error
    if extension is not None:
        return str(Path(filename).with_suffix(f".{extension}")) if filename else f"data.{extension}"
    if filename and Path(filename).suffix:
        return filename
    raise DataSourceError(_NOT_RECOGNIZED.format("/vsimem/pyogrio"))


class _Source:
    """A dataset to read: a path, or bytes spilled into a temporary directory removed on close.

    `shapefiles` is the directory or the archive of shapefiles the dataset is, and its layers,
    `(name, path)` each; None for other datasets, whose layers terrio lists.
    """

    _names: list[str] | None

    def __init__(self, path_or_buffer):
        self._tmp = None
        self._names = None
        filename = None
        if isinstance(path_or_buffer, Path):
            path_or_buffer = str(path_or_buffer)
        if hasattr(path_or_buffer, "read") and not isinstance(path_or_buffer, (str, bytes)):
            source_name = getattr(path_or_buffer, "name", None)
            if isinstance(source_name, (str, os.PathLike)):
                # Uploaded basenames are literal names, not the archive/member path syntax.
                filename = Path(source_name).name.replace("!", "_")
            data = path_or_buffer.read()
            if hasattr(path_or_buffer, "seekable") and path_or_buffer.seekable():
                path_or_buffer.seek(0)
            if isinstance(data, str):
                data = data.encode("utf-8")
            path_or_buffer = data
        if isinstance(path_or_buffer, (bytes, bytearray, memoryview)):
            data = bytes(path_or_buffer)
            name = _spill_name(data, filename)
            self._tmp = tempfile.TemporaryDirectory(prefix="terrio-pyogrio-")
            spilled = os.path.join(self._tmp.name, name)
            with open(spilled, "wb") as f:
                f.write(data)
            self.path, self.shapefiles = _local_path(spilled)
            return
        if not isinstance(path_or_buffer, str):
            path_or_buffer = str(path_or_buffer)
        self.path, self.shapefiles = _local_path(path_or_buffer)

    def layer_names(self) -> list[str]:
        """The dataset's layers, in its order.

        The shapefiles, or the layers `terrio.list_layers` lists, which reads no feature of a
        GeoPackage or a tile archive.
        """
        if self._names is None:
            if self.shapefiles is not None:
                self._names = [name for name, _ in self.shapefiles[1]]
            else:
                try:
                    self._names = _terrio.list_layers(self.path)
                except _terrio_errors.NotFoundError as e:
                    raise DataSourceError(f"{self.path}: No such file or directory") from e
                except _terrio_errors.TerrioError as e:
                    raise DataSourceError(str(e)) from e
            if (
                not self._names
                and self._tmp is not None
                and self.path.lower().endswith((".zip", ".kmz"))
            ):
                raise DataSourceError(f"{self.path}: No vector layers found")
        return self._names

    def name(self) -> str:
        """The dataset's file name, as the warnings quote it."""
        container = self.shapefiles[0] if self.shapefiles is not None else self.path
        return os.path.basename(os.path.normpath(container))

    def close(self):
        if self._tmp is not None:
            self._tmp.cleanup()
            self._tmp = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def _select_layer(names: list[str], layer, path: str) -> str:
    """The layer of `names` that `layer` designates.

    An int (a bool too) numbers the layers from 0. A str is a layer's name as written, else in
    any case of its ASCII letters; a dataset of one layer also answers to its file's stem, as
    terrio's own reads do.

    Raises:
        DataLayerError: If `layer` designates no layer.
        ValueError: If `layer` is neither a str nor an int.
    """
    if isinstance(layer, int):
        if 0 <= layer < len(names):
            return names[layer]
    elif isinstance(layer, str):
        if layer in names:
            return layer
        folded = layer.encode("utf-8").lower()
        for name in names:
            if name.encode("utf-8").lower() == folded:
                return name
        if len(names) == 1 and layer == Path(path).stem:
            return names[0]
    else:
        raise ValueError(f"'layer' parameter must be a str or int, got {type(layer)}")
    raise DataLayerError(f"Layer '{layer}' could not be opened")


def _warn_first_layer(dataset: str, names: list[str]) -> None:
    """Warns that a dataset of several layers is read without `layer`, naming the others."""
    others = ", ".join(f"'{n}'" for n in names[1:])
    # stacklevel: the call of read_dataframe or read_info (geopandas adds a frame).
    warnings.warn(
        f"More than one layer found in '{dataset}': '{names[0]}' (default), {others}. "
        "Specify layer parameter to avoid this warning.",
        stacklevel=5,
    )


def _open(
    path: str,
    skip_features: int,
    max_features: int | None,
    read_geometry: bool,
    layer: str | None = None,
) -> dict:
    try:
        return _terrio.pyogrio_read(path, skip_features, max_features, read_geometry, layer)
    except _terrio_errors.NotFoundError as e:
        raise DataSourceError(f"{path}: No such file or directory") from e
    except _terrio_errors.TerrioError as e:
        raise DataSourceError(str(e)) from e


# ----------------------------------------------------------------------------------------------
# Field types, as GDAL types them and pyogrio turns them into arrays
# ----------------------------------------------------------------------------------------------

_DATETIME = re.compile(
    r"^(-?\d{1,4})[-/](\d{1,2})[-/](\d{1,2})"
    r"(?:[T ](\d{1,2}):(\d{1,2})(?::(\d{1,2}(?:\.\d*)?))?)?"
    r"\s*(Z|[+-]\d{2}(?::?\d{2})?)?$"
)
_TIME = re.compile(r"^(\d{1,2}):(\d{1,2})(?::(\d{1,2}(?:\.\d*)?))?\s*(Z|[+-]\d{2}(?::?\d{2})?)?$")


def _seconds(text: str | None) -> tuple[int, int, np.float32]:
    """Whole seconds and milliseconds as GDAL keeps them: a 32-bit float rounded to ms."""
    f = np.float32(float(text or 0))
    whole = int(f)
    return whole, int((float(f) - whole) * 1000 + 0.5), f


def _iso_datetime(text: str) -> str:
    """GDAL's `OGR_F_GetFieldAsISO8601DateTime` of a date-time read from `text`."""
    m = _DATETIME.match(text.strip())
    if not m:
        return text
    y, mo, d, h, mi, s, tz = m.groups()
    whole, ms, f = _seconds(s)
    sec = f"{float(f):06.3f}" if ms else f"{whole:02d}"
    out = f"{int(y):04d}-{int(mo):02d}-{int(d):02d}T{int(h or 0):02d}:{int(mi or 0):02d}:{sec}"
    if tz:
        if tz == "Z":
            return out + "Z"
        sign = -1 if tz[0] == "-" else 1
        digits = tz[1:].replace(":", "")
        offset = sign * (int(digits[:2]) * 60 + int(digits[2:4] or 0))
        flag = 100 + offset // 15 if offset >= 0 else 100 - (-offset // 15)
        if flag == 100:
            return out + "Z"
        minutes = (flag - 100) * 15
        hours = int(minutes / 60)
        out += f"{'-' if minutes < 0 else '+'}{abs(hours):02d}:{abs(minutes - hours * 60):02d}"
    return out


def _iso_date(text: str) -> str:
    m = _DATETIME.match(text.strip())
    if not m:
        return text
    y, mo, d = m.groups()[:3]
    return _dt.date(int(y), int(mo), int(d)).isoformat()


def _time(text: str):
    m = _TIME.match(text.strip())
    if not m:
        return text
    h, mi, s, _tz = m.groups()
    whole, ms, _f = _seconds(s)
    return _dt.time(int(h), int(mi), whole, ms * 1000)


def _list_kind(value) -> tuple[str, str]:
    """GDAL's `GeoJSONPropertyToFieldType` of an array value: `(type, subtype)`."""
    json_ = ("OFTString", "OFSTJSON")
    if not isinstance(value, list) or not value:
        return json_
    t, sub = "OFTIntegerList", "OFSTNone"
    for i, x in enumerate(value):
        if isinstance(x, bool):
            if i == 0 or (t == "OFTIntegerList" and sub == "OFSTBoolean"):
                sub = "OFSTBoolean"
            else:
                return json_
        elif isinstance(x, str):
            if i == 0 or t == "OFTStringList":
                t = "OFTStringList"
            else:
                return json_
        elif isinstance(x, float):
            if sub == "OFSTNone" and (
                i == 0 or t in ("OFTRealList", "OFTIntegerList", "OFTInteger64List")
            ):
                t = "OFTRealList"
            else:
                return json_
        elif isinstance(x, int):
            if sub == "OFSTNone" and t == "OFTIntegerList":
                if not _INT32[0] <= x <= _INT32[1]:
                    t = "OFTInteger64List"
            elif sub == "OFSTNone" and t in ("OFTInteger64List", "OFTRealList"):
                pass
            else:
                return json_
        else:
            return json_
    return t, sub


def _merge_list_kinds(cur: tuple[str, str], new: tuple[str, str], empty: bool) -> tuple[str, str]:
    """GDAL's `OGRGeoJSONReaderAddOrUpdateField` for list fields."""
    t, _ = cur
    nt, _ = new
    json_ = ("OFTString", "OFSTJSON")
    if t == "OFTString":
        return cur
    if nt == "OFTString":
        return cur if empty else json_
    if t == "OFTIntegerList":
        return new if nt in ("OFTInteger64List", "OFTRealList", "OFTStringList") else cur
    if t == "OFTInteger64List":
        return new if nt in ("OFTRealList", "OFTStringList") else cur
    if t == "OFTRealList":
        return new if nt == "OFTStringList" else cur
    return cur


_LIST_DTYPES = {"OFTIntegerList": np.int32, "OFTInteger64List": np.int64, "OFTRealList": np.float64}


def _json_field(values: list):
    """A GeoJSON field of JSON text, typed as GDAL types it.

    List types when its arrays make one, else a string field of the JSON subtype.
    """
    parsed: list[Any] = []
    kind = None
    for v in values:
        if v is None:
            parsed.append(None)
            continue
        try:
            p = json.loads(v)
        except ValueError:
            p = v
        parsed.append(p)
        new = _list_kind(p)
        empty = isinstance(p, list) and not p
        kind = new if kind is None else _merge_list_kinds(kind, new, empty)
    if kind is None or kind[0] == "OFTString":
        return "OFTString", "OFSTJSON", "object", np.array(values, dtype=object)
    out = np.empty(len(values), dtype=object)
    for i, p in enumerate(parsed):
        if p is None:
            out[i] = None
        elif kind[0] == "OFTStringList":
            out[i] = np.array([str(x) for x in p])
        else:
            out[i] = np.array(p, dtype=_LIST_DTYPES[kind[0]])
    dtype = {
        "OFTIntegerList": "list(int32)",
        "OFTInteger64List": "list(int64)",
        "OFTRealList": "list(float64)",
    }.get(kind[0], "list(str)")
    return kind[0], kind[1], dtype, out


def _numbers(values: list, dtype) -> np.ndarray:
    """An array of numbers, float64 with NaN when some are missing, as pyogrio fills it."""
    if any(v is None for v in values):
        return np.array([np.nan if v is None else v for v in values], dtype=np.float64)
    return np.array(values, dtype=dtype)


def _field(name: str, ftype: str, width: int, driver: str, values: list):
    """`(ogr_type, ogr_subtype, dtype, array)` of a field read by terrio."""
    if ftype == "Integer":
        if driver == "ESRI Shapefile":
            wide = width >= 10
        else:
            wide = any(v is not None and not _INT32[0] <= v <= _INT32[1] for v in values)
        dtype = "int64" if wide else "int32"
        return (
            ("OFTInteger64" if wide else "OFTInteger"),
            "OFSTNone",
            dtype,
            _numbers(values, dtype),
        )
    if ftype == "Float":
        return "OFTReal", "OFSTNone", "float64", _numbers(values, np.float64)
    if ftype == "Boolean":
        return "OFTInteger", "OFSTBoolean", "bool", _numbers(values, bool)
    if ftype == "Date":
        return (
            "OFTDate",
            "OFSTNone",
            "datetime64[D]",
            np.array([None if v is None else _iso_date(v) for v in values], dtype=object),
        )
    if ftype == "DateTime":
        out = np.array([None if v is None else _iso_datetime(v) for v in values], dtype=object)
        return "OFTDateTime", "OFSTNone", "datetime64[ms]", out
    if ftype == "Json":
        return _json_field(values)
    if ftype == "Blob":
        return "OFTBinary", "OFSTNone", "object", np.array(values, dtype=object)
    if (
        driver == "GeoJSON"
        and any(v is not None for v in values)
        and all(v is None or _TIME.match(v) for v in values)
    ):
        return (
            "OFTTime",
            "OFSTNone",
            "object",
            np.array([None if v is None else _time(v) for v in values], dtype=object),
        )
    return "OFTString", "OFSTNone", "object", np.array(values, dtype=object)


def read(
    path_or_buffer,
    layer=None,
    columns=None,
    read_geometry=True,
    skip_features=0,
    max_features=None,
    return_fids=False,
):
    """Pyogrio's `raw.read`, for the arguments terrio supports: `(meta, fids, geometry, fields)`.

    Shapefile paging counts live features, unlike pyogrio/GDAL physical-slot fast seeking
    when DBF records have been deleted. Returned FIDs retain physical slot numbers.
    """
    if skip_features < 0:
        raise ValueError("'skip_features' must be >= 0")
    if max_features is not None and max_features < 0:
        raise ValueError("'max_features' must be >= 0")
    if not (read_geometry or return_fids or columns is None or len(columns) > 0):
        raise ValueError(
            "at least one of read_geometry or return_fids must be True or columns must be None "
            "or non-empty"
        )
    with _Source(path_or_buffer) as source:
        return read_source(
            source, layer, columns, read_geometry, skip_features, max_features, return_fids
        )


def read_source(
    source: _Source,
    layer,
    columns,
    read_geometry: bool,
    skip_features: int,
    max_features: int | None,
    return_fids: bool,
):
    """`read` of an open `_Source`: the layer `layer` designates, or the first.

    Without `layer`, a dataset of several layers is read whole and its first layer kept, with a
    warning naming the others; with one, the layer is the only one read.
    """
    if layer is None:
        path, layer_name = source.path, None
    else:
        layer_name = _select_layer(source.layer_names(), layer, source.path)
        path = source.path
        if source.shapefiles is not None:
            # The shapefile is that layer alone.
            path, layer_name = dict(source.shapefiles[1])[layer_name], None
    raw = _open(path, skip_features, max_features or None, read_geometry, layer_name)
    if layer is None:
        names = source.layer_names() if source.shapefiles is not None else raw["layers"]
        if len(names) > 1:
            _warn_first_layer(source.name(), names)
    driver = raw["driver"]
    fields = []
    for (name, ftype, width, _precision), values in zip(raw["fields"], raw["columns"], strict=True):
        if columns is not None and name not in columns:
            continue
        fields.append((name, *_field(name, ftype, width, driver, values)))
    wkb = raw["wkb"]
    geometry = None
    if read_geometry and wkb is not None:
        geometry = np.empty(len(wkb), dtype=object)
        geometry[:] = wkb
    fids = np.array(raw["fids"], dtype=np.int64) if return_fids else None
    meta = {
        "crs": _crs(raw["crs"], fallback=raw.get("crs_wkt")),
        "encoding": "UTF-8",
        "fields": np.array([f[0] for f in fields], dtype=object),
        "dtypes": np.array([f[3] for f in fields], dtype=object),
        "ogr_types": [f[1] for f in fields],
        "ogr_subtypes": [f[2] for f in fields],
        "geometry_type": raw.get("geometry_type"),
        "driver": driver,
        "feature_count": raw["feature_count"],
        "layer": raw["layer"],
    }
    return meta, fids, geometry, [f[4] for f in fields]
