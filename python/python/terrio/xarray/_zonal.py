"""Zonal statistics of every 2-D slice of a cube, streamed with `terrio.ZonalReducer`.

The result is a vector data cube: a `geometry` dimension indexed by the zones' geometries
(Shapely, in their own CRS, recorded as the grid mapping), the array's other dimensions, and
`statistic`. The zones' attributes are coordinates along `geometry`.
"""

from __future__ import annotations

import math
import os
import sys
import warnings
from collections.abc import Hashable, Sequence
from concurrent.futures import ThreadPoolExecutor
from types import FrameType
from typing import Any

import numpy as np
import xarray

from terrio import _terrio
from terrio.xarray import _ops

# Pixels per strip: a zone's values are dropped once its last strip is reduced.
_STRIP_PIXELS = 2**20
# Bytes held by default: the pixels read, the batch read ahead and the values sorted
# statistics keep.
BATCH_BYTES = 512 * 2**20
# Statistics that keep a zone's values until it is complete.
_SORTED = ("median", "mode", "majority", "minority", "variety")


def _outside_level() -> int:
    """The `stacklevel` of the first caller outside terrio, for a warning about its call."""
    here = os.path.dirname(os.path.dirname(__file__))
    frame: FrameType | None = sys._getframe(1)
    level = 1
    while frame is not None and frame.f_code.co_filename.startswith(here):
        frame, level = frame.f_back, level + 1
    return level


def zones_layer(zones: Any, crs: Any, layer: str | None = None) -> _terrio.Layer:
    """`zones` as one `Layer` with its attributes; plain geometries are in `crs`."""
    if layer is not None:
        if _ops._is_path(zones):
            return _terrio.open_vector(os.fspath(zones))[layer]
        if isinstance(zones, _terrio.VectorDataset):
            return zones[layer]
        if isinstance(zones, _terrio.Layer) and zones.name != layer:
            raise ValueError(f"no layer named '{layer}': the zones are layer '{zones.name}'")
    src = _ops.in_crs(zones, crs)
    if isinstance(src, _terrio.Layer):
        return src
    if getattr(src, "crs", None) is not None and hasattr(src, "geometry"):
        # GeoPandas, with its columns
        frame = src if hasattr(src, "columns") else src.to_frame(name="geometry")
        try:
            return _terrio.Layer.from_arrow(frame)
        except ImportError:  # no pyarrow
            return _terrio.Layer.from_geojson(frame.__geo_interface__, crs=frame.crs.to_wkt())
    if (
        _ops._is_path(src)
        or isinstance(src, _terrio.VectorDataset)
        or hasattr(src, "__arrow_c_stream__")
        or hasattr(src, "__arrow_c_array__")
    ):
        return _ops.geometry_input(src)
    return _terrio.Layer.from_geojson(src)  # no CRS on either side: EPSG:4326


