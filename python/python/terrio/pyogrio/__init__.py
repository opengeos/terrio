# SPDX-FileCopyrightText: Copyright (c) 2020-2024 Brendan C. Ward and pyogrio contributors
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (adapted and modified):
#   pyogrio 0.13.0:
#     pyogrio/geopandas.py
#     pyogrio/core.py

"""pyogrio's API on terrio's vector drivers: what geopandas uses of `pyogrio`, without GDAL.

geopandas 1.1 reads and writes vector files through a module named `pyogrio`: `read_dataframe`
and `write_dataframe`, `read_info` and `list_layers`, the GDAL configuration functions and
`pyogrio.errors`. This module provides them for GeoJSON and ESRI Shapefile, the formats an
application allows, returning what pyogrio 0.13.0 (GDAL 3.12) returns: the same columns
in the same order, dtypes (including GDAL's date, date-time, list and JSON fields), index,
geometries (ISO WKB, measures dropped as pyogrio drops them) and CRS (``"EPSG:xxxx"`` when
identified, otherwise WKT).

Registering it as `pyogrio` (``sys.modules["pyogrio"] = terrio.pyogrio``, or
`terrio.compat.install`) before geopandas is imported makes ``geopandas.read_file`` and
``GeoDataFrame.to_file`` use terrio.

Datasets open through `terrio.open_vector`, so `terrio.set_allowed_drivers` restricts them as it
restricts every terrio open; bytes and file objects are first written to a private temporary
file, as pyogrio writes them to GDAL's `/vsimem/`.

A dataset of several layers (the tables of a GeoPackage, the folders of a KML document, the
layers of a GPX file, the shapefiles of a directory or a zip archive...) is read a layer at a
time: `list_layers` lists them, `layer=` names one (in any case) or numbers it from 0, and
without `layer` the first is read, with a warning naming the others.

Not provided (they raise `NotImplementedError`): `where`, `sql`, `fids`, `encoding` (set the
`SHAPE_ENCODING` option instead), dataset open options, writing any driver but GeoJSON, and
`force_2d` and `mask` of `read_arrow` and `open_arrow` (`read_dataframe` takes both).

GeoParquet files are read too. `read_arrow` and `open_arrow` hand out their Arrow record batches
as pyogrio does through GDAL's Arrow stream (decoded in parallel, read by range requests from a
URL), and `read_dataframe(..., use_arrow=True)` builds its GeoDataFrame from them. Arrow reads
of other formats are not provided, and `use_arrow=True` reads them by the regular path.
"""

from __future__ import annotations

import contextlib
import datetime as _dt
import json
import os
import re
import warnings
from typing import Any

import numpy as np

from terrio import _terrio
from terrio._warnings import warn_user
from terrio.pyogrio import errors
from terrio.pyogrio._read import _Source
from terrio.pyogrio._read import read as _read
from terrio.pyogrio._read import read_source as _read_source
from terrio.pyogrio.errors import DataSourceError

__version__ = "0.13.0"
"""The pyogrio version whose behaviour this module reproduces."""

__gdal_version__ = (3, 12, 4)
"""The GDAL version pyogrio 0.13.0 bundles, whose behaviour this module reproduces."""

__gdal_version_string__ = "3.12.4"
__gdal_geos_version__ = None
__terrio_version__ = _terrio.__version__

_DRIVERS = {"GeoJSON": "rw", "ESRI Shapefile": "r", "Parquet": "r"}
_EXTENSIONS = {
    ".geojson": "GeoJSON",
    ".json": "GeoJSON",
    ".shp": "ESRI Shapefile",
    ".dbf": "ESRI Shapefile",
}

# Options set with `set_gdal_config_options`, process-wide as GDAL keeps them: applied to this
# module's reads and writes (pyogrio's GDAL is its own; rasterio's options do not reach it).
_config: dict[str, str] = {}


