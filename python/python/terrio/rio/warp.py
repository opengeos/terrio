# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/warp.py
#     rasterio/_features.pyx

"""Reprojection: coordinates, geometries, bounds and rasters, with rasterio's functions.

Rasters are warped by terrio's `warp`, which follows GDAL's warper (the default output grid of
`GDALSuggestedWarpOutput2()`, the approximate transformer and GDAL's resampling kernels).
"""

from __future__ import annotations

import warnings
from collections.abc import Mapping
from math import ceil, floor
from typing import Any

import numpy as np
from affine import Affine

from terrio import _terrio
from terrio.rio.crs import CRS
from terrio.rio.dtypes import _gdal_typename
from terrio.rio.enums import Resampling
from terrio.rio.errors import CRSError, RasterioDeprecationWarning, RPCError, TransformError
from terrio.rio.transform import array_bounds, from_bounds

SUPPORTED_RESAMPLING = [
    Resampling.nearest,
    Resampling.bilinear,
    Resampling.cubic,
    Resampling.cubic_spline,
    Resampling.lanczos,
    Resampling.average,
    Resampling.mode,
    Resampling.max,
    Resampling.min,
    Resampling.med,
    Resampling.q1,
    Resampling.q3,
    Resampling.sum,
    Resampling.rms,
]

_WARP_NAMES = {
    Resampling.nearest: "near",
    Resampling.bilinear: "bilinear",
    Resampling.cubic: "cubic",
    Resampling.cubic_spline: "cubicspline",
    Resampling.lanczos: "lanczos",
    Resampling.average: "average",
    Resampling.mode: "mode",
    Resampling.max: "max",
    Resampling.min: "min",
    Resampling.med: "med",
    Resampling.q1: "q1",
    Resampling.q3: "q3",
    Resampling.sum: "sum",
    Resampling.rms: "rms",
}


def _crs_text(crs: Any) -> str:
    """A CRS definition terrio parses.

    Args:
        crs: Anything `CRS.from_user_input` accepts.

    Returns:
        str: `EPSG:n` or WKT.

    Raises:
        CRSError: If the CRS is missing or invalid.
    """
    if crs is None:
        raise CRSError("A CRS is required")
    c = CRS.from_user_input(crs)
    code = c.to_epsg()
    return f"EPSG:{code}" if code is not None else c.to_wkt()


def _transform_arrays(
    src: str, dst: str, xs: np.ndarray, ys: np.ndarray
) -> tuple[np.ndarray, np.ndarray]:
    """Transforms coordinate arrays; points that fail become infinite, as in GDAL.

    Args:
        src: The source CRS.
        dst: The target CRS.
        xs: The x coordinates.
        ys: The y coordinates.

    Returns:
        tuple: The transformed coordinates.
    """
    try:
        return _terrio.transform_points(xs, ys, src, dst)
    except Exception:
        ox = np.full(len(xs), np.inf)
        oy = np.full(len(ys), np.inf)
        for i, (x, y) in enumerate(zip(xs, ys, strict=False)):
            try:
                a, b = _terrio.transform_points([x], [y], src, dst)
                ox[i], oy[i] = a[0], b[0]
            except Exception:
                pass
        return ox, oy


def transform(src_crs: Any, dst_crs: Any, xs: Any, ys: Any, zs: Any = None) -> Any:
    """Transforms coordinates from one CRS to another.

    Args:
        src_crs: The source CRS.
        dst_crs: The target CRS.
        xs: The x coordinates (longitudes for geographic CRSs).
        ys: The y coordinates.
        zs: Heights, kept unchanged.

    Returns:
        tuple: Lists `(xs, ys)`, or `(xs, ys, zs)` with heights.

    Raises:
        TransformError: If the arrays differ in length.
    """
    if len(xs) != len(ys):
        raise TransformError("xs and ys arrays must be the same length")
    if zs is not None and len(xs) != len(zs):
        raise TransformError("zs, xs, and ys arrays must be the same length")
    if len(xs) == 0:
        return ([], [], []) if zs is not None else ([], [])
    ox, oy = _transform_arrays(
        _crs_text(src_crs), _crs_text(dst_crs), np.asarray(xs, float), np.asarray(ys, float)
    )
    if zs is not None:
        return ox.tolist(), oy.tolist(), [float(z) for z in zs]
    return ox.tolist(), oy.tolist()


