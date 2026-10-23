"""Rasterizing geometries and polygonizing rasters, with rasterio's functions.

The work is done by terrio's `rasterize` and `polygonize`, which follow GDAL's algorithms.
"""

from __future__ import annotations

import math
import warnings
from collections.abc import Iterable, Iterator
from typing import Any

import numpy as np
from affine import Affine

from terrio import _terrio
from terrio.rio import windows
from terrio.rio.dtypes import (
    float32,
    float64,
    int8,
    int16,
    int32,
    int64,
    uint8,
    uint16,
    uint32,
    uint64,
)
from terrio.rio.enums import MergeAlg
from terrio.rio.errors import RasterioDeprecationWarning, ShapeSkipWarning, WindowError
from terrio.rio.transform import IDENTITY, compose, guard_transform

_VALID_DTYPES = (int8, int16, int32, int64, uint8, uint16, uint32, uint64, float32, float64)
_PIXELS = (0.0, 1.0, 0.0, 0.0, 0.0, -1.0)


def _geo_interface(geom: Any) -> Any:
    """A geometry's GeoJSON mapping.

    Args:
        geom: A GeoJSON-like mapping or an object with `__geo_interface__`.

    Returns:
        dict: The mapping.
    """
    return getattr(geom, "__geo_interface__", None) or geom


def is_valid_geom(geom: Any) -> bool:
    """Whether a geometry is a valid GeoJSON geometry for rasterizing.

    Args:
        geom: A GeoJSON-like geometry.

    Returns:
        bool: True when valid.
    """
    types = {
        "Point",
        "MultiPoint",
        "LineString",
        "LinearRing",
        "MultiLineString",
        "Polygon",
        "MultiPolygon",
    }
    geom = _geo_interface(geom)
    try:
        t = geom["type"]
    except (KeyError, TypeError):
        return False
    if t == "GeometryCollection":
        gs = geom.get("geometries")
        return bool(gs) and all(is_valid_geom(g) for g in gs)
    if t not in types or "coordinates" not in geom:
        return False
    c = geom["coordinates"]
    try:
        if t == "Point":
            return len(c) >= 2
        if t == "MultiPoint":
            return len(c) > 0 and len(c[0]) >= 2
        if t == "LineString":
            return len(c) >= 2 and len(c[0]) >= 2
        if t == "LinearRing":
            return len(c) >= 4 and len(c[0]) >= 2
        if t == "MultiLineString":
            return len(c) > 0 and len(c[0]) >= 2 and len(c[0][0]) >= 2
        if t == "Polygon":
            return len(c) > 0 and len(c[0]) >= 4 and len(c[0][0]) >= 2
        return len(c) > 0 and len(c[0]) > 0 and len(c[0][0]) >= 4 and len(c[0][0][0]) >= 2
    except TypeError:
        return False


def _pixel_geometry(geom: dict[str, Any], inv: Affine) -> dict[str, Any]:
    """A geometry in pixel space, with rows pointing down as `-y`.

    Args:
        geom: A GeoJSON geometry.
        inv: The inverse of the grid's transform.

    Returns:
        dict: The geometry, coordinates `(col, -row)`.
    """

    def tr(c: Any) -> Any:
        if c and isinstance(c[0], (int, float)):
            x, y = c[0], c[1]
            col = inv.a * x + inv.b * y + inv.c
            row = inv.d * x + inv.e * y + inv.f
            return [col, -row]
        return [tr(p) for p in c]

    if geom["type"] == "GeometryCollection":
        return {
            "type": "GeometryCollection",
            "geometries": [_pixel_geometry(g, inv) for g in geom["geometries"]],
        }
    out = {"type": geom["type"], "coordinates": tr(geom["coordinates"])}
    if out["type"] == "LinearRing":
        out["type"] = "LineString"
    return out