class _Scope:
    def __enter__(self):
        self.pushed = bool(_config)
        if self.pushed:
            _terrio.config_push(dict(_config))

    def __exit__(self, *exc):
        if self.pushed:
            _terrio.config_pop()


def _scope():
    return _Scope()


def set_gdal_config_options(options: dict) -> None:
    """Sets configuration options for this module's reads and writes, as pyogrio sets GDAL's.

    `True`/`False` become ``"ON"``/``"OFF"``; `None` clears an option.
    """
    for name, value in options.items():
        if value is None:
            _config.pop(name, None)
        elif isinstance(value, bool):
            _config[name] = "ON" if value else "OFF"
        else:
            _config[name] = str(value)


def get_gdal_config_option(name: str):
    """The value of a configuration option, as pyogrio returns it.

    An int for digits, `True` and `False` for ``"ON"`` and ``"OFF"``, otherwise the string, or
    None when it is not set.
    """
    value = _config.get(name)
    if value is None:
        value = _terrio.config_get(name)
    if not value:
        return None
    if value.isdigit():
        return int(value)
    if value == "ON":
        return True
    if value == "OFF":
        return False
    return value


def get_gdal_data_path():
    """None: terrio needs no GDAL data files."""
    return None


def list_drivers(read: bool = False, write: bool = False) -> dict:
    """The drivers available, as pyogrio lists them: name to ``"r"`` or ``"rw"``."""
    drivers = dict(_DRIVERS)
    if read:
        drivers = {k: v for k, v in drivers.items() if "r" in v}
    if write:
        drivers = {k: v for k, v in drivers.items() if "w" in v}
    return drivers


def list_drivers_details() -> dict:
    """The drivers available, with their modes."""
    return {name: {"modes": mode} for name, mode in _DRIVERS.items()}


def detect_write_driver(path) -> str:
    """The driver writing `path`, from its extension."""
    ext = os.path.splitext(str(path))[1].lower()
    if ext in _EXTENSIONS:
        return _EXTENSIONS[ext]
    raise ValueError(f"Could not infer driver from path: {path}; please specify driver explicitly")


def _unsupported(**kwargs):
    for name, value in kwargs.items():
        if value is not None and value is not False and value != {}:
            raise NotImplementedError(f"terrio.pyogrio does not support '{name}'")


