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

Not provided (they raise `NotImplementedError`): `where`, `sql`, `fids`, `encoding` (set the
`SHAPE_ENCODING` option instead), dataset open options, `use_arrow=True` results (the regular
path is used), and writing any driver but GeoJSON.
"""

from __future__ import annotations

import json
import os
import warnings

import numpy as np

from terrio import _terrio
from terrio.pyogrio import errors
from terrio.pyogrio._read import read as _read
from terrio.pyogrio.errors import DataSourceError

__version__ = "0.13.0"
"""The pyogrio version whose behaviour this module reproduces."""

__gdal_version__ = (3, 12, 4)
"""The GDAL version pyogrio 0.13.0 bundles, whose behaviour this module reproduces."""

__gdal_version_string__ = "3.12.4"
__gdal_geos_version__ = None
__terrio_version__ = _terrio.__version__

_DRIVERS = {"GeoJSON": "rw", "ESRI Shapefile": "r"}
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


def _parse_datetime(ser, datetime_as_string: bool, mixed_offsets_as_utc: bool):
    """Date and date-time strings to datetime64[ms], as pyogrio 0.13 parses them with pandas."""
    import datetime

    import pandas as pd

    if datetime_as_string:
        return ser
    warning = "Error parsing datetimes, original strings are returned: {message}"
    try:
        with warnings.catch_warnings():
            warnings.filterwarnings(
                "ignore", ".*parsing datetimes with mixed time zones will raise.*", FutureWarning
            )
            res = pd.to_datetime(ser, format="ISO8601")
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
                    res = ser.map(datetime.datetime.fromisoformat, na_action="ignore")
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

    geoms = (
        shapely.from_wkb(geometry, on_invalid="ignore")
        if geometry is not None
        else np.array([], dtype=object)
    )
    present = geoms[~shapely.is_missing(geoms)] if len(geoms) else geoms
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


def read_bounds(*args, **kwargs):
    """Not provided."""
    raise NotImplementedError("terrio.pyogrio does not support read_bounds")


def list_layers(path_or_buffer, /):
    """The layers of a dataset and their geometry types, as an array of `[name, type]` rows."""
    info = read_info(path_or_buffer)
    return np.array([[info["layer_name"], info["geometry_type"]]], dtype=object)


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


def read_arrow(*args, **kwargs):
    """Not provided."""
    raise NotImplementedError("terrio.pyogrio does not support Arrow reads")


def open_arrow(*args, **kwargs):
    """Not provided."""
    raise NotImplementedError("terrio.pyogrio does not support Arrow reads")


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
