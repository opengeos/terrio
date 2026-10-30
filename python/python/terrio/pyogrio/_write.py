# SPDX-FileCopyrightText: Copyright (c) 1999, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 2008-2014, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright 2007, Mateusz Loskot
# SPDX-FileCopyrightText: Copyright 2008-2024, Even Rouault <even.rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 2007, Mateusz Loskot
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (translated to Python and modified):
#   GDAL v3.12.1:
#     ogr/ogrutils.cpp
#     ogr/ogrlibjsonutils.cpp
#     ogr/ogrgeojsonwriter.cpp

"""Writing GeoJSON the way GDAL 3.12's GeoJSON driver writes what pyogrio 0.13 gives it.

The text matches GDAL's byte for byte for the layouts measured (see the parity tests): the
collection header, one feature per line in json-c's spaced style, properties with 17 significant
digits and coordinates with 15 decimals, each trimmed as GDAL trims them.
"""

from __future__ import annotations

import datetime as _dt
import math
import os

import numpy as np


def _escape(s: str) -> str:
    """A JSON string as json-c escapes it (GDAL keeps `/` unescaped)."""
    out = ['"']
    for ch in s:
        o = ord(ch)
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\r":
            out.append("\\r")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\b":
            out.append("\\b")
        elif ch == "\f":
            out.append("\\f")
        elif o < 0x20 or o == 0x7F:
            out.append(f"\\u{o:04x}")
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def _has_rounding_noise(s: str) -> bool:
    dot = s.find(".")
    return dot >= 0 and ("999999" in s[dot:] or "000000" in s[dot:])


def format_real(v: float) -> str:
    """A real property as GDAL writes it.

    17 significant digits, fewer (down to 14) when that removes a run of 0s or 9s left by binary
    rounding, and `.0` on whole numbers.
    """
    if math.isinf(v):
        return "Infinity" if v > 0 else "-Infinity"
    if math.isnan(v):
        return "NaN"
    s = f"{v:.17g}"
    if _has_rounding_noise(s):
        for digits in (16, 15, 14):
            t = f"{v:.{digits}g}"
            if "." in t and not _has_rounding_noise(t):
                s = t
                break
    if "." not in s and "e" not in s:
        s += ".0"
    return s


def _roundup(s: str) -> str:
    """GDAL's `roundup`: adds one to the last digit of a decimal string, carrying."""
    digits = list(s)
    i = len(digits) - 1
    while i >= 0:
        c = digits[i]
        if c == ".":
            i -= 1
            continue
        if c == "-":
            break
        if c == "9":
            digits[i] = "0"
            i -= 1
            continue
        digits[i] = chr(ord(c) + 1)
        return "".join(digits)
    # Carried past the first digit.
    pos = 1 if digits and digits[0] == "-" else 0
    digits.insert(pos, "1")
    return "".join(digits)


def _intelliround(s: str) -> str:
    """GDAL's `intelliround` of a fixed-point string."""
    n = len(s)
    if n <= 10:
        return s
    dot = s.find(".")
    if dot < 0 or "e" in s or "E" in s:
        return s
    before = dot - 1 - (1 if s[0] == "-" else 0)
    if (
        s[n - 2] == "0"
        and s[n - 3] == "0"
        and s[n - 4] == "0"
        and s[n - 5] == "0"
        and s[n - 6] == "0"
    ):
        return s[:-1]
    if (
        dot < n - 8
        and (before >= 4 or s[n - 3] == "0")
        and (before >= 5 or s[n - 4] == "0")
        and (before >= 6 or s[n - 5] == "0")
        and (before >= 7 or s[n - 6] == "0")
        and (before >= 8 or s[n - 7] == "0")
        and s[n - 8] == "0"
        and s[n - 9] == "0"
    ):
        return s[: n - 8]
    if (
        s[n - 2] == "9"
        and s[n - 3] == "9"
        and s[n - 4] == "9"
        and s[n - 5] == "9"
        and s[n - 6] == "9"
    ):
        return _roundup(s[: n - 6])
    if (
        dot < n - 9
        and (before >= 4 or s[n - 3] == "9")
        and (before >= 5 or s[n - 4] == "9")
        and (before >= 6 or s[n - 5] == "9")
        and (before >= 7 or s[n - 6] == "9")
        and (before >= 8 or s[n - 7] == "9")
        and s[n - 8] == "9"
        and s[n - 9] == "9"
    ):
        return _roundup(s[: n - 9])
    return s


def format_coord(v: float) -> str:
    """A coordinate as GDAL writes it: 15 decimals, intelli-rounded, trailing zeros trimmed."""
    if math.isinf(v):
        return "inf" if v > 0 else "-inf"
    if math.isnan(v):
        return "nan"
    if abs(v) > 1e50:
        return f"{v:.17g}"
    s = _intelliround(f"{v:.15f}")
    if "." in s:
        s = s.rstrip("0")
        if s.endswith("."):
            s += "0"
    return s


def _coords(cs) -> str:
    return "[ " + ", ".join(format_coord(float(c)) for c in cs) + " ]"


def _ring(coords) -> str:
    return "[ " + ", ".join(_coords(c) for c in coords) + " ]"


