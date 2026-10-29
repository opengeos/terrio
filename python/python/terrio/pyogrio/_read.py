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
from terrio.pyogrio.errors import DataLayerError, DataSourceError

_INT32 = (-(2**31), 2**31 - 1)


# ----------------------------------------------------------------------------------------------
# Locating the dataset
# ----------------------------------------------------------------------------------------------


_NOT_RECOGNIZED = (
    "'{}' not recognized as being in a supported file format. It might help to specify the correct "
    "driver explicitly by prefixing the file path with '<DRIVER>:', e.g. 'CSV:path'."
)


def _zip_main_member(archive: str) -> str | None:
    """The member GDAL opens for `/vsizip/<archive>` without a member.

    The only file of a single-file archive, else the first top-level shapefile (the archive is a
    directory to GDAL, which its Shapefile driver opens).
    """
    try:
        with zipfile.ZipFile(archive) as z:
            names = [n for n in z.namelist() if not n.endswith("/")]
    except (OSError, zipfile.BadZipFile) as e:
        raise DataSourceError(f"{archive}: {e}") from None
    if len(names) == 1:
        return names[0]
    shapefiles = [n for n in names if "/" not in n and n.lower().endswith(".shp")]
    return shapefiles[0] if shapefiles else None


def _directory_layer(directory: str) -> str:
    """The shapefile GDAL's Shapefile driver opens as the first layer of a directory.

    Sorted by name here; GDAL takes the order the file system lists them in.
    """
    names = sorted(n for n in os.listdir(directory) if n.lower().endswith(".shp"))
    if not names:
        raise DataSourceError(_NOT_RECOGNIZED.format(directory))
    if len(names) > 1:
        others = ", ".join(f"'{Path(n).stem}'" for n in names[1:])
        warnings.warn(
            f"More than one layer found in '{os.path.basename(directory)}': "
            f"'{Path(names[0]).stem}' (default), {others}. "
            "Specify layer parameter to avoid this warning.",
            stacklevel=4,
        )
    return os.path.join(directory, names[0])


def _vsizip(archive: str, member: str | None) -> str:
    if member is None:
        member = _zip_main_member(archive)
        if member is None:
            raise DataSourceError(_NOT_RECOGNIZED.format(f"/vsizip/{archive}"))
    return f"/vsizip/{archive}/{member.lstrip('/')}"


def _local_path(path: str) -> str:
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
        return path
    if path.split("!")[0].lower().endswith(".zip") and "://" not in path:
        archive, _, member = path.partition("!")
        return _vsizip(archive, member or None)
    if os.path.isdir(path):
        return _directory_layer(path)
    return path


def _spill_name(data: bytes) -> str:
    """A file name telling terrio the format of bytes, from their content.

    As GDAL identifies `/vsimem/` files: zip archives and GeoJSON.
    """
    if data[:4] == b"PK\x03\x04":
        return "data.zip"
    head = data[:64].lstrip(b"\xef\xbb\xbf").lstrip()
    if head[:1] in (b"{", b"["):
        return "data.geojson"
    raise DataSourceError(_NOT_RECOGNIZED.format("/vsimem/pyogrio"))


class _Source:
    """A dataset to read: a path, or bytes spilled into a temporary directory removed on close."""

    def __init__(self, path_or_buffer):
        self._tmp = None
        if isinstance(path_or_buffer, Path):
            path_or_buffer = str(path_or_buffer)
        if hasattr(path_or_buffer, "read") and not isinstance(path_or_buffer, (str, bytes)):
            data = path_or_buffer.read()
            if hasattr(path_or_buffer, "seekable") and path_or_buffer.seekable():
                path_or_buffer.seek(0)
            if isinstance(data, str):
                data = data.encode("utf-8")
            path_or_buffer = data
        if isinstance(path_or_buffer, (bytes, bytearray, memoryview)):
            data = bytes(path_or_buffer)
            name = _spill_name(data)
            self._tmp = tempfile.TemporaryDirectory(prefix="terrio-pyogrio-")
            spilled = os.path.join(self._tmp.name, name)
            with open(spilled, "wb") as f:
                f.write(data)
            self.path = _local_path(spilled)
            return
        if not isinstance(path_or_buffer, str):
            path_or_buffer = str(path_or_buffer)
        self.path = _local_path(path_or_buffer)

    def close(self):
        if self._tmp is not None:
            self._tmp.cleanup()
            self._tmp = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def _open(path: str, skip_features: int, max_features: int | None, read_geometry: bool) -> dict:
    try:
        return _terrio.pyogrio_read(path, skip_features, max_features, read_geometry)
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


def _crs(crs: str | None) -> str | None:
    """The CRS as pyogrio gives it.

    `EPSG:<code>` when GDAL identifies an EPSG code, otherwise GDAL's WKT1 (here PROJ's
    `WKT1_GDAL` export, which names axes as GDAL does).
    """
    if crs is None or crs.startswith("EPSG:"):
        return crs
    try:
        from pyproj import CRS

        return CRS.from_user_input(crs).to_wkt("WKT1_GDAL")
    except Exception:  # noqa: BLE001
        return crs


def read(
    path_or_buffer,
    layer=None,
    columns=None,
    read_geometry=True,
    skip_features=0,
    max_features=None,
    return_fids=False,
):
    """Pyogrio's `raw.read`, for the arguments terrio supports: `(meta, fids, geometry, fields)`."""
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
        raw = _open(source.path, skip_features, max_features or None, read_geometry)
    if (
        layer is not None
        and layer not in (0, raw["layer"])
        and not (isinstance(layer, str) and layer == Path(source.path).stem)
    ):
        raise DataLayerError(f"Layer '{layer}' could not be opened")
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
        "crs": _crs(raw["crs"]),
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
