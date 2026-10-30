# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 1999, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 2007-2013, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (translated to Python and modified):
#   rasterio 1.5.2:
#     rasterio/transform.py
#   GDAL v3.12.2:
#     gcore/gdal_misc.cpp

"""Affine transformations between pixel and map coordinates.

Transforms are `affine.Affine` objects, as in rasterio.
"""

from __future__ import annotations

import warnings
from collections.abc import Callable, Iterator, Sequence
from contextlib import contextmanager
from numbers import Number
from typing import Any

import numpy as np
from affine import Affine

from terrio.rio.enums import TransformDirection, TransformMethod
from terrio.rio.errors import RasterioDeprecationWarning, TransformError, TransformWarning

IDENTITY = Affine.identity()
GDAL_IDENTITY = IDENTITY.to_gdal()


@contextmanager
def _quiet_mul() -> Iterator[None]:
    """Silences affine's deprecation of `*` for matrix products.

    Yields:
        None.
    """
    with warnings.catch_warnings():
        warnings.filterwarnings(
            "ignore", "Use.*matmul instead of.*mul operator", category=PendingDeprecationWarning
        )
        yield


def compose(a: Affine, b: Affine) -> Affine:
    """The matrix product `a * b` of two affine transforms.

    Args:
        a: The left transform.
        b: The right transform.

    Returns:
        Affine: The product.
    """
    with _quiet_mul():
        out: Affine = a * b
    return out


def apply(t: Affine, x: float, y: float) -> tuple[float, float]:
    """Applies an affine transform to a point.

    Args:
        t: The transform.
        x: The x (column) coordinate.
        y: The y (row) coordinate.

    Returns:
        tuple: The transformed `(x, y)`.
    """
    with _quiet_mul():
        rx, ry = t * (x, y)
    return rx, ry


def tastes_like_gdal(seq: Sequence[float]) -> bool:
    """Whether a sequence looks like a GDAL geotransform rather than affine coefficients.

    Args:
        seq: Six numbers.

    Returns:
        bool: True for a GDAL-style north-up geotransform.
    """
    return tuple(seq) == GDAL_IDENTITY or (seq[2] == seq[4] == 0.0 and seq[1] > 0 and seq[5] < 0)


def guard_transform(transform: Any) -> Affine:
    """Checks that a transform is an `Affine`, converting coefficient sequences.

    Args:
        transform: An `Affine`, or affine coefficients `(a, b, c, d, e, f)`.

    Returns:
        Affine: The transform.

    Raises:
        TypeError: If a GDAL-style geotransform is given.
    """
    if not isinstance(transform, Affine):
        if tastes_like_gdal(transform):
            raise TypeError(
                "GDAL-style transforms have been deprecated.  This exception will be raised for "
                "a period of time to highlight potentially confusing errors, but will eventually "
                "be removed."
            )
        transform = Affine(*transform)
    t: Affine = transform
    return t


def from_origin(west: float, north: float, xsize: float, ysize: float) -> Affine:
    """The transform of a north-up grid from its upper left corner and pixel size.

    Args:
        west: The x of the upper left corner.
        north: The y of the upper left corner.
        xsize: The pixel width.
        ysize: The pixel height (positive).

    Returns:
        Affine: The transform.
    """
    return compose(Affine.translation(west, north), Affine.scale(xsize, -ysize))


def from_bounds(
    west: float, south: float, east: float, north: float, width: int, height: int
) -> Affine:
    """The transform of a north-up grid covering bounds with `width` x `height` pixels.

    Args:
        west: The left bound.
        south: The bottom bound.
        east: The right bound.
        north: The top bound.
        width: The number of columns.
        height: The number of rows.

    Returns:
        Affine: The transform.
    """
    return compose(
        Affine.translation(west, north),
        Affine.scale((east - west) / width, (south - north) / height),
    )