def read_dataframe(
    path_or_buffer,
    /,
    layer=None,
    encoding=None,
    columns=None,
    read_geometry=True,
    force_2d=False,
    skip_features=0,
    max_features=None,
    where=None,
    bbox=None,
    mask=None,
    fids=None,
    sql=None,
    sql_dialect=None,
    fid_as_index=False,
    use_arrow=None,
    on_invalid="raise",
    arrow_to_pandas_kwargs=None,
    datetime_as_string=False,
    mixed_offsets_as_utc=True,
    **kwargs,
):
    """Reads a GeoJSON or ESRI Shapefile dataset into a GeoDataFrame, as pyogrio does.

    Takes pyogrio's arguments; `bbox` and `mask` keep the features whose geometry intersects
    them. Returns a DataFrame when `read_geometry` is false.

    Shapefile paging counts live features and skips deleted DBF slots. This intentionally
    differs from pyogrio/GDAL fast seeking, which can count physical slots including deletions.
    """
    import geopandas as gp  # type: ignore[import-untyped, import-not-found, unused-ignore]
    import pandas as pd  # type: ignore[import-untyped, import-not-found, unused-ignore]
    import shapely  # type: ignore[import-untyped, import-not-found, unused-ignore]

    _unsupported(
        encoding=encoding, where=where, fids=fids, sql=sql, sql_dialect=sql_dialect, **kwargs
    )
    if bbox is not None and mask is not None:
        raise ValueError("cannot set both 'bbox' and 'mask'")
    if hasattr(path_or_buffer, "__fspath__") and not hasattr(path_or_buffer, "read"):
        path_or_buffer = os.fspath(path_or_buffer)
    if use_arrow and _is_geoparquet(path_or_buffer) and mask is None and not fid_as_index:
        return _read_geoparquet_dataframe(
            path_or_buffer,
            layer=layer,
            columns=columns,
            read_geometry=read_geometry,
            force_2d=force_2d,
            skip_features=skip_features,
            max_features=max_features,
            bbox=bbox,
            arrow_to_pandas_kwargs=arrow_to_pandas_kwargs,
        )
    spatial = bbox is not None or mask is not None
    with _scope():
        meta, index, geometry, field_data = _read(
            path_or_buffer,
            layer=layer,
            columns=columns,
            read_geometry=read_geometry or spatial,
            skip_features=0 if spatial else skip_features,
            max_features=None if spatial else max_features,
            return_fids=fid_as_index,
        )
    if spatial:
        # GDAL filters before skipping and limiting: so does this.
        area = shapely.box(*bbox) if bbox is not None else mask
        geoms = shapely.from_wkb(geometry, on_invalid="ignore")
        keep = np.flatnonzero(shapely.intersects(geoms, area))
        keep = keep[skip_features:]
        if max_features:
            keep = keep[:max_features]
        geometry = geometry[keep] if read_geometry else None
        field_data = [a[keep] for a in field_data]
        index = index[keep] if index is not None else None

    columns = meta["fields"].tolist()
    data = {columns[i]: field_data[i] for i in range(len(columns))}
    if fid_as_index:
        index = pd.Index(index, name="fid")
    else:
        index = None
    df = pd.DataFrame(data, columns=columns, index=index)
    for dtype, c in zip(meta["dtypes"], meta["fields"], strict=True):
        if dtype.startswith("datetime"):
            df[c] = _parse_datetime(df[c], datetime_as_string, mixed_offsets_as_utc)
    for subtype, c in zip(meta["ogr_subtypes"], meta["fields"], strict=True):
        if subtype == "OFSTJSON":
            if pd.api.types.infer_dtype(df[c]) == "string":
                try:
                    df[c] = df[c].map(json.loads, na_action="ignore")
                except Exception:
                    warnings.warn(
                        f"Could not parse column '{c}' as JSON; leaving as string", stacklevel=2
                    )

    if geometry is None or not read_geometry:
        return df
    geometry = shapely.from_wkb(geometry, on_invalid=on_invalid)
    if force_2d:
        geometry = shapely.force_2d(geometry)
    return gp.GeoDataFrame(df, geometry=geometry, crs=meta["crs"])


def _read_geoparquet_dataframe(
    path,
    *,
    layer,
    columns,
    read_geometry,
    force_2d,
    skip_features,
    max_features,
    bbox,
    arrow_to_pandas_kwargs,
):
    """A GeoParquet file as `read_dataframe(..., use_arrow=True)` reads it.

    Built from the Arrow table `read_arrow` gives, the geometry column last, with the CRS GDAL
    names.
    """
    import geopandas as gp  # type: ignore[import-untyped, import-not-found, unused-ignore]
    import pyarrow as pa  # type: ignore[import-untyped, import-not-found, unused-ignore]
    import shapely  # type: ignore[import-untyped, import-not-found, unused-ignore]

    meta, table = read_arrow(
        path,
        layer=layer,
        columns=columns,
        read_geometry=read_geometry,
        skip_features=skip_features,
        max_features=max_features,
        bbox=bbox,
    )
    kwargs = arrow_to_pandas_kwargs or {}
    name = meta["geometry_name"]
    if not read_geometry or not name:
        return table.to_pandas(**kwargs)
    column = table[name]
    storage = column.type
    if getattr(storage, "extension_name", "") == "geoarrow.wkb":
        storage = storage.storage_type
        column = pa.chunked_array([chunk.storage for chunk in column.chunks], type=storage)
    if pa.types.is_binary(storage) or pa.types.is_large_binary(storage):
        geometry = shapely.from_wkb(column.to_numpy(zero_copy_only=False))
        df = table.drop_columns([name]).to_pandas(**kwargs)
        gdf = gp.GeoDataFrame(
            df, geometry=gp.GeoSeries(geometry, index=df.index, name=name), crs=meta["crs"]
        )
    else:
        gdf = gp.GeoDataFrame.from_arrow(table, to_pandas_kwargs=kwargs).set_crs(
            meta["crs"], allow_override=True
        )
    if force_2d:
        gdf[name] = shapely.force_2d(gdf[name].values)
    return gdf