def _burn(
    geoms: list[dict[str, Any]],
    values: list[Any],
    shape: tuple[int, int],
    transform: Affine,
    all_touched: bool,
    add: bool,
) -> np.ndarray:
    """Burns geometries with terrio, in pixel space.

    Args:
        geoms: GeoJSON geometries.
        values: The value of each geometry (feature numbers for `add=False`).
        shape: The output `(rows, cols)`.
        transform: The grid's transform.
        all_touched: Burn every pixel a geometry touches.
        add: Add values instead of replacing them.

    Returns:
        numpy.ndarray: The burnt values, float64 (for `add`) or int64.
    """
    h, w = shape
    north_up = transform.b == 0 and transform.d == 0 and transform.a > 0 and transform.e < 0
    if north_up:
        # Map coordinates on the exact grid of the transform.
        grid = transform.to_gdal()
        feats = [
            {"type": "Feature", "properties": {"v": v}, "geometry": g}
            for g, v in zip(geoms, values, strict=False)
        ]
    else:
        # Other transforms: geometries in pixel space, rows pointing down as -y.
        grid = _PIXELS
        inv = ~transform
        feats = [
            {"type": "Feature", "properties": {"v": v}, "geometry": _pixel_geometry(g, inv)}
            for g, v in zip(geoms, values, strict=False)
        ]
    like = _terrio.Raster.from_array(np.zeros((h, w), np.uint8), grid)
    r = _terrio.rasterize(
        {"type": "FeatureCollection", "features": feats},
        attribute="v",
        like=like,
        ot="Float64" if add else "Int64",
        init=0,
        all_touched=all_touched,
        add=add,
    )
    return np.asarray(r.read(1))


def rasterize(
    shapes: Iterable[Any],
    out_shape: Any = None,
    fill: Any = 0,
    nodata: Any = None,
    masked: bool = False,
    out: np.ndarray | None = None,
    transform: Affine = IDENTITY,
    all_touched: bool = False,
    merge_alg: MergeAlg = MergeAlg.replace,
    default_value: Any = 1,
    dtype: Any = None,
    skip_invalid: bool = True,
    dst_path: Any = None,
    dst_kwds: dict[str, Any] | None = None,
) -> Any:
    """Burns geometries into an array.

    Args:
        shapes: Geometries, or `(geometry, value)` pairs. Geometries are GeoJSON-like mappings
            or objects with `__geo_interface__`.
        out_shape: The output shape `(rows, cols)`, when `out` is not given.
        fill: The value of pixels no geometry covers.
        nodata: The nodata value of a written dataset, and the masked value with `masked`.
        masked: Return a masked array, masking `nodata`.
        out: An array to burn into (its values are kept where no geometry falls).
        transform: The output grid's transform.
        all_touched: Burn every pixel a geometry touches, not only those whose centre it
            covers.
        merge_alg: `MergeAlg.replace` (later geometries win) or `MergeAlg.add`.
        default_value: The value of geometries given without one.
        dtype: The output data type; by default, that of the values.
        skip_invalid: Skip invalid geometries (with a warning) instead of failing.
        dst_path: A path (or writer) to write the result to.
        dst_kwds: Creation options for `dst_path`.

    Returns:
        numpy.ndarray: The array (None when written to `dst_path`).

    Raises:
        ValueError: If the data type, the shape or a geometry is not valid.
    """
    if out is not None:
        dtype = out.dtype.name
    elif dtype is not None:
        dtype = np.dtype(dtype).name
    if dtype is not None and dtype not in _VALID_DTYPES:
        raise ValueError("Data type specified by out array or dtype parameter is not supported.")
    geoms: list[dict[str, Any]] = []
    values: list[Any] = []
    for index, item in enumerate(shapes):
        if isinstance(item, (tuple, list)):
            geom, value = item
            if value is None:
                value = fill
        else:
            geom, value = item, default_value
        geom = _geo_interface(geom)
        if not is_valid_geom(geom):
            if skip_invalid:
                warnings.warn(
                    f"Invalid or empty shape {geom} at index {index} will not be rasterized.",
                    ShapeSkipWarning,
                    stacklevel=2,
                )
                continue
            raise ValueError(f"Invalid geometry object at index {index}")
        geoms.append(geom)
        values.append(value)
    if not dtype and values:
        dtype = np.array(values).dtype.name
    if dtype is None:
        dtype = "float64" if isinstance(fill, float) else "uint8"
    writer = None
    if dst_path is not None:
        from terrio.rio import open as rio_open
        from terrio.rio.io import DatasetWriter

        if isinstance(dst_path, DatasetWriter):
            writer = dst_path
            out = np.full((dst_path.height, dst_path.width), fill, dtype=dtype)
        else:
            profile = dict(**(dst_kwds or {}))
            if nodata is not None:
                profile["nodata"] = nodata
            profile["dtype"] = dtype
            writer = rio_open(dst_path, "w", **profile)
            out = np.full((writer.height, writer.width), fill, dtype=dtype)
    elif out is None:
        if out_shape is None:
            raise ValueError("Either an out_shape or image must be provided")
        if len(out_shape) != 2:
            raise ValueError("Invalid out_shape, must be 2D")
        out = np.empty(out_shape, dtype=dtype)
        out.fill(fill)
    if min(out.shape) == 0:
        raise ValueError("width and height must be > 0")
    transform = guard_transform(transform)
    if geoms:
        shape = (out.shape[-2], out.shape[-1])
        if merge_alg == MergeAlg.add:
            sums = _burn(geoms, [float(v) for v in values], shape, transform, all_touched, True)
            touched = (
                _burn(geoms, list(range(1, len(geoms) + 1)), shape, transform, all_touched, False)
                > 0
            )
            out[touched] = (out[touched].astype(np.float64) + sums[touched]).astype(out.dtype)
        else:
            idx = _burn(geoms, list(range(1, len(geoms) + 1)), shape, transform, all_touched, False)
            hit = idx > 0
            vals = np.array(values, dtype=object if out.dtype.kind in "iu" else np.float64)
            burnt = (
                np.array([vals[i - 1] for i in idx[hit]], dtype=np.float64)
                if out.dtype.kind == "f"
                else np.array([vals[i - 1] for i in idx[hit]])
            )
            out[hit] = burnt.astype(out.dtype) if burnt.size else burnt
    if writer is not None:
        writer.write(out, 1)
        if not isinstance(dst_path, type(writer)) or dst_path is not writer:
            writer.close()
        return None
    if masked and not hasattr(out, "mask"):
        return np.ma.masked_equal(out, nodata, copy=False)
    return out