def _prepared(da: xarray.DataArray) -> tuple:
    """`da` as terrio reads it: other dims, then y and x, north-up.

    Also its other dims, their coords, its nodata (None for NaN) and its grid (width, height,
    transform, CRS, strip rows).
    """
    g = _ops._grid(da)
    other = [d for d in da.dims if d not in (g.y, g.x)]
    coords = {
        name: c
        for name, c in da.coords.items()
        if set(c.dims) <= set(other) and "crs_wkt" not in c.attrs  # not the grid mapping
    }
    da = da.transpose(*other, g.y, g.x)
    if g.ascending:
        da = da.isel({g.y: slice(None, None, -1)})
    if g.reversed_x:
        da = da.isel({g.x: slice(None, None, -1)})
    nodata = g.nodata
    if nodata is not None and np.isnan(nodata):
        nodata = None  # NaN: terrio leaves non-finite pixels out
    rows = max(1, _STRIP_PIXELS // max(da.sizes[g.x], 1))
    y_chunks = da.chunksizes.get(g.y) if da.chunks is not None else None
    if y_chunks and len(y_chunks) > 1:
        rows = _strip_rows(rows, y_chunks)
    grid = (
        da.sizes[g.x],
        da.sizes[g.y],
        tuple(g.transform.to_gdal()),
        g.crs.to_wkt() if g.crs is not None else None,
        rows,
    )
    return da, other, coords, None if nodata is None else float(nodata), grid


def _strip_rows(rows: int, y_chunks: Sequence[int]) -> int:
    """Rows per strip, about `rows`, so that no strip crosses a dask chunk edge.

    A strip across an edge would compute the next chunk once for itself and once more for that
    chunk's own strips. Strips are then whole chunks (equal chunks, `rows` at least one chunk
    high) or divide every chunk (the largest divisor of the chunks' common height).
    """
    inner = y_chunks[:-1]  # the last chunk ends at the array's edge
    if len(set(inner)) == 1 and rows >= inner[0]:
        return rows - rows % inner[0]
    common = math.gcd(*inner)
    return max(d for d in range(1, min(rows, common) + 1) if common % d == 0)


def zonal_cube(
    arrays: Sequence[xarray.DataArray],
    zones: _terrio.Layer,
    *,
    stats: Sequence[str] | None = None,
    all_touched: bool = False,
    fractional: bool = False,
    batch_bytes: int | None = None,
) -> list[xarray.DataArray]:
    """The zonal statistics of every 2-D slice of each of `arrays`.

    Each has dims `(geometry, ..., statistic)`.
    The zones are rasterized once per grid, then the strips holding zone pixels are reduced in
    order, for a batch of slices at a time (whole chunks of the first non-spatial dim, about
    `batch_bytes` held). A lazy array reads only those strips; a dask array computes each chunk
    once. Arrays on the same grid, dims and chunks are read in one dask computation, so what
    they share (a mask...) is computed once.
    """
    shapely = _shapely()
    prepared = [_prepared(da) for da in arrays]
    groups: dict = {}
    for i, (da, other, _, _, grid) in enumerate(prepared):
        chunks = tuple(da.chunks) if da.chunks is not None else None
        groups.setdefault((grid, tuple(other), da.shape[:-2], chunks), []).append(i)
    reducers: dict = {}
    values: list = [None] * len(arrays)
    for (grid, other, shape, _), members in groups.items():
        if grid not in reducers:
            *shape_crs, strip_rows = grid
            reducers[grid] = _terrio.ZonalReducer(
                zones,
                *shape_crs,
                stats=None if stats is None else list(stats),
                all_touched=all_touched,
                fractional=fractional,
                strip_rows=strip_rows,
            )
        reducer = reducers[grid]
        outs = _reduce(
            [prepared[i][0] for i in members],
            list(other),
            shape,
            [prepared[i][3] for i in members],
            reducer,
            fractional,
            BATCH_BYTES if batch_bytes is None else batch_bytes,
        )
        for i, out in zip(members, outs, strict=True):
            values[i] = out
    names = next(iter(reducers.values())).stats if reducers else []
    geometry = shapely.from_wkb(zones.to_wkb())
    columns = zones.columns()
    results = []
    for (da, other, coords, _, _), out in zip(prepared, values, strict=True):
        attributes = _attributes(columns, {"geometry", "statistic", *other, *coords})
        result = xarray.DataArray(
            out,
            dims=(*other, "geometry", "statistic"),
            coords=coords | attributes | {"geometry": geometry, "statistic": names},
            name=da.name,
        ).transpose("geometry", *other, "statistic")
        if zones.crs_wkt is not None:
            result = result.terrio.write_crs(zones.crs_wkt)
        results.append(result)
    return results


def _attributes(columns: dict, taken: set) -> dict:
    """The zones' attributes as coords along `geometry`.

    A name already taken by a coordinate gets `_2`, `_3`... (as the Zarr writer of zonal cubes
    does).
    """
    out = {}
    for name, values in columns.items():
        unique, n = name, 1
        while unique in taken:
            n += 1
            unique = f"{name}_{n}"
        taken.add(unique)
        out[unique] = ("geometry", np.asarray(values))
    return out


def _shapely() -> Any:
    try:
        import shapely  # type: ignore[import-untyped, unused-ignore]
    except ImportError as e:
        raise ImportError(
            "zonal statistics of an array with more than its two spatial dimensions return the "
            "zones' geometries as Shapely geometries: install shapely"
        ) from e
    return shapely


def _batches(
    da: xarray.DataArray, dim: Hashable, per_index: int, budget: int
) -> list[tuple[int, int]]:
    """(start, stop) runs along `dim` of about `budget` bytes.

    Made of whole dask chunks (each computed once), at least one chunk each.
    """
    sizes = da.chunksizes.get(dim) or (1,) * da.sizes[dim]
    want = max(1, budget // max(per_index, 1))
    out, start, stop = [], 0, 0
    for size in sizes:
        if stop > start and stop + size - start > want:
            out.append((start, stop))
            start = stop
        stop += size
    if stop > start:
        out.append((start, stop))
    return out


def _blocks(array: xarray.DataArray, windows: list) -> list[tuple]:
    """The reads covering `windows`, in order: (rows, cols, [window numbers]).

    A dask array reads each y chunk once, whole across x; a lazy one reads each window.
    """
    if array.chunks is None:
        return [(slice(y, y + h), slice(x, x + w), [i]) for i, (x, y, w, h) in enumerate(windows)]
    ends = np.cumsum(array.chunks[-2])
    groups: dict = {}
    for i, (_, y, _, _) in enumerate(windows):
        groups.setdefault(int(np.searchsorted(ends, y, side="right")), []).append(i)
    out = []
    for block in groups.values():
        y0 = min(windows[i][1] for i in block)
        y1 = max(windows[i][1] + windows[i][3] for i in block)
        out.append((slice(y0, y1), slice(None), block))
    return out


def _reduce(arrays, other, shape, fills, reducer, fractional, budget) -> list[np.ndarray]:
    """The (..., zone, statistic) values of `arrays`, holding about `budget` bytes at once.

    The arrays share a grid, dims and chunks.
    """
    stats = reducer.stats
    n = reducer.feature_count
    windows = reducer.windows()
    blocks = _blocks(arrays[0], windows)
    # float32 in, float32 out (as xarray reduces), else float64: the result is a big part of
    # the memory of a long season of many fields.
    outs = [
        np.full(
            (*shape, n, len(stats)),
            np.nan,
            np.float32 if a.dtype in (np.float16, np.float32) else np.float64,
        )
        for a in arrays
    ]
    everything = list(range(len(arrays)))
    if other:
        # Bytes held for one index of the first dim of each array: its largest read (a dask
        # chunk is computed whole, so its full height and the array's width) and the zone values
        # a sorted statistic keeps, about twice the largest strip's (zones open across a strip
        # boundary).
        def read_pixels(rows: slice, cols: slice) -> int:
            if arrays[0].chunks is not None:
                ends = np.cumsum(arrays[0].chunks[-2])
                first = int(np.searchsorted(ends, rows.start, side="right"))
                last = int(np.searchsorted(ends, rows.stop - 1, side="right"))
                return int(ends[last] - (ends[first - 1] if first else 0)) * arrays[0].shape[-1]
            return (rows.stop - rows.start) * (cols.stop - cols.start)

        largest = max((read_pixels(r, c) for r, c, _ in blocks), default=0)
        kept = 2 * max((w * h for _, _, w, h in windows), default=0)
        kept *= (16 if fractional else 8) * any(
            s in _SORTED or s.startswith("quantile") for s in stats
        )
        per = math.prod(shape[1:])
        pixels = [per * largest * a.dtype.itemsize for a in arrays]
        size = [p + per * kept for p in pixels]
        chunk = (arrays[0].chunksizes.get(other[0]) or (1,))[0] if arrays[0].chunks else 1
        # The next batch is read while one is reduced when two fit the budget.
        ahead = 2 * max(size) * chunk <= budget
        budget = budget // 2 if ahead else budget
        if max(size) * chunk > budget:
            v = size.index(max(size))
            mib = 2**20
            warnings.warn(
                f"zonal_stats holds about {size[v] * chunk / mib:.0f} MiB for one chunk of "
                f"{arrays[v].name!r}, more than batch_bytes ({budget / mib:.0f} MiB), "
                f"{pixels[v] * chunk / mib:.0f} MiB of it pixels: smaller chunks hold fewer",
                RuntimeWarning,
                stacklevel=_outside_level(),
            )
        # Arrays read together: as many as one index of them fits the budget, at least one.
        sets: list[list[int]] = [[]]
        used = 0
        for v in everything:
            if sets[-1] and used + size[v] > budget:
                sets, used = [*sets, []], 0
            sets[-1].append(v)
            used += size[v]
        batches: list[tuple[list[int], tuple[int, int] | None]] = [
            (vs, batch)
            for vs in sets
            for batch in _batches(arrays[0], other[0], sum(size[v] for v in vs), budget)
        ]
    else:
        batches, ahead = [(everything, None)], False  # 2-D arrays are one batch

    def read(vs: list, batch: tuple[int, int] | None, block: tuple) -> list:
        rows, cols, _ = block
        subs = [arrays[v] for v in vs]
        subs = [a if batch is None else a.isel({other[0]: slice(*batch)}) for a in subs]
        subs = [s[..., rows, cols].data for s in subs]
        if any(hasattr(s, "dask") for s in subs):
            import dask

            subs = dask.compute(*subs)  # one computation: shared inputs computed once
        return [np.asarray(s) for s in subs]

    def put(start: int, streams: list) -> None:
        for v, idx, stream in streams:
            columns = stream.finish()
            at = (start + idx[0], *idx[1:]) if idx else ()
            for j, stat in enumerate(stats):
                outs[v][(*at, slice(None), j)] = columns[stat]

    if not windows:  # no zone pixel on the grid: every slice reduces nothing
        for out, fill in zip(outs, fills, strict=True):
            columns = reducer.stream(fill).finish()
            for j, stat in enumerate(stats):
                out[..., j] = columns[stat]
        return outs
    # Reads in order, batch after batch, each reduced strip by strip; with `ahead`, the reader
    # computes the next one meanwhile.
    jobs = [(vs, batch, b) for vs, batch in batches for b in range(len(blocks))]
    if not jobs:  # an empty first dim
        return outs
    with ThreadPoolExecutor() as pool, ThreadPoolExecutor(1) as reader:
        future = reader.submit(read, *jobs[0][:2], blocks[0])
        for k, (vs, batch, b) in enumerate(jobs):
            data = dict(zip(vs, future.result(), strict=True))
            following = jobs[k + 1] if k + 1 < len(jobs) else None
            if following and ahead:
                future = reader.submit(read, *following[:2], blocks[following[2]])
            if b == 0:
                streams = [
                    (v, idx, reducer.stream(fills[v]))
                    for v, block in data.items()
                    for idx in np.ndindex(block.shape[:-2])
                ]
            rows, cols, numbers = blocks[b]
            for i in numbers:
                x, y, w, h = windows[i]
                x0 = x - (cols.start or 0)
                cut = (slice(y - rows.start, y - rows.start + h), slice(x0, x0 + w))
                list(pool.map(lambda j: j[2].add(i, data[j[0]][j[1]][cut]), streams))  # noqa: B023
            if b == len(blocks) - 1:
                put(0 if batch is None else batch[0], streams)
            data = {}  # not held while the next one is read
            if following and not ahead:
                future = reader.submit(read, *following[:2], blocks[following[2]])
    return outs
