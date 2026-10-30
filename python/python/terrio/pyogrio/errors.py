# SPDX-FileCopyrightText: Copyright (c) 2020-2024 Brendan C. Ward and pyogrio contributors
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (adapted and modified):
#   pyogrio 0.13.0:
#     pyogrio/errors.py

"""Errors raised by :mod:`terrio.pyogrio`, with pyogrio's names and hierarchy.

`geopandas` and callers catch these by name (``pyogrio.errors.DataSourceError``), so they are
the same classes pyogrio defines, not subclasses of terrio's errors.
"""


class DataSourceError(RuntimeError):
    """Errors relating to opening or closing a data source (with one or more layers)."""


class DataLayerError(RuntimeError):
    """Errors relating to working with a single layer."""


class CRSError(DataLayerError):
    """Errors relating to getting or setting CRS values."""


class FeatureError(DataLayerError):
    """Errors related to reading or writing a feature."""


class GeometryError(DataLayerError):
    """Errors relating to getting or setting a geometry field."""


class FieldError(DataLayerError):
    """Errors relating to getting or setting a non-geometry field."""