def array_bounds(height: int, width: int, transform: Affine) -> tuple[float, float, float, float]:
    """The bounds of an array.

    Args:
        height: The number of rows.
        width: The number of columns.
        transform: The array's transform.

    Returns:
        tuple: `(west, south, east, north)`.
    """
    a, b, c, d, e, f, _, _, _ = transform
    if b == d == 0:
        return c, f + e * height, c + a * width, f
    corners = [(c, f), apply(transform, 0, height), apply(transform, width, height)]
    corners.append(apply(transform, width, 0))
    xs = [p[0] for p in corners]
    ys = [p[1] for p in corners]
    return min(xs), min(ys), max(xs), max(ys)


class TransformerBase:
    """Converts between pixel `(row, col)` and map `(x, y)` coordinates."""

    def __init__(self) -> None:
        """Creates the transformer."""
        self._transformer: Any = None

    @staticmethod
    def _ensure_arr_input(
        xs: Any, ys: Any, zs: Any = None
    ) -> tuple[np.ndarray[Any, Any], np.ndarray[Any, Any], np.ndarray[Any, Any]]:
        """Converts coordinates to arrays that broadcast together.

        Args:
            xs: The first coordinates.
            ys: The second coordinates.
            zs: Heights, or None.

        Returns:
            tuple: The three arrays.

        Raises:
            TransformError: If the arrays do not broadcast.
        """
        xa = np.atleast_1d(xs)
        ya = np.atleast_1d(ys)
        za = np.atleast_1d(zs) if zs is not None else np.zeros(1)
        try:
            np.broadcast(xa, ya, za)
        except ValueError as err:
            raise TransformError() from err
        return xa, ya, za

    def __enter__(self) -> TransformerBase:
        """Enters the context.

        Returns:
            TransformerBase: The transformer.
        """
        return self

    def __exit__(self, *args: object) -> None:
        """Leaves the context.

        Args:
            *args: Exception details.
        """

    def rowcol(
        self, xs: Any, ys: Any, zs: Any = None, op: Any = None, precision: Any = None
    ) -> tuple[Any, Any]:
        """The rows and columns of map coordinates.

        Args:
            xs: The x coordinates.
            ys: The y coordinates.
            zs: Heights (unused by affine transforms).
            op: A function rounding the fractional rows and columns, `numpy.floor` by default
                (giving int32 values).
            precision: Unused, deprecated.

        Returns:
            tuple: `(rows, cols)`: scalars for scalar input, else arrays.

        Raises:
            TransformError: If the inputs are not valid.
        """
        if precision is not None:
            warnings.warn(
                "The precision parameter is unused, deprecated, and will be removed in 2.0.0.",
                RasterioDeprecationWarning,
                stacklevel=2,
            )
        scalar = isinstance(xs, Number) and isinstance(ys, Number)
        xa, ya, za = self._ensure_arr_input(xs, ys, zs)
        try:
            cols, rows = self._transform(xa, ya, za, TransformDirection.reverse)
            if op is None:
                rows = np.floor(rows).astype(np.int32)
                cols = np.floor(cols).astype(np.int32)
            elif isinstance(op, np.ufunc):
                op(rows, out=rows)
                op(cols, out=cols)
            else:
                rows = np.array(list(map(op, rows)))
                cols = np.array(list(map(op, cols)))
        except TypeError as err:
            raise TransformError("Invalid inputs") from err
        if scalar:
            return rows[0], cols[0]
        return rows, cols

    def xy(self, rows: Any, cols: Any, zs: Any = None, offset: str = "center") -> tuple[Any, Any]:
        """The map coordinates of pixels.

        Args:
            rows: Row indices.
            cols: Column indices.
            zs: Heights (unused by affine transforms).
            offset: Which point of the pixels: `center`, `ul`, `ur`, `ll` or `lr`.

        Returns:
            tuple: `(xs, ys)`: scalars for scalar input, else arrays.

        Raises:
            TransformError: If the offset or the inputs are not valid.
        """
        offsets = {"center": (0.5, 0.5), "ul": (0, 0), "ur": (1, 0), "ll": (0, 1), "lr": (1, 1)}
        if offset not in offsets:
            raise TransformError("Invalid offset")
        coff, roff = offsets[offset]
        scalar = isinstance(rows, Number) and isinstance(cols, Number)
        ra, ca, za = self._ensure_arr_input(rows, cols, zs)
        try:
            shift = AffineTransformer(Affine.translation(coff, roff))
            ocols, orows = shift._transform(ca, ra, za, TransformDirection.forward)
            xs, ys = self._transform(ocols, orows, za, TransformDirection.forward)
        except TypeError as err:
            raise TransformError("Invalid inputs") from err
        if scalar:
            return xs[0], ys[0]
        return xs, ys

    def _transform(
        self, xs: Any, ys: Any, zs: Any, transform_direction: TransformDirection
    ) -> tuple[Any, Any]:
        """Transforms coordinates.

        Args:
            xs: First coordinates.
            ys: Second coordinates.
            zs: Heights.
            transform_direction: Forward (pixel to map) or reverse.

        Raises:
            NotImplementedError: Always, in the base class.
        """
        raise NotImplementedError