def _geometry(g) -> str:
    if g is None:
        return "null"
    kind = g.geom_type
    if kind == "GeometryCollection":
        return (
            '{ "type": "GeometryCollection", "geometries": [ '
            + ", ".join(_geometry(p) for p in g.geoms)
            + " ] }"
        )
    if g.is_empty:
        body = "[ ]"
    elif kind == "Point":
        body = _coords(g.coords[0])
    elif kind == "LineString" or kind == "LinearRing":
        body = _ring(g.coords)
    elif kind == "Polygon":
        body = "[ " + ", ".join(_ring(r.coords) for r in [g.exterior, *g.interiors]) + " ]"
    elif kind == "MultiPoint":
        body = "[ " + ", ".join(_coords(p.coords[0]) for p in g.geoms) + " ]"
    elif kind == "MultiLineString":
        body = "[ " + ", ".join(_ring(line.coords) for line in g.geoms) + " ]"
    elif kind == "MultiPolygon":
        body = (
            "[ "
            + ", ".join(
                "[ " + ", ".join(_ring(r.coords) for r in [p.exterior, *p.interiors]) + " ]"
                for p in g.geoms
            )
            + " ]"
        )
    else:
        raise ValueError(f"cannot write a {kind} geometry to GeoJSON")
    return '{ "type": "' + kind + '", "coordinates": ' + body + " }"


def _iso(ts) -> str:
    """A date-time as GDAL writes an `OFTDateTime`.

    Milliseconds when there are any, `Z` for UTC and `+HH:MM` for other offsets.
    """
    out = f"{ts.year:04d}-{ts.month:02d}-{ts.day:02d}T{ts.hour:02d}:{ts.minute:02d}:{ts.second:02d}"
    ms = ts.microsecond // 1000
    if ms:
        out += f".{ms:03d}"
    off = ts.utcoffset() if ts.tzinfo is not None else None
    if off is not None:
        minutes = int(off.total_seconds() // 60)
        if minutes == 0:
            out += "Z"
        else:
            sign = "-" if minutes < 0 else "+"
            out += f"{sign}{abs(minutes) // 60:02d}:{abs(minutes) % 60:02d}"
    return out


def _value(v, kind: str) -> str:
    if kind == "bool":
        return "true" if v else "false"
    if kind == "int":
        return str(int(v))
    if kind == "real":
        return format_real(float(v))
    if kind == "datetime":
        return _escape(_iso(v))
    if kind == "date":
        return _escape(v.isoformat())
    return _escape(str(v))


def _column_kind(series) -> str:
    import pandas as pd  # type: ignore[import-untyped, import-not-found, unused-ignore]
    from pandas.api import types  # type: ignore[import-untyped, import-not-found, unused-ignore]

    dtype = series.dtype
    if types.is_bool_dtype(dtype):
        return "bool"
    if types.is_integer_dtype(dtype):
        return "int"
    if types.is_float_dtype(dtype):
        return "real"
    if types.is_datetime64_any_dtype(dtype):
        return "datetime"
    if isinstance(dtype, pd.ArrowDtype):
        return "string"
    return "string"


def _is_null(v) -> bool:
    if v is None:
        return True
    try:
        import pandas as pd

        return bool(pd.isna(v)) if not isinstance(v, (list, tuple, np.ndarray, dict)) else False
    except (TypeError, ValueError):
        return False


def _crs_member(crs) -> str | None:
    if crs is None:
        return None
    epsg = crs.to_epsg()
    if epsg is None:
        return None
    name = "urn:ogc:def:crs:OGC:1.3:CRS84" if epsg == 4326 else f"urn:ogc:def:crs:EPSG::{epsg}"
    return '{ "type": "name", "properties": { "name": ' + _escape(name) + " } }"


def to_geojson(df, layer: str) -> str:
    """The GeoJSON text GDAL writes for a (Geo)DataFrame through pyogrio."""
    geometry_name = getattr(df, "_geometry_column_name", None)
    has_geometry = geometry_name is not None and geometry_name in df.columns
    columns = [c for c in df.columns if not (has_geometry and c == geometry_name)]
    kinds = [_column_kind(df[c]) for c in columns]
    values = [df[c].tolist() for c in columns]
    geoms = df[geometry_name].values if has_geometry else [None] * len(df)
    crs = df.crs if has_geometry else None
    lines = ["{", '"type": "FeatureCollection",', f'"name": {_escape(layer)},']
    member = _crs_member(crs)
    if member is not None:
        lines.append(f'"crs": {member},')
    lines.append('"features": [')
    feats = []
    names = [_escape(str(c)) for c in columns]
    for i in range(len(df)):
        props = []
        for name, kind, vals in zip(names, kinds, values, strict=True):
            v = vals[i]
            props.append(f"{name}: " + ("null" if _is_null(v) else _value(v, kind)))
        body = "{ " + ", ".join(props) + " }" if props else "{ }"
        feats.append(
            '{ "type": "Feature", "properties": '
            + body
            + ', "geometry": '
            + _geometry(geoms[i])
            + " }"
        )
    lines.append(",\n".join(feats))
    lines.append("]")
    lines.append("}")
    return "\n".join(lines) + "\n"


def write_geojson(df, path: str, layer: str | None) -> None:
    if layer is None:
        layer = os.path.splitext(os.path.basename(path))[0]
    text = to_geojson(df, layer)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


__all__ = ["format_coord", "format_real", "to_geojson", "write_geojson"]
_ = _dt
