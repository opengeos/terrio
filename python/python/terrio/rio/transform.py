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
from terrio.rio.errors import RasterioDeprecationWarning, TransformError

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
    """A transformer fitted to ground control points, as GDAL's GCP transformer: polynomials
    (GDAL's default order), or thin plate splines with `tps`."""

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


def get_transformer(transform: Any, **rpc_options: Any) -> Callable[[], TransformerBase]:
    """A transformer factory for a transform.

    Args:
        transform: An `Affine`, or a sequence of `GroundControlPoint`s.
        **rpc_options: Unused: RPC transformers are not supported.

    Returns:
        callable: A function creating the transformer.

    Raises:
        ValueError: If the transform is missing.
        TransformError: If the transform is RPCs.
    """
    from terrio.rio.rpc import RPC

    if transform is None:
        raise ValueError("Invalid transform")
    if isinstance(transform, Affine):
        return lambda: AffineTransformer(transform)
    if isinstance(transform, RPC):
        raise TransformError("RPC transformers are not supported by terrio.rio")
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
        transform: The transform (an `Affine`), or GCPs.
        rows: Row indices.
        cols: Column indices.
        zs: Heights (unused).
        offset: Which point of the pixels: `center`, `ul`, `ur`, `ll` or `lr`.
        **rpc_options: Unused.

    Returns:
        tuple: `(xs, ys)`.
    """
    with get_transformer(transform)() as t:
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
        transform: The transform (an `Affine`), or GCPs.
        xs: The x coordinates.
        ys: The y coordinates.
        zs: Heights (unused).
        op: The rounding function, `numpy.floor` by default.
        precision: Unused, deprecated.
        **rpc_options: Unused.

    Returns:
        tuple: `(rows, cols)`.
    """
    with get_transformer(transform)() as t:
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
            z: Height (unused).
            offset: Which point of the pixel: `center`, `ul`, `ur`, `ll` or `lr`.
            transform_method: `TransformMethod.affine`, or `TransformMethod.gcps` for the
                dataset's GCPs.
            **rpc_options: Unused.

        Returns:
            tuple: `(x, y)`.

        Raises:
            AttributeError: If the dataset has no such transform.
        """
        return xy(self._transform_for(transform_method), row, col, zs=z, offset=offset)

    def _transform_for(self, method: TransformMethod) -> Any:
        """The transform of a transform method: the affine transform, or the GCPs.

        Args:
            method: The method.

        Returns:
            Any: An `Affine` or GCPs.

        Raises:
            AttributeError: If the dataset has none.
        """
        if method is TransformMethod.affine:
            return self.transform
        if method is TransformMethod.gcps:
            gcps: Any = getattr(self, "gcps", ([], None))[0]
            if gcps:
                return gcps
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
            z: Height (unused).
            op: The rounding function, `numpy.floor` by default.
            precision: Unused, deprecated.
            transform_method: `TransformMethod.affine`, or `TransformMethod.gcps` for the
                dataset's GCPs.
            **rpc_options: Unused.

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
        return tuple(int(v) for v in rowcol(t, x, y, zs=z, op=op))


__all__ = [
    "IDENTITY",
    "Affine",
    "AffineTransformer",
    "GCPTransformer",
    "TransformMethodsMixin",
    "array_bounds",
    "from_bounds",
    "from_origin",
    "get_transformer",
    "guard_transform",
    "rowcol",
    "tastes_like_gdal",
    "xy",
]