def transform_geom(
    src_crs: Any,
    dst_crs: Any,
    geom: Any,
    antimeridian_cutting: Any = None,
    antimeridian_offset: Any = None,
    precision: int = -1,
) -> Any:
    """Transforms GeoJSON geometries from one CRS to another.

    Args:
        src_crs: The source CRS.
        dst_crs: The target CRS.
        geom: A GeoJSON geometry (or an object with `__geo_interface__`), or a sequence of them.
        antimeridian_cutting: Unused, deprecated.
        antimeridian_offset: Unused, deprecated.
        precision: Round coordinates to this many decimal places (-1 to keep them).

    Returns:
        dict | list: The transformed geometry, or geometries, built as rasterio builds them
        from GDAL's (`GeomBuilder`): a new dict of `type` and `coordinates` (or `geometries`),
        positions as tuples in lists (a Point's coordinates a tuple), every position of a
        geometry with a height when one has it (0 where it had none), rings closed. With a
        precision, rasterio's `recursive_round` makes every sequence a list.

    Raises:
        ValueError: For a geometry without coordinates (or members), or of an unsupported type.
    """
    if antimeridian_cutting is not None or antimeridian_offset is not None:
        warnings.warn(
            "The antimeridian_cutting and antimeridian_offset parameters are "
            "deprecated and no longer have any effect.",
            RasterioDeprecationWarning,
            stacklevel=2,
        )
    src, dst = _crs_text(src_crs), _crs_text(dst_crs)
    if isinstance(geom, Mapping) or hasattr(geom, "__geo_interface__"):
        return _transform_one_geom(src, dst, geom, precision)
    return [_transform_one_geom(src, dst, g, precision) for g in geom]


# GeoJSON geometry types with coordinates, and how deep each nests them down to a position.
_GEOM_DEPTH = {
    "Point": 0,
    "MultiPoint": 1,
    "LineString": 1,
    "LinearRing": 1,
    "MultiLineString": 2,
    "Polygon": 2,
    "MultiPolygon": 3,
}


def _ogr_geometry(geom: Any) -> tuple[dict[str, Any], bool]:
    """A geometry as rasterio's `OGRGeomBuilder` builds it for GDAL, as plain GeoJSON.

    Positions become lists `[x, y, z]`, `z` None where none was given; rings are closed as
    `OGR_G_CloseRings()` closes them (a missing height compares as 0).

    Args:
        geom: A GeoJSON geometry or an object with `__geo_interface__`.

    Returns:
        tuple: The geometry (`type` and `coordinates`, or `geometries`), and whether a position
        anywhere in it has a height, which makes the whole geometry 3-D in GDAL
        (`HomogenizeDimensionalityWith()`).

    Raises:
        ValueError: As `OGRGeomBuilder.build()`: no coordinates or members, or another type.
    """
    has_z = False

    def pos(c: Any) -> list[Any]:
        nonlocal has_z
        if len(c) == 2:
            x, y = c
            return [x, y, None]
        x, y, z = c[:3]
        has_z = True
        return [x, y, z]

    def ring(c: Any) -> list[Any]:
        pts = [pos(p) for p in c]
        if len(pts) >= 2:
            (x0, y0, z0), (x1, y1, z1) = pts[0], pts[-1]
            if x0 != x1 or y0 != y1 or (z0 or 0.0) != (z1 or 0.0):
                pts.append(list(pts[0]))
        return pts

    def build(g: Any) -> dict[str, Any]:
        g = getattr(g, "__geo_interface__", g)
        kind = g["type"]
        if kind in _GEOM_DEPTH:
            c = g.get("coordinates")
            if not (c and len(c) > 0):
                raise ValueError("Input is not a valid geometry object")
            if kind == "Point":
                coords: Any = pos(c)
            elif kind in ("MultiPoint", "LineString"):
                coords = [pos(p) for p in c]
            elif kind == "LinearRing":
                coords = ring(c)
            elif kind == "MultiLineString":
                coords = [[pos(p) for p in line] for line in c]
            elif kind == "Polygon":
                coords = [ring(r) for r in c]
            else:
                coords = [[ring(r) for r in poly] for poly in c]
            return {"type": kind, "coordinates": coords}
        if kind == "GeometryCollection":
            members = g.get("geometries")
            if not (members and len(members) > 0):
                raise ValueError("Input is not a valid geometry object")
            return {"type": kind, "geometries": [build(m) for m in members]}
        raise ValueError(f"Unsupported geometry type {kind}")

    out = build(geom)
    return out, has_z


def _geom_positions(g: dict[str, Any]) -> list[list[Any]]:
    """The positions of a geometry `_ogr_geometry` built, in order.

    Args:
        g: The geometry.

    Returns:
        list: The position lists themselves (not copies).
    """
    if "geometries" in g:
        return [p for m in g["geometries"] for p in _geom_positions(m)]
    out: list[list[Any]] = []

    def walk(c: Any, depth: int) -> None:
        if depth == 0:
            out.append(c)
        else:
            for p in c:
                walk(p, depth - 1)

    walk(g["coordinates"], _GEOM_DEPTH[g["type"]])
    return out


