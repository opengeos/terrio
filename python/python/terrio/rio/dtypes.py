# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/dtypes.py

"""Pixel data types, named as NumPy and rasterio name them."""

from __future__ import annotations

from typing import Any

import numpy as np
from numpy.typing import ArrayLike, DTypeLike

bool_ = np.dtype(bool).name
ubyte = uint8 = np.dtype(np.uint8).name
sbyte = int8 = np.dtype(np.int8).name
uint16 = np.dtype(np.uint16).name
int16 = np.dtype(np.int16).name
uint32 = np.dtype(np.uint32).name
int32 = np.dtype(np.int32).name
uint64 = np.dtype(np.uint64).name
int64 = np.dtype(np.int64).name
float16 = np.dtype(np.float16).name
float32 = np.dtype(np.float32).name
float64 = np.dtype(np.float64).name
complex_ = "complex"
complex64 = np.dtype(np.complex64).name
complex128 = np.dtype(np.complex128).name
complex_int16 = "complex_int16"

# GDAL data type codes.
dtype_fwd: dict[int, str | None] = {
    0: None,
    1: ubyte,
    2: uint16,
    3: int16,
    4: uint32,
    5: int32,
    6: float32,
    7: float64,
    8: complex_int16,
    9: complex64,
    10: complex64,
    11: complex128,
    12: uint64,
    13: int64,
    14: sbyte,
    15: float16,
    16: complex64,
}
dtype_rev: dict[str | None, int] = {v: k for k, v in dtype_fwd.items()}
dtype_rev[uint8] = 1
dtype_rev[complex_] = 11
dtype_rev[complex64] = 10
dtype_rev[complex_int16] = 8

typename_fwd: dict[int, str] = {
    0: "Unknown",
    1: "Byte",
    2: "UInt16",
    3: "Int16",
    4: "UInt32",
    5: "Int32",
    6: "Float32",
    7: "Float64",
    8: "CInt16",
    9: "CInt32",
    10: "CFloat32",
    11: "CFloat64",
    12: "UInt64",
    13: "Int64",
    14: "Int8",
    15: "Float16",
    16: "CFloat16",
}
typename_rev: dict[str, int] = {v: k for k, v in typename_fwd.items()}

_f16 = np.finfo(np.float16)
_f32 = np.finfo(np.float32)
_f64 = np.finfo(np.float64)
dtype_ranges: dict[str, tuple[float, float]] = {
    int8: (-128, 127),
    uint8: (0, 255),
    uint16: (0, 65535),
    int16: (-32768, 32767),
    uint32: (0, 4294967295),
    int32: (-2147483648, 2147483647),
    float16: (float(_f16.min), float(_f16.max)),
    float32: (float(_f32.min), float(_f32.max)),
    float64: (float(_f64.min), float(_f64.max)),
    int64: (-9223372036854775808, 9223372036854775807),
    uint64: (0, 18446744073709551615),
}


def in_dtype_range(value: float, dtype: DTypeLike) -> bool:
    """Whether a value fits in a data type (NaN and infinities fit float types).

    Args:
        value: The value.
        dtype: The data type.

    Returns:
        bool: True when the value is in the type's range.
    """
    dt = np.dtype(dtype)
    if dt.kind in "fc":
        if np.isnan(value) or np.isinf(value):
            return True
        finfo = np.finfo(dt)
        return bool(finfo.min <= value <= finfo.max)
    iinfo = np.iinfo(dt)
    return bool(iinfo.min <= value <= iinfo.max)


def _gdal_typename(dt: Any) -> str:
    """The GDAL name of a data type, such as `Byte` or `Float32`.

    Args:
        dt: A data type name or NumPy type.

    Returns:
        str: The GDAL name.
    """
    try:
        return typename_fwd[dtype_rev[dt]]
    except KeyError:
        return typename_fwd[dtype_rev[np.dtype(dt).name]]


def check_dtype(dt: Any) -> bool:
    """Whether a data type is supported.

    Args:
        dt: A data type name or NumPy type.

    Returns:
        bool: True when supported.
    """
    if str(dt) in dtype_rev:
        return True
    try:
        return callable(dt) and str(np.dtype(dt)) in dtype_rev
    except TypeError:
        return False


def get_minimum_dtype(values: ArrayLike) -> str:
    """The smallest data type that holds all values.

    Args:
        values: Values.

    Returns:
        str: The data type name.
    """
    arr = np.asanyarray(values)
    lo, hi = arr.min(), arr.max()
    if arr.dtype.kind in "iu":
        if lo >= 0:
            if hi <= 255:
                return uint8
            if hi <= 65535:
                return uint16
            if hi <= 4294967295:
                return uint32
            return uint64
        if lo >= -128 and hi <= 127:
            return int8
        if lo >= -32768 and hi <= 32767:
            return int16
        if lo >= -2147483648 and hi <= 2147483647:
            return int32
        return int64
    if lo >= _f16.min and hi <= _f16.max:
        return float16
    if lo >= -3.4028235e38 and hi <= 3.4028235e38:
        return float32
    return float64


def is_ndarray(array: Any) -> bool:
    """Whether an object is an array or can be converted to one.

    Args:
        array: The object.

    Returns:
        bool: True for arrays and objects with `__array__`.
    """
    return isinstance(array, np.ndarray) or hasattr(array, "__array__")


def _getnpdtype(dtype: Any) -> np.dtype[Any]:
    """The NumPy dtype of a data type (complex integers as complex64).

    Args:
        dtype: A data type.

    Returns:
        numpy.dtype: The NumPy dtype.
    """
    if isinstance(dtype, str) and dtype.startswith("complex_int"):
        return np.dtype(np.complex64)
    out: np.dtype[Any] = np.dtype(dtype)
    return out


def can_cast_dtype(values: ArrayLike, dtype: DTypeLike) -> bool:
    """Whether values convert to a data type without loss.

    Args:
        values: Values.
        dtype: The target data type.

    Returns:
        bool: True when the conversion keeps the values.
    """
    arr = np.asanyarray(values)
    if arr.dtype.name == _getnpdtype(dtype).name:
        return True
    if arr.dtype.kind == "f":
        return bool(np.allclose(arr, arr.astype(dtype), equal_nan=True))
    return bool(np.array_equal(arr, arr.astype(dtype)))


def validate_dtype(values: ArrayLike, valid_dtypes: Any) -> bool:
    """Whether values have, or fit in, one of the valid data types.

    Args:
        values: Values.
        valid_dtypes: Data type names.

    Returns:
        bool: True when valid.
    """
    arr = np.asanyarray(values)
    return arr.dtype.name in valid_dtypes or get_minimum_dtype(arr) in valid_dtypes


def _is_complex_int(dtype: Any) -> bool:
    """Whether a data type is a complex integer type.

    Args:
        dtype: A data type.

    Returns:
        bool: True for `complex_int16`.
    """
    return isinstance(dtype, str) and dtype.startswith("complex_int")


__all__ = [
    "bool_",
    "can_cast_dtype",
    "check_dtype",
    "complex128",
    "complex64",
    "complex_",
    "complex_int16",
    "dtype_ranges",
    "float16",
    "float32",
    "float64",
    "get_minimum_dtype",
    "in_dtype_range",
    "int16",
    "int32",
    "int64",
    "int8",
    "is_ndarray",
    "sbyte",
    "ubyte",
    "uint16",
    "uint32",
    "uint64",
    "uint8",
    "validate_dtype",
]