class AffineTransformer(TransformerBase):
    """A transformer for an affine transform."""

    def __init__(self, affine_transform: Affine) -> None:
        """Creates the transformer.

        Args:
            affine_transform: The transform.

        Raises:
            ValueError: If the transform is not an `Affine`.
        """
        super().__init__()
        if not isinstance(affine_transform, Affine):
            raise ValueError("Not an affine transform")
        self._transformer = affine_transform
        self._transform_arr = np.asarray(affine_transform, dtype="float64").reshape(3, 3)

    def _transform(
        self, xs: Any, ys: Any, zs: Any, transform_direction: TransformDirection
    ) -> tuple[Any, Any]:
        """Transforms coordinates with the matrix (or by solving it, in reverse).

        Args:
            xs: First coordinates.
            ys: Second coordinates.
            zs: Heights (unused).
            transform_direction: Forward (pixel to map) or reverse.

        Returns:
            tuple: The transformed coordinates.
        """
        bi = np.broadcast(xs, ys)
        m = np.empty((3, bi.size))
        m[0] = bi.iters[0]
        m[1] = bi.iters[1]
        m[2] = 1
        if transform_direction is TransformDirection.forward:
            out = np.matmul(self._transform_arr, m, out=m)
        else:
            out = np.linalg.solve(self._transform_arr, m)
        return out[0], out[1]

    def __repr__(self) -> str:
        """The representation.

        Returns:
            str: `<AffineTransformer>`.
        """
        return "<AffineTransformer>"


class GCPTransformer(TransformerBase):
    """A transformer fitted to ground control points, as GDAL's GCP transformer.

    Polynomials of GDAL's default order, or thin plate splines with `tps`.
    """

    def __init__(self, gcps: Any, tps: bool = False) -> None:
        """Creates the transformer.

        Args:
            gcps: `GroundControlPoint`s.
            tps: Use thin plate splines instead of polynomials.

        Raises:
            ValueError: If the points are not `GroundControlPoint`s.
        """
        from terrio.rio.control import GroundControlPoint

        super().__init__()
        gcps = list(gcps)
        if gcps and not isinstance(gcps[0], GroundControlPoint):
            raise ValueError("GCPTransformer requires sequence of GroundControlPoint")
        self._gcps = [
            (float(g.col), float(g.row), float(g.x), float(g.y), float(g.z or 0.0)) for g in gcps
        ]
        self._method = "tps" if tps else "polynomial"
        self._transformer = self._gcps

    def _transform(
        self, xs: Any, ys: Any, zs: Any, transform_direction: TransformDirection
    ) -> tuple[Any, Any]:
        """Transforms coordinates through the fitted functions.

        Args:
            xs: First coordinates.
            ys: Second coordinates.
            zs: Heights (unused).
            transform_direction: Forward (pixel to map) or reverse.

        Returns:
            tuple: The transformed coordinates.

        Raises:
            TransformError: If the GCPs cannot be fitted.
        """
        from terrio import _terrio

        bi = np.broadcast(xs, ys)
        xa = np.array([v for v, _ in bi], dtype="float64")
        bi = np.broadcast(xs, ys)
        ya = np.array([v for _, v in bi], dtype="float64")
        try:
            ox, oy = _terrio.gcp_transform(
                self._gcps,
                xa.tolist(),
                ya.tolist(),
                inverse=transform_direction is TransformDirection.reverse,
                method=self._method,
            )
        except Exception as err:
            raise TransformError(str(err)) from err
        return np.asarray(ox), np.asarray(oy)

    def __repr__(self) -> str:
        """The representation.

        Returns:
            str: `<open GCPTransformer>`.
        """
        return "<open GCPTransformer>"