def _transform_one_geom(src: str, dst: str, geom: Any, precision: int) -> dict[str, Any]:
    """Transforms one geometry and builds it as rasterio's `GeomBuilder` does.

    Args:
        src: The source CRS.
        dst: The target CRS.
        geom: A GeoJSON geometry or an object with `__geo_interface__`.
        precision: Round to this many decimal places when not negative, as rasterio's
            `recursive_round` (lists, heights rounded too).

    Returns:
        dict: The transformed geometry.
    """
    g, has_z = _ogr_geometry(geom)
    pts = _geom_positions(g)
    if pts:
        xs = np.array([p[0] for p in pts], float)
        ys = np.array([p[1] for p in pts], float)
        ox, oy = _transform_arrays(src, dst, xs, ys)
        for p, x, y in zip(pts, ox.tolist(), oy.tolist(), strict=True):
            p[0], p[1] = x, y
            if has_z:
                p[2] = 0.0 if p[2] is None else float(p[2])
            else:
                del p[2]

    def leaf(p: list[Any]) -> Any:
        return [round(v, precision) for v in p] if precision >= 0 else tuple(p)

    def finish(m: dict[str, Any]) -> dict[str, Any]:
        if "geometries" in m:
            return {"type": m["type"], "geometries": [finish(x) for x in m["geometries"]]}

        def walk(c: Any, depth: int) -> Any:
            return leaf(c) if depth == 0 else [walk(p, depth - 1) for p in c]

        # A ring comes back from GDAL as a line string (`OGRLinearRing::getGeometryType()`).
        kind = "LineString" if m["type"] == "LinearRing" else m["type"]
        return {"type": kind, "coordinates": walk(m["coordinates"], _GEOM_DEPTH[m["type"]])}

    return finish(g)


def transform_bounds(
    src_crs: Any,
    dst_crs: Any,
    left: float,
    bottom: float,
    right: float,
    top: float,
    densify_pts: int = 21,
) -> tuple[float, float, float, float]:
    """Transforms bounds, densifying their edges.

    Args:
        src_crs: The source CRS.
        dst_crs: The target CRS.
        left: The left bound.
        bottom: The bottom bound.
        right: The right bound.
        top: The top bound.
        densify_pts: Points added along each edge.

    Returns:
        tuple: `(left, bottom, right, top)` in the target CRS.

    Raises:
        CRSError: If no point can be transformed.
    """
    src, dst = _crs_text(src_crs), _crs_text(dst_crs)
    n = max(densify_pts, 0) + 2
    t = np.linspace(0.0, 1.0, n)
    xs_h = left + (right - left) * t
    ys_v = bottom + (top - bottom) * t
    xs = np.concatenate([xs_h, xs_h, np.full(n, left), np.full(n, right)])
    ys = np.concatenate([np.full(n, bottom), np.full(n, top), ys_v, ys_v])
    ox, oy = _transform_arrays(src, dst, xs, ys)
    ok = np.isfinite(ox) & np.isfinite(oy)
    if not ok.any():
        raise CRSError("Unable to transform bounds")
    return float(ox[ok].min()), float(oy[ok].min()), float(ox[ok].max()), float(oy[ok].max())


def _gcp_args(gcps: Any, kwargs: dict[str, Any]) -> dict[str, Any]:
    """The GCP arguments of terrio's warp: the points and the transformer GDAL's options ask for.

    Args:
        gcps: `GroundControlPoint`s.
        kwargs: Transformer options: `MAX_GCP_ORDER` (the polynomial order; 0, the default, picks
            GDAL's; -1 for thin plate splines) and `SRC_METHOD` (`GCP_POLYNOMIAL`, `GCP_TPS` or
            `GCP_HOMOGRAPHY`; by default, GDAL's choice).

    Returns:
        dict: `gcps`, `order` and `gcp_method` keyword arguments.

    Raises:
        ValueError: If the method is not a GCP one.
    """
    opts = {str(k).upper(): v for k, v in kwargs.items()}
    method = str(opts.get("SRC_METHOD", "AUTO")).upper()
    if method not in ("AUTO", "GCP_POLYNOMIAL", "GCP_TPS", "GCP_HOMOGRAPHY"):
        raise ValueError(
            f"terrio.rio warps GCPs with GCP_POLYNOMIAL, GCP_TPS or GCP_HOMOGRAPHY, not {method}"
        )
    order = int(opts.get("MAX_GCP_ORDER", 0))
    if method == "AUTO" and order < 0:
        method = "GCP_TPS"
    return {
        "gcps": [
            (float(g.col), float(g.row), float(g.x), float(g.y), float(g.z or 0.0)) for g in gcps
        ],
        "order": max(order, 0),
        "gcp_method": method.lower(),
    }


def _rpc_args(rpcs: Any, kwargs: dict[str, Any]) -> dict[str, Any]:
    """The RPC arguments of terrio's warp: GDAL's RPC items and the RPC transformer options.

    Args:
        rpcs: A `terrio.rio.rpc.RPC` (or rasterio's), or a dict of GDAL's RPC items.
        kwargs: GDAL transformer (and warp) options; the `RPC_*` ones are kept, upper-cased as
            rasterio passes them (but the `RPC_DEM` path).

    Returns:
        dict: `rpc` and `to` keyword arguments.
    """
    from terrio.rio.transform import _rpc_items, _rpc_options

    opts = {k: v for k, v in kwargs.items() if str(k).upper().startswith("RPC_")}
    return {"rpc": _rpc_items(rpcs), "to": _rpc_options(opts)}