_SUBMICROSECOND_ISO = re.compile(r"[.,][0-9]{6}[0-9]*[1-9]")


def _parse_iso_datetime(pd, value: str, *, warn_precision_loss: bool = False) -> Any:
    """Parse one ISO datetime for offset inspection or Python datetime output.

    Args:
        pd: The pandas module.
        value: An ISO date-time string.
        warn_precision_loss: Report sub-microsecond loss when producing Python datetime output.

    Returns:
        datetime: A value preserving its UTC offset, or pandas' NaT for missing strings.

    Raises:
        ValueError: The string is not a valid ISO datetime.
    """
    try:
        parsed = _dt.datetime.fromisoformat(value[:-1] + "+00:00" if value.endswith("Z") else value)
    except (AttributeError, TypeError, ValueError):
        # A singleton cannot mix offsets or emit the mixed-timezone warning.
        # The shared bridge below reports precision loss at the caller on both paths.
        parsed = pd.to_datetime([value], format="ISO8601")[0].to_pydatetime(warn=False)
    if warn_precision_loss and _SUBMICROSECOND_ISO.search(value):
        # Offset inspection discards this temporary value; Python output must retain the
        # precision diagnostic that pandas' to_pydatetime gives for the same conversion.
        warn_user("Discarding nonzero nanoseconds in conversion.")
    return parsed


def _to_datetime_iso8601(pd, ser):
    """Parse ISO date-times without changing process-wide warning filters.

    Args:
        pd: The pandas module.
        ser: A series of date-time strings and missing values.

    Returns:
        Series: Parsed values for a uniform timezone or naive input.

    Raises:
        ValueError: Timezones are mixed, or pandas refuses an invalid input.
    """
    if int(pd.__version__.split(".")[0]) < 3:
        offsets = set()
        for value in pd.unique(ser):
            if pd.isna(value):
                continue
            try:
                parsed = _parse_iso_datetime(pd, value)
                if parsed is pd.NaT:
                    continue
                offset = parsed.utcoffset()
            except (AttributeError, TypeError, ValueError):
                # Let pandas retain its own invalid-input diagnostic.
                break
            if len(offsets) < 2:
                offsets.add(offset)
        else:
            if len(offsets) > 1:
                # Match pandas 3's error before pandas 2 can emit its FutureWarning. The caller
                # already handles both UTC conversion and per-value offset preservation.
                raise ValueError("Mixed timezones detected")
    return pd.to_datetime(ser, format="ISO8601")