def _rpc_items(rpcs: Any) -> dict[str, str]:
    """GDAL's `RPC` metadata items of RPCs.

    Args:
        rpcs: A `terrio.rio.rpc.RPC` (or rasterio's), or a dict of GDAL's items.

    Returns:
        dict: The items, as strings.
    """
    items = rpcs.to_gdal() if hasattr(rpcs, "to_gdal") else dict(rpcs)
    return {str(k).upper(): str(v) for k, v in items.items()}


def _rpc_options(options: dict[str, Any]) -> dict[str, str]:
    """GDAL's RPC transformer options, as rasterio passes them.

    Names and values are upper-cased, except the path of `RPC_DEM`.

    Args:
        options: The options, such as `{"rpc_height": 100}`.

    Returns:
        dict: The options, as strings.
    """
    out = {}
    for k, v in options.items():
        key = str(k).upper()
        out[key] = str(v) if key == "RPC_DEM" else str(v).upper()
    return out


class RPCTransformer(TransformerBase):
    """A transformer by rational polynomial coefficients, as GDAL's RPC transformer.

    Pixel/line to WGS 84 longitude and latitude by GDAL's iterative solution, and back by
    evaluating the polynomials, at the heights given (`zs`) above `RPC_HEIGHT` or a DEM's
    (`RPC_DEM`). Options are GDAL's `GDALCreateRPCTransformerV2` options, passed as keyword
    arguments (`rpc_height=100`, `RPC_DEM="dem.tif"`, ...). Points that fail to transform are
    infinite, with a `TransformWarning`.
    """

    def __init__(self, rpcs: Any, **rpc_options: Any) -> None:
        """Creates the transformer.

        Args:
            rpcs: A `terrio.rio.rpc.RPC` (or rasterio's), or a dict of GDAL's RPC items.
            **rpc_options: GDAL's RPC transformer options (`RPC_HEIGHT`, `RPC_HEIGHT_SCALE`,
                `RPC_DEM`, `RPC_DEMINTERPOLATION`, `RPC_DEM_MISSING_VALUE`, `RPC_DEM_SRS`,
                `RPC_DEM_APPLY_VDATUM_SHIFT`, `RPC_PIXEL_ERROR_THRESHOLD`,
                `RPC_MAX_ITERATIONS`).

        Raises:
            ValueError: If `rpcs` are not RPCs.
            TransformError: If the transformer cannot be created.
        """
        from terrio import _terrio

        super().__init__()
        if not (isinstance(rpcs, dict) or hasattr(rpcs, "to_gdal")):
            raise ValueError("RPCTransformer requires RPC")
        try:
            self._transformer = _terrio.RpcTransformer(_rpc_items(rpcs), _rpc_options(rpc_options))
        except Exception as err:
            raise TransformError(str(err)) from err

    def _transform(
        self, xs: Any, ys: Any, zs: Any, transform_direction: TransformDirection
    ) -> tuple[Any, Any]:
        """Transforms coordinates through the RPCs.

        Args:
            xs: Columns (or longitudes, in reverse).
            ys: Rows (or latitudes).
            zs: Heights above `RPC_HEIGHT` or the DEM.
            transform_direction: Forward (pixel/line to longitude/latitude) or reverse.

        Returns:
            tuple: The transformed coordinates (infinite where they failed).

        Raises:
            ValueError: If the transformer is closed.
        """
        if self._transformer is None:
            raise ValueError("Unexpected NULL transformer")
        bi = np.broadcast(xs, ys, zs)
        pts = np.array([tuple(v) for v in bi], dtype="float64").reshape(-1, 3)
        ox, oy, ok = self._transformer.transform(
            pts[:, 0].tolist(),
            pts[:, 1].tolist(),
            pts[:, 2].tolist(),
            to_pixel=transform_direction is TransformDirection.reverse,
        )
        if not all(ok):
            # rasterio warns twice: GDALRPCTransform failed, and one or more points did.
            warnings.warn("Could not transform points using RPCs.", TransformWarning, stacklevel=3)
            warnings.warn(
                "One or more points could not be transformed using RPCs.",
                TransformWarning,
                stacklevel=3,
            )
        return np.asarray(ox, dtype="float64"), np.asarray(oy, dtype="float64")

    def close(self) -> None:
        """Releases the transformer (and its DEM)."""
        self._transformer = None

    @property
    def closed(self) -> bool:
        """Whether the transformer is closed.

        Returns:
            bool: True once closed.
        """
        return self._transformer is None

    def __exit__(self, *args: object) -> None:
        """Leaves the context, closing the transformer.

        Args:
            *args: Exception details.
        """
        self.close()

    def __repr__(self) -> str:
        """The representation.

        Returns:
            str: `<open RPCTransformer>` or `<closed RPCTransformer>`.
        """
        return f"<{'closed' if self.closed else 'open'} RPCTransformer>"