def _warp_extras(kwargs: dict[str, Any]) -> dict[str, str]:
    """The GDAL warp options terrio's warp takes (`SOURCE_EXTRA`, `SAMPLE_STEPS`).

    Args:
        kwargs: GDAL transformer and warp options.

    Returns:
        dict: The warp options terrio uses.
    """
    out = {}
    for k, v in kwargs.items():
        key = str(k).upper()
        if key in ("SOURCE_EXTRA", "SAMPLE_STEPS"):
            out[key] = str(v).upper()
    return out


def calculate_default_transform(
    src_crs: Any,
    dst_crs: Any,
    width: int,
    height: int,
    left: float | None = None,
    bottom: float | None = None,
    right: float | None = None,
    top: float | None = None,
    gcps: Any = None,
    rpcs: Any = None,
    resolution: Any = None,
    dst_width: int | None = None,
    dst_height: int | None = None,
    src_geoloc_array: Any = None,
    **kwargs: Any,
) -> tuple[Affine, int, int]:
    """The output grid GDAL suggests for reprojecting a raster.

    Args:
        src_crs: The source CRS.
        dst_crs: The target CRS.
        width: The source width.
        height: The source height.
        left: The source's left bound.
        bottom: The source's bottom bound.
        right: The source's right bound.
        top: The source's top bound.
        gcps: Ground control points georeferencing the source instead of bounds.
        rpcs: Rational polynomial coefficients georeferencing the source instead of bounds (an
            `RPC` or GDAL's RPC items); the source CRS is then WGS 84.
        resolution: The output resolution (a number or `(x, y)`).
        dst_width: The output width (with `dst_height`).
        dst_height: The output height.
        src_geoloc_array: Unsupported.
        **kwargs: Transformer options: `MAX_GCP_ORDER`, `SRC_METHOD` (`GCP_POLYNOMIAL`, `GCP_TPS` or
            `GCP_HOMOGRAPHY`) with GCPs; GDAL's `RPC_*` options (`RPC_HEIGHT`, `RPC_DEM`...)
            with RPCs.

    Returns:
        tuple: `(transform, width, height)`.

    Raises:
        ValueError: If the bounds are missing or the options conflict.
    """
    if src_geoloc_array is not None:
        raise ValueError("terrio.rio supports bounds, GCPs and RPCs, not geolocation arrays")
    if sum(p is not None for p in (left, gcps, rpcs)) > 1:
        raise ValueError(
            "Bounds, gcps, rpcs, and src_geoloc_array are mutually exclusive parameters and may "
            "not be used together."
        )
    if rpcs is not None:
        left, bottom, right, top = 0.0, -float(height), float(width), 0.0
    if gcps is not None:
        if any(v is not None for v in (left, bottom, right, top)):
            raise ValueError("Bounding box parameters and gcps are mutually exclusive.")
        left, bottom, right, top = 0.0, -float(height), float(width), 0.0
    if left is None or bottom is None or right is None or top is None:
        if any(v is not None for v in (left, bottom, right, top)):
            raise ValueError("Some, but not all, bounding box parameters were provided.")
        raise ValueError("Bounds, gcps, rpcs, or src_geoloc_array are required.")
    if (dst_width is None) != (dst_height is None):
        raise ValueError("Either dst_width and dst_height must be specified or none of them.")
    dims = (dst_width, dst_height) if dst_width is not None else None
    if resolution and dims:
        raise ValueError("Resolution cannot be used with dst_width and dst_height.")
    t = from_bounds(left, bottom, right, top, width, height)
    gcp_kw = _gcp_args(gcps, kwargs) if gcps is not None else {}
    if rpcs is not None:
        rpc_kw = _rpc_args(rpcs, kwargs)
        gt, w, h = _terrio.suggested_grid(
            int(width),
            int(height),
            t.to_gdal(),
            "EPSG:4326",
            _crs_text(dst_crs),
            rpc=rpc_kw["rpc"],
            rpc_options=rpc_kw["to"],
        )
    else:
        gt, w, h = _terrio.suggested_grid(
            int(width), int(height), t.to_gdal(), _crs_text(src_crs), _crs_text(dst_crs), **gcp_kw
        )
    aff = Affine.from_gdal(*gt)
    out_w, out_h = int(w), int(h)
    if resolution:
        try:
            res = (float(resolution), float(resolution))
        except TypeError:
            res = (resolution[0], resolution[0]) if len(resolution) == 1 else tuple(resolution[:2])
        xres, yres = res[0], -res[1]
        xratio, yratio = aff.a / xres, aff.e / yres
        aff = Affine(xres, aff.b, aff.c, aff.d, yres, aff.f)
        out_w = ceil(out_w * xratio)
        out_h = ceil(out_h * yratio)
    if dims:
        assert dst_width is not None and dst_height is not None
        xratio, yratio = out_w / dst_width, out_h / dst_height
        out_w, out_h = int(dst_width), int(dst_height)
        aff = Affine(aff.a * xratio, aff.b, aff.c, aff.d, aff.e * yratio, aff.f)
    return aff, out_w, out_h