def _parse_datetime(ser, datetime_as_string: bool, mixed_offsets_as_utc: bool):
    """Date and date-time strings to datetime64[ms], as pyogrio 0.13 parses them with pandas."""
    import pandas as pd

    if datetime_as_string:
        return ser
    warning = "Error parsing datetimes, original strings are returned: {message}"
    try:
        res = _to_datetime_iso8601(pd, ser)
        if not mixed_offsets_as_utc and res.dtype == "object":
            res = res.map(lambda x: x.to_pydatetime(), na_action="ignore")
    except Exception as ex:
        if isinstance(ex, ValueError) and "Mixed timezones detected" in str(ex):
            if mixed_offsets_as_utc:
                try:
                    res = pd.to_datetime(ser, utc=True, format="ISO8601")
                except Exception as ex2:
                    warnings.warn(warning.format(message=str(ex2)), stacklevel=3)
                    return ser
            else:
                try:
                    values = {
                        value: _parse_iso_datetime(pd, value, warn_precision_loss=True)
                        for value in pd.unique(ser)
                        if not pd.isna(value)
                    }
                    res = ser.map(values, na_action="ignore")
                except Exception as ex2:
                    warnings.warn(warning.format(message=str(ex2)), stacklevel=3)
                    return ser
        else:
            warnings.warn(warning.format(message=str(ex)), stacklevel=3)
            return ser
    if mixed_offsets_as_utc and res.dtype in ("object", "string"):
        try:
            res = pd.to_datetime(ser, utc=True, format="ISO8601")
        except Exception as ex:
            warnings.warn(warning.format(message=str(ex)), stacklevel=3)
    if res.dtype.kind == "M":
        res = res.dt.as_unit("ms")
    return res


def read_info(
    path_or_buffer,
    /,
    layer=None,
    encoding=None,
    force_feature_count=False,
    force_total_bounds=False,
    **kwargs,
):
    """A dataset's metadata, as pyogrio's `read_info` returns it (from a full read)."""
    _unsupported(encoding=encoding, **kwargs)
    with _scope():
        meta, _, geometry, field_data = _read(path_or_buffer, layer=layer, read_geometry=True)
    import shapely

    present = _present(geometry)
    geometry_type = _geometry_type(meta, present)
    bounds = tuple(shapely.total_bounds(present)) if len(present) else None
    return {
        "layer_name": meta["layer"],
        "crs": meta["crs"],
        "fields": meta["fields"],
        "dtypes": meta["dtypes"],
        "ogr_types": meta["ogr_types"],
        "ogr_subtypes": meta["ogr_subtypes"],
        "encoding": meta["encoding"],
        "fid_column": "",
        "geometry_name": "",
        "geometry_type": geometry_type,
        "features": meta["feature_count"],
        "total_bounds": bounds,
        "driver": meta["driver"],
        "capabilities": {
            "random_read": True,
            "fast_set_next_by_index": True,
            "fast_spatial_filter": False,
            "fast_feature_count": True,
            "fast_total_bounds": False,
        },
        "layer_metadata": None,
        "dataset_metadata": None,
    }


def _present(geometry):
    """The geometries of a layer read, as shapely objects, without the missing ones."""
    import shapely

    if geometry is None:
        return np.array([], dtype=object)
    geoms = shapely.from_wkb(geometry, on_invalid="ignore")
    return geoms[~shapely.is_missing(geoms)] if len(geoms) else geoms


def _geometry_type(meta, present) -> str | None:
    """`read_info`'s `geometry_type` of a layer read (`present`: its geometries)."""
    import shapely

    # GDAL's GeoJSON drivers declare the layer type from the features' geometry objects (the
    # first position's values, a collection's first member, type names without case), which the
    # geometries read do not always show: the declared type when there is one.
    geometry_type = meta["geometry_type"]
    if geometry_type is None:
        types = {g.geom_type for g in present}
        has_z = bool(len(present)) and bool(shapely.has_z(present).any())
        geometry_type = types.pop() if len(types) == 1 else ("Unknown" if types else None)
        if geometry_type is not None and has_z:
            geometry_type += " Z"
    return geometry_type


def read_bounds(*args, **kwargs):
    """Not provided."""
    raise NotImplementedError("terrio.pyogrio does not support read_bounds")


def list_layers(path_or_buffer, /):
    """The layers of a dataset and their geometry types, as an array of `[name, type]` rows.

    Every layer, in the dataset's order (as `terrio.list_layers` lists them; the shapefiles of a
    directory or a zip archive), each with the geometry type `read_info` gives it, from its
    features: each layer is read.
    """
    rows = []
    with _scope(), _Source(path_or_buffer) as source:
        # A loop, not a comprehension: before Python 3.12 one is a frame of its own.
        for name in source.layer_names():
            rows.append([name, _layer_geometry_type(source, name)])
    return np.array(rows, dtype=object).reshape(len(rows), 2)