def geometry_mask(
    geometries: Iterable[Any],
    out_shape: tuple[int, int],
    transform: Affine,
    all_touched: bool = False,
    invert: bool = False,
) -> np.ndarray:
    """A mask of geometries.

    Args:
        geometries: GeoJSON-like geometries.
        out_shape: The mask's shape `(rows, cols)`.
        transform: The grid's transform.
        all_touched: Include every pixel a geometry touches.
        invert: True inside the geometries (by default, True outside).

    Returns:
        numpy.ndarray: The boolean mask.
    """
    fill, value = (0, 1) if invert else (1, 0)
    arr = rasterize(
        geometries,
        out_shape=out_shape,
        transform=transform,
        all_touched=all_touched,
        fill=fill,
        default_value=value,
        dtype=uint8,
    )
    mask: np.ndarray = arr.view(bool)
    return mask


def shapes(
    source: Any, mask: Any = None, connectivity: int = 4, transform: Affine = IDENTITY
) -> Iterator[tuple[dict[str, Any], Any]]:
    """The polygons of connected pixels of equal value.

    Args:
        source: A 2-D array (int16, int32, uint8, uint16 or float32), or a masked array.
        mask: Pixels to include (True or non-zero); by default all (or the masked array's
            unmasked pixels).
        connectivity: 4 or 8.
        transform: The array's transform.

    Yields:
        tuple: `(geometry, value)`, the geometry a GeoJSON Polygon mapping.

    Raises:
        ValueError: If the connectivity or the data type is not valid.
    """
    if hasattr(source, "mask") and mask is None:
        mask = ~source.mask
        source = source.data
    transform = guard_transform(transform)
    if connectivity not in (4, 8):
        raise ValueError("Connectivity Option must be 4 or 8")
    arr = np.asarray(source)
    if arr.ndim == 3 and arr.shape[0] == 1:
        arr = arr[0]
    if arr.dtype.name not in (
        "int8",
        "int16",
        "int32",
        "uint8",
        "uint16",
        "uint32",
        "float32",
        "float64",
        "int64",
        "uint64",
    ):
        raise ValueError(
            "image dtype must be one of: int16, int32, uint8, uint16, float32, float64; "
            f"got {arr.dtype}"
        )
    src = _terrio.Raster.from_array(np.ascontiguousarray(arr), _PIXELS)
    m = None
    if mask is not None:
        marr = np.asarray(mask)
        if marr.shape != arr.shape:
            raise ValueError("Mask must have same shape as image")
        m = _terrio.Raster.from_array(np.ascontiguousarray(marr.astype(np.uint8)), _PIXELS)
    layer = _terrio.polygonize(src, connectedness=connectivity, mask=m, nomask=True)
    fc = layer.to_geojson()
    a, b, c, d, e, f = transform.a, transform.b, transform.c, transform.d, transform.e, transform.f

    def pixels(coords: Any) -> Any:
        # (col, -row) from terrio to (col, row), without repeated vertices.
        if coords and isinstance(coords[0], (int, float)):
            return (float(coords[0]), -float(coords[1]) + 0.0)
        out = [pixels(p) for p in coords]
        if out and isinstance(out[0], tuple):
            out = [p for i, p in enumerate(out) if i == 0 or p != out[i - 1]]
        return out

    def to_map(coords: Any) -> Any:
        if isinstance(coords, tuple):
            col, row = coords
            return (c + col * a + row * b, f + col * d + row * e)
        return [to_map(p) for p in coords]

    polys = []
    for feat in fc["features"]:
        geom = feat["geometry"]
        rings = pixels(geom["coordinates"])
        ext = rings[0]
        top = min(p[1] for p in ext)
        bottom = max(p[1] for p in ext)
        first = min(p[0] for p in ext if p[1] == top)
        polys.append(((bottom, top, first), geom["type"], rings, feat["properties"].get("DN")))
    # GDAL emits polygons as they are completed, scanning rows down: by their last row, then
    # their first pixel.
    polys.sort(key=lambda p: p[0])
    for _, gtype, rings, value in polys:
        yield (
            {"type": gtype, "coordinates": to_map(rings)},
            (None if value is None else float(value)),
        )