def aligned_target(
    transform: Affine, width: int, height: int, resolution: Any
) -> tuple[Affine, int, int]:
    """A grid aligned on multiples of a resolution, covering a raster.

    Args:
        transform: The raster's transform.
        width: Its width.
        height: Its height.
        resolution: The resolution (a number or `(x, y)`).

    Returns:
        tuple: `(transform, width, height)`.
    """
    res = (
        (float(resolution), float(resolution))
        if isinstance(resolution, (int, float))
        else resolution
    )
    xmin = transform.xoff
    ymin = transform.yoff + height * transform.e
    xmax = transform.xoff + width * transform.a
    ymax = transform.yoff
    xmin = floor(xmin / res[0]) * res[0]
    xmax = ceil(xmax / res[0]) * res[0]
    ymin = floor(ymin / res[1]) * res[1]
    ymax = ceil(ymax / res[1]) * res[1]
    t = Affine(res[0], 0, xmin, 0, -res[1], ymax)
    return t, max(int(ceil((xmax - xmin) / res[0])), 1), max(int(ceil((ymax - ymin) / res[1])), 1)


def _nodata_arg(v: Any) -> Any:
    """A nodata value as terrio's warp takes it.

    Args:
        v: The value or None.

    Returns:
        str | float | None: The value (`"nan"` for NaN).
    """
    if v is None:
        return None
    return "nan" if isinstance(v, float) and np.isnan(v) else float(v)


def _alpha_max(dtype: Any) -> float:
    """GDAL's default `SRC_ALPHA_MAX`/`DST_ALPHA_MAX` for an alpha band of this type.

    Args:
        dtype: The alpha band's data type.

    Returns:
        float: 32767 for Int16, 65535 for UInt16, else 255.
    """
    dt = np.dtype(dtype)
    if dt == np.int16:
        return 32767.0
    if dt == np.uint16:
        return 65535.0
    return 255.0


def _transparent(alpha: np.ndarray) -> np.ndarray:
    """The source pixels GDAL's warper treats as transparent, from a binary alpha band.

    GDAL scales alpha to a density (`alpha / SRC_ALPHA_MAX` in float32, clamped at 1) and
    skips densities below 1e-9. A binary alpha band (0 or at least its maximum) is a validity
    mask: GDAL warps it exactly as a unified nodata mask. Partial transparency blends values
    by density, which terrio's warp kernel does not do, so it is refused rather than
    approximated.

    Args:
        alpha: The source alpha band.

    Returns:
        np.ndarray: True where the source pixel is transparent.

    Raises:
        ValueError: If the alpha band holds partial transparency.
    """
    density = alpha.astype(np.float32) * np.float32(1.0 / _alpha_max(alpha.dtype))
    opaque = density >= np.float32(1.0)
    transparent = density.astype(np.float64) < 1e-9
    if not np.all(opaque | transparent):
        raise ValueError(
            "terrio.rio reprojects binary alpha bands only (0 or the alpha maximum); "
            "partial transparency is not supported"
        )
    return transparent


def _sentinel(src3: np.ndarray, valid: np.ndarray) -> Any:
    """A value no valid source pixel holds in every band, to mark invalid pixels as nodata.

    With unified source nodata a pixel is invalid only when all its bands equal the nodata
    value, so the value only has to differ from every valid pixel whose bands are all equal.

    Args:
        src3: The source bands.
        valid: True where the source pixel is valid.

    Returns:
        The value (NaN for floating-point data when no valid pixel is NaN in every band).

    Raises:
        ValueError: If every value of the data type is taken.
    """
    v = src3[:, valid]
    if src3.dtype.kind == "f":
        if not np.all(np.isnan(v), axis=0).any():
            return float("nan")
        info = np.finfo(src3.dtype)
        used = set(np.unique(v[0][np.all(v == v[0], axis=0)]).tolist())
        for c in (float("-inf"), float("inf"), float(info.min), float(info.max)):
            if c not in used:
                return c
    else:
        iinfo = np.iinfo(src3.dtype)
        used_arr = np.unique(v[0][np.all(v == v[0], axis=0)]).astype(np.int64)
        used = set(used_arr.tolist())
        for c in (int(iinfo.min), int(iinfo.max)):
            if c not in used:
                return c
        for a, b in zip(used_arr[:-1].tolist(), used_arr[1:].tolist(), strict=True):
            if b - a > 1:
                return a + 1
    raise ValueError("no value is free to mark transparent source pixels")


