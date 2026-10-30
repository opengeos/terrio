# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/warp.py

"""Reprojection: coordinates, geometries, bounds and rasters, with rasterio's functions.

Rasters are warped by terrio's `warp`, which follows GDAL's warper (the default output grid of
`GDALSuggestedWarpOutput2()`, the approximate transformer and GDAL's resampling kernels).
"""

from __future__ import annotations

from math import ceil, floor
from typing import Any

import numpy as np
from affine import Affine

from terrio import _terrio
from terrio.rio.crs import CRS
from terrio.rio.dtypes import _gdal_typename
from terrio.rio.enums import Resampling
from terrio.rio.errors import CRSError, RPCError, TransformError
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
        dict | list: The transformed geometry, or geometries.
    """
    src, dst = _crs_text(src_crs), _crs_text(dst_crs)
    if isinstance(geom, (list, tuple)):
        return [transform_geom(src, dst, g, precision=precision) for g in geom]
    g = getattr(geom, "__geo_interface__", None) or geom
    pts: list[tuple[float, float]] = []

    def collect(c: Any) -> None:
        if c and isinstance(c[0], (int, float)):
            pts.append((c[0], c[1]))
        else:
            for p in c:
                collect(p)

    def rebuild(c: Any, it: Any) -> Any:
        if c and isinstance(c[0], (int, float)):
            x, y = next(it)
            if precision >= 0:
                x, y = round(x, precision), round(y, precision)
            return (x, y) + tuple(c[2:]) if isinstance(c, tuple) else [x, y] + list(c[2:])
        return (
            [rebuild(p, it) for p in c] if isinstance(c, list) else tuple(rebuild(p, it) for p in c)
        )

    if g["type"] == "GeometryCollection":
        return {
            "type": "GeometryCollection",
            "geometries": [
                transform_geom(src, dst, x, precision=precision) for x in g["geometries"]
            ],
        }
    collect(g["coordinates"])
    if not pts:
        return dict(g)
    xs = np.array([p[0] for p in pts], float)
    ys = np.array([p[1] for p in pts], float)
    ox, oy = _transform_arrays(src, dst, xs, ys)
    it = iter(zip(ox.tolist(), oy.tolist(), strict=False))
    out = dict(g)
    out["coordinates"] = rebuild(g["coordinates"], it)
    return out


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
        destination: A 2-D or 3-D array (or a `Band` of a writer) to fill; created when None.
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
        src_alpha: Unsupported (0).
        dst_alpha: Unsupported (0).
        masked: Return a masked array when the destination is created.
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
    if src_alpha or dst_alpha:
        raise ValueError("terrio.rio does not support alpha bands in reproject")
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
    if isinstance(arr, np.ma.MaskedArray):
        if src_nodata is None:
            fill = arr.fill_value
            src_nodata = float(fill) if fill is not None else None
        arr = arr.filled(src_nodata)
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

    out_dtype = dest_arr.dtype if dest_arr is not None else np.dtype(dest_band.dtype)  # type: ignore[union-attr]
    src_raster = _terrio.Raster.from_array(
        np.ascontiguousarray(src3),
        src_transform.to_gdal(),
        _crs_text(src_crs),
        None if src_nodata is None else float(src_nodata),
    )
    keep = not init_dest_nodata
    ot = "Float64" if keep else _gdal_typename(np.dtype(out_dtype).name)
    dstnd: Any = "nan" if keep else (_nodata_arg(dst_nodata) if dst_nodata is not None else "none")
    out = _terrio.warp(
        src_raster,
        t_srs=_crs_text(dst_crs),
        dst_transform=Affine(*dst_transform[:6]).to_gdal(),
        ts=(int(dst_w), int(dst_h)),
        resampling=name,
        # An explicit source nodata is unified across bands, as rasterio's warps
        # (UNIFIED_SRC_NODATA=YES).
        srcnodata=_nodata_arg(src_nodata) if src_nodata is not None else "none",
        dstnodata=dstnd,
        ot=ot,
        # As rasterio: exact transformations with an RPC DEM.
        et=0.0 if "to" in rpc_kw and "RPC_DEM" in rpc_kw["to"] else tolerance,
        wo=_warp_extras(kwargs),
        **gcp_kw,
        **rpc_kw,
    )
    res = np.asarray(out.read())
    if keep:
        written = ~np.isnan(res)
        base = dest_arr if dest_arr is not None else dest_band.ds.read(dest_band.bidx)  # type: ignore[union-attr]
        base3 = np.asarray(base)
        base3 = base3[np.newaxis] if base3.ndim == 2 else base3
        from terrio.rio._resample import gdal_cast

        merged = base3.copy()
        merged[written] = gdal_cast(res, merged.dtype)[written]
        res = merged
    if dest_band is not None:
        dest_band.ds.write(res if not isinstance(dest_band.bidx, int) else res[0], dest_band.bidx)
        return dest_band, dst_transform
    assert dest_arr is not None
    if dest_arr.ndim == 2:
        dest_arr[...] = res[0]
    else:
        dest_arr[...] = res
    if masked and isinstance(dest_arr, np.ma.MaskedArray) and dst_nodata is not None:
        dest_arr.mask = (
            np.isnan(dest_arr.data) if np.isnan(dst_nodata) else dest_arr.data == dst_nodata
        )
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