def from_gcps(gcps: Sequence[Any]) -> Affine:
    """The affine transform best fitting ground control points, as GDAL computes it.

    This is `GDALGCPsToGeoTransform`, as `rasterio.transform.from_gcps` calls it.

    Two GCPs give a north-up transform; four GCPs at the corners of a north-up grid give it
    exactly; otherwise the transform is the least squares fit, computed on coordinates
    normalized to [0, 1].

    Args:
        gcps: `GroundControlPoint`s (with `row`, `col`, `x` and `y`).

    Returns:
        Affine: The transform.

    Raises:
        ValueError: If the GCPs do not determine a transform.
    """
    pts = [(float(g.col), float(g.row), float(g.x), float(g.y)) for g in gcps]
    n = len(pts)
    if n < 2:
        raise ValueError("At least two GCPs are required")
    if n == 2:
        (p0, l0, x0, y0), (p1, l1, x1, y1) = pts
        if p0 == p1 or l0 == l1:
            raise ValueError("The GCPs do not determine a transform")
        a = (x1 - x0) / (p1 - p0)
        e = (y1 - y0) / (l1 - l0)
        return Affine(a, 0.0, x0 - p0 * a, 0.0, e, y0 - l0 * e)
    if (
        n == 4
        and pts[0][1] == pts[1][1]
        and pts[2][1] == pts[3][1]
        and pts[0][0] == pts[3][0]
        and pts[1][0] == pts[2][0]
        and pts[0][1] != pts[2][1]
        and pts[0][0] != pts[1][0]
        and pts[0][3] == pts[1][3]
        and pts[2][3] == pts[3][3]
        and pts[0][2] == pts[3][2]
        and pts[1][2] == pts[2][2]
        and pts[0][3] != pts[2][3]
        and pts[0][2] != pts[1][2]
    ):
        a = (pts[1][2] - pts[0][2]) / (pts[1][0] - pts[0][0])
        e = (pts[2][3] - pts[1][3]) / (pts[2][1] - pts[1][1])
        return Affine(a, 0.0, pts[0][2] - pts[0][0] * a, 0.0, e, pts[0][3] - pts[0][1] * e)

    def normalize(vals: list[float]) -> tuple[float, float]:
        """The offset and scale mapping values to [0, 1].

        Args:
            vals: The values.

        Returns:
            tuple: The offset and the scale.

        Raises:
            ValueError: If the values are all equal.
        """
        lo, hi = min(vals), max(vals)
        if hi == lo:
            raise ValueError("The GCPs do not determine a transform")
        return -lo / (hi - lo), 1.0 / (hi - lo)

    pl_x = normalize([p[0] for p in pts])
    pl_y = normalize([p[1] for p in pts])
    geo_x = normalize([p[2] for p in pts])
    geo_y = normalize([p[3] for p in pts])
    sum_x = sum_y = sum_xy = sum_xx = sum_yy = 0.0
    sum_lon = sum_lonx = sum_lony = sum_lat = sum_latx = sum_laty = 0.0
    for p, line, gx, gy in pts:
        x = pl_x[0] + p * pl_x[1]
        y = pl_y[0] + line * pl_y[1]
        lon = geo_x[0] + gx * geo_x[1]
        lat = geo_y[0] + gy * geo_y[1]
        sum_x += x
        sum_y += y
        sum_xy += x * y
        sum_xx += x * x
        sum_yy += y * y
        sum_lon += lon
        sum_lonx += lon * x
        sum_lony += lon * y
        sum_lat += lat
        sum_latx += lat * x
        sum_laty += lat * y
    divisor = (
        n * (sum_xx * sum_yy - sum_xy * sum_xy)
        + 2 * sum_x * sum_y * sum_xy
        - sum_y * sum_y * sum_xx
        - sum_x * sum_x * sum_yy
    )
    if divisor == 0.0:
        raise ValueError("The GCPs do not determine a transform")
    c0 = sum_xx * sum_yy - sum_xy * sum_xy
    c1 = sum_y * sum_xy - sum_x * sum_yy
    c2 = sum_x * sum_xy - sum_y * sum_xx
    g0 = (sum_lon * c0 + sum_lonx * c1 + sum_lony * c2) / divisor
    g3 = (sum_lat * c0 + sum_latx * c1 + sum_laty * c2) / divisor
    g1 = (
        sum_lon * c1
        + sum_lonx * (n * sum_yy - sum_y * sum_y)
        + sum_lony * (sum_x * sum_y - sum_xy * n)
    ) / divisor
    g2 = (
        sum_lon * c2
        + sum_lonx * (sum_x * sum_y - n * sum_xy)
        + sum_lony * (n * sum_xx - sum_x * sum_x)
    ) / divisor
    g4 = (
        sum_lat * c1
        + sum_latx * (n * sum_yy - sum_y * sum_y)
        + sum_laty * (sum_x * sum_y - sum_xy * n)
    ) / divisor
    g5 = (
        sum_lat * c2
        + sum_latx * (sum_x * sum_y - n * sum_xy)
        + sum_laty * (n * sum_xx - sum_x * sum_x)
    ) / divisor
    # Pixel/line normalized, then the fit, then the inverse of the geographic normalization.
    pl = Affine(pl_x[1], 0.0, pl_x[0], 0.0, pl_y[1], pl_y[0])
    fit = Affine(g1, g2, g0, g4, g5, g3)
    geo_inv = Affine(
        1.0 / geo_x[1], 0.0, -geo_x[0] / geo_x[1], 0.0, 1.0 / geo_y[1], -geo_y[0] / geo_y[1]
    )
    return compose(compose(geo_inv, fit), pl)