def sieve(
    source: Any, size: int, out: Any = None, mask: Any = None, connectivity: int = 4
) -> np.ndarray:
    """Removes connected regions smaller than a size, merging them into their largest neighbour.

    Args:
        source: A 2-D array.
        size: The minimum region size in pixels.
        out: An array to write into.
        mask: Pixels to consider (True or non-zero).
        connectivity: 4 or 8.

    Returns:
        numpy.ndarray: The sieved array.
    """
    arr = np.asarray(source)
    src = _terrio.Raster.from_array(np.ascontiguousarray(arr), _PIXELS)
    m = None
    if mask is not None:
        m = _terrio.Raster.from_array(
            np.ascontiguousarray(np.asarray(mask).astype(np.uint8)), _PIXELS
        )
    r = _terrio.sieve(src, threshold=size, connectedness=connectivity, mask=m)
    res = np.asarray(r.read(1)).astype(arr.dtype)
    if out is not None:
        out[...] = res
        return out  # type: ignore[no-any-return]
    return res


def _coords(geom: dict[str, Any]) -> Iterator[tuple[float, float]]:
    """Every coordinate of a GeoJSON geometry, feature or collection.

    Args:
        geom: The GeoJSON mapping.

    Yields:
        tuple: `(x, y)`.
    """
    if "features" in geom:
        for feat in geom["features"]:
            yield from _coords(feat["geometry"] if "geometry" in feat else feat)
        return
    if "geometry" in geom and "type" in geom and geom["type"] == "Feature":
        yield from _coords(geom["geometry"])
        return
    if "geometries" in geom:
        for g in geom["geometries"]:
            yield from _coords(g)
        return

    def walk(c: Any) -> Iterator[tuple[float, float]]:
        if c and isinstance(c[0], (int, float)):
            yield (c[0], c[1])
        else:
            for p in c:
                yield from walk(p)

    yield from walk(geom["coordinates"])


def bounds(
    geometry: Any, north_up: bool = True, transform: Affine | None = None
) -> tuple[float, float, float, float]:
    """The bounds of a GeoJSON geometry, feature or collection.

    Args:
        geometry: The GeoJSON mapping or an object with `__geo_interface__`.
        north_up: Return `(left, bottom, right, top)` with top > bottom (else top < bottom).
        transform: Apply this transform to the coordinates first.

    Returns:
        tuple: `(left, bottom, right, top)`.

    Raises:
        ValueError: If the input is not GeoJSON.
    """
    geometry = _geo_interface(geometry)
    if "bbox" in geometry and north_up and not transform:
        b = geometry["bbox"]
        return (b[0], b[1], b[2], b[3])
    geom = geometry.get("geometry") or geometry
    if not ("coordinates" in geom or "geometries" in geom or "features" in geom):
        raise ValueError(
            "geometry must be a GeoJSON-like geometry, GeometryCollection, or FeatureCollection"
        )
    xs, ys = [], []
    for x, y in _coords(geom):
        if transform is not None:
            x, y = (
                transform.c + x * transform.a + y * transform.b,
                transform.f + x * transform.d + y * transform.e,
            )
        xs.append(x)
        ys.append(y)
    if north_up:
        return min(xs), min(ys), max(xs), max(ys)
    return min(xs), max(ys), max(xs), min(ys)