def _layer_geometry_type(source, name: str) -> str | None:
    """`read_info`'s `geometry_type` of the layer `name` of an open source.

    A frame of its own, as `read` is for `read_info`, so that the read's warnings point at the
    call of `list_layers`.
    """
    meta, _, geometry, _ = _read_source(source, name, [], True, 0, None, False)
    return _geometry_type(meta, _present(geometry))


def write_dataframe(
    df,
    path,
    layer=None,
    driver=None,
    encoding=None,
    geometry_type=None,
    promote_to_multi=None,
    nan_as_null=True,
    append=False,
    use_arrow=None,
    dataset_metadata=None,
    layer_metadata=None,
    metadata=None,
    dataset_options=None,
    layer_options=None,
    **kwargs,
):
    """Writes a (Geo)DataFrame as GeoJSON, as pyogrio and GDAL write it."""
    from terrio.pyogrio._write import write_geojson

    _unsupported(
        encoding=encoding,
        geometry_type=geometry_type,
        promote_to_multi=promote_to_multi,
        append=append,
        dataset_metadata=dataset_metadata,
        layer_metadata=layer_metadata,
        metadata=metadata,
        dataset_options=dataset_options,
        layer_options=layer_options,
        **kwargs,
    )
    if not nan_as_null:
        raise NotImplementedError("terrio.pyogrio does not support 'nan_as_null=False'")
    if hasattr(path, "write") or not isinstance(path, (str, os.PathLike)):
        raise NotImplementedError("terrio.pyogrio writes to paths only")
    path = os.fspath(path)
    if driver is None:
        driver = detect_write_driver(path)
    if driver != "GeoJSON":
        raise DataSourceError(f"terrio.pyogrio cannot write the {driver} driver")
    allowed = _terrio.allowed_drivers()
    if allowed is not None and "GEOJSON" not in allowed:
        raise DataSourceError(
            "Could not obtain driver: GeoJSON (it is not among the allowed drivers)"
        )
    if not df.columns.is_unique:
        raise ValueError("Cannot write a DataFrame with duplicate column names")
    write_geojson(df, path, layer)


def vsi_listtree(*args, **kwargs):
    """Not provided."""
    raise NotImplementedError("terrio.pyogrio has no virtual file systems")


def vsi_rmtree(*args, **kwargs):
    """Not provided."""
    raise NotImplementedError("terrio.pyogrio has no virtual file systems")


def vsi_unlink(*args, **kwargs):
    """Not provided."""
    raise NotImplementedError("terrio.pyogrio has no virtual file systems")


def vsi_curl_clear_cache(*args, **kwargs):
    """Nothing to clear: terrio keeps no `/vsicurl/` cache here."""
    return None


def _is_geoparquet(path) -> bool:
    """Whether `path` names a GeoParquet file, by its extension."""
    if not isinstance(path, (str, os.PathLike)):
        return False
    return os.fspath(path).split("?", 1)[0].lower().endswith((".parquet", ".geoparquet"))