def get_transformer(transform: Any, **rpc_options: Any) -> Callable[[], TransformerBase]:
    """A transformer factory for a transform.

    Args:
        transform: An `Affine`, a sequence of `GroundControlPoint`s, or an `RPC`.
        **rpc_options: GDAL's RPC transformer options, for RPCs.

    Returns:
        callable: A function creating the transformer.

    Raises:
        ValueError: If the transform is missing.
    """
    if transform is None:
        raise ValueError("Invalid transform")
    if isinstance(transform, Affine):
        return lambda: AffineTransformer(transform)
    if hasattr(transform, "to_gdal"):
        return lambda: RPCTransformer(transform, **rpc_options)
    return lambda: GCPTransformer(transform)


def xy(
    transform: Any,
    rows: Any,
    cols: Any,
    zs: Any = None,
    offset: str = "center",
    **rpc_options: Any,
) -> tuple[Any, Any]:
    """The map coordinates of pixels.

    Args:
        transform: The transform (an `Affine`), GCPs, or RPCs.
        rows: Row indices.
        cols: Column indices.
        zs: Heights, for RPCs (0 by default).
        offset: Which point of the pixels: `center`, `ul`, `ur`, `ll` or `lr`.
        **rpc_options: GDAL's RPC transformer options, for RPCs.

    Returns:
        tuple: `(xs, ys)`.
    """
    with get_transformer(transform, **rpc_options)() as t:
        return t.xy(rows, cols, zs=zs, offset=offset)