def geometry_window(
    dataset: Any,
    shapes: Iterable[Any],
    pad_x: float = 0,
    pad_y: float = 0,
    north_up: Any = None,
    rotated: Any = None,
    pixel_precision: Any = None,
    boundless: bool = False,
) -> windows.Window:
    """The window of a dataset covering geometries.

    Args:
        dataset: The dataset.
        shapes: GeoJSON-like geometries in the dataset's CRS.
        pad_x: Columns to add on each side.
        pad_y: Rows to add on each side.
        north_up: Unused, deprecated.
        rotated: Unused, deprecated.
        pixel_precision: Unused, deprecated.
        boundless: Do not crop the window to the dataset.

    Returns:
        Window: The window.
    """
    for name, v in (
        ("north_up", north_up),
        ("rotated", rotated),
        ("pixel_precision", pixel_precision),
    ):
        if v is not None:
            warnings.warn(
                f"The {name} parameter is unused, deprecated, and will be removed in the future.",
                RasterioDeprecationWarning,
                stacklevel=2,
            )
    wins = []
    for shape in shapes:
        b = bounds(shape)
        try:
            w = windows.from_bounds(*b, transform=dataset.transform)
        except WindowError:
            w = windows.from_bounds(*bounds(shape, north_up=False), transform=dataset.transform)
        col_off = math.floor(w.col_off - pad_x)
        row_off = math.floor(w.row_off - pad_y)
        width = math.ceil(w.col_off + w.width + pad_x) - col_off
        height = math.ceil(w.row_off + w.height + pad_y) - row_off
        wins.append(windows.Window(col_off, row_off, width, height))
    win: windows.Window = windows.union(*wins)
    if not boundless:
        win = win.intersection(windows.Window(0, 0, dataset.width, dataset.height))
    return win


def dataset_features(
    src: Any,
    bidx: int | None = None,
    sampling: int = 1,
    band: bool = True,
    as_mask: bool = False,
    with_nodata: bool = False,
    geographic: bool = True,
    precision: int = -1,
) -> Iterator[dict[str, Any]]:
    """GeoJSON features of a dataset's regions of equal value (or valid data).

    Args:
        src: The dataset.
        bidx: The band.
        sampling: The inverse of the sampling fraction: 10 decimates by 10.
        band: Polygonize band values (else the mask).
        as_mask: Polygonize non-zero values as one value.
        with_nodata: Include nodata regions.
        geographic: Output longitude/latitude.
        precision: Decimal places of the output coordinates (-1 to keep them).

    Yields:
        dict: GeoJSON features.
    """
    import os

    from terrio.rio.warp import transform_geom

    if bidx is not None and bidx > src.count:
        raise ValueError("bidx is out of range for raster")
    msk = None
    transform = src.transform
    shape = (src.height, src.width)
    if sampling > 1:
        # Decimate, and correct the georeferencing shift of the decimation, as rasterio does.
        shape = (int(math.ceil(src.height / sampling)), int(math.ceil(src.width / sampling)))
        x_sampling = src.width / shape[1]
        y_sampling = src.height / shape[0]
        transform = compose(
            transform, Affine.translation(src.width % x_sampling, src.height % y_sampling)
        )
        transform = compose(transform, Affine.scale(x_sampling, y_sampling))
    if not band or (band and not as_mask and not with_nodata):
        if sampling == 1:
            msk = src.read_masks(bidx)
        else:
            msk_shape = shape if bidx is not None else (src.count, *shape)
            msk = src.read_masks(bidx, out_shape=msk_shape)
        if bidx is None:
            msk = np.logical_or.reduce(msk).astype(np.uint8)
        img = msk
    if band:
        if sampling == 1:
            img = src.read(bidx, masked=False)
        else:
            img = src.read(bidx, out_shape=shape, masked=False)
    if as_mask:
        tmp = np.ones_like(img, np.uint8) * 255
        tmp[img == 0] = 0
        img = tmp
        if not with_nodata:
            msk = tmp
    kwargs: dict[str, Any] = {"transform": transform}
    if not with_nodata:
        kwargs["mask"] = msk
    base = os.path.basename(src.name)
    for i, (g, val) in enumerate(shapes(img, **kwargs)):
        if geographic:
            g = transform_geom(src.crs, "EPSG:4326", g, precision=precision)
        xs, ys = zip(*_coords(g), strict=False)
        yield {
            "type": "Feature",
            "id": f"{base}:{i}",
            "properties": {"val": val, "filename": base},
            "bbox": [min(xs), min(ys), max(xs), max(ys)],
            "geometry": g,
        }


__all__ = [
    "bounds",
    "dataset_features",
    "geometry_mask",
    "geometry_window",
    "is_valid_geom",
    "rasterize",
    "shapes",
    "sieve",
]