@contextlib.contextmanager
def open_arrow(
    path_or_buffer,
    layer=None,
    encoding=None,
    columns=None,
    read_geometry=True,
    force_2d=False,
    skip_features=0,
    max_features=None,
    where=None,
    bbox=None,
    mask=None,
    fids=None,
    sql=None,
    sql_dialect=None,
    return_fids=False,
    batch_size=65536,
    use_pyarrow=False,
    **kwargs,
):
    """A GeoParquet file's Arrow stream, as pyogrio's `open_arrow` gives GDAL's.

    A context manager of `(meta, reader)`, the batches decoded as they are asked for (see
    `terrio.open_stream`). The columns keep the file's Arrow types, the covering column is left
    out, and `meta` is GDAL 3.13's: the fields as OGR has them (structs flattened into
    `name.member`), the declared geometry type and the CRS. Other formats are not provided.
    """
    import pyarrow as pa

    if not _is_geoparquet(path_or_buffer):
        raise NotImplementedError("terrio.pyogrio reads Arrow streams from GeoParquet files only")
    _unsupported(
        encoding=encoding,
        where=where,
        mask=mask,
        fids=fids,
        sql=sql,
        sql_dialect=sql_dialect,
        force_2d=force_2d,
        return_fids=return_fids,
        **kwargs,
    )
    if skip_features or max_features:
        raise ValueError("specifying 'max_features' is not supported for Arrow")
    with _scope():
        stream = _terrio.open_stream(
            os.fspath(path_or_buffer), layer, batch_size=batch_size, bbox=bbox, columns=columns
        )
        meta = {
            "crs": stream.crs,
            "encoding": "UTF-8",
            "fields": np.array(list(stream.fields), dtype=object),
            "geometry_type": stream.geometry_type,
            "geometry_name": "",
            "fid_column": "OGC_FID",
        }
        reader = pa.RecordBatchReader.from_stream(stream)
    geometry = [
        f.name
        for f in reader.schema
        if (f.metadata or {}).get(b"ARROW:extension:name", b"").startswith(b"geoarrow.")
        or getattr(f.type, "extension_name", "").startswith("geoarrow.")
    ]
    meta["geometry_name"] = geometry[0] if geometry else ""
    if not read_geometry and geometry:
        schema = pa.schema(
            [f for f in reader.schema if f.name not in geometry], reader.schema.metadata
        )
        batches = (b.drop_columns(geometry) for b in reader)
        reader = pa.RecordBatchReader.from_batches(schema, batches)
    yield meta, reader


def read_arrow(
    path_or_buffer,
    /,
    layer=None,
    encoding=None,
    columns=None,
    read_geometry=True,
    force_2d=False,
    skip_features=0,
    max_features=None,
    where=None,
    bbox=None,
    mask=None,
    fids=None,
    sql=None,
    sql_dialect=None,
    return_fids=False,
    **kwargs,
):
    """A GeoParquet file as pyogrio's `read_arrow` reads it through GDAL's Arrow stream.

    Returns `(meta, pyarrow.Table)` (see `open_arrow`), `skip_features` and `max_features`
    applied after `bbox`. Other formats are not provided.
    """
    with open_arrow(
        path_or_buffer,
        layer=layer,
        encoding=encoding,
        columns=columns,
        read_geometry=read_geometry,
        force_2d=force_2d,
        where=where,
        bbox=bbox,
        mask=mask,
        fids=fids,
        sql=sql,
        sql_dialect=sql_dialect,
        return_fids=return_fids,
        **kwargs,
    ) as (meta, reader):
        table = reader.read_all()
    if skip_features or max_features is not None:
        table = table.slice(skip_features, max_features)
    return meta, table


def write_arrow(*args, **kwargs):
    """Not provided."""
    raise NotImplementedError("terrio.pyogrio does not support Arrow writes")


__all__ = [
    "__gdal_geos_version__",
    "__gdal_version__",
    "__gdal_version_string__",
    "__version__",
    "detect_write_driver",
    "errors",
    "get_gdal_config_option",
    "get_gdal_data_path",
    "list_drivers",
    "list_drivers_details",
    "list_layers",
    "open_arrow",
    "read_arrow",
    "read_bounds",
    "read_dataframe",
    "read_info",
    "set_gdal_config_options",
    "vsi_curl_clear_cache",
    "vsi_listtree",
    "vsi_rmtree",
    "vsi_unlink",
    "write_arrow",
    "write_dataframe",
]