def rowcol(
    transform: Any,
    xs: Any,
    ys: Any,
    zs: Any = None,
    op: Any = None,
    precision: Any = None,
    **rpc_options: Any,
) -> tuple[Any, Any]:
    """The rows and columns of map coordinates.

    Args:
        transform: The transform (an `Affine`), GCPs, or RPCs.
        xs: The x coordinates.
        ys: The y coordinates.
        zs: Heights, for RPCs (0 by default).
        op: The rounding function, `numpy.floor` by default.
        precision: Unused, deprecated.
        **rpc_options: GDAL's RPC transformer options, for RPCs.

    Returns:
        tuple: `(rows, cols)`.
    """
    with get_transformer(transform, **rpc_options)() as t:
        return t.rowcol(xs, ys, zs=zs, op=op, precision=precision)


class TransformMethodsMixin:
    """`xy` and `index` methods for datasets with a `transform`."""

    transform: Affine

    def xy(
        self,
        row: Any,
        col: Any,
        z: Any = None,
        offset: str = "center",
        transform_method: TransformMethod = TransformMethod.affine,
        **rpc_options: Any,
    ) -> tuple[Any, Any]:
        """The map coordinates of a pixel.

        Args:
            row: The row index.
            col: The column index.
            z: Height, for RPCs.
            offset: Which point of the pixel: `center`, `ul`, `ur`, `ll` or `lr`.
            transform_method: `TransformMethod.affine`, `TransformMethod.gcps` for the dataset's
                GCPs, or `TransformMethod.rpcs` for its RPCs.
            **rpc_options: GDAL's RPC transformer options, for RPCs.

        Returns:
            tuple: `(x, y)`.

        Raises:
            AttributeError: If the dataset has no such transform.
        """
        t = self._transform_for(transform_method)
        return xy(t, row, col, zs=z, offset=offset, **rpc_options)

    def _transform_for(self, method: TransformMethod) -> Any:
        """The transform of a transform method: the affine transform, the GCPs or the RPCs.

        Args:
            method: The method.

        Returns:
            Any: An `Affine`, GCPs or RPCs.

        Raises:
            AttributeError: If the dataset has none.
        """
        if method is TransformMethod.affine:
            return self.transform
        if method is TransformMethod.gcps:
            gcps: Any = getattr(self, "gcps", ([], None))[0]
            if gcps:
                return gcps
        if method is TransformMethod.rpcs:
            rpcs = getattr(self, "rpcs", None)
            if rpcs:
                return rpcs
        raise AttributeError(f"Dataset has no {method}")

    def index(
        self,
        x: Any,
        y: Any,
        z: Any = None,
        op: Any = None,
        precision: Any = None,
        transform_method: TransformMethod = TransformMethod.affine,
        **rpc_options: Any,
    ) -> tuple[int, ...]:
        """The `(row, col)` of the pixel holding a point.

        Args:
            x: The x coordinate.
            y: The y coordinate.
            z: Height, for RPCs.
            op: The rounding function, `numpy.floor` by default.
            precision: Unused, deprecated.
            transform_method: `TransformMethod.affine`, `TransformMethod.gcps` for the dataset's
                GCPs, or `TransformMethod.rpcs` for its RPCs.
            **rpc_options: GDAL's RPC transformer options, for RPCs.

        Returns:
            tuple: `(row, col)`, as ints.

        Raises:
            AttributeError: If the dataset has no such transform.
        """
        if precision is not None:
            warnings.warn(
                "The precision parameter is unused, deprecated, and will be removed in 2.0.0.",
                RasterioDeprecationWarning,
                stacklevel=2,
            )
        t = self._transform_for(transform_method)
        return tuple(int(v) for v in rowcol(t, x, y, zs=z, op=op, **rpc_options))


__all__ = [
    "IDENTITY",
    "Affine",
    "AffineTransformer",
    "GCPTransformer",
    "RPCTransformer",
    "TransformMethodsMixin",
    "array_bounds",
    "from_bounds",
    "from_gcps",
    "from_origin",
    "get_transformer",
    "guard_transform",
    "rowcol",
    "tastes_like_gdal",
    "xy",
]