def reproject(
    source: Any,
    destination: Any = None,
    src_transform: Affine | None = None,
    gcps: Any = None,
    rpcs: Any = None,
    src_crs: Any = None,
    src_nodata: Any = None,
    dst_transform: Affine | None = None,
    dst_crs: Any = None,
    dst_nodata: Any = None,
    dst_resolution: Any = None,
    src_alpha: int = 0,
    dst_alpha: int = 0,
    masked: bool = False,
    resampling: Resampling = Resampling.nearest,
    num_threads: int = 1,
    init_dest_nodata: bool = True,
    tolerance: float = 0.125,
    warp_mem_limit: int = 0,
    src_geoloc_array: Any = None,
    **kwargs: Any,
) -> tuple[Any, Affine]:
    """Reprojects a raster array (or band) into another grid.

    Args:
        source: A 2-D or 3-D array, a `Band` (`terrio.rio.band(dataset, bidx)`) or a dataset.
            A masked array is warped with its mask as an alpha band, as rasterio does.
        destination: A 2-D or 3-D array (or a `Band` of a writer) to fill; created when None.
            For a masked array, a new masked array over its data is returned, masked where
            nothing was warped, as rasterio does.
        src_transform: The source's transform (arrays only).
        gcps: Ground control points georeferencing the source (arrays; datasets with GCPs give
            theirs), warped with a GCP polynomial or thin plate splines (see `**kwargs`).
        rpcs: Rational polynomial coefficients georeferencing the source (an `RPC` or GDAL's RPC
            items; a dataset source is warped by its own RPCs), warped with GDAL's RPC
            transformer, in WGS 84 (see `**kwargs`).
        src_crs: The source CRS (arrays only; datasets give theirs).
        src_nodata: The source nodata value (by default, the dataset's).
        dst_transform: The destination's transform; computed when None.
        dst_crs: The destination CRS (by default, the source's).
        dst_nodata: The destination nodata value (by default, the source's).
        dst_resolution: The destination resolution, when its grid is computed.
        src_alpha: The 1-based index of the source alpha band (binary alpha only: 0 is
            transparent, the alpha maximum opaque). It is warped as a data band too, as
            rasterio does.
        dst_alpha: The 1-based index of the destination band to receive alpha: the alpha
            maximum where a pixel was warped, else 0.
        masked: Return a masked array (see `destination`) when the destination is created.
        resampling: The resampling method.
        num_threads: Unused.
        init_dest_nodata: Fill the destination with nodata first (else keep its values where
            nothing is warped).
        tolerance: The approximate transformer's maximum error, in pixels.
        warp_mem_limit: Unused.
        src_geoloc_array: Unsupported.
        **kwargs: GDAL transformer options: `MAX_GCP_ORDER` and `SRC_METHOD` (`GCP_POLYNOMIAL`,
            `GCP_TPS` or `GCP_HOMOGRAPHY`) with GCPs; `RPC_HEIGHT`, `RPC_HEIGHT_SCALE`, `RPC_DEM`
            (warped with exact transformations, as rasterio), `RPC_DEMINTERPOLATION`,
            `RPC_DEM_MISSING_VALUE` and the other `RPC_*` options with RPCs; the warp options
            `SOURCE_EXTRA` and `SAMPLE_STEPS`. Other options are unused.

    Returns:
        tuple: `(destination, dst_transform)`.

    Raises:
        ValueError: If the arguments conflict or are not supported.
    """
    from terrio.rio import Band
    from terrio.rio.io import DatasetBase

    if sum(p is not None for p in (src_transform, gcps, rpcs, src_geoloc_array)) > 1:
        raise ValueError(
            "src_transform, gcps, rpcs, and src_geoloc_array are mutually exclusive parameters "
            "and may not be used together."
        )
    if src_geoloc_array is not None:
        raise ValueError("terrio.rio reprojects with affine transforms, GCPs and RPCs only")
    try:
        if resampling == 7:
            raise ValueError("Gauss resampling is not supported")
        rs = Resampling(resampling)
        name = _WARP_NAMES[rs]
    except (ValueError, KeyError):
        raise ValueError(
            "resampling must be one of: "
            + ", ".join(f"Resampling.{r.name}" for r in SUPPORTED_RESAMPLING)
        ) from None
    if destination is None and dst_transform is not None:
        raise ValueError("Must provide destination if dst_transform is provided.")

    # The source pixels and grid.
    if isinstance(source, DatasetBase):
        source = Band(source, list(source.indexes), source.dtypes[0], source.shape)
    if isinstance(source, Band):
        ds, bidx = source.ds, source.bidx
        arr = ds.read(bidx)
        src_transform = ds.transform
        src_crs = src_crs or ds.crs
        if rpcs:
            # The dataset's own RPCs, as GDAL warps a dataset with SRC_METHOD=RPC.
            rpcs = ds.rpcs or rpcs
            # As rasterio, which checks the CRS where it computes the destination grid.
            computes_grid = dst_transform is None and not isinstance(destination, Band)
            if (
                computes_grid
                and src_crs is not None
                and CRS.from_user_input(src_crs).to_epsg() != 4326
            ):
                raise RPCError(
                    "Reprojecting with rational polynomial coefficients using source CRS other "
                    "than EPSG:4326"
                )
        else:
            rpcs = None
            ds_gcps, ds_gcp_crs = ds.gcps
            if ds_gcps and not ds._georeferenced:
                gcps = ds_gcps
                src_crs = src_crs or ds_gcp_crs
        if src_nodata is None:
            src_nodata = ds.nodata
    else:
        arr = source
        if src_transform is None and gcps is None and rpcs is None:
            raise ValueError("src_transform is required for arrays")
    src_valid = None
    if isinstance(arr, np.ma.MaskedArray):
        # rasterio warps a masked array with its mask as an alpha band: a pixel masked in any
        # band is transparent, and the fill value is not a nodata value.
        m = np.ma.getmaskarray(arr)
        src_valid = ~np.logical_or.reduce(m[np.newaxis] if m.ndim == 2 else m, axis=0)
        arr = np.ma.getdata(arr)
    arr = np.asarray(arr)
    src_2d = arr.ndim == 2
    src3 = arr[np.newaxis] if src_2d else arr
    src_count, src_h, src_w = src3.shape
    if src_crs is None and rpcs is not None:
        src_crs = "EPSG:4326"
    if src_crs is None:
        raise CRSError("A source CRS is required")
    if dst_crs is None:
        dst_crs = src_crs

    # The destination grid.
    dest_band = None
    if isinstance(destination, Band):
        dest_band = destination
        dst_h, dst_w = destination.shape
        if dst_transform is None:
            dst_transform = destination.ds.transform
        dst_crs = dst_crs if dst_crs is not None else destination.ds.crs
        if dst_nodata is None:
            dst_nodata = destination.ds.nodata
        dest_arr = None
    elif destination is not None:
        dest_arr = destination
        d3 = destination.shape
        dst_h, dst_w = d3[-2], d3[-1]
    else:
        dest_arr = None
    gcp_kw = _gcp_args(gcps, kwargs) if gcps is not None else {}
    rpc_kw = _rpc_args(rpcs, kwargs) if rpcs is not None else {}
    if gcps is not None or rpcs is not None:
        src_transform = Affine.identity()
    assert src_transform is not None
    if dst_transform is None:
        kw: dict[str, Any] = {"resolution": dst_resolution}
        if dest_arr is not None:
            kw = {"dst_width": dst_w, "dst_height": dst_h}
        if rpcs is not None:
            # As rasterio: without the transformer options.
            dst_transform, dst_w, dst_h = calculate_default_transform(
                src_crs, dst_crs, src_w, src_h, rpcs=rpcs, **kw
            )
        elif gcps is not None:
            dst_transform, dst_w, dst_h = calculate_default_transform(
                src_crs, dst_crs, src_w, src_h, gcps=gcps, **kw, **kwargs
            )
        else:
            left, bottom, right, top = array_bounds(src_h, src_w, src_transform)
            dst_transform, dst_w, dst_h = calculate_default_transform(
                src_crs, dst_crs, src_w, src_h, left, bottom, right, top, **kw
            )
    if dest_arr is None and dest_band is None:
        dest_arr = np.empty((src_count, dst_h, dst_w), dtype=arr.dtype)
        if masked:
            dest_arr = np.ma.masked_array(dest_arr)
    if dst_nodata is None and src_nodata is not None:
        dst_nodata = src_nodata
    # rasterio gives a masked destination an alpha band, and its mask from that alpha.
    dst_masked = isinstance(dest_arr, np.ma.MaskedArray)
    if (src_alpha or dst_alpha or src_valid is not None or dst_masked) and not init_dest_nodata:
        raise ValueError(
            "terrio.rio does not support alpha bands or masked arrays with init_dest_nodata=False"
        )

    out_dtype = dest_arr.dtype if dest_arr is not None else np.dtype(dest_band.dtype)  # type: ignore[union-attr]
    if src_alpha and not 1 <= src_alpha <= src_count:
        raise ValueError(f"src_alpha {src_alpha} is not a source band (1-{src_count})")
    # A binary source alpha (or a masked array's mask) is a unified validity mask: transparent
    # pixels become nodata in every band, under the given nodata value or a free sentinel.
    transparent = None
    if src_alpha:
        transparent = _transparent(src3[src_alpha - 1])
    elif src_valid is not None:
        if _alpha_max(src3.dtype) != 255.0 and not (dst_masked or dst_alpha):
            # The mask becomes a 16-bit alpha band of 255s, a 1/257 or 1/128 density, which
            # GDAL blends into an unmasked destination's initial values.
            raise ValueError(
                "terrio.rio does not reproject a masked 16-bit array into an unmasked destination"
            )
        transparent = ~src_valid
    invalid = None
    warp_nodata = src_nodata
    if transparent is not None and transparent.any():
        invalid = transparent
        if src_nodata is not None:
            nd = float(src_nodata)
            same = np.isnan(src3) if np.isnan(nd) else src3 == nd
            invalid = invalid | np.all(same, axis=0)
            fill = src_nodata
        else:
            fill = _sentinel(src3, ~transparent)
        src3 = src3.copy()
        src3[:, transparent] = fill
        warp_nodata = fill
    src_raster = _terrio.Raster.from_array(
        np.ascontiguousarray(src3),
        src_transform.to_gdal(),
        _crs_text(src_crs),
        None if warp_nodata is None else float(warp_nodata),
    )
    ot = _gdal_typename(np.dtype(out_dtype).name)
    dstnd: Any = _nodata_arg(dst_nodata) if dst_nodata is not None else "none"
    initial = None
    if not init_dest_nodata:
        # As GDAL without INIT_DEST: the warp starts from the destination's values, which pixels
        # nothing is warped to keep, and from which GDAL makes each chunk's validity mask.
        base = dest_arr if dest_arr is not None else dest_band.ds.read(dest_band.bidx)  # type: ignore[union-attr]
        base3 = np.asarray(base)
        base3 = base3[np.newaxis] if base3.ndim == 2 else base3[: src3.shape[0]]
        initial = _terrio.Raster.from_array(
            np.ascontiguousarray(base3),
            Affine(*dst_transform[:6]).to_gdal(),
            _crs_text(dst_crs),
            None,
        )
    common = dict(
        t_srs=_crs_text(dst_crs),
        dst_transform=Affine(*dst_transform[:6]).to_gdal(),
        ts=(int(dst_w), int(dst_h)),
        resampling=name,
        # As rasterio: exact transformations with an RPC DEM.
        et=0.0 if "to" in rpc_kw and "RPC_DEM" in rpc_kw["to"] else tolerance,
        wo=_warp_extras(kwargs),
        **gcp_kw,
        **rpc_kw,
    )
    out = _terrio.warp(
        src_raster,
        # An explicit source nodata is unified across bands, as rasterio's warps
        # (UNIFIED_SRC_NODATA=YES).
        srcnodata=_nodata_arg(warp_nodata) if warp_nodata is not None else "none",
        dstnodata=dstnd,
        ot=ot,
        init=initial,
        **common,
    )
    res = np.asarray(out.read())
    written = None
    if dst_alpha or dst_masked:
        # The pixels the warp wrote, from a one-band warp with the same validity: GDAL sets
        # the destination alpha to its maximum there and to 0 elsewhere.
        if invalid is None and src_nodata is not None:
            nd = float(src_nodata)
            same = np.isnan(src3) if np.isnan(nd) else src3 == nd
            invalid = np.all(same, axis=0)
        ones = np.ones((1, src_h, src_w), np.float32)
        if invalid is not None:
            ones[0][invalid] = 0
        has_invalid = invalid is not None and bool(invalid.any())
        written = ~np.isnan(
            np.asarray(
                _terrio.warp(
                    _terrio.Raster.from_array(
                        ones,
                        src_transform.to_gdal(),
                        _crs_text(src_crs),
                        0.0 if has_invalid else None,
                    ),
                    srcnodata=0.0 if has_invalid else "none",
                    dstnodata="nan",
                    ot="Float32",
                    **common,
                ).read()
            )[0]
        )
    alpha_out = None
    if dst_alpha:
        assert written is not None
        alpha_out = (written * _alpha_max(out_dtype)).astype(out_dtype)
    if dest_band is not None:
        # GDAL writes the destination alpha before the warped bands.
        if alpha_out is not None:
            dest_band.ds.write(alpha_out, dst_alpha)
        dest_band.ds.write(res if not isinstance(dest_band.bidx, int) else res[0], dest_band.bidx)
        return dest_band, dst_transform
    assert dest_arr is not None
    data = np.ma.getdata(dest_arr) if dst_masked else dest_arr
    if alpha_out is not None:
        if data.ndim != 3 or not 1 <= dst_alpha <= data.shape[0]:
            raise ValueError(f"dst_alpha {dst_alpha} is not a destination band")
        data[dst_alpha - 1] = alpha_out
    if data.ndim == 2:
        data[...] = res[0]
    elif data.shape[0] != res.shape[0] and data.shape[0] > res.shape[0]:
        # rasterio warps source band i to destination band i when the counts differ.
        data[: res.shape[0]] = res
    else:
        data[...] = res
    if dst_masked:
        # As rasterio: a new masked array over the destination's data, masked where nothing
        # was warped.
        assert written is not None
        mask = ~written if data.ndim == 2 else np.repeat(~written[np.newaxis], data.shape[0], 0)
        return np.ma.masked_array(data, mask=mask), dst_transform
    return dest_arr, dst_transform


__all__ = [
    "SUPPORTED_RESAMPLING",
    "aligned_target",
    "calculate_default_transform",
    "reproject",
    "transform",
    "transform_bounds",
    "transform_geom",
]
