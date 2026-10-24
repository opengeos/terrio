"""Compare terrio.rio, terrio's rasterio-compatible API, with rasterio.

Usage:
    python scripts/compare_rio_with_rasterio.py [DATA_DIR] [--remote] [--output REPORT.md]
        [--only SECTION] [--verbose]

Every check runs the same code twice, once with ``rasterio`` and once with ``terrio.rio`` passed
as the module, and compares what the two runs return: arrays, masks, profiles, tags, transforms,
windows, CRSs (by EPSG code, or by PROJ parameters when there is none), geometries. Each check
states what it expects:

- ``identical``: the results must be equal (arrays bit for bit, NaNs included);
- ``tolerance``: the results may differ within stated bounds, for a stated reason (floating-point
  arithmetic in reprojections, GDAL resampling kernels at nodata edges);
- ``known``: the results are known to differ, for a stated reason; the report shows by how much.

Checks without DATA_DIR use synthetic rasters; DATA_DIR (the directory
``scripts/fetch_test_data.sh`` fills, also read from ``TERRIO_TEST_DATA``) adds checks on real
files: GeoTIFFs and COGs of several data types, multiband Landsat, DEMs with nodata, a palette
raster. ``--remote`` adds a public Cloud Optimized GeoTIFF read over HTTP. The section
``examples`` runs the scripts of ``scripts/rio_examples`` (rasterio documentation examples) with
rasterio and, unchanged but for the import, with terrio.rio, and compares their output and files.

Prints a Markdown agreement table; exits non-zero when a result differs more than its check
allows. Needs rasterio and terrio (with NumPy and affine); shapely for the polygon comparisons.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import warnings
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import numpy as np

warnings.simplefilter("ignore")

REMOTE_COG = (
    "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/"
    "USGS_1_n40w106.tif"
)
EXAMPLES = Path(__file__).resolve().parent / "rio_examples"

# Real files: GeoTIFFs and COGs of several data types, with and without nodata, multiband, a
# palette, strips and tiles, overviews.
TIFFS = [
    "cog.tif",
    "dem.tif",
    "landsat.tif",
    "landsat7.tif",
    "srtm_sub.tif",
    "srtm90.tif",
    "nlcd_knoxville.tif",
    "OPERA_L3_DSWx_WTR.tif",
    "las_vegas_dem.tif",
    "usgs_dem.tif",
    "ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif",
    "gtiff_codecs/rgb.tif",
    "gtiff_codecs/u16.tif",
    "gtiff_codecs/f32.tif",
]
OTHER_FORMATS = ["lv_dem.asc", "lv_dem.img", "srtm_sub.bil", "srtm_sub.rst"]


# ─── Checks ─────────────────────────────────────────────────────────────────────────────────────


@dataclass
class Check:
    """A comparison: a function run with each module, and what agreement it expects."""

    section: str
    name: str
    func: Callable[[Any, Ctx], Any]
    expect: str = "identical"
    atol: float = 0.0
    rtol: float = 0.0
    max_frac: float = 0.0
    reason: str = ""
    needs_data: bool = False
    needs_remote: bool = False
    # The oldest rasterio whose behaviour terrio.rio follows here, as (major, minor, patch).
    min_rasterio: tuple[int, ...] = ()


@dataclass
class Ctx:
    """What checks get besides the module: directories, and the reference module."""

    data: Path | None
    out: Path
    reference: Any
    files: dict[str, Any] = field(default_factory=dict)

    def path(self, name: str) -> str:
        """The path of a real test file.

        Args:
            name: The file name, relative to the data directory.

        Returns:
            str: The path.
        """
        assert self.data is not None
        return str(self.data / name)


CHECKS: list[Check] = []


def check(section: str, name: str, **kw: Any) -> Callable[[Callable[[Any, Ctx], Any]], Callable[[Any, Ctx], Any]]:
    """Registers a check.

    Args:
        section: The section of the report.
        name: The check's name.
        **kw: `Check` fields: `expect`, `atol`, `rtol`, `max_frac`, `reason`, `needs_data`...

    Returns:
        callable: The decorator.
    """

    def deco(f: Callable[[Any, Ctx], Any]) -> Callable[[Any, Ctx], Any]:
        CHECKS.append(Check(section, name, f, **kw))
        return f

    return deco


# ─── Normalizing results ────────────────────────────────────────────────────────────────────────


def norm(v: Any) -> Any:
    """Turns a result into plain values both modules' results compare by.

    Args:
        v: A result.

    Returns:
        The plain value: CRSs as their EPSG code or PROJ parameters, transforms and windows as
        tuples, enums as names, arrays kept.
    """
    cls = type(v).__name__
    mod = type(v).__module__ or ""
    if v is None or isinstance(v, (bool, int, str)):
        return v
    if isinstance(v, float):
        return v
    if isinstance(v, np.generic):
        return v.item()
    if isinstance(v, np.ndarray):
        return v
    if cls == "CRS":
        code = v.to_epsg()
        if code is not None:
            return ("EPSG", code)
        d = v.to_dict()
        return ("PROJ", tuple(sorted((k, _round(x)) for k, x in d.items())))
    if cls == "Affine":
        return ("Affine",) + tuple(float(x) for x in v[:6])
    if cls == "Window":
        return ("Window", v.col_off, v.row_off, v.width, v.height)
    if hasattr(v, "name") and hasattr(v, "value") and "enum" in mod:
        return (cls, v.name)
    if isinstance(v, dict) or cls in ("Profile",):
        return {str(k): norm(x) for k, x in dict(v).items()}
    if isinstance(v, (list, tuple)):
        return [norm(x) for x in v]
    if hasattr(v, "_asdict"):
        return [norm(x) for x in v]
    return repr(v)


def _round(x: Any) -> Any:
    """Rounds PROJ parameter values to 9 significant digits.

    Args:
        x: The value.

    Returns:
        The rounded value.
    """
    if isinstance(x, float):
        return float(f"{x:.9g}")
    return x


@dataclass
class Outcome:
    """How two results compare."""

    status: str  # identical, tolerance, known, different, error
    detail: str = ""


def compare(a: Any, b: Any, c: Check) -> Outcome:
    """Compares two normalized results.

    Args:
        a: rasterio's result.
        b: terrio.rio's result.
        c: The check, with its tolerances.

    Returns:
        Outcome: The comparison.
    """
    worst = Outcome("identical")
    notes: list[str] = []
    # Differences beyond tolerance, listed before the tolerated ones in the detail.
    failures: list[str] = []

    def walk(x: Any, y: Any, path: str) -> bool:
        nonlocal worst
        if isinstance(x, np.ndarray) or isinstance(y, np.ndarray):
            return arrays(x, y, path)
        if isinstance(x, float) and isinstance(y, (float, int)) or isinstance(y, float) and isinstance(x, (float, int)):
            if x == y or (math.isnan(x) and math.isnan(y)):
                return True
            if c.expect == "tolerance" and abs(x - y) <= c.atol + c.rtol * abs(x):
                notes.append(f"{path}: {x!r} vs {y!r}")
                worst = Outcome("tolerance")
                return True
            failures.append(f"{path}: {x!r} != {y!r}")
            return False
        # Structures that differ still have their shared values compared, so every difference
        # is listed.
        if isinstance(x, dict) and isinstance(y, dict):
            same = set(x) == set(y)
            if not same:
                failures.append(f"{path}: keys {sorted(set(x) ^ set(y), key=str)}")
            return all([walk(x[k], y[k], f"{path}.{k}") for k in x if k in y]) and same
        if isinstance(x, (list, tuple)) and isinstance(y, (list, tuple)):
            same = len(x) == len(y)
            if not same:
                failures.append(f"{path}: length {len(x)} != {len(y)}")
            return all([walk(p, q, f"{path}[{i}]") for i, (p, q) in enumerate(zip(x, y))]) and same
        if x != y:
            failures.append(f"{path}: {str(x)[:120]} != {str(y)[:120]}")
            return False
        return True

    def arrays(x: Any, y: Any, path: str) -> bool:
        nonlocal worst
        if not (isinstance(x, np.ndarray) and isinstance(y, np.ndarray)):
            failures.append(f"{path}: {type(x).__name__} vs {type(y).__name__}")
            return False
        if np.ma.isMaskedArray(x) != np.ma.isMaskedArray(y):
            failures.append(f"{path}: masked {np.ma.isMaskedArray(x)} vs {np.ma.isMaskedArray(y)}")
            return False
        if x.shape != y.shape or x.dtype != y.dtype:
            failures.append(f"{path}: {x.shape} {x.dtype} vs {y.shape} {y.dtype}")
            return False
        if np.ma.isMaskedArray(x):
            if not np.array_equal(np.ma.getmaskarray(x), np.ma.getmaskarray(y)):
                frac = float(np.mean(np.ma.getmaskarray(x) != np.ma.getmaskarray(y)))
                if c.expect == "tolerance" and frac <= c.max_frac:
                    notes.append(f"{path}: mask differs on {frac:.3%}")
                    worst = Outcome("tolerance")
                else:
                    failures.append(f"{path}: mask differs on {frac:.3%}")
                    return False
            fx, fy = x.fill_value, y.fill_value
            both_nan = bool(np.all(np.isnan(fx)) and np.all(np.isnan(fy))) if np.asarray(fx).dtype.kind == "f" else False
            if not (fx == fy or both_nan):
                failures.append(f"{path}: fill_value {fx} vs {fy}")
                return False
            x, y = np.ma.getdata(x), np.ma.getdata(y)
        if np.array_equal(x, y, equal_nan=x.dtype.kind in "fc"):
            return True
        if x.dtype.kind in "OUS":
            failures.append(f"{path}: arrays differ")
            return False
        d = np.abs(x.astype(np.float64) - y.astype(np.float64))
        nan_mismatch = np.isnan(x.astype(np.float64)) != np.isnan(y.astype(np.float64))
        d[np.isnan(d) & ~nan_mismatch] = 0
        d[nan_mismatch] = np.inf
        # A NaN on one side only is beyond any tolerance (NaN bounds compare false).
        bad = (d > (c.atol + c.rtol * np.abs(x.astype(np.float64)))) | nan_mismatch
        frac = float(np.mean(d > 0))
        bad_frac = float(np.mean(bad))
        detail = f"{path}: {frac:.4%} of pixels differ, max {float(np.max(d)):.4g}"
        if c.expect == "tolerance" and bad_frac <= c.max_frac:
            notes.append(detail)
            worst = Outcome("tolerance")
            return True
        failures.append(detail)
        return False

    ok = walk(a, b, "")
    if not ok:
        return Outcome("known" if c.expect == "known" else "different", "; ".join(failures + notes)[:600])
    worst.detail = "; ".join(notes)[:600]
    return worst


def rasterio_version(rasterio: Any) -> tuple[int, ...]:
    """The installed rasterio's version.

    Args:
        rasterio: The rasterio module.

    Returns:
        tuple: The leading numeric parts, e.g. (1, 5, 2).
    """
    parts = []
    for p in str(rasterio.__version__).split(".")[:3]:
        digits = re.match(r"\d+", p)
        if digits is None:
            break
        parts.append(int(digits.group()))
    return tuple(parts)


def run_check(c: Check, rasterio: Any, trio: Any, data: Path | None, out: Path) -> Outcome:
    """Runs a check with both modules.

    Args:
        c: The check.
        rasterio: The rasterio module.
        trio: The terrio.rio module.
        data: The data directory, or None.
        out: A scratch directory.

    Returns:
        Outcome: The comparison (`error` when a run raised and the other did not).
    """
    results = []
    for mod, sub in ((rasterio, "rasterio"), (trio, "terrio")):
        d = out / sub / re.sub(r"\W+", "_", f"{c.section}_{c.name}")
        d.mkdir(parents=True, exist_ok=True)
        try:
            results.append(("ok", norm(c.func(mod, Ctx(data, d, rasterio)))))
        except Exception as err:  # Both may raise the same error: misuse checks.
            results.append(("raise", f"{type(err).__name__}"))
    (sa, a), (sb, b) = results
    if sa != sb or (sa == "raise" and a != b):
        return Outcome("error", f"rasterio: {sa} {a if sa == 'raise' else ''} | terrio.rio: {sb} {b if sb == 'raise' else ''}")
    if sa == "raise":
        return Outcome("identical", f"both raise {a}")
    return compare(a, b, c)


# ─── Synthetic data ─────────────────────────────────────────────────────────────────────────────


def synthetic(rio: Any, ctx: Ctx, name: str = "synth.tif", overviews: bool = True, **kw: Any) -> str:
    """Writes a synthetic GeoTIFF with the reference module (rasterio), shared by both runs.

    Args:
        rio: Unused (the reference writes the file).
        ctx: The context.
        name: The file name.
        overviews: Build overviews (by 2 and 4).
        **kw: Profile overrides.

    Returns:
        str: The path.
    """
    ref = ctx.reference
    path = ctx.out.parent.parent / f"input_{name}"
    if path.exists():
        return str(path)
    rng = np.random.default_rng(42)
    profile = dict(
        driver="GTiff",
        width=301,
        height=203,
        count=3,
        dtype="int16",
        crs="EPSG:32611",
        transform=ref.transform.from_origin(500000, 4100000, 30, 30),
        nodata=-9999,
        tiled=True,
        blockxsize=64,
        blockysize=64,
        compress="deflate",
    )
    profile.update(kw)
    shape = (profile["count"], profile["height"], profile["width"])
    yy, xx = np.mgrid[0 : shape[1], 0 : shape[2]]
    data = np.stack([(xx * (b + 1) + yy * 3) % 2000 for b in range(shape[0])]).astype(profile["dtype"])
    data = data + rng.integers(0, 5, shape).astype(profile["dtype"])
    if profile.get("nodata") is not None:
        data[:, 10:30, 40:80] = profile["nodata"]
    with ref.open(str(path), "w", **profile) as dst:
        dst.write(data)
        if overviews:
            dst.build_overviews([2, 4], ref.enums.Resampling.nearest)
        dst.update_tags(source="synthetic")
        dst.set_band_description(1, "first")
    return str(path)


def meta_of(ds: Any) -> dict[str, Any]:
    """A dataset's metadata, as rasterio reports it.

    Args:
        ds: A dataset.

    Returns:
        dict: Profile, tags, bands' metadata, masks' sources, blocks, overviews, color table.
    """
    try:
        cmap = ds.colormap(1)
    except ValueError:
        cmap = None
    return {
        "profile": ds.profile,
        "bounds": ds.bounds,
        "res": ds.res,
        "shape": ds.shape,
        "indexes": ds.indexes,
        "dtypes": ds.dtypes,
        "nodatavals": ds.nodatavals,
        "tags": ds.tags(),
        "band_tags": [ds.tags(i) for i in ds.indexes],
        "image_structure": ds.tags(ns="IMAGE_STRUCTURE"),
        "descriptions": ds.descriptions,
        "colorinterp": ds.colorinterp,
        "scales": ds.scales,
        "offsets": ds.offsets,
        "units": ds.units,
        "mask_flag_enums": ds.mask_flag_enums,
        "block_shapes": ds.block_shapes,
        "overviews": [ds.overviews(i) for i in ds.indexes],
        "colormap": cmap,
        "compression": ds.compression,
        "interleaving": ds.interleaving,
    }


# ─── Helpers (no files) ─────────────────────────────────────────────────────────────────────────


@check("helpers", "transform: from_origin, from_bounds, array_bounds")
def _transforms(rio: Any, ctx: Ctx) -> Any:
    t = rio.transform.from_origin(-120.5, 38.25, 0.001, 0.002)
    t2 = rio.transform.from_bounds(-120, 37, -119, 38, 377, 211)
    return [t, t2, rio.transform.array_bounds(211, 377, t2), rio.transform.guard_transform(t)]


@check("helpers", "transform: rowcol, xy (offsets, arrays, op)")
def _rowcol(rio: Any, ctx: Ctx) -> Any:
    t = rio.transform.from_origin(500000, 4100000, 30, 30)
    xs = [500000.0, 500015.5, 503333.3, 499990.0]
    ys = [4100000.0, 4099985.0, 4096666.6, 4100010.0]
    out = [rio.transform.rowcol(t, xs, ys), rio.transform.rowcol(t, 500045.0, 4099955.0)]
    out.append(rio.transform.rowcol(t, xs, ys, op=math.ceil))
    out.append(rio.transform.rowcol(t, xs, ys, op=np.round))
    for off in ("center", "ul", "ur", "ll", "lr"):
        out.append(rio.transform.xy(t, [0, 5, 100], [0, 7, 3], offset=off))
    out.append(rio.transform.xy(t, 3, 4))
    with rio.transform.AffineTransformer(t) as tr:
        out.append(tr.rowcol(xs, ys))
        out.append(tr.xy(2, 3))
    return out


@check("helpers", "transform: invalid offset raises TransformError")
def _bad_offset(rio: Any, ctx: Ctx) -> Any:
    return rio.transform.xy(rio.transform.from_origin(0, 0, 1, 1), 1, 1, offset="middle")


@check("helpers", "transform: GDAL geotransform tuple raises TypeError")
def _guard(rio: Any, ctx: Ctx) -> Any:
    return rio.transform.guard_transform((500000.0, 30.0, 0.0, 4100000.0, 0.0, -30.0))


@check("helpers", "windows: from_bounds, bounds, transform, rounding")
def _windows(rio: Any, ctx: Ctx) -> Any:
    W = rio.windows
    t = rio.transform.from_origin(500000, 4100000, 30, 30)
    w = W.from_bounds(500100, 4096000, 503000, 4099500, transform=t)
    out = [w, w.round_offsets(), w.round_lengths(), w.round(2), W.bounds(w, t), W.transform(w, t)]
    out += [w.toslices(), w.toranges(), w.flatten(), dict(w.todict())]
    out.append(W.Window(-5, -3, 10, 10).crop(100, 100))
    out.append(W.Window.from_slices((1, 5), (2, 9)))
    out.append(W.Window.from_slices(slice(-5, None), slice(None, -2), height=100, width=50))
    out.append(W.Window.from_slices((-5, 10), (-2, 8), boundless=True))
    out.append(W.evaluate(((1, 3), (4, 8)), 10, 10))
    out.append(W.shape(W.Window(1, 2, 3, 4)))
    out.append(W.window_index(W.Window(1.5, 2.5, 3, 4)))
    return out


@check("helpers", "windows: intersection, union, intersect, subdivide, full blocks")
def _windows_set(rio: Any, ctx: Ctx) -> Any:
    W = rio.windows
    a, b, c = W.Window(0, 0, 10, 10), W.Window(5, 5, 10, 10), W.Window(20, 20, 2, 2)
    out = [W.intersection(a, b), W.intersection([a, b]), W.union(a, b, c), W.intersect(a, c), W.intersect([a, b])]
    out.append(a.intersection(b))
    out.append(W.subdivide(W.Window(0, 0, 25, 13), 10, 10))
    out.append(W.round_window_to_full_blocks(W.Window(3, 5, 30, 40), [(16, 16)]))
    out.append(W.get_data_window(np.pad(np.ones((3, 4)), 2), nodata=0))
    return out


@check("helpers", "windows: empty intersection raises WindowError")
def _windows_empty(rio: Any, ctx: Ctx) -> Any:
    return rio.windows.intersection(rio.windows.Window(0, 0, 1, 1), rio.windows.Window(5, 5, 1, 1))


@check("helpers", "windows: from_bounds inconsistent with transform raises WindowError")
def _windows_bad(rio: Any, ctx: Ctx) -> Any:
    t = rio.transform.from_origin(0, 10, 1, 1)
    return rio.windows.from_bounds(5, 5, 1, 9, transform=t)


@check("helpers", "crs: EPSG codes, strings, units, equality")
def _crs(rio: Any, ctx: Ctx) -> Any:
    C = rio.crs.CRS
    out = []
    for s in ["EPSG:4326", "EPSG:3857", "EPSG:32611", "EPSG:2263", "EPSG:5070", "epsg:4269"]:
        c = C.from_string(s)
        out.append([c.to_epsg(), c.to_string(), c.is_geographic, c.is_projected, c.linear_units, c.units_factor, c.is_valid, c.is_epsg_code, repr(c), str(c)])
        if c.is_projected:
            out.append(c.linear_units_factor)
    a = C.from_epsg(4326)
    out.append([a == "EPSG:4326", a == 4326, a == C.from_user_input("EPSG:4326"), a == C.from_epsg(3857)])
    out.append(C.from_user_input(32611).to_epsg())
    out.append(C.from_dict(init="epsg:3857").to_epsg())
    out.append(C.from_proj4("+proj=utm +zone=13 +datum=WGS84 +units=m +no_defs").to_epsg())
    out.append(C.from_wkt(C.from_epsg(32633).to_wkt()).to_epsg())
    out.append(C.from_epsg(4326).to_dict())
    out.append(C.from_epsg(32611).to_dict())
    return out


@check("helpers", "crs: ESRI codes, and the ESRI CRS of a GeoTIFF GDAL wrote")
def _crs_esri(rio: Any, ctx: Ctx) -> Any:
    C = rio.crs.CRS
    out = []
    # (Lowercase `esri:54009`, which PROJ reads, rasterio refuses.)
    for s in ["ESRI:54009", "ESRI:102003"]:
        c = C.from_string(s)
        out.append([c.to_epsg(), c.to_authority(), c.to_string(), c.is_epsg_code, c.is_projected, c.linear_units])
    out.append([C.from_user_input("ESRI:102003") == "ESRI:102003", C.from_string("ESRI:54009") == "EPSG:4326"])
    # GDAL writes it as a user-defined CRS with its ESRI PE string, which GDAL identifies.
    path = str(ctx.out / "mollweide.tif")
    t = ctx.reference.transform.from_origin(0, 0, 1000, 1000)
    with ctx.reference.open(path, "w", driver="GTiff", width=4, height=4, count=1, dtype="float32", crs="ESRI:54009", transform=t) as dst:
        dst.write(np.zeros((1, 4, 4), "float32"))
    with rio.open(path) as ds:
        c = ds.crs
        out.append([c.to_string(), c.to_authority(), c.to_epsg(), c.is_epsg_code, c == "ESRI:54009", c.linear_units])
    return out


@check("helpers", "crs: invalid definitions raise CRSError")
def _crs_bad(rio: Any, ctx: Ctx) -> Any:
    return rio.crs.CRS.from_string("not a crs")


@check("helpers", "crs: geographic CRS has no linear units factor (CRSError)")
def _crs_lin(rio: Any, ctx: Ctx) -> Any:
    return rio.crs.CRS.from_epsg(4326).linear_units_factor


@check("helpers", "dtypes, coords, profiles, enums")
def _misc(rio: Any, ctx: Ctx) -> Any:
    D = rio.dtypes
    out = [D.get_minimum_dtype([0, 200]), D.get_minimum_dtype([-1, 200]), D.get_minimum_dtype([0, 70000]), D.get_minimum_dtype([0.5, 1e39])]
    out += [D.can_cast_dtype([1, 2], "uint8"), D.can_cast_dtype([1.5], "int16"), D.check_dtype("uint8"), D.check_dtype("foo"), D.in_dtype_range(300, "uint8")]
    out += [D._gdal_typename("float32"), D.uint8, D.float64]
    out.append(rio.coords.BoundingBox(1, 2, 3, 4))
    out.append(rio.coords.disjoint_bounds((0, 0, 1, 1), (2, 2, 3, 3)))
    out.append(dict(rio.profiles.DefaultGTiffProfile(count=3)))
    out.append([(r.name, int(r)) for r in rio.enums.Resampling])
    out.append([(r.name, int(r)) for r in rio.enums.ColorInterp])
    out.append([(r.name, int(r)) for r in rio.enums.MaskFlags])
    return out


@check("helpers", "features: bounds, is_valid_geom")
def _fbounds(rio: Any, ctx: Ctx) -> Any:
    poly = {"type": "Polygon", "coordinates": [[(0, 0), (3, 0), (3, 2), (0, 2), (0, 0)]]}
    fc = {"type": "FeatureCollection", "features": [{"type": "Feature", "geometry": poly, "properties": {}}]}
    # Not compared: bounds with a transform, whose results changed between rasterio 1.5.0 and 1.5.2.
    return [rio.features.bounds(poly), rio.features.bounds(fc), rio.features.bounds(poly, north_up=False), rio.features.is_valid_geom(poly), rio.features.is_valid_geom({"type": "Polygon", "coordinates": [[(0, 0)]]})]


# ─── Synthetic dataset checks ───────────────────────────────────────────────────────────────────


@check("synthetic", "metadata of a tiled int16 GeoTIFF with nodata and overviews")
def _s_meta(rio: Any, ctx: Ctx) -> Any:
    with rio.open(synthetic(rio, ctx)) as ds:
        return meta_of(ds)


@check("synthetic", "reads: full, bands, windows, fractional windows")
def _s_reads(rio: Any, ctx: Ctx) -> Any:
    W = rio.windows.Window
    with rio.open(synthetic(rio, ctx)) as ds:
        return [ds.read(), ds.read(2), ds.read([3, 1]), ds.read(window=W(10, 20, 100, 50)), ds.read(1, window=((5, 25), (7, 70))), ds.read(window=W(10.4, 20.6, 100.3, 50.7)), ds.read(window=W(250, 150, 100, 100))]


@check("synthetic", "reads: masked, fill_value, out_dtype, out")
def _s_masked(rio: Any, ctx: Ctx) -> Any:
    with rio.open(synthetic(rio, ctx)) as ds:
        out = np.zeros((3, 203, 301), "int16")
        ds.read(out=out)
        return [ds.read(masked=True), ds.read(1, masked=True, window=((0, 50), (0, 100))), ds.read(fill_value=7), ds.read(out_dtype="uint8"), ds.read(1, out_dtype="float32"), out]


@check("synthetic", "reads: boundless (filled, masked, fill_value)")
def _s_boundless(rio: Any, ctx: Ctx) -> Any:
    W = rio.windows.Window
    with rio.open(synthetic(rio, ctx)) as ds:
        return [ds.read(window=W(-20, -10, 100, 80), boundless=True), ds.read(1, window=W(250, 180, 100, 60), boundless=True, masked=True), ds.read(window=W(-5, -5, 20, 20), boundless=True, fill_value=3)]


@check("synthetic", "reads: out_shape nearest (decimation through overviews, upsampling)")
def _s_outshape(rio: Any, ctx: Ctx) -> Any:
    W = rio.windows.Window
    with rio.open(synthetic(rio, ctx)) as ds:
        return [ds.read(out_shape=(3, 101, 150)), ds.read(out_shape=(3, 50, 75)), ds.read(1, out_shape=(37, 41)), ds.read(1, window=W(0, 0, 40, 30), out_shape=(90, 120)), ds.read(1, window=W(3.5, 4.25, 60.5, 33.3), out_shape=(20, 30)), ds.read(masked=True, out_shape=(3, 60, 90))]


@check(
    "synthetic",
    "reads: out_shape bilinear, cubic, average, mode",
    expect="tolerance",
    atol=1,
    max_frac=0.05,
    reason="GDAL's resampling kernels next to nodata pixels",
)
def _s_resampled(rio: Any, ctx: Ctx) -> Any:
    R = rio.enums.Resampling
    with rio.open(synthetic(rio, ctx)) as ds:
        return [ds.read(out_shape=(3, 90, 133), resampling=r) for r in (R.bilinear, R.cubic, R.average, R.mode)]


@check("synthetic", "masks: read_masks, dataset_mask, mask flags")
def _s_masks(rio: Any, ctx: Ctx) -> Any:
    W = rio.windows.Window
    with rio.open(synthetic(rio, ctx)) as ds:
        return [ds.read_masks(), ds.read_masks(2, window=W(30, 0, 60, 40)), ds.dataset_mask(), ds.dataset_mask(out_shape=(50, 75)), ds.mask_flag_enums]


@check("synthetic", "masks: .msk sidecars written and read (GDAL_TIFF_INTERNAL_MASK=NO, ENVI), kept on update")
def _s_msk_sidecars(rio: Any, ctx: Ctx) -> Any:
    W = rio.windows.Window
    with rio.open(synthetic(rio, ctx)) as ds:
        data, profile = ds.read(), ds.profile
    mask = np.where(data[0] > 0, 255, 0).astype("uint8")
    out = []
    with rio.Env(GDAL_TIFF_INTERNAL_MASK=False):
        with rio.open(str(ctx.out / "ext.tif"), "w", **profile) as dst:
            dst.write(data)
            dst.write_mask(mask)
    plain = {k: v for k, v in profile.items() if k not in ("tiled", "blockxsize", "blockysize", "compress", "interleave")}
    with rio.open(str(ctx.out / "m.envi"), "w", **{**plain, "driver": "ENVI"}) as dst:
        dst.write(data)
        dst.write_mask(mask)
    with rio.open(str(ctx.out / "ext.tif"), "r+") as dst:
        dst.write(data[:1] + 1, 1)
    # With overviews: the sidecar gets its own, read for lower resolutions.
    with rio.Env(GDAL_TIFF_INTERNAL_MASK=False):
        with rio.open(str(ctx.out / "ovr.tif"), "w", **profile) as dst:
            dst.write(data)
            dst.write_mask(mask)
            dst.build_overviews([2, 4], rio.enums.Resampling.nearest)
    # Written again without a mask: the earlier sidecar goes.
    with rio.open(str(ctx.out / "gone.tif"), "w", **profile) as dst:
        dst.write(data)
        dst.write_mask(mask)
    with rio.Env(GDAL_TIFF_INTERNAL_MASK=False):
        with rio.open(str(ctx.out / "gone.tif"), "w", **profile) as dst:
            dst.write(data)
            dst.write_mask(mask)
    with rio.open(str(ctx.out / "gone.tif"), "w", **profile) as dst:
        dst.write(data)
    out.append(os.path.exists(str(ctx.out / "gone.tif.msk")))
    for name in ("ext.tif", "m.envi", "ovr.tif"):
        with rio.open(str(ctx.out / name)) as ds:
            files = sorted(os.path.basename(f) for f in ds.files if not f.endswith(".aux.xml"))
            out.append([files, ds.mask_flag_enums, ds.read_masks(), ds.read_masks(2, window=W(30, 0, 60, 40)), ds.dataset_mask(out_shape=(50, 75))])
        with rio.open(str(ctx.out / name) + ".msk") as m:
            out.append([m.tags(), m.read(1), m.overviews(1)])
    return out


@check("synthetic", "sample, index, xy, window helpers, block windows")
def _s_sample(rio: Any, ctx: Ctx) -> Any:
    with rio.open(synthetic(rio, ctx)) as ds:
        pts = [(500015, 4099985), (504000, 4096000), (501300, 4099500), (400000, 4000000)]
        w = ds.window(501000, 4096000, 505000, 4099000)
        return [list(ds.sample(pts)), list(ds.sample(pts, indexes=2)), [x.tolist() if hasattr(x, "tolist") else x for x in ds.sample(pts, masked=True)], ds.index(501234, 4098765), ds.xy(10, 20), ds.xy(10, 20, offset="ul"), w, ds.window_transform(w), ds.window_bounds(w), list(ds.block_windows(1)), ds.block_window(1, 1, 2)]


@check("synthetic", "checksum, lnglat, statistics", expect="tolerance", rtol=1e-12, reason="sums in another order")
def _s_stats(rio: Any, ctx: Ctx) -> Any:
    with rio.open(synthetic(rio, ctx)) as ds:
        stats = [[s.min, s.max, s.mean, s.std] for s in ds.stats()]
        return [ds.checksum(1), ds.checksum(2, window=rio.windows.Window(5, 7, 50, 40)), ds.lnglat(), stats]


@check("synthetic", "misuse: band 0, band out of range, reading in w mode")
def _s_misuse(rio: Any, ctx: Ctx) -> Any:
    out = []
    with rio.open(synthetic(rio, ctx)) as ds:
        for f in (lambda: ds.read(0), lambda: ds.read(4), lambda: ds.read([]), lambda: ds.read(out_shape=(1, 2), out=np.zeros((3, 1, 2), "int16"))):
            try:
                f()
                out.append("no error")
            except Exception as err:
                out.append(type(err).__name__)
    with rio.open(str(ctx.out / "w.tif"), "w", driver="GTiff", width=4, height=4, count=1, dtype="uint8") as dst:
        try:
            dst.read()
            out.append("no error")
        except Exception as err:
            out.append(type(err).__name__)
    for kw in (dict(width=None), dict(dtype=None), dict(nodata=300), dict(tiled=True, blockxsize=10, blockysize=10)):
        p = dict(driver="GTiff", width=40, height=40, count=1, dtype="uint8")
        p.update(kw)
        try:
            rio.open(str(ctx.out / "bad.tif"), "w", **p).close()
            out.append("no error")
        except Exception as err:
            out.append(type(err).__name__)
    try:
        rio.open(str(ctx.out / "missing.tif"))
        out.append("no error")
    except Exception as err:
        out.append(type(err).__name__)
    return out


# ─── Writing: written by each module, read back by rasterio ─────────────────────────────────────


WRITE_CASES: list[tuple[str, dict[str, Any]]] = [
    ("float32 nodata strips", dict(count=1, dtype="float32", nodata=-9999.0)),
    ("RGB Byte", dict(count=3, dtype="uint8")),
    ("RGBA Byte", dict(count=4, dtype="uint8")),
    ("int16 tiled deflate band-interleaved", dict(count=2, dtype="int16", tiled=True, blockxsize=64, blockysize=32, compress="deflate", interleave="band")),
    ("uint16 lzw predictor", dict(count=1, dtype="uint16", compress="lzw", predictor=2)),
    ("float32 deflate floating-point predictor", dict(count=2, dtype="float32", compress="deflate", predictor=3, tiled=True)),
    ("int16 zstd predictor, pixel-interleaved", dict(count=3, dtype="int16", compress="zstd", predictor=2)),
    ("float64 NaN nodata tiled", dict(count=3, dtype="float64", nodata=float("nan"), tiled=True)),
    ("int32 zstd", dict(count=1, dtype="int32", compress="zstd")),
    ("Byte gray + alpha", dict(count=2, dtype="uint8", alpha="yes")),
    ("int8", dict(count=1, dtype="int8")),
    ("uint32 packbits", dict(count=1, dtype="uint32", compress="packbits")),
    ("COG driver", dict(count=1, dtype="uint8", driver="COG")),
    ("no georeferencing", dict(count=1, dtype="uint8", transform=None, crs=None)),
]


def _write_case(rio: Any, ctx: Ctx, kw: dict[str, Any], extra: bool) -> str:
    """Writes a raster with a module.

    Args:
        rio: The module.
        ctx: The context.
        kw: Profile options.
        extra: Write in two windows, with tags, descriptions, units, scales, offsets.

    Returns:
        str: The path.
    """
    rng = np.random.default_rng(1)
    p: dict[str, Any] = dict(driver="GTiff", width=150, height=97, crs="EPSG:32611", transform=rio.transform.from_origin(500000, 4100000, 30, 30))
    p.update(kw)
    n, dt = p["count"], p["dtype"]
    if np.dtype(dt).kind == "f":
        data = rng.normal(100, 50, (n, 97, 150)).astype(dt)
    else:
        info = np.iinfo(dt)
        data = rng.integers(max(info.min, -1000), min(info.max, 1000), (n, 97, 150)).astype(dt)
    path = str(ctx.out / "out.tif")
    with rio.open(path, "w", **p) as dst:
        if extra:
            dst.write(data[:, :50], window=rio.windows.Window(0, 0, 150, 50))
            dst.write(data[0, 50:], 1, window=rio.windows.Window(0, 50, 150, 47))
            if n > 1:
                dst.write(data[1:, 50:], list(range(2, n + 1)), window=rio.windows.Window(0, 50, 150, 47))
            dst.update_tags(a="1", b="x<y&z")
            dst.update_tags(1, foo="bar")
            dst.set_band_description(1, "first")
            dst.set_band_unit(1, "m")
            dst.scales = [2.0] + [1.0] * (n - 1)
            dst.offsets = [0.5] + [0.0] * (n - 1)
            if n == 1 and dt == "uint8":
                dst.write_colormap(1, {0: (255, 0, 0), 1: (0, 255, 0, 128), 7: (1, 2, 3, 4)})
        else:
            dst.write(data)
    return path


for _name, _kw in WRITE_CASES:
    for _extra in (False, True):

        def _make(kw: dict[str, Any] = _kw, extra: bool = _extra) -> Callable[[Any, Ctx], Any]:
            def f(rio: Any, ctx: Ctx) -> Any:
                path = _write_case(rio, ctx, kw, extra)
                with ctx.reference.open(path) as ds:
                    return [meta_of(ds), ds.read(), ds.read_masks()]

            return f

        check("writing", f"{_name}{' (windows, metadata)' if _extra else ''}: read back by rasterio")(_make())


@check("writing", "files rasterio writes, read by each module")
def _w_cross(rio: Any, ctx: Ctx) -> Any:
    out = []
    for name, kw in WRITE_CASES:
        path = _write_case(ctx.reference, ctx, kw, True)
        with rio.open(path) as ds:
            out.append([meta_of(ds), ds.read(), ds.read_masks()])
    return out


@check("writing", "r+ update: windowed write, tags, nodata")
def _w_update(rio: Any, ctx: Ctx) -> Any:
    src = synthetic(rio, ctx)
    path = str(ctx.out / "update.tif")
    shutil.copy(src, path)
    with rio.open(path, "r+") as ds:
        a = ds.read(1, window=rio.windows.Window(0, 0, 50, 50))
        ds.write(a * 2, 1, window=rio.windows.Window(10, 10, 50, 50))
        ds.update_tags(edited="yes")
        ds.update_tags(2, band="two")
        ds.nodata = -1
    with ctx.reference.open(path) as ds:
        return [meta_of(ds), ds.read()]


@check("writing", "build_overviews nearest, average, mode (read through rasterio)", expect="tolerance", atol=1, max_frac=0.005, reason="GDAL's averaging and mode tie-breaking arithmetic on a few pixels")
def _w_ovr(rio: Any, ctx: Ctx) -> Any:
    out = []
    for r in ("nearest", "average", "mode"):
        path = str(ctx.out / f"ovr_{r}.tif")
        shutil.copy(synthetic(rio, ctx, "synth_noovr.tif", overviews=False), path)
        with rio.open(path, "r+") as ds:
            ds.build_overviews([2, 4, 8], rio.enums.Resampling[r])
        with ctx.reference.open(path) as ds:
            out.append([ds.overviews(1), [ds.read(out_shape=(3, -(-203 // f), -(-301 // f))) for f in (2, 4, 8)]])
    return out


@check("writing", "open in w mode without a driver: the driver from the extension")
def _w_driver_from_ext(rio: Any, ctx: Ctx) -> Any:
    out = []
    data = np.arange(12, dtype="uint8").reshape(1, 3, 4)
    for ext in (".tif", ".asc", ".png", ".nc"):
        path = str(ctx.out / f"no_driver_{rio.__name__.replace('.', '_')}{ext}")
        with rio.open(path, "w", width=4, height=3, count=1, dtype="uint8", crs="EPSG:32611", transform=rio.transform.from_origin(500000, 4100000, 30, 30)) as dst:
            dst.write(data)
        with ctx.reference.open(path) as ds:
            out.append([ext, ds.driver, ds.read()])
    return out


@check("writing", "MemoryFile: write then read back")
def _w_memfile(rio: Any, ctx: Ctx) -> Any:
    with rio.open(synthetic(rio, ctx)) as src:
        profile, data = src.profile, src.read()
    with rio.io.MemoryFile() as mem:
        with mem.open(**profile) as dst:
            dst.write(data)
        buf = mem.read()
    with rio.io.MemoryFile(buf) as mem, mem.open() as ds:
        return [ds.read(), ds.profile["blockxsize"], ds.nodata]


# ─── Georeferencing, masks, nodata, file tools ──────────────────────────────────────────────────


def _sub(rio: Any, name: str) -> Any:
    """A submodule of the module under test, such as `shutil` or `stack`.

    Args:
        rio: `rasterio` or `terrio.rio`.
        name: The submodule.

    Returns:
        module: The submodule.
    """
    import importlib

    return importlib.import_module(f"{rio.__name__}.{name}")


def _small(rio: Any, path: str, transform: Any, **kw: Any) -> None:
    """Writes a small 3-band uint8 GeoTIFF.

    Args:
        rio: The module writing it.
        path: The output path.
        transform: Its transform.
        **kw: Profile overrides.
    """
    data = (np.arange(3 * 40 * 50).reshape(3, 40, 50) % 251).astype("uint8")
    profile = dict(driver="GTiff", width=50, height=40, count=3, dtype="uint8", crs="EPSG:32611", transform=transform)
    profile.update(kw)
    with rio.open(path, "w", **profile) as dst:
        dst.write(data)


def _gcps(rio: Any, n: int) -> list[Any]:
    """Ground control points over a 150 x 120 raster: 4 (corners) or 16 (a grid), with distortion.

    Args:
        rio: The module whose `GroundControlPoint` to use.
        n: 4 or 16.

    Returns:
        list: The points.
    """
    gcp = _sub(rio, "control").GroundControlPoint
    steps = ((0, 150), (0, 120)) if n == 4 else ((0, 50, 100, 150), (0, 40, 80, 120))
    out = []
    for r in steps[1]:
        for c in steps[0]:
            x = 500000 + c * 10 + r * 1.5 + 0.002 * c * r
            y = 4100000 - r * 10 + c * 0.8 - 0.0005 * c * c
            out.append(gcp(row=r, col=c, x=x, y=y, z=0.0))
    return out


@check("georeferencing", "rotated and south-up transforms: written, read back by both, windows")
def _g_rotated(rio: Any, ctx: Ctx) -> Any:
    out = []
    for name, t in (("rot", rio.Affine(10, 2, 500000, 1.5, -10, 4100000)), ("south", rio.Affine(10, 0, 500000, 0, 10, 4000000))):
        path = str(ctx.out / f"{name}.tif")
        _small(rio, path, t)
        for reader in (rio, ctx.reference):
            with reader.open(path) as ds:
                out.append([tuple(ds.transform), ds.bounds, ds.res, ds.read(), ds.read(2, window=((5, 15), (7, 20)))])
    return out


@check("georeferencing", "rotated and south-up sources reprojected (nearest, bilinear, to EPSG:4326)", expect="tolerance", rtol=1e-12, atol=1e-9, reason="PROJ and terrio's projection arithmetic differ in the last digits")
def _g_rotated_reproject(rio: Any, ctx: Ctx) -> Any:
    out = []
    for name, t in (("rot", rio.Affine(10, 2, 500000, 1.5, -10, 4100000)), ("south", rio.Affine(10, 0, 500000, 0, 10, 4000000))):
        path = str(ctx.out / f"{name}.tif")
        _small(ctx.reference, path, t)
        with rio.open(path) as src:
            for crs, r in (("EPSG:32611", "nearest"), ("EPSG:32611", "bilinear"), ("EPSG:4326", "cubic")):
                dst, dt = rio.warp.reproject(rio.band(src, 1), dst_crs=crs, resampling=rio.enums.Resampling[r])
                out.append([dst, tuple(dt)])
    return out


@check("georeferencing", "GCPs: written as tie points, read back by both (CRS, no transform)")
def _g_gcps(rio: Any, ctx: Ctx) -> Any:
    out = []
    for n in (4, 16):
        path = str(ctx.out / f"gcps{n}.tif")
        with rio.open(path, "w", driver="GTiff", width=150, height=120, count=1, dtype="uint8") as dst:
            dst.write(np.ones((1, 120, 150), "uint8"))
            dst.gcps = (_gcps(rio, n), "EPSG:32611")
        for reader in (rio, ctx.reference):
            with reader.open(path) as ds:
                gcps, crs = ds.gcps
                out.append([[(g.row, g.col, g.x, g.y, g.z, g.id) for g in gcps], crs, ds.crs, tuple(ds.transform)])
    return out


@check("georeferencing", "reproject with GCPs: GDAL's default (homography from 4, polynomial from 16), MAX_GCP_ORDER 2 and 3, TPS", expect="tolerance", rtol=1e-12, atol=1e-9, reason="PROJ and terrio's projection arithmetic differ in the last digits")
def _g_gcp_reproject(rio: Any, ctx: Ctx) -> Any:
    rng = np.random.default_rng(1)
    src = np.cumsum((rng.random((1, 120, 150)) * 250).astype("uint8"), axis=2).astype("uint8")
    out = []
    for n, opts in ((4, {}), (16, {}), (16, {"MAX_GCP_ORDER": 2}), (16, {"MAX_GCP_ORDER": 3}), (16, {"SRC_METHOD": "GCP_TPS"}), (4, {"SRC_METHOD": "GCP_TPS"})):
        for crs in ("EPSG:32611", "EPSG:4326"):
            t = rio.warp.calculate_default_transform("EPSG:32611", crs, 150, 120, gcps=_gcps(rio, n), **opts)
            dst, dt = rio.warp.reproject(src, gcps=_gcps(rio, n), src_crs="EPSG:32611", dst_crs=crs, **opts)
            out.append([tuple(t[0]), t[1:], dst, tuple(dt)])
    return out


@check("georeferencing", "xy, rowcol and dataset xy/index with GCPs (GDAL's GCP polynomial)", expect="tolerance", rtol=1e-12, atol=1e-6, reason="least squares solved in another order")
def _g_gcp_xy(rio: Any, ctx: Ctx) -> Any:
    out = []
    for n in (4, 16):
        g = _gcps(rio, n)
        out.append(rio.transform.xy(g, [5, 60.5, 100], [3, 70, 140]))
        out.append(rio.transform.rowcol(g, [500500, 500900], [4099500, 4099100]))
        path = str(ctx.out / f"gcp_xy{n}.tif")
        with ctx.reference.open(path, "w", driver="GTiff", width=150, height=120, count=1, dtype="uint8") as dst:
            dst.write(np.zeros((1, 120, 150), "uint8"))
            dst.gcps = (_gcps(ctx.reference, n), "EPSG:32611")
        with rio.open(path) as ds:
            m = rio.enums.TransformMethod.gcps
            out.append([ds.xy(10, 20, transform_method=m), ds.index(500300, 4099800, transform_method=m)])
    return out


@check("georeferencing", "RPCs: written to the RPC tag, read back by both")
def _g_rpcs(rio: Any, ctx: Ctx) -> Any:
    items = {
        "HEIGHT_OFF": "250", "HEIGHT_SCALE": "500", "LAT_OFF": "35.6", "LAT_SCALE": "0.05", "LONG_OFF": "-115.2", "LONG_SCALE": "0.06",
        "LINE_OFF": "1000", "LINE_SCALE": "1000", "SAMP_OFF": "1200", "SAMP_SCALE": "1200",
        "LINE_NUM_COEFF": " ".join(str(0.001 * i) for i in range(20)), "LINE_DEN_COEFF": "1 " + " ".join("0" for _ in range(19)),
        "SAMP_NUM_COEFF": " ".join(str(-0.002 * i) for i in range(20)), "SAMP_DEN_COEFF": "1 " + " ".join("0" for _ in range(19)),
    }
    rpc = _sub(rio, "rpc").RPC.from_gdal(items)
    path = str(ctx.out / "rpcs.tif")
    with rio.open(path, "w", driver="GTiff", width=20, height=10, count=1, dtype="uint8") as dst:
        dst.write(np.zeros((1, 10, 20), "uint8"))
        dst.rpcs = rpc
    out = []
    for reader in (rio, ctx.reference):
        with reader.open(path) as ds:
            r = ds.rpcs
            out.append(r.to_dict())
    return out


# RPCs of a 240 x 200 image of a mountain range: rows and columns move with the height (a
# parallax of a few pixels per 100 m) and with second-order terms, over a non-constant denominator.
SYNTH_RPC = {
    "LINE_OFF": "100", "SAMP_OFF": "120", "LAT_OFF": "35.6", "LONG_OFF": "-115.2", "HEIGHT_OFF": "800",
    "LINE_SCALE": "100", "SAMP_SCALE": "120", "LAT_SCALE": "0.05", "LONG_SCALE": "0.06", "HEIGHT_SCALE": "1000",
    "LINE_NUM_COEFF": "0.002 0.005 -1 0.06 0.004 0.001 -0.002 0.001 -0.01 0 0.0005 0 0.0002 0 0.0001 0 0 0 0 0",
    "LINE_DEN_COEFF": "1 0.001 -0.002 0.0005 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
    "SAMP_NUM_COEFF": "-0.001 1 0.02 0.09 0.02 0.003 0 0.01 0.002 0 0 0.0003 0 0 0 0 0 0 0 0",
    "SAMP_DEN_COEFF": "1 0.001 -0.002 0.0005 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
}


def rpc_files(ctx: Ctx) -> dict[str, str]:
    """An image georeferenced by RPCs and DEMs for it, written once by the reference module.

    The image (240 x 200, uint8) carries `SYNTH_RPC` in the GeoTIFF RPC tag and no transform. The
    DEMs cover most of its footprint, with holes of nodata: one in WGS 84 (the rows GDAL samples
    whole), one in UTM 11N (sampled through a coordinate transformation).

    Args:
        ctx: The context.

    Returns:
        dict: The paths, under `image`, `dem` and `dem_utm`.
    """
    ref = ctx.reference
    out = {k: str(ctx.out / f"rpc_{k}.tif") for k in ("image", "dem", "dem_utm")}
    if Path(out["image"]).exists():
        return out
    yy, xx = np.mgrid[0:200, 0:240]
    img = ((np.sin(xx / 7.0) + np.cos(yy / 5.0)) * 60 + 128 + (xx * yy) % 17).astype("uint8")
    with ref.open(out["image"], "w", driver="GTiff", width=240, height=200, count=1, dtype="uint8") as dst:
        dst.write(img[None])
        dst.rpcs = ref.rpc.RPC.from_gdal(SYNTH_RPC)

    def relief(lon: Any, lat: Any) -> Any:
        """Heights of a synthetic range of hills.

        Args:
            lon: Longitudes.
            lat: Latitudes.

        Returns:
            numpy.ndarray: Heights, in metres.
        """
        return 800 + 600 * np.sin((lon + 115.2) * 60) * np.cos((lat - 35.6) * 45) + 200 * (lat - 35.55) / 0.1

    # WGS 84, 0.001 degree cells, covering all but the northern and eastern edges.
    t = ref.transform.from_origin(-115.275, 35.645, 0.001, 0.001)
    rows, cols = np.mgrid[0:100, 0:130]
    lon, lat = t.c + (cols + 0.5) * t.a, t.f + (rows + 0.5) * t.e
    z = relief(lon, lat).astype("float32")
    z[40:44, 60:70] = -9999
    z[70:73, 20:21] = -9999
    with ref.open(out["dem"], "w", driver="GTiff", width=130, height=100, count=1, dtype="float32", crs="EPSG:4326", transform=t, nodata=-9999) as dst:
        dst.write(z[None])
    # UTM 11N, 90 m cells, short of the eastern edge.
    t = ref.transform.from_origin(655000, 3950000, 90, 90)
    rows, cols = np.mgrid[0:130, 0:140]
    x, y = t.c + (cols + 0.5) * t.a, t.f + (rows + 0.5) * t.e
    lon, lat = ref.warp.transform("EPSG:32611", "EPSG:4326", x.ravel(), y.ravel())
    z = relief(np.array(lon), np.array(lat)).reshape(130, 140).astype("float32")
    z[60:64, 70:75] = -9999
    with ref.open(out["dem_utm"], "w", driver="GTiff", width=140, height=130, count=1, dtype="float32", crs="EPSG:32611", transform=t, nodata=-9999) as dst:
        dst.write(z[None])
    return out


def _rpc_options(files: dict[str, str]) -> list[dict[str, Any]]:
    """The RPC transformer options the checks run with.

    Args:
        files: The paths of `rpc_files`.

    Returns:
        list: Option sets: none, heights, a WGS 84 DEM with each interpolation, a missing value.
    """
    return [
        {},
        {"RPC_HEIGHT": 1500},
        {"RPC_HEIGHT": 300, "RPC_HEIGHT_SCALE": 2},
        {"RPC_DEM": files["dem"]},
        {"RPC_DEM": files["dem"], "RPC_DEMINTERPOLATION": "near"},
        {"RPC_DEM": files["dem"], "RPC_DEMINTERPOLATION": "cubic", "RPC_HEIGHT": 20},
        {"RPC_DEM": files["dem"], "RPC_DEM_MISSING_VALUE": 500, "RPC_HEIGHT_SCALE": 0.5},
        {"RPC_PIXEL_ERROR_THRESHOLD": 0.001, "RPC_MAX_ITERATIONS": 30},
    ]


def _rpc_points(rio: Any, ctx: Ctx, options: list[dict[str, Any]]) -> Any:
    """Points transformed by the RPCs of `rpc_files` with each set of options.

    Args:
        rio: The module.
        ctx: The context.
        options: Sets of RPC transformer options.

    Returns:
        list: The results.
    """
    files = rpc_files(ctx)
    T = rio.transform
    with rio.open(files["image"]) as ds:
        rpcs = ds.rpcs
        rng = np.random.default_rng(7)
        rows, cols = rng.uniform(-40, 240, 300), rng.uniform(-40, 280, 300)
        zs = rng.uniform(-100, 400, 300)
        out = []
        for opts in options:
            with T.RPCTransformer(rpcs, **opts) as t:
                xs, ys = t.xy(rows, cols)
                out.append([xs, ys, t.xy(rows, cols, zs=zs), t.xy(5, 7, offset="ul")])
                ok = np.isfinite(xs)
                # Back to rows and columns: scattered points, and a row of points at one latitude
                # (sampled from one row of a WGS 84 DEM).
                out.append(t.rowcol(xs[ok], ys[ok], op=float))
                lons = np.linspace(-115.24, -115.16, 40)
                out.append(t.rowcol(lons, np.full(40, 35.61), zs=10.0, op=float))
            out.append(T.xy(rpcs, [10, 150], [20, 200], **opts))
            out.append(T.rowcol(rpcs, -115.21, 35.59, **opts))
            m = rio.enums.TransformMethod.rpcs
            out.append([ds.xy(50, 60, transform_method=m, **opts), ds.index(-115.2, 35.6, transform_method=m, **opts)])
        out.append(repr(T.RPCTransformer(rpcs)))
        t = T.RPCTransformer(rpcs.to_gdal())
        t.close()
        out.append([repr(t), t.closed])
    return out


@check("georeferencing", "RPCs: RPCTransformer forward (iterative) and inverse, with heights, RPC_HEIGHT(_SCALE), a DEM (bilinear, cubic, missing value, nodata holes); xy/rowcol(rpcs); dataset xy/index by TransformMethod.rpcs")
def _g_rpc_transformer(rio: Any, ctx: Ctx) -> Any:
    options = [o for o in _rpc_options(rpc_files(ctx)) if o.get("RPC_DEMINTERPOLATION") != "near"]
    return _rpc_points(rio, ctx, options)


@check("georeferencing", "RPCs: RPCTransformer with a DEM sampled by nearest neighbour (forward and inverse)")
def _g_rpc_transformer_near(rio: Any, ctx: Ctx) -> Any:
    return _rpc_points(rio, ctx, [o for o in _rpc_options(rpc_files(ctx)) if o.get("RPC_DEMINTERPOLATION") == "near"])


@check("georeferencing", "RPCs: RPCTransformer with a DEM in UTM 11N (sampled through a coordinate transformation)", expect="tolerance", rtol=1e-12, atol=1e-9, reason="the DEM is sampled at positions projected by terrio's UTM, which differs from PROJ's in the last digits")
def _g_rpc_transformer_utm(rio: Any, ctx: Ctx) -> Any:
    files = rpc_files(ctx)
    return _rpc_points(rio, ctx, [{"RPC_DEM": files["dem_utm"]}, {"RPC_DEM": files["dem_utm"], "RPC_DEMINTERPOLATION": "cubic"}])


@check("warp", "RPCs: reproject (arrays and a dataset's band; to WGS 84, UTM and polar stereographic; nearest, bilinear, cubic), calculate_default_transform with RPC_HEIGHT and DEMs, WarpedVRT by RPCs", expect="tolerance", rtol=1e-9, atol=1, max_frac=0.0005, reason="the polar stereographic grid differs in the last digits (terrio's projection, not PROJ's); bilinear weights rounded differently on a few pixels")
def _wp_rpc(rio: Any, ctx: Ctx) -> Any:
    files = rpc_files(ctx)
    W = rio.warp
    out = []
    with rio.open(files["image"]) as ds:
        rpcs = ds.rpcs
        arr = ds.read(1)
        for opts in _rpc_options(files)[:-1] + [{"RPC_DEM": files["dem_utm"]}]:
            # GDAL 3.12 aborts (std::length_error) suggesting a projected grid with a DEM sampled
            # by nearest neighbour; the polar grid is far from these RPCs' area.
            crss = ["EPSG:4326"]
            if opts.get("RPC_DEMINTERPOLATION") != "near":
                crss.append("EPSG:32611")
            if "RPC_DEM" not in opts:
                crss.append("EPSG:3031")
            crss = crss if "RPC_DEM" not in opts else crss[:2]
            # The Windows GDAL bundled with rasterio 1.5.2 hangs in suggested output
            # with an RPC DEM, even for WGS 84 and bilinear DEM interpolation. Keep
            # comparing the actual DEM warps below; grid suggestions with a DEM
            # are still compared on Linux and macOS.
            if sys.platform != "win32" or "RPC_DEM" not in opts:
                for crs in crss:
                    out.append(W.calculate_default_transform("EPSG:4326", crs, 240, 200, rpcs=rpcs, **opts))
            for crs, r in (("EPSG:4326", "nearest"), ("EPSG:32611", "bilinear"), ("EPSG:4326", "cubic")):
                dst, t = W.reproject(arr, rpcs=rpcs, src_crs="EPSG:4326", dst_crs=crs, resampling=rio.enums.Resampling[r], **opts)
                out += [dst, t]
        # A dataset's band: warped by its own RPCs, onto a given grid.
        t = rio.transform.from_origin(-115.27, 35.65, 0.0004, 0.0004)
        dst = np.zeros((300, 340), "uint8")
        W.reproject(rio.band(ds, 1), dst, rpcs=rpcs, dst_transform=t, dst_crs="EPSG:4326", RPC_HEIGHT=900)
        out.append(dst)
        for kw in (dict(crs="EPSG:4326"), dict(crs="EPSG:32611", RPC_HEIGHT=700, resampling=rio.enums.Resampling.bilinear)):
            with rio.vrt.WarpedVRT(ds, **kw) as v:
                out += [v.crs, v.transform, v.width, v.height, v.read()]
        try:
            W.reproject(rio.band(ds, 1), np.zeros((10, 10), "uint8"), rpcs=rpcs, src_crs="EPSG:32611", dst_transform=t, dst_crs="EPSG:4326")
        except Exception as e:
            out.append(type(e).__name__)
    return out


@check("masks", "internal masks: write_mask saved to the file (tiled, striped, r+), read back by both")
def _m_internal(rio: Any, ctx: Ctx) -> Any:
    m = np.full((40, 50), 255, np.uint8)
    m[10:20, 5:30] = 0
    out = []
    for name, kw in (("tiled", dict(tiled=True, blockxsize=16, blockysize=16, compress="deflate")), ("strips", {})):
        path = str(ctx.out / f"mask_{name}.tif")
        _small(rio, path, rio.Affine(10, 0, 500000, 0, -10, 4100000), **kw)
        with rio.open(path, "r+") as dst:
            dst.write_mask(m)
        for reader in (rio, ctx.reference):
            with reader.open(path) as ds:
                out.append([ds.mask_flag_enums, ds.dataset_mask(), ds.read_masks(2, window=((8, 22), (0, 40))), ds.read(1, masked=True), ds.dataset_mask(out_shape=(20, 25))])
        with rio.open(path, "r+") as dst:
            dst.update_tags(edited="yes")
        with ctx.reference.open(path) as ds:
            out.append(ds.dataset_mask())
    return out


@check("masks", "per-band nodata from a .aux.xml sidecar: nodatavals, masks, masked reads")
def _m_band_nodata(rio: Any, ctx: Ctx) -> Any:
    path = str(ctx.out / "band_nodata.tif")
    _small(ctx.reference, path, rio.Affine(10, 0, 500000, 0, -10, 4100000))
    with open(path + ".aux.xml", "w") as f:
        f.write('<PAMDataset><PAMRasterBand band="1"><NoDataValue>0</NoDataValue></PAMRasterBand>'
                '<PAMRasterBand band="2"><NoDataValue>5</NoDataValue></PAMRasterBand>'
                '<PAMRasterBand band="3"><NoDataValue>250</NoDataValue></PAMRasterBand></PAMDataset>')
    with rio.open(path) as ds:
        return [ds.nodatavals, ds.nodata, ds.read_masks(), ds.read(masked=True), ds.mask_flag_enums]


@check("tools", "shutil: exists, copy (to GTiff, with creation options), copyfiles, delete")
def _t_shutil(rio: Any, ctx: Ctx) -> Any:
    sh = _sub(rio, "shutil")
    src = synthetic(rio, ctx)
    out: list[Any] = [sh.exists(src), sh.exists(str(ctx.out / "missing.tif"))]
    dst = str(ctx.out / "copy.tif")
    sh.copy(src, dst, compress="lzw", tiled=True)
    with ctx.reference.open(dst) as ds:
        out.append([ds.read(), ds.nodata, ds.crs, tuple(ds.transform), ds.compression, ds.is_tiled, ds.tags()["source"], ds.descriptions])
    with rio.open(src) as s:
        sh.copy(s, str(ctx.out / "copy2.tif"))
    with ctx.reference.open(str(ctx.out / "copy2.tif")) as ds:
        out.append(ds.read())
    files = str(ctx.out / "files.tif")
    sh.copyfiles(dst, files)
    with ctx.reference.open(files) as ds:
        out.append(ds.read())
    sh.delete(files)
    out.append(sh.exists(files))
    for bad in (lambda: sh.delete(str(ctx.out / "missing.tif")), lambda: sh.copy(dst, dst), lambda: sh.copyfiles(dst, dst)):
        try:
            bad()
            out.append("no error")
        except Exception as e:  # noqa: BLE001
            out.append(type(e).__name__)
    return out


# rasterio.stack changed after 1.5.0 (which wrote each source over the whole output instead of
# by chunk); terrio.rio follows 1.5.2.
@check("tools", "stack: bands of several files, indexes, bounds, resolution, masked, dst_path", min_rasterio=(1, 5, 2))
def _t_stack(rio: Any, ctx: Ctx) -> Any:
    stack = _sub(rio, "stack").stack
    a = synthetic(rio, ctx)
    b = synthetic(rio, ctx, "synth_b.tif", overviews=False, transform=ctx.reference.transform.from_origin(503000, 4098000, 30, 30), nodata=None)
    out = []
    arr, t = stack([a, b])
    out.append([arr, tuple(t)])
    arr, t = stack([a, b], indexes=[[1, 3], 2], bounds=(500500, 4093000, 508000, 4099500), res=45, masked=True)
    out.append([arr, tuple(t)])
    with rio.open(a) as s1, rio.open(b) as s2:
        arr, t = stack([s1, s2], use_highest_res=True, target_aligned_pixels=True, res=(30, 30))
        out.append([arr, tuple(t)])
    dst = str(ctx.out / "stacked.tif")
    stack([a, b], dst_path=dst, dst_kwds={"compress": "deflate"})
    with ctx.reference.open(dst) as ds:
        out.append([meta_of(ds)["profile"], ds.read()])
    return out


# ─── Features, warping, merging, masking ────────────────────────────────────────────────────────


GEOMS = [
    {"type": "Polygon", "coordinates": [[(500100, 4099000), (502000, 4099500), (501500, 4097000), (500100, 4099000)]]},
    ({"type": "Polygon", "coordinates": [[(501000, 4098000), (503000, 4098000), (503000, 4096000), (501000, 4096000), (501000, 4098000)], [(501500, 4097500), (502500, 4097500), (502500, 4096500), (501500, 4096500), (501500, 4097500)]]}, 5),
    ({"type": "LineString", "coordinates": [(500000, 4096500), (504000, 4099900)]}, 9),
    ({"type": "Point", "coordinates": (502345, 4097777)}, 3),
    ({"type": "MultiPolygon", "coordinates": [[[(503100, 4099100), (504100, 4099100), (504100, 4098100), (503100, 4099100)]]]}, 2),
]


@check("features", "rasterize: replace, all_touched, add, out, dtypes")
def _f_rasterize(rio: Any, ctx: Ctx) -> Any:
    F = rio.features
    t = rio.transform.from_origin(500000, 4100000, 30, 30)
    out = np.full((120, 150), 1, "float32")
    F.rasterize(GEOMS, out=out, transform=t)
    return [
        F.rasterize(GEOMS, out_shape=(120, 150), transform=t),
        F.rasterize(GEOMS, out_shape=(120, 150), transform=t, all_touched=True, fill=7),
        F.rasterize(GEOMS, out_shape=(120, 150), transform=t, merge_alg=rio.enums.MergeAlg.add, dtype="float32"),
        F.rasterize(GEOMS, out_shape=(120, 150), transform=t, dtype="int64", default_value=1000000),
        out,
    ]


@check("features", "geometry_mask, geometry_window, raster_geometry_mask")
def _f_gmask(rio: Any, ctx: Ctx) -> Any:
    t = rio.transform.from_origin(500000, 4100000, 30, 30)
    shapes = [g if isinstance(g, dict) else g[0] for g in GEOMS[:2]]
    with rio.open(synthetic(rio, ctx)) as ds:
        return [rio.features.geometry_mask(shapes, (120, 150), t), rio.features.geometry_mask(shapes, (120, 150), t, invert=True, all_touched=True), rio.features.geometry_window(ds, shapes), rio.features.geometry_window(ds, shapes, pad_x=2, pad_y=1), rio.mask.raster_geometry_mask(ds, shapes, crop=True)]


def polygons(results: Any) -> Any:
    """Polygonize results as an order-independent set: value and topologically normalized shape.

    Args:
        results: `(geometry, value)` pairs.

    Returns:
        list: Sorted `(value, normalized WKB hex)`.
    """
    import shapely.geometry

    return sorted((v, shapely.geometry.shape(g).normalize().wkb_hex) for g, v in results)


@check("features", "shapes: int and float rasters, mask, 8-connectivity (compared as sets)")
def _f_shapes(rio: Any, ctx: Ctx) -> Any:
    with rio.open(synthetic(rio, ctx)) as ds:
        a = (ds.read(1)[:80, :100] // 300).astype("int16")
        t = ds.transform
    return [polygons(rio.features.shapes(a, transform=t)), polygons(rio.features.shapes(a, mask=a > 1, connectivity=8)), polygons(rio.features.shapes(a.astype("float32") / 2, transform=t)), polygons(rio.features.shapes(np.ma.masked_equal(a, 2)))]


@check("warp", "transform, transform_geom, transform_bounds", expect="tolerance", rtol=1e-12, atol=1e-9, reason="PROJ and terrio's projection arithmetic differ in the last digits")
def _wp_transform(rio: Any, ctx: Ctx) -> Any:
    W = rio.warp
    g = {"type": "Polygon", "coordinates": [[(-117.0, 37.0), (-116.0, 37.0), (-116.0, 38.0), (-117.0, 37.0)]]}
    return [W.transform("EPSG:4326", "EPSG:32611", [-117, -116.5], [37, 37.5]), W.transform("EPSG:4326", "EPSG:3857", [10.0], [50.0], [100.0]), W.transform_geom("EPSG:4326", "EPSG:3857", g), W.transform_geom("EPSG:4326", "EPSG:32611", g, precision=2), W.transform_bounds("EPSG:32611", "EPSG:4326", 500000, 4000000, 600000, 4100000), W.transform_bounds("EPSG:4326", "EPSG:5070", -100, 30, -90, 40)]


@check("warp", "calculate_default_transform (default, resolution, dimensions)", expect="tolerance", rtol=1e-12, atol=1e-9, reason="the projected edges of the source differ in the last digits")
def _wp_cdt(rio: Any, ctx: Ctx) -> Any:
    W = rio.warp
    out = [W.calculate_default_transform("EPSG:32611", "EPSG:4326", 301, 203, 500000, 4093910, 509030, 4100000)]
    out.append(W.calculate_default_transform("EPSG:32611", "EPSG:3857", 301, 203, 500000, 4093910, 509030, 4100000, resolution=50))
    out.append(W.calculate_default_transform("EPSG:4326", "EPSG:32611", 100, 100, -117.5, 36.5, -116.5, 37.5, dst_width=200, dst_height=180))
    return out


@check("warp", "reproject arrays and bands: nearest")
def _wp_reproject(rio: Any, ctx: Ctx) -> Any:
    W = rio.warp
    out = []
    with rio.open(synthetic(rio, ctx)) as ds:
        t, w, h = W.calculate_default_transform(ds.crs, "EPSG:4326", ds.width, ds.height, *ds.bounds)
        ref = ctx.reference.warp.calculate_default_transform("EPSG:32611", "EPSG:4326", ds.width, ds.height, *ds.bounds)
        t, w, h = ref
        t = rio.transform.Affine(*t[:6])
        dst = np.zeros((3, h, w), "int16")
        W.reproject(rio.band(ds, [1, 2, 3]), dst, dst_transform=t, dst_crs="EPSG:4326")
        out.append(dst)
        a = ds.read(1)
        d2 = np.zeros((h, w), "float32")
        W.reproject(a, d2, src_transform=ds.transform, src_crs=ds.crs, src_nodata=-9999, dst_transform=t, dst_crs="EPSG:4326", dst_nodata=np.nan)
        out.append(d2)
        d3 = np.full((h, w), 5, "int16")
        W.reproject(a, d3, src_transform=ds.transform, src_crs=ds.crs, src_nodata=-9999, dst_transform=t, dst_crs="EPSG:4326", init_dest_nodata=False)
        out.append(d3)
    return out


@check("warp", "reproject: bilinear, cubic, average, lanczos", expect="tolerance", atol=2, max_frac=0.01, reason="GDAL's warp kernels next to nodata pixels")
def _wp_reproject_r(rio: Any, ctx: Ctx) -> Any:
    W = rio.warp
    out = []
    with rio.open(synthetic(rio, ctx)) as ds:
        t, w, h = ctx.reference.warp.calculate_default_transform("EPSG:32611", "EPSG:4326", ds.width, ds.height, *ds.bounds)
        t = rio.transform.Affine(*t[:6])
        for r in ("bilinear", "cubic", "average", "lanczos"):
            dst = np.zeros((3, h, w), "int16")
            W.reproject(rio.band(ds, [1, 2, 3]), dst, dst_transform=t, dst_crs="EPSG:4326", resampling=rio.enums.Resampling[r])
            out.append(dst)
    return out


@check("warp", "WarpedVRT: grid, metadata, full and windowed reads (nearest)", expect="tolerance", rtol=1e-12, atol=1e-9, reason="the suggested output grid differs in the last digits (pixels are identical)")
def _wp_vrt(rio: Any, ctx: Ctx) -> Any:
    out = []
    with rio.open(synthetic(rio, ctx)) as ds:
        with rio.vrt.WarpedVRT(ds, crs="EPSG:4326") as v:
            out += [v.crs, v.width, v.height, v.transform, v.nodata, v.count, v.dtypes, v.read(), v.read(2, window=rio.windows.Window(10, 5, 60, 40)), v.read(masked=True)]
        t = rio.transform.from_origin(-117.0, 37.04, 0.0005, 0.0005)
        with rio.vrt.WarpedVRT(ds, crs="EPSG:4326", transform=t, width=60, height=50, resampling=rio.enums.Resampling.nearest) as v:
            out += [v.transform, v.read()]
    return out


@check("warp", "rotated destination grids: reproject (nearest, bilinear, cubic) and WarpedVRT (nearest, and bilinear block by block)", expect="tolerance", atol=2, max_frac=0.01, reason="GDAL's warp kernels next to nodata pixels")
def _wp_rotated_dst(rio: Any, ctx: Ctx) -> Any:
    W = rio.warp
    out = []
    with rio.open(synthetic(rio, ctx)) as ds:
        x, y = ds.transform * (ds.width / 4, ds.height / 4)
        for deg in (30, -65, 180):
            th = math.radians(deg)
            t = rio.transform.Affine(40 * math.cos(th), 40 * math.sin(th), x, 40 * math.sin(th), -40 * math.cos(th), y)
            for r in ("nearest", "bilinear", "cubic"):
                dst = np.zeros((3, 64, 64), "int16")
                W.reproject(rio.band(ds, [1, 2, 3]), dst, dst_transform=t, dst_crs=ds.crs, resampling=rio.enums.Resampling[r])
                out.append(dst)
            with rio.vrt.WarpedVRT(ds, crs=ds.crs, transform=t, width=64, height=64) as v:
                out += [v.transform, v.read()]
            # GDAL's whole-image read of a warped VRT (GDAL_VRT_WARP_USE_DATASET_RASTERIO) samples
            # rotated grids differently from its own blocks and from reproject: read row by row.
            with rio.vrt.WarpedVRT(ds, crs=ds.crs, transform=t, width=64, height=64, resampling=rio.enums.Resampling.bilinear) as v:
                out.append(np.concatenate([v.read(window=rio.windows.Window(0, row, 64, 1)) for row in range(64)], axis=1))
    return out


@check("merge", "merge: union, resolution, bounds, methods, indexes, nodata, target_aligned_pixels")
def _m_merge(rio: Any, ctx: Ctx) -> Any:
    a = synthetic(rio, ctx)
    b = synthetic(rio, ctx, "synth_b.tif", transform=ctx.reference.transform.from_origin(504000, 4097000, 30, 30), count=3)
    out = []
    with rio.open(a) as da, rio.open(b) as db:
        for kw in (dict(), dict(res=45), dict(bounds=(502000, 4093000, 509000, 4098000)), dict(method="last"), dict(method="max"), dict(method="min"), dict(method="sum", dtype="int32"), dict(method="count"), dict(indexes=[2]), dict(nodata=0), dict(res=44, target_aligned_pixels=True)):
            arr, t = rio.merge.merge([da, db], **kw)
            out.append([arr, t])
    arr, t = rio.merge.merge([a, b], masked=True)
    out.append([arr, t])
    path = str(ctx.out / "merged.tif")
    rio.merge.merge([a, b], dst_path=path)
    with ctx.reference.open(path) as m:
        out.append([meta_of(m)["profile"], m.read()])
    return out


@check("mask", "mask: crop, filled, invert, all_touched, pad, indexes, nodata")
def _k_mask(rio: Any, ctx: Ctx) -> Any:
    shapes = [GEOMS[0], GEOMS[1][0]]
    out = []
    with rio.open(synthetic(rio, ctx)) as ds:
        for kw in (dict(), dict(crop=True), dict(crop=True, filled=False), dict(invert=True), dict(all_touched=True, crop=True), dict(crop=True, pad=True), dict(indexes=2, crop=True), dict(indexes=[3, 1], nodata=7, crop=True)):
            arr, t = rio.mask.mask(ds, shapes, **kw)
            out.append([arr, t])
    return out


@check("mask", "mask: shapes outside the raster raise ValueError with crop")
def _k_outside(rio: Any, ctx: Ctx) -> Any:
    with rio.open(synthetic(rio, ctx)) as ds:
        return rio.mask.mask(ds, [{"type": "Polygon", "coordinates": [[(0, 0), (1, 0), (1, 1), (0, 0)]]}], crop=True)


# ─── Real files ─────────────────────────────────────────────────────────────────────────────────


def _for_files(
    names: list[str], section: str, title: str, known: dict[str, str] | None = None, **kw: Any
) -> Callable[[Callable[[Any, Ctx, str], Any]], None]:
    """Registers a check per file.

    Args:
        names: File names in the data directory.
        section: The section.
        title: The check's title (the file name is prepended).
        known: Files whose results are known to differ, with the reason.
        **kw: `Check` fields.

    Returns:
        callable: The decorator.
    """

    def deco(f: Callable[[Any, Ctx, str], Any]) -> None:
        for name in names:

            def g(rio: Any, ctx: Ctx, name: str = name) -> Any:
                p = ctx.path(name)
                if not os.path.exists(p):
                    raise FileNotFoundError(p)
                return f(rio, ctx, p)

            fields = dict(kw)
            if known and name in known:
                fields.update(expect="known", reason=known[name])
            CHECKS.append(Check(section, f"{name}: {title}", g, needs_data=True, **fields))

    return deco


@_for_files(TIFFS, "real files", "metadata (profile, tags, bands, masks, blocks, overviews)")
def _r_meta(rio: Any, ctx: Ctx, p: str) -> Any:
    with rio.open(p) as ds:
        return meta_of(ds)


@_for_files(
    OTHER_FORMATS,
    "real files",
    "metadata and pixels (formats terrio reads whole)",
    known={"srtm_sub.bil": "GDAL identifies EPSG:4326 in the ESRI .prj; terrio keeps the WKT (same CRS)"},
)
def _r_other(rio: Any, ctx: Ctx, p: str) -> Any:
    with rio.open(p) as ds:
        return [ds.driver, ds.profile["dtype"], ds.shape, ds.count, ds.nodata, ds.bounds, ds.crs, ds.read(), ds.read(masked=True)]


@_for_files(
    TIFFS,
    "real files",
    "reads: full, window, fractional window, masked, masks, boundless",
    known={
        "las_vegas_dem.tif": "with a -inf nodata, rasterio's boundless masked read masks nothing (its VRT's "
        "nodata is the float32 minimum); terrio.rio masks the pixels outside the raster"
    },
)
def _r_reads(rio: Any, ctx: Ctx, p: str) -> Any:
    W = rio.windows.Window
    with rio.open(p) as ds:
        w, h = ds.width, ds.height
        win = W(w // 5, h // 4, min(300, w // 2), min(200, h // 2))
        frac = W(w / 7.3, h / 5.1, w / 3.7, h / 4.3)
        return [ds.read(), ds.read(1, window=win), ds.read(window=frac), ds.read(masked=True), ds.read_masks(window=win), ds.dataset_mask(), ds.read(window=W(-30, -20, 200, 150), boundless=True), ds.read(1, window=W(w - 50, h - 40, 120, 100), boundless=True, masked=True)]


@_for_files(TIFFS, "real files", "reads: out_shape nearest (through overviews), out_dtype, sample")
def _r_outshape(rio: Any, ctx: Ctx, p: str) -> Any:
    with rio.open(p) as ds:
        w, h = ds.width, ds.height
        pts = [ds.xy(h // 3, w // 3), ds.xy(h - 1, 0), ds.xy(h // 2, w - 2), (ds.bounds.left - 100, ds.bounds.top + 100)]
        return [ds.read(out_shape=(ds.count, max(1, h // 3), max(1, w // 3))), ds.read(1, out_shape=(max(1, h // 9), max(1, w // 7))), ds.read(1, masked=True, out_shape=(max(1, h // 4), max(1, w // 4))), ds.read(1, out_shape=(h * 2, w * 2), window=rio.windows.Window(0, 0, w // 2, h // 2)), ds.read(1, out_dtype="uint8", window=rio.windows.Window(0, 0, w // 2, h // 2)), list(ds.sample(pts)), [ds.index(*pt) for pt in pts[:3]]]


@_for_files(
    TIFFS,
    "real files",
    "reads: out_shape bilinear, cubic, average",
    expect="known",
    reason="next to nodata pixels, GDAL 3.12's masked convolution (bilinear, cubic) differs from "
    "terrio's resampling kernels; GDAL averages the colors of palette rasters; float sums differ "
    "in the last digits",
)
def _r_resampled(rio: Any, ctx: Ctx, p: str) -> Any:
    R = rio.enums.Resampling
    with rio.open(p) as ds:
        shape = (ds.count, max(1, ds.height // 3), max(1, ds.width // 3))
        return [ds.read(out_shape=shape, resampling=r) for r in (R.bilinear, R.cubic, R.average)]


@_for_files(
    ["srtm_sub.tif", "landsat7.tif", "OPERA_L3_DSWx_WTR.tif", "las_vegas_dem.tif"],
    "real files",
    "warp.reproject nearest and WarpedVRT nearest (to EPSG:4326 / UTM)",
    known={
        "OPERA_L3_DSWx_WTR.tif": "GDAL warps a VRT window in its own chunks: its windowed read "
        "differs from its full read on 0.013% of pixels (approximate transformer); terrio.rio cuts "
        "windows from the full warp"
    },
)
def _r_reproject(rio: Any, ctx: Ctx, p: str) -> Any:
    W = rio.warp
    with rio.open(p) as ds:
        dst_crs = "EPSG:32611" if ds.crs.is_geographic else "EPSG:4326"
        ref = ctx.reference.open(p)
        t, w, h = ctx.reference.warp.calculate_default_transform(ref.crs, dst_crs, ref.width, ref.height, *ref.bounds)
        ref.close()
        t = rio.transform.Affine(*t[:6])
        dst = np.zeros((ds.count, h, w), ds.dtypes[0])
        W.reproject(rio.band(ds, list(ds.indexes)), dst, dst_transform=t, dst_crs=dst_crs)
        with rio.vrt.WarpedVRT(ds, crs=dst_crs, transform=t, width=w, height=h) as v:
            vr = v.read()
            vw = v.read(1, window=rio.windows.Window(w // 4, h // 4, w // 3, h // 3))
        return [dst, vr, vw]


@_for_files(
    ["srtm_sub.tif", "landsat7.tif", "OPERA_L3_DSWx_WTR.tif"],
    "real files",
    "warp.reproject bilinear, cubic",
    expect="tolerance",
    atol=255,
    max_frac=0.01,
    reason="GDAL's warp kernels next to nodata pixels (under 1% of pixels)",
)
def _r_reproject_r(rio: Any, ctx: Ctx, p: str) -> Any:
    W = rio.warp
    with rio.open(p) as ds:
        dst_crs = "EPSG:32611" if ds.crs.is_geographic else "EPSG:4326"
        ref = ctx.reference.open(p)
        t, w, h = ctx.reference.warp.calculate_default_transform(ref.crs, dst_crs, ref.width, ref.height, *ref.bounds)
        ref.close()
        t = rio.transform.Affine(*t[:6])
        out = []
        for r in ("bilinear", "cubic"):
            dst = np.zeros((ds.count, h, w), ds.dtypes[0])
            W.reproject(rio.band(ds, list(ds.indexes)), dst, dst_transform=t, dst_crs=dst_crs, resampling=rio.enums.Resampling[r])
            out.append(dst)
        return out


@_for_files(["srtm_sub.tif", "landsat7.tif", "OPERA_L3_DSWx_WTR.tif", "las_vegas_dem.tif", "nlcd_knoxville.tif"], "real files", "warp.calculate_default_transform", expect="tolerance", rtol=1e-12, atol=1e-9, reason="the projected edges of the source differ in the last digits")
def _r_cdt(rio: Any, ctx: Ctx, p: str) -> Any:
    with rio.open(p) as ds:
        dst = "EPSG:32611" if ds.crs.is_geographic else "EPSG:4326"
        return rio.warp.calculate_default_transform(ds.crs, dst, ds.width, ds.height, *ds.bounds)


@check("real files", "TN_Counties.geojson: rasterize 95 counties (values, all_touched)", needs_data=True)
def _r_counties(rio: Any, ctx: Ctx) -> Any:
    with open(ctx.path("TN_Counties.geojson")) as f:
        fc = json.load(f)
    shapes = [(feat["geometry"], i + 1) for i, feat in enumerate(fc["features"])]
    t = rio.transform.from_bounds(-90.4, 34.9, -81.6, 36.7, 1600, 400)
    return [rio.features.rasterize(shapes, (400, 1600), transform=t, dtype="int32"), rio.features.rasterize(shapes, (400, 1600), transform=t, dtype="int32", all_touched=True), rio.features.geometry_mask([s for s, _ in shapes[:10]], (400, 1600), t)]


@check("real files", "nlcd_knoxville.tif: shapes of a land cover raster (compared as sets)", needs_data=True)
def _r_shapes(rio: Any, ctx: Ctx) -> Any:
    with rio.open(ctx.path("nlcd_knoxville.tif")) as ds:
        a = ds.read(1)[:300, :400]
        return [polygons(rio.features.shapes(a, transform=ds.transform)), polygons(rio.features.shapes(a, mask=a != 11, connectivity=8, transform=ds.transform))]


@check("real files", "las_vegas_dem.tif: mask.mask by the city bounds", needs_data=True)
def _r_mask(rio: Any, ctx: Ctx) -> Any:
    with open(ctx.path("las_vegas_bounds.geojson")) as f:
        fc = json.load(f)
    out = []
    with rio.open(ctx.path("las_vegas_dem.tif")) as ds:
        shapes = [ctx.reference.warp.transform_geom("EPSG:4326", ds.crs.to_string(), feat["geometry"]) for feat in fc["features"]]
        for kw in (dict(crop=True), dict(), dict(crop=True, all_touched=True), dict(crop=True, invert=True), dict(crop=True, filled=False), dict(crop=True, pad=True)):
            out.append(list(rio.mask.mask(ds, shapes, **kw)))
    return out


@check("real files", "USGS 3DEP tiles: merge (resolution, bounds, methods)", needs_data=True)
def _r_merge(rio: Any, ctx: Ctx) -> Any:
    # usgs_dem.tif is the n40w106 tile.
    fs = [ctx.path(n) for n in ("usgs_dem.tif", "USGS_1_n40w107.tif", "USGS_1_n41w106.tif", "USGS_1_n41w107.tif")]
    out = [list(rio.merge.merge(fs, res=0.002))]
    out.append(list(rio.merge.merge(fs, bounds=(-106.5, 39.5, -105.5, 40.5), res=0.001, method="max")))
    out.append(list(rio.merge.merge(fs[:2], res=0.003, method="last", masked=True)))
    return out


@check("real files", "landsat.tif: profile copied to a new file, written by window, read by rasterio", needs_data=True)
def _r_copy(rio: Any, ctx: Ctx) -> Any:
    path = str(ctx.out / "copy.tif")
    with rio.open(ctx.path("landsat.tif")) as src:
        profile = src.profile
        with rio.open(path, "w", **profile) as dst:
            for _, w in src.block_windows(1):
                dst.write(src.read(window=w), window=w)
            dst.update_tags(**src.tags())
            dst.descriptions = src.descriptions
            dst.build_overviews([2, 4], rio.enums.Resampling.nearest)
    with ctx.reference.open(path) as ds:
        return [meta_of(ds), ds.read(), ds.read(out_shape=(4, 391, 532))]


@check("remote", "USGS 3DEP COG over HTTP: metadata, window, decimated and masked reads, sample", needs_remote=True)
def _remote(rio: Any, ctx: Ctx) -> Any:
    W = rio.windows.Window
    with rio.open(REMOTE_COG) as ds:
        pts = [(-105.5, 39.5), (-105.2, 39.9), (-104.9, 39.1)]
        return [meta_of(ds), ds.read(1, window=W(1000, 1000, 512, 512)), ds.read(1, out_shape=(400, 400)), ds.read(1, masked=True, out_shape=(150, 150)), list(ds.sample(pts))]


# ─── The rio command line ───────────────────────────────────────────────────────────────────────

# The command line checks run rasterio's `rio` and terrio.rio's (`python -m terrio.rio`, the
# `terrio-rio` script) with the same arguments, and compare their exit status, their output
# (JSON and GeoJSON parsed, geometries compared as sets), and the files they write, read back by
# rasterio (metadata, pixels, masks).

# Public COGs: Copernicus GLO-30 DEM tiles and the bands of a Sentinel-2 L2A scene (Earth Search).
COPERNICUS = "https://copernicus-dem-30m.s3.amazonaws.com/Copernicus_DSM_COG_10_{0}_DEM/Copernicus_DSM_COG_10_{0}_DEM.tif"
DEM_TILES = ["N40_00_W106_00", "N39_00_W106_00", "N40_00_W107_00"]
SENTINEL2 = "https://sentinel-cogs.s3.us-west-2.amazonaws.com/sentinel-s2-l2a-cogs/13/T/DE/2026/10/S2C_13TDE_20261019_0_L2A/{0}.tif"

CLI_TOL = dict(expect="tolerance", rtol=1e-9, atol=1e-9, reason="coordinates projected by terrio and PROJ differ in the last digits")


def rio_command(rio: Any) -> list[str]:
    """The `rio` command line of a module.

    Args:
        rio: `rasterio` or `terrio.rio`.

    Returns:
        list: The command: rasterio's `rio`, or `python -m terrio.rio`.
    """
    if rio.__name__.startswith("terrio"):
        return [sys.executable, "-m", "terrio.rio"]
    return [sys.executable, "-c", "import sys; from rasterio.rio.main import main_group; sys.exit(main_group(prog_name='rio'))"]


def cli_output(text: str) -> Any:
    """A command's output, parsed: one JSON text, JSON texts one per line, or lines.

    Args:
        text: The standard output.

    Returns:
        The parsed JSON, or a list of the lines (parsed when they are JSON).
    """
    text = text.strip()
    if not text:
        return []
    try:
        return json.loads(text.replace("\x1e", ""))
    except ValueError:
        pass
    head, _, rest = text.partition("\n")
    try:
        json.loads(head)
    except ValueError:
        try:
            # A title, then a JSON text (`rio warp --dry-run`).
            return [head, json.loads(rest)]
        except ValueError:
            pass
    out = []
    for line in text.splitlines():
        line = line.strip("\x1e")
        try:
            out.append(json.loads(line))
        except ValueError:
            # Numbers separated by spaces (`rio info --bounds`, `--stats`) as numbers.
            try:
                out.append([float(v) for v in line.split()] or line)
            except ValueError:
                out.append(line)
    return out


def raster_result(ref: Any, path: Path) -> Any:
    """A file a command wrote, read by rasterio: metadata, pixels and masks.

    Args:
        ref: The rasterio module.
        path: The file.

    Returns:
        list | None: The metadata, the bands and the masks, or None if there is no file.
    """
    if not path.exists():
        return None
    with ref.open(path) as ds:
        out = [norm(meta_of(ds)), ds.read(), ds.read_masks()]
        factors = ds.overviews(1)
        if factors:
            # The first overview, read at its size.
            f = factors[0]
            out.append(ds.read(out_shape=(ds.count, -(-ds.height // f), -(-ds.width // f))))
        return out


def rio_run(rio: Any, ctx: Ctx, *args: Any, stdin: str | None = None, files: Sequence[str] = (), output: Callable[[Any], Any] | None = None) -> dict[str, Any]:
    """Runs a `rio` command of a module in the check's directory.

    Args:
        rio: `rasterio` or `terrio.rio`.
        ctx: The context (outputs go to `ctx.out`).
        *args: The command's arguments.
        stdin: Text given on standard input.
        files: Files the command writes (relative to `ctx.out`), read back by rasterio.
        output: A function normalizing the parsed output (geometries as sets...).

    Returns:
        dict: The exit status, the output and the files.
    """
    # A stalled command (a remote read) fails the check rather than blocking the run.
    r = subprocess.run(rio_command(rio) + [str(a) for a in args], input=stdin, capture_output=True, text=True, cwd=str(ctx.out), timeout=900)
    if r.returncode and os.environ.get("RIO_CLI_DEBUG"):
        print(f"{rio.__name__} {args}: {r.stderr[-500:]}", file=sys.stderr)
    out = cli_output(r.stdout)
    if output is not None and r.returncode == 0:
        out = output(out)
    result: dict[str, Any] = {"exit": r.returncode, "stdout": out}
    for f in files:
        result[f] = raster_result(ctx.reference, ctx.out / f)
    if r.returncode not in (0, 1, 2):
        result["stderr"] = r.stderr[-300:]
    return result


def feature_set(obj: Any) -> Any:
    """GeoJSON features as an order-independent list: properties and normalized coordinates.

    Args:
        obj: A feature collection, or a list of features.

    Returns:
        list: `(value, centroid, coordinates)` per feature, sorted; bboxes separately.
    """
    import shapely.geometry

    if isinstance(obj, dict) and obj.get("type") == "Feature":
        obj = [obj]
    feats = obj["features"] if isinstance(obj, dict) and "features" in obj else obj
    out = []
    for f in feats:
        if not isinstance(f, dict) or f.get("type") != "Feature":
            out.append(("bbox", 0.0, 0.0, np.asarray(f, dtype="float64")))
            continue
        g = shapely.geometry.shape(f["geometry"]).normalize()
        c = g.centroid
        props = {k: v for k, v in f["properties"].items()}
        coords = np.asarray([xy for part in getattr(g, "geoms", [g]) for ring in [part.exterior, *part.interiors] for xy in ring.coords], dtype="float64")
        out.append((json.dumps(props, sort_keys=True), round(c.x, 6), round(c.y, 6), coords))
    out.sort(key=lambda t: t[:3])
    head = {k: v for k, v in obj.items() if k != "features"} if isinstance(obj, dict) else {}
    return [head, [[k, x, y, a] for k, x, y, a in out]]


def cli_raster(rio: Any, ctx: Ctx) -> str:
    """The synthetic input of the command line checks: 3 int16 bands with nodata, overviews.

    Args:
        rio: Unused (rasterio writes the file).
        ctx: The context.

    Returns:
        str: The path.
    """
    return synthetic(rio, ctx, "cli.tif", width=301, height=203)


def cli_classes(rio: Any, ctx: Ctx) -> str:
    """A synthetic land cover-like raster: uint8 classes in blobs, with nodata, for `shapes`.

    Args:
        rio: Unused (rasterio writes the file).
        ctx: The context.

    Returns:
        str: The path.
    """
    ref = ctx.reference
    path = ctx.out.parent.parent / "input_cli_classes.tif"
    if not path.exists():
        yy, xx = np.mgrid[0:90, 0:120]
        a = ((np.sin(xx / 9.0) + np.cos(yy / 7.0) + 2) * 2).astype("uint8")
        a[5:15, 10:30] = 255
        profile = dict(driver="GTiff", width=120, height=90, count=1, dtype="uint8", crs="EPSG:32611", transform=ref.transform.from_origin(500000, 4100000, 30, 30), nodata=255)
        with ref.open(path, "w", **profile) as dst:
            dst.write(a, 1)
    return str(path)


def cli_gcps(rio: Any, ctx: Ctx) -> str:
    """A synthetic raster georeferenced by 16 GCPs.

    Args:
        rio: Unused (rasterio writes the file).
        ctx: The context.

    Returns:
        str: The path.
    """
    ref = ctx.reference
    path = ctx.out.parent.parent / "input_cli_gcps.tif"
    if not path.exists():
        with ref.open(path, "w", driver="GTiff", width=150, height=120, count=1, dtype="uint8") as dst:
            dst.write(np.arange(150 * 120, dtype="uint32").reshape(1, 120, 150).astype("uint8"))
            dst.gcps = (_gcps(ref, 16), "EPSG:32611")
    return str(path)


def cli_geojson(ctx: Ctx, crs_geoms: str = "utm") -> str:
    """GeoJSON features over the synthetic rasters, with a `value` property.

    Args:
        ctx: The context.
        crs_geoms: `utm` (the rasters' CRS).

    Returns:
        str: The path.
    """
    path = ctx.out.parent.parent / "input_cli_features.geojson"
    if not path.exists():
        feats = []
        for i, g in enumerate(GEOMS):
            geom, val = (g, 1) if isinstance(g, dict) else g
            geom = json.loads(json.dumps(geom))
            feats.append({"type": "Feature", "properties": {"value": val * 10 + i} if i != 1 else {}, "geometry": geom})
        path.write_text(json.dumps({"type": "FeatureCollection", "features": feats}))
    return str(path)


def copy_input(ctx: Ctx, path: str, name: str) -> str:
    """Copies an input into the check's directory, for commands that change files in place.

    Args:
        ctx: The context.
        path: The input.
        name: The copy's name.

    Returns:
        str: The copy's name (commands run in `ctx.out`).
    """
    shutil.copyfile(path, ctx.out / name)
    return name


@check("rio cli", "info: JSON, --indent, -v, --tags, --namespace", **CLI_TOL)
def _c_info(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    out = [rio_run(rio, ctx, "info", p), rio_run(rio, ctx, "info", p, "--indent", "2"), rio_run(rio, ctx, "info", p, "--tags"), rio_run(rio, ctx, "info", p, "--tags", "--namespace", "IMAGE_STRUCTURE")]
    out.append(rio_run(rio, ctx, "info", copy_input(ctx, p, "copy.tif"), "-v"))
    return out


@check("rio cli", "info: --count, -t, --nodata, -f, --shape, --height, --width, --crs, --bounds, -r, --lnglat, --stats, --checksum, --subdatasets", **CLI_TOL)
def _c_info_items(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    out = [rio_run(rio, ctx, "info", p, opt) for opt in ("--count", "-t", "--nodata", "-f", "--shape", "--height", "--width", "--crs", "--bounds", "-r", "--lnglat", "--subdatasets", "--meta")]
    c = copy_input(ctx, p, "copy.tif")
    out += [rio_run(rio, ctx, "info", c, "--stats", "-b", "2"), rio_run(rio, ctx, "info", c, "--checksum", "--bidx", "3")]
    return out


@check("rio cli", "info and gcps of a raster georeferenced by GCPs (points, CRS, from_gcps transform)", **CLI_TOL)
def _c_gcps(rio: Any, ctx: Ctx) -> Any:
    p = cli_gcps(rio, ctx)
    out = [rio_run(rio, ctx, "info", p), rio_run(rio, ctx, "gcps", p), rio_run(rio, ctx, "gcps", p, "--projected"), rio_run(rio, ctx, "gcps", p, "--collection", "--precision", "3")]
    out += [rio_run(rio, ctx, "gcps", p, "--projected", "--compact", "--collection"), rio_run(rio, ctx, "gcps", p, "--rs", "--indent", "2", "--projected"), rio_run(rio, ctx, "gcps", p, "--indent", "2")]
    return out


@check("rio cli", "bounds: features, --sequence/--collection, --bbox, --projected, --mercator, --dst-crs, --precision, --rs, --indent, --compact", **CLI_TOL)
def _c_bounds(rio: Any, ctx: Ctx) -> Any:
    p, q = cli_raster(rio, ctx), cli_classes(rio, ctx)
    runs = [[], ["--collection"], ["--bbox"], ["--bbox", "--collection"], ["--projected"], ["--mercator", "--precision", "2"], ["--dst-crs", "EPSG:3857"], ["--rs"], ["--indent", "2", "--collection"], ["--compact"], ["--feature", "--bbox"]]
    return [rio_run(rio, ctx, "bounds", p, q, *a) for a in runs]


@check("rio cli", "blocks: collection, --sequence, --projected, --bidx, --precision, -o", **CLI_TOL)
def _c_blocks(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    out = [rio_run(rio, ctx, "blocks", p, *a) for a in ([], ["--projected"], ["--sequence", "--bidx", "2"], ["--precision", "3"], ["--bidx", "7"])]
    out.append([rio_run(rio, ctx, "blocks", p, "--projected", "-o", "blocks.json"), cli_output((ctx.out / "blocks.json").read_text())])
    return out


@check("rio cli", "shapes: band, --mask, --as-mask, --with-nodata, --sampling, --projected, --collection, --bbox, --precision (geometries compared as sets)", **CLI_TOL)
def _c_shapes(rio: Any, ctx: Ctx) -> Any:
    p = cli_classes(rio, ctx)
    runs = [["--projected"], [], ["--mask", "--projected"], ["--as-mask", "--projected"], ["--with-nodata", "--projected"], ["--sampling", "2", "--projected"], ["--collection", "--projected"], ["--precision", "4"], ["--bidx", "2"]]
    out = [rio_run(rio, ctx, "shapes", p, *a, output=feature_set) for a in runs]
    out.append(rio_run(rio, ctx, "shapes", p, "--bbox", "--projected", output=lambda o: sorted(map(tuple, o))))
    return out


@check("rio cli", "clip: --bounds, --geographic, --like, --to-data-window, --with-complement, --nodata, --co; errors")
def _c_clip(rio: Any, ctx: Ctx) -> Any:
    p, q = cli_raster(rio, ctx), cli_classes(rio, ctx)
    out = [rio_run(rio, ctx, "clip", p, "a.tif", "--bounds", "501000 4096000 505000 4099000", files=["a.tif"])]
    out.append(rio_run(rio, ctx, "clip", p, "-o", "b.tif", "--bounds", "[-116.99, 37.0, -116.95, 37.03]", "--geographic", "--co", "compress=lzw", files=["b.tif"]))
    out.append(rio_run(rio, ctx, "clip", p, "c.tif", "--like", q, "--nodata", "-1", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "clip", p, "d.tif", "--bounds", "499000 4096000 502000 4101000", "--with-complement", files=["d.tif"]))
    out.append(rio_run(rio, ctx, "clip", copy_input(ctx, q, "classes.tif"), "e.tif", "--to-data-window", files=["e.tif"]))
    out.append(rio_run(rio, ctx, "clip", p, "a.tif", "--bounds", "501000 4096000 505000 4099000"))
    out.append(rio_run(rio, ctx, "clip", p, "f.tif", "--bounds", "0 0 10 10"))
    out.append(rio_run(rio, ctx, "clip", p, "f.tif"))
    return out


@check("rio cli", "convert: --dtype, --scale-ratio, --scale-offset, --format, --co, --rgb, -o, --overwrite")
def _c_convert(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    out = [rio_run(rio, ctx, "convert", p, "a.tif", files=["a.tif"])]
    # A nodata value out of range of the new data type fails in both (rasterio leaves a broken file).
    out.append(rio_run(rio, ctx, "convert", p, "b.tif", "--dtype", "uint8", "--scale-ratio", "0.1", "--scale-offset", "5"))
    out.append(rio_run(rio, ctx, "convert", p, "b2.tif", "--dtype", "uint8", "--scale-ratio", "0.1", "--scale-offset", "5", "--co", "nodata=none", files=["b2.tif"]))
    out.append(rio_run(rio, ctx, "convert", p, "-o", "c.tif", "-t", "float32", "--scale-ratio", "0.5", "--co", "tiled=true", "--co", "blockxsize=128", "--co", "blockysize=128", "--co", "compress=zstd", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "convert", p, "d.tif", "-t", "uint8", "--rgb", "--co", "nodata=null", "--scale-ratio", "0.12", files=["d.tif"]))
    out.append(rio_run(rio, ctx, "convert", p, "e.tif", "-f", "GTiff", "--co", "compress=deflate", "--co", "predictor=2", files=["e.tif"]))
    out.append(rio_run(rio, ctx, "convert", p, "a.tif"))
    out.append(rio_run(rio, ctx, "convert", p, "a.tif", "--overwrite", "--scale-offset", "3", files=["a.tif"]))
    return out


def cli_tiles(rio: Any, ctx: Ctx) -> list[str]:
    """Two overlapping synthetic tiles for `merge` and `stack`.

    Args:
        rio: The module (unused: rasterio writes the files).
        ctx: The context.

    Returns:
        list: The paths.
    """
    t = ctx.reference.transform.from_origin
    a = synthetic(rio, ctx, "cli_tile_a.tif", overviews=False, width=160, height=120, count=2)
    b = synthetic(rio, ctx, "cli_tile_b.tif", overviews=False, width=150, height=130, count=2, transform=t(503000, 4098500, 30, 30))
    return [a, b]


@check("rio cli", "merge: --res, --bounds, --method, --nodata, --dtype, -b, --target-aligned-pixels, --use-highest-res, --co")
def _c_merge(rio: Any, ctx: Ctx) -> Any:
    a, b = cli_tiles(rio, ctx)
    out = [rio_run(rio, ctx, "merge", a, b, "m1.tif", files=["m1.tif"])]
    out.append(rio_run(rio, ctx, "merge", b, a, "-o", "m2.tif", "--res", "45", "--method", "max", files=["m2.tif"]))
    out.append(rio_run(rio, ctx, "merge", a, b, "m3.tif", "--bounds", "502000 4095000 506000 4098000", "--method", "last", "--nodata", "0", files=["m3.tif"]))
    out.append(rio_run(rio, ctx, "merge", a, b, "m4.tif", "-b", "2", "--method", "sum", "-t", "int32", "--res", "40", "--res", "35", "--target-aligned-pixels", files=["m4.tif"]))
    out.append(rio_run(rio, ctx, "merge", a, b, "m5.tif", "--method", "count", "--co", "compress=lzw", "--use-highest-res", files=["m5.tif"]))
    out.append(rio_run(rio, ctx, "merge", a, b, "m6.tif", "--method", "min", "--res", "60", files=["m6.tif"]))
    return out


@check("rio cli", "merge: --resampling bilinear", expect="tolerance", atol=4, max_frac=0.001, reason="reads resampled with bilinear kernels differ next to nodata pixels (GDAL's masked convolution)")
def _c_merge_bilinear(rio: Any, ctx: Ctx) -> Any:
    a, b = cli_tiles(rio, ctx)
    return rio_run(rio, ctx, "merge", a, b, "bilinear.tif", "--resampling", "bilinear", "--res", "60", files=["bilinear.tif"])


@check("rio cli", "merge: --resampling cubic", expect="known", reason="reads resampled with cubic kernels differ next to nodata pixels: GDAL 3.12's masked convolution is not terrio's (0.7% of pixels, valid or not)")
def _c_merge_cubic(rio: Any, ctx: Ctx) -> Any:
    a, b = cli_tiles(rio, ctx)
    return rio_run(rio, ctx, "merge", a, b, "cubic.tif", "--resampling", "cubic", "--res", "60", files=["cubic.tif"])


@check("rio cli", "overview: --build (list, powers, auto), --resampling, --ls, --rebuild")
def _c_overview(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    out = [rio_run(rio, ctx, "overview", p, "--ls")]
    a = copy_input(ctx, synthetic(rio, ctx, "cli_noovr.tif", overviews=False), "a.tif")
    out.append(rio_run(rio, ctx, "overview", a, "--build", "2,4,8", "--resampling", "average", files=["a.tif"]))
    # rasterio takes the method of --rebuild from the rio_overview domain, which terrio does not write.
    out.append(rio_run(rio, ctx, "overview", a, "--rebuild", "--resampling", "average", files=["a.tif"]))
    b = copy_input(ctx, synthetic(rio, ctx, "cli_noovr.tif", overviews=False), "b.tif")
    out.append(rio_run(rio, ctx, "overview", b, "--build", "2^1..3", files=["b.tif"]))
    c = copy_input(ctx, synthetic(rio, ctx, "cli_noovr.tif", overviews=False), "c.tif")
    out.append(rio_run(rio, ctx, "overview", c, "--build", "auto", "--resampling", "mode", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "overview", b, "--build", "2,x"))
    out.append(rio_run(rio, ctx, "overview", b, "--build", "2", "--resampling", "cubic"))
    out.append(rio_run(rio, ctx, "overview", b))
    return out


@check("rio cli", "overview --ls after --build: the method", expect="known", reason="terrio writes metadata of the default domain only, so the rio_overview domain holding the method is not saved: --ls says 'unknown'")
def _c_overview_ls(rio: Any, ctx: Ctx) -> Any:
    a = copy_input(ctx, synthetic(rio, ctx, "cli_noovr.tif", overviews=False), "a.tif")
    return [rio_run(rio, ctx, "overview", a, "--build", "2,4", "--resampling", "average"), rio_run(rio, ctx, "overview", a, "--ls")]


@check("rio cli", "warp: --dst-crs, --res, --dimensions, --bounds, --src-bounds, --like, --resampling, nodata, --target-aligned-pixels, --dry-run", expect="tolerance", rtol=1e-9, atol=2, max_frac=0.01, reason="the output grids differ in the last digits; GDAL's warp kernels next to nodata pixels")
def _c_warp(rio: Any, ctx: Ctx) -> Any:
    p, q = cli_raster(rio, ctx), cli_classes(rio, ctx)
    out = [rio_run(rio, ctx, "warp", p, "a.tif", "--dst-crs", "EPSG:4326", files=["a.tif"])]
    out.append(rio_run(rio, ctx, "warp", p, "b.tif", "--res", "45", files=["b.tif"]))
    out.append(rio_run(rio, ctx, "warp", p, "c.tif", "--dimensions", "100", "80", "--dst-crs", "EPSG:32610", "--resampling", "bilinear", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "warp", p, "d.tif", "--bounds", "-116.99", "37.0", "-116.93", "37.04", "--res", "0.0005", "--dst-crs", "EPSG:4326", files=["d.tif"]))
    out.append(rio_run(rio, ctx, "warp", p, "e.tif", "--like", q, files=["e.tif"]))
    out.append(rio_run(rio, ctx, "warp", p, "f.tif", "--src-bounds", "501000", "4096000", "505000", "4099000", "--res", "25", "--src-nodata", "-9999", "--dst-nodata", "-1", files=["f.tif"]))
    out.append(rio_run(rio, ctx, "warp", p, "g.tif", "--dst-crs", "EPSG:3857", "--res", "50", "--target-aligned-pixels", "--resampling", "average", files=["g.tif"]))
    out.append(rio_run(rio, ctx, "warp", p, "h.tif", "--res", "45", "--dry-run"))
    out.append(rio_run(rio, ctx, "warp", p, "i.tif", "--like", q, "--res", "10"))
    out.append(rio_run(rio, ctx, "warp", p, "j.tif", "--dst-crs", "EPSG:4326", "--resampling", "cubic", "--co", "compress=lzw", files=["j.tif"]))
    return out


@check("rio cli", "warp --dry-run with --dst-crs: the profile's CRS", expect="known", reason="rasterio's --dry-run prints the source's CRS as the output's; terrio.rio prints the output CRS")
def _c_warp_dry_run(rio: Any, ctx: Ctx) -> Any:
    return [rio_run(rio, ctx, "warp", cli_raster(rio, ctx), "h.tif", "--dst-crs", "EPSG:4326", "--dry-run")]


@check("rio cli", "sample: points from stdin and argument, --bidx forms")
def _c_sample(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    pts = "[501000, 4099000]\n[500015.5, 4099985]\n[508000, 4094000]\n[501500, 4099500]\n"
    out = [rio_run(rio, ctx, "sample", p, stdin=pts), rio_run(rio, ctx, "sample", p, "[503333.3, 4096666.6]")]
    out += [rio_run(rio, ctx, "sample", p, "-b", b, stdin=pts) for b in ("2", "..2", "2..", "1,3")]
    (ctx.out / "pts.txt").write_text(pts)
    out.append(rio_run(rio, ctx, "sample", p, "pts.txt", "--bidx", "3"))
    return out


@check("rio cli", "transform: stdin, --src-crs, --dst-crs (codes, PROJ strings, a dataset's CRS), --precision", **CLI_TOL)
def _c_transform(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    pts = "[-117.9, 37.02]\n[-117.5, 37.5, -118.0, 36.9]\n"
    out = [rio_run(rio, ctx, "transform", "--dst-crs", "EPSG:32611", stdin=pts), rio_run(rio, ctx, "transform", "--dst-crs", "EPSG:32611", "--precision", "2", stdin=pts)]
    out.append(rio_run(rio, ctx, "transform", "[500000, 4100000]", "--src-crs", p, "--dst_crs", "EPSG:4326"))
    out.append(rio_run(rio, ctx, "transform", "--src_crs", "+proj=utm +zone=11 +datum=WGS84", "--dst-crs", "EPSG:3857", stdin="[500000, 4100000]\n"))
    return out


@check("rio cli", "mask: --geojson-mask, --crop, --invert, --all, stdin, no features (copy)")
def _c_mask(rio: Any, ctx: Ctx) -> Any:
    p, g = cli_raster(rio, ctx), cli_geojson(ctx)
    out = [rio_run(rio, ctx, "mask", p, "a.tif", "-j", g, files=["a.tif"])]
    out.append(rio_run(rio, ctx, "mask", p, "b.tif", "--geojson-mask", g, "--crop", files=["b.tif"]))
    out.append(rio_run(rio, ctx, "mask", p, "c.tif", "-j", g, "--invert", "--all", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "mask", p, "d.tif", "-j", "-", "--crop", "--invert", stdin=Path(g).read_text(), files=["d.tif"]))
    out.append(rio_run(rio, ctx, "mask", p, "e.tif", files=["e.tif"]))
    out.append(rio_run(rio, ctx, "mask", p, "f.tif", "-j", "-", "--crop", stdin='{"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]}'))
    return out


@check("rio cli", "rasterize: --res, --dimensions, --bounds, --like, --property, --default-value, --fill, --all, --nodata, an existing output")
def _c_rasterize(rio: Any, ctx: Ctx) -> Any:
    g, q = cli_geojson(ctx), cli_classes(rio, ctx)
    out = [rio_run(rio, ctx, "rasterize", g, "a.tif", "--res", "30", "--src-crs", "EPSG:32611", files=["a.tif"])]
    out.append(rio_run(rio, ctx, "rasterize", g, "b.tif", "--dimensions", "150", "120", "--bounds", "500000 4096000 504500 4100000", "--src-crs", "EPSG:32611", "--property", "value", "--all", files=["b.tif"]))
    out.append(rio_run(rio, ctx, "rasterize", g, "c.tif", "--like", q, "--default-value", "7", "--fill", "3", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "rasterize", g, "d.tif", "--res", "40", "--res", "20", "--src-crs", "EPSG:32611", "--nodata", "0", "--co", "compress=lzw", files=["d.tif"]))
    e = copy_input(ctx, q, "e.tif")
    out.append(rio_run(rio, ctx, "rasterize", g, e, "--property", "value", files=["e.tif"]))
    out.append(rio_run(rio, ctx, "rasterize", "-o", "f.tif", "--dimensions", "40", "30", "--bounds", "-118 37 -117.5 37.4", stdin='{"type": "Feature", "properties": {}, "geometry": {"type": "Polygon", "coordinates": [[[-117.9, 37.1], [-117.6, 37.1], [-117.7, 37.3], [-117.9, 37.1]]]}}', files=["f.tif"]))
    out.append(rio_run(rio, ctx, "rasterize", g, "h.tif", "--src-crs", "EPSG:32611"))
    return out


@check("rio cli", "stack: all bands, --bidx forms per input, --rgb, --res, --bounds, --dtype, --nodata", min_rasterio=(1, 5, 2))
def _c_stack(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    a, b = cli_tiles(rio, ctx)
    out = [rio_run(rio, ctx, "stack", p, "s1.tif", files=["s1.tif"])]
    out.append(rio_run(rio, ctx, "stack", p, "--bidx", "..2", p, "--bidx", "3..", "-o", "s2.tif", files=["s2.tif"]))
    out.append(rio_run(rio, ctx, "stack", p, "-b", "1,3", p, "-b", "2", "s3.tif", "--rgb", "-t", "int32", files=["s3.tif"]))
    out.append(rio_run(rio, ctx, "stack", a, b, "s4.tif", "--res", "45", "--nodata", "-1", files=["s4.tif"]))
    out.append(rio_run(rio, ctx, "stack", a, b, "s5.tif", "--bounds", "502000 4095000 506000 4098000", "-b", "1", "-b", "2", files=["s5.tif"]))
    return out


@check("rio cli", "edit-info: --nodata, --unset-nodata, --crs, --transform, --tag, --units, --description, --colorinterp, --like (--all, like), errors")
def _c_edit_info(rio: Any, ctx: Ctx) -> Any:
    p, q = cli_raster(rio, ctx), cli_classes(rio, ctx)
    out = []
    a = copy_input(ctx, p, "a.tif")
    out.append(rio_run(rio, ctx, "edit-info", a, "--nodata", "-1", "--tag", "author=me", "--tag", "year=2026", "--units", "metres", "--description", "elevation", "-b", "2", files=["a.tif"]))
    out.append(rio_run(rio, ctx, "edit-info", a, "--crs", "EPSG:32610", "--transform", "[20.0, 0.0, 400000.0, 0.0, -20.0, 4000000.0]", files=["a.tif"]))
    out.append(rio_run(rio, ctx, "edit-info", a, "--colorinterp", "RGB", files=["a.tif"]))
    out.append(rio_run(rio, ctx, "edit-info", a, "--unset-nodata", files=["a.tif"]))
    b = copy_input(ctx, p, "b.tif")
    out.append(rio_run(rio, ctx, "edit-info", b, "--like", q, "--crs", "like", "--nodata", "like", "--transform", "like", files=["b.tif"]))
    c = copy_input(ctx, q, "c.tif")
    out.append(rio_run(rio, ctx, "edit-info", c, "--like", p, "--all", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "edit-info", c, "--crs", '{"proj": "utm", "zone": 12, "datum": "WGS84"}', "--colorinterp", "red", files=["c.tif"]))
    d = copy_input(ctx, q, "d.tif")
    out += [rio_run(rio, ctx, "edit-info", d, *bad) for bad in (["--nodata", "300"], ["--crs", "EPSG:0"], ["--unset-nodata", "--nodata", "1"], ["--colorinterp", "red,green"], ["--colorinterp", "purple"])]
    return out


@check("rio cli", "calc: arithmetic, read with band and dtype, asarray, --name and take, comparisons, masked and not, --dtype, --co")
def _c_calc(rio: Any, ctx: Ctx) -> Any:
    p, q = cli_raster(rio, ctx), cli_classes(rio, ctx)
    out = [rio_run(rio, ctx, "calc", "(+ 2 (* 0.95 (read 1)))", p, "a.tif", files=["a.tif"])]
    out.append(rio_run(rio, ctx, "calc", "(asarray (+ 125 (read 1 1)) (read 1 2) (- (read 1 3) 1000))", p, "b.tif", files=["b.tif"]))
    out.append(rio_run(rio, ctx, "calc", "(/ (- (take a 2) (take a 1)) (+ (take a 2) (take a 1) 1.0))", "--name", f"a={p}", "c.tif", "-t", "float32", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "calc", "(* (read 1 1 'float64') 1.5e-1)", p, "d.tif", "--dtype", "float64", "--not-masked", "--co", "compress=lzw", files=["d.tif"]))
    out.append(rio_run(rio, ctx, "calc", "(asarray (> (read 1 1) 500) (& (>= (read 1 2) 100) (<= (read 1 2) 900)))", p, "e.tif", "-t", "uint8", "--not-masked", "--co", "nodata=none", files=["e.tif"]))
    out.append(rio_run(rio, ctx, "calc", "(> (read 1 1) 500)", p, "e2.tif", "-t", "uint8"))
    out.append(rio_run(rio, ctx, "calc", "(+ (read 1) 1)", q, "f.tif", "--mem-limit", "1", files=["f.tif"]))
    out.append(rio_run(rio, ctx, "calc", "(+ (read 1) foo)", q, "g.tif"))
    return out


@check("rio cli", "create, rm, env --credentials, insp, global options, errors")
def _c_misc(rio: Any, ctx: Ctx) -> Any:
    p = cli_raster(rio, ctx)
    out = [rio_run(rio, ctx, "create", "a.tif", "-f", "GTiff", "-t", "uint8", "-n", "3", "-h", "64", "-w", "80", "--crs", "EPSG:32611", "--bounds", "500000 4098080 502400 4100000", "--nodata", "0", "--co", "tiled=true", "--co", "blockxsize=32", "--co", "blockysize=32", files=["a.tif"])]
    out.append(rio_run(rio, ctx, "create", "b.tif", "-t", "float32", "-n", "1", "--height", "10", "--width", "20", "--transform", "[1.0, 0.0, 0.0, 0.0, -1.0, 10.0]", files=["b.tif"]))
    out.append(rio_run(rio, ctx, "create", "a.tif", "-t", "uint8", "-n", "1", "-h", "8", "-w", "8"))
    c = copy_input(ctx, p, "c.tif")
    out.append(rio_run(rio, ctx, "rm", c, "--yes"))
    out.append(sorted(f.name for f in ctx.out.iterdir() if f.name.startswith("c.")))
    out.append(rio_run(rio, ctx, "rm", "missing.tif", "--yes"))
    out.append(rio_run(rio, ctx, "env", "--credentials"))
    out.append(rio_run(rio, ctx, "env", output=lambda o: "GTiff" in [line.split(":")[0] for line in o]))
    out.append(rio_run(rio, ctx, "insp", p, stdin="print(src.count, src.shape, src.crs.to_epsg())\nprint(stats((src, 2)))\n"))
    out.append(rio_run(rio, ctx, "--aws-no-sign-requests", "info", p, "--count"))
    out.append(rio_run(rio, ctx, "-q", "info", p, "--width"))
    out.append(rio_run(rio, ctx, "info", "missing.tif"))
    out.append(rio_run(rio, ctx, "info", p, "--unknown-option"))
    out.append(rio_run(rio, ctx, "bounds"))
    return out


# Real data: public COGs, read over HTTP and downloaded (once) into a cache directory.


def public_file(ctx: Ctx, name: str, url: str, window: Any = None) -> str:
    """A public raster, downloaded (or a window of it, read over HTTP by rasterio) and cached.

    The cache is `TERRIO_RIO_CLI_DATA` when it is set, else the run's scratch directory.

    Args:
        ctx: The context.
        name: The cached file's name.
        url: The COG's URL.
        window: `((row_start, row_stop), (col_start, col_stop))`: keep this window only, with the
            file's profile.

    Returns:
        str: The local path.
    """
    import urllib.request

    cache = Path(os.environ.get("TERRIO_RIO_CLI_DATA") or ctx.out.parent.parent / "cli-data")
    cache.mkdir(parents=True, exist_ok=True)
    path = cache / name
    if path.exists():
        return str(path)
    tmp = path.with_suffix(".part")
    if window is None:
        with urllib.request.urlopen(url, timeout=120) as resp, open(tmp, "wb") as f:
            shutil.copyfileobj(resp, f)
    else:
        ref = ctx.reference
        with ref.open(url) as src:
            w = ref.windows.Window.from_slices(*window)
            profile = src.profile
            profile.update(width=int(w.width), height=int(w.height), transform=src.window_transform(w), driver="GTiff")
            with ref.open(tmp, "w", **profile) as dst:
                dst.write(src.read(window=w))
    tmp.rename(path)
    return str(path)


def dem(ctx: Ctx, tile: str = DEM_TILES[0]) -> str:
    """A Copernicus GLO-30 DEM tile (3600 x 3600 float32), downloaded.

    Args:
        ctx: The context.
        tile: The tile, such as `N40_00_W106_00`.

    Returns:
        str: The path.
    """
    return public_file(ctx, f"Copernicus_DSM_COG_10_{tile}_DEM.tif", COPERNICUS.format(tile))


def s2(ctx: Ctx, band: str) -> str:
    """A 2048 x 2048 window of a Sentinel-2 L2A band (B04, B08 at 10 m; SCL at 20 m: 1024 x 1024).

    Args:
        ctx: The context.
        band: `B04`, `B08`, `TCI` or `SCL`.

    Returns:
        str: The path.
    """
    # The east of the tile, with the edge of the swath (nodata, 0) in the west of the windows.
    w = ((3000, 4024), (4250, 5274)) if band == "SCL" else ((6000, 8048), (8500, 10548))
    return public_file(ctx, f"S2_{band}_window.tif", SENTINEL2.format(band), w)


REMOTE_TOL = dict(needs_remote=True, **CLI_TOL)


@check("rio cli, public COGs", "info of COGs over HTTP: Copernicus DEM, Sentinel-2 B04, TCI and SCL (JSON, --crs, --bounds, --res, --lnglat)", **REMOTE_TOL)
def _cr_info(rio: Any, ctx: Ctx) -> Any:
    urls = [COPERNICUS.format(DEM_TILES[0]), SENTINEL2.format("B04"), SENTINEL2.format("TCI"), SENTINEL2.format("SCL")]
    out = []
    for u in urls:
        out.append(rio_run(rio, ctx, "info", u))
        out += [rio_run(rio, ctx, "info", u, opt) for opt in ("--crs", "--bounds", "-r", "--lnglat", "--shape")]
    return out


@check("rio cli, public COGs", "bounds of the COGs over HTTP (geographic, --projected, --mercator, --bbox, --collection)", **REMOTE_TOL)
def _cr_bounds(rio: Any, ctx: Ctx) -> Any:
    urls = [COPERNICUS.format(t) for t in DEM_TILES] + [SENTINEL2.format("B04"), SENTINEL2.format("SCL")]
    return [rio_run(rio, ctx, "bounds", *urls, *a) for a in ([], ["--projected"], ["--mercator", "--collection"], ["--bbox", "--precision", "5"])]


@check("rio cli, public COGs", "blocks of the Sentinel-2 SCL COG over HTTP; sample of the DEM and TCI over HTTP", **REMOTE_TOL)
def _cr_blocks(rio: Any, ctx: Ctx) -> Any:
    out = [rio_run(rio, ctx, "blocks", SENTINEL2.format("SCL")), rio_run(rio, ctx, "blocks", SENTINEL2.format("SCL"), "--projected", "--sequence")]
    out.append(rio_run(rio, ctx, "sample", COPERNICUS.format(DEM_TILES[0]), stdin="[-105.5, 40.5]\n[-105.27, 40.01]\n[-105.99, 40.99]\n[-104.0, 40.0]\n"))
    out.append(rio_run(rio, ctx, "sample", SENTINEL2.format("TCI"), stdin="[505000, 4440000]\n[500000, 4499000]\n[509000, 4391000]\n[400000, 4499000]\n"))
    return out


@check("rio cli, public COGs", "clip the Sentinel-2 TCI COG over HTTP (--geographic bounds), and the DEM by --like and --bounds", needs_remote=True)
def _cr_clip(rio: Any, ctx: Ctx) -> Any:
    out = [rio_run(rio, ctx, "clip", SENTINEL2.format("TCI"), "tci.tif", "--bounds", "-104.98 40.0 -104.94 40.04", "--geographic", files=["tci.tif"])]
    out.append(rio_run(rio, ctx, "clip", dem(ctx), "dem_like.tif", "--like", s2(ctx, "SCL"), files=["dem_like.tif"]))
    out.append(rio_run(rio, ctx, "clip", dem(ctx), "dem_box.tif", "--bounds", "-105.6 40.2 -105.4 40.4", "--co", "compress=lzw", files=["dem_box.tif"]))
    return out


@check("rio cli, public COGs", "merge three Copernicus DEM tiles (--res, --bounds, --method)", needs_remote=True)
def _cr_merge(rio: Any, ctx: Ctx) -> Any:
    tiles = [dem(ctx, t) for t in DEM_TILES]
    out = [rio_run(rio, ctx, "merge", *tiles, "m1.tif", "--res", "0.002", files=["m1.tif"])]
    out.append(rio_run(rio, ctx, "merge", *tiles, "m2.tif", "--bounds", "-106.2 39.8 -105.8 40.2", "--method", "max", files=["m2.tif"]))
    out.append(rio_run(rio, ctx, "merge", *tiles[:2], "m3.tif", "--bounds", "-106 39.9 -105.5 40.1", "--res", "0.0005", "--method", "last", files=["m3.tif"]))
    return out


@check("rio cli, public COGs", "warp the DEM to UTM (--dst-crs, --res, bilinear) and Sentinel-2 to WGS 84 (nearest, --dimensions)", needs_remote=True, expect="tolerance", rtol=1e-9, atol=0.5, max_frac=0.01, reason="the output grids differ in the last digits; bilinear weights computed by terrio and GDAL differ in rounding")
def _cr_warp(rio: Any, ctx: Ctx) -> Any:
    box = public_file(ctx, "dem_box_window.tif", COPERNICUS.format(DEM_TILES[0]), ((1800, 2600), (1000, 1800)))
    out = [rio_run(rio, ctx, "warp", box, "a.tif", "--dst-crs", "EPSG:32613", files=["a.tif"])]
    out.append(rio_run(rio, ctx, "warp", box, "b.tif", "--dst-crs", "EPSG:32613", "--res", "50", "--resampling", "bilinear", files=["b.tif"]))
    out.append(rio_run(rio, ctx, "warp", s2(ctx, "B04"), "c.tif", "--dst-crs", "EPSG:4326", files=["c.tif"]))
    out.append(rio_run(rio, ctx, "warp", s2(ctx, "SCL"), "d.tif", "--dst-crs", "EPSG:4326", "--dimensions", "600", "500", files=["d.tif"]))
    out.append(rio_run(rio, ctx, "warp", s2(ctx, "B08"), "e.tif", "--like", s2(ctx, "SCL"), "--resampling", "average", files=["e.tif"]))
    return out


@check("rio cli, public COGs", "calc NDVI from Sentinel-2 B04 and B08, convert scaled to uint8, stack, overviews", needs_remote=True, expect="tolerance", atol=1e-6, rtol=1e-6, max_frac=0.0, reason="float32 division")
def _cr_calc(rio: Any, ctx: Ctx) -> Any:
    red, nir = s2(ctx, "B04"), s2(ctx, "B08")
    ndvi = "(/ (- (read 2 1 'float32') (read 1 1 'float32')) (+ (read 2 1 'float32') (read 1 1 'float32')))"
    out = [rio_run(rio, ctx, "calc", ndvi, red, nir, "ndvi.tif", "-t", "float32", files=["ndvi.tif"])]
    out.append(rio_run(rio, ctx, "calc", "(/ (- (take n 1) (take r 1)) (+ (take n 1) (take r 1) 0.0))", "--name", f"r={red}", "--name", f"n={nir}", "ndvi2.tif", "-t", "float32", "--co", "compress=deflate", files=["ndvi2.tif"]))
    out.append(rio_run(rio, ctx, "convert", red, "red8.tif", "--dtype", "uint8", "--scale-ratio", "0.0625", files=["red8.tif"]))
    out.append(rio_run(rio, ctx, "stack", red, nir, s2(ctx, "B04"), "--bidx", "1", "rgb.tif", "--rgb", files=["rgb.tif"]))
    box = public_file(ctx, "dem_box_window.tif", COPERNICUS.format(DEM_TILES[0]), ((1800, 2600), (1000, 1800)))
    a = copy_input(ctx, box, "dem.tif")
    out.append(rio_run(rio, ctx, "overview", a, "--build", "2,4,8", "--resampling", "average", files=["dem.tif"]))
    return out


@check("rio cli, public COGs", "overview --build on a downloaded COG (Copernicus DEM)", needs_remote=True, expect="known", reason="GDAL refuses to update a file with the COG layout (IGNORE_COG_LAYOUT_BREAK); terrio.rio rewrites it")
def _cr_overview_cog(rio: Any, ctx: Ctx) -> Any:
    a = copy_input(ctx, dem(ctx), "dem.tif")
    return rio_run(rio, ctx, "overview", a, "--build", "2,4,8,16", "--resampling", "average")


@check("rio cli, public COGs", "shapes of the Sentinel-2 scene classification (SCL), as sets; mask and rasterize on the DEM", needs_remote=True, **CLI_TOL)
def _cr_shapes(rio: Any, ctx: Ctx) -> Any:
    scl = s2(ctx, "SCL")
    small = public_file(ctx, "S2_SCL_small.tif", SENTINEL2.format("SCL"), ((3000, 3200), (4500, 4750)))
    out = [rio_run(rio, ctx, "shapes", small, "--projected", output=feature_set), rio_run(rio, ctx, "shapes", small, "--collection", "--precision", "6", output=feature_set)]
    out.append(rio_run(rio, ctx, "shapes", scl, "--as-mask", "--projected", "--sampling", "4", output=feature_set))
    box = public_file(ctx, "dem_box_window.tif", COPERNICUS.format(DEM_TILES[0]), ((1800, 2600), (1000, 1800)))
    poly = {"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {"h": 2000}, "geometry": {"type": "Polygon", "coordinates": [[[-105.68, 40.42], [-105.55, 40.45], [-105.6, 40.33], [-105.68, 40.42]]]}}, {"type": "Feature", "properties": {"h": 3000}, "geometry": {"type": "LineString", "coordinates": [[-105.7, 40.3], [-105.5, 40.47]]}}]}
    g = ctx.out / "poly.geojson"
    g.write_text(json.dumps(poly))
    out.append(rio_run(rio, ctx, "mask", box, "masked.tif", "-j", g.name, "--crop", files=["masked.tif"]))
    out.append(rio_run(rio, ctx, "mask", box, "masked2.tif", "-j", g.name, "--invert", "--all", files=["masked2.tif"]))
    out.append(rio_run(rio, ctx, "rasterize", g.name, "burn.tif", "--like", box, "--property", "h", "--all", files=["burn.tif"]))
    out.append(rio_run(rio, ctx, "edit-info", copy_input(ctx, box, "edited.tif"), "--nodata", "-9999", "--tag", "source=Copernicus GLO-30", "--description", "elevation", "--units", "m", files=["edited.tif"]))
    return out


# ─── Examples ───────────────────────────────────────────────────────────────────────────────────


def to_terrio(source: str) -> str:
    """Rewrites a script's rasterio imports to terrio.rio, and nothing else.

    Args:
        source: The script.

    Returns:
        str: The script importing terrio.rio as rasterio.
    """
    source = re.sub(r"^import rasterio$", "import terrio.rio as rasterio", source, flags=re.M)
    source = re.sub(r"^import rasterio\.(\w+)$", r"import terrio.rio.\1", source, flags=re.M)
    source = re.sub(r"^from rasterio(\.\w+)? import", r"from terrio.rio\1 import", source, flags=re.M)
    return source


def run_example(script: Path, data: Path, out: Path) -> Outcome:
    """Runs an example with rasterio and with terrio.rio, and compares output and files.

    Args:
        script: The example script.
        data: The data directory.
        out: A scratch directory.

    Returns:
        Outcome: The comparison.
    """
    import rasterio

    results = {}
    for name in ("rasterio", "terrio"):
        d = out / "examples" / script.stem / name
        d.mkdir(parents=True, exist_ok=True)
        src = script.read_text()
        if name == "terrio":
            src = to_terrio(src)
            if "terrio.rio" not in src:
                return Outcome("error", "import not rewritten")
        path = d / script.name
        path.write_text(src)
        r = subprocess.run([sys.executable, str(path), str(data), str(d)], capture_output=True, text=True, cwd=str(d))
        if r.returncode != 0:
            return Outcome("error", f"{name}: {r.stderr.strip().splitlines()[-1] if r.stderr.strip() else r.returncode}")
        files = {}
        for f in sorted(d.iterdir()):
            if f.suffix == ".tif":
                with rasterio.open(f) as ds:
                    files[f.name] = [norm(meta_of(ds)), ds.read()]
            elif f.suffix == ".json":
                files[f.name] = json.loads(f.read_text())
        results[name] = [r.stdout.replace("terrio.rio", "rasterio"), files]
    c = Check("examples", script.stem, lambda m, c: None, expect="tolerance", rtol=1e-12, reason="output grids differ in the last digits")
    return compare(results["rasterio"], results["terrio"], c)


# ─── Main ───────────────────────────────────────────────────────────────────────────────────────


@dataclass
class Row:
    """A line of the report."""

    section: str
    name: str
    expect: str
    outcome: Outcome
    reason: str


def run_all(data: Path | None, remote: bool, only: str | None = None, out: Path | None = None) -> list[Row]:
    """Runs every applicable check.

    Args:
        data: The data directory, or None for synthetic checks only.
        remote: Include the remote checks.
        only: Run this section only.
        out: A scratch directory (temporary by default).

    Returns:
        list: The report's rows.
    """
    import rasterio
    import rasterio.features
    import rasterio.io
    import rasterio.mask
    import rasterio.merge
    import rasterio.vrt
    import rasterio.warp

    import terrio.rio as trio

    tmp = Path(tempfile.mkdtemp(prefix="rio-compare-")) if out is None else out
    rows = []
    try:
        for c in CHECKS:
            if only and c.section != only:
                continue
            if (c.needs_data and data is None) or (c.needs_remote and not remote):
                continue
            if c.min_rasterio and rasterio_version(rasterio) < c.min_rasterio:
                continue
            o = run_check(c, rasterio, trio, data, tmp)
            rows.append(Row(c.section, c.name, c.expect, o, c.reason))
        if data is not None and (only is None or only == "examples"):
            for script in sorted(EXAMPLES.glob("*.py")):
                rows.append(Row("examples", script.name, "tolerance", run_example(script, data, tmp), "output grids differ in the last digits"))
    finally:
        if out is None:
            shutil.rmtree(tmp, ignore_errors=True)
    return rows


def failed(row: Row) -> bool:
    """Whether a row is a failure.

    Args:
        row: The row.

    Returns:
        bool: True when the results differ more than the check allows.
    """
    s = row.outcome.status
    return s in ("different", "error") or (s == "tolerance" and row.expect == "identical")


def report(rows: list[Row]) -> str:
    """The Markdown report.

    Args:
        rows: The rows.

    Returns:
        str: The report.
    """
    counts = {s: sum(r.outcome.status == s for r in rows) for s in ("identical", "tolerance", "known", "different", "error")}
    lines = [
        "# terrio.rio compared with rasterio",
        "",
        f"{len(rows)} checks: {counts['identical']} identical, {counts['tolerance']} within tolerance, "
        f"{counts['known']} known differences, {counts['different']} different, {counts['error']} errors.",
        "",
        "| Section | Check | Result | Details |",
        "|---|---|---|---|",
    ]
    for r in rows:
        status = {"identical": "identical", "tolerance": "within tolerance", "known": "known difference", "different": "**DIFFERENT**", "error": "**ERROR**"}[r.outcome.status]
        detail = r.outcome.detail
        if r.outcome.status in ("tolerance", "known") and r.reason:
            detail = f"{r.reason}: {detail}"
        detail = detail.replace("|", "\\|").replace("\n", " ")
        lines.append(f"| {r.section} | {r.name} | {status} | {detail[:400]} |")
    return "\n".join(lines) + "\n"


def main() -> int:
    """Runs the comparison from the command line.

    Returns:
        int: The exit status.
    """
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("data", nargs="?", default=os.environ.get("TERRIO_TEST_DATA"))
    ap.add_argument("--remote", action="store_true", help="also read a public COG over HTTP")
    ap.add_argument("--output", help="write the Markdown report to this file")
    ap.add_argument("--only", help="run one section")
    args = ap.parse_args()
    data = Path(args.data) if args.data else None
    rows = run_all(data, args.remote, args.only)
    text = report(rows)
    print(text)
    if args.output:
        Path(args.output).write_text(text)
    return 1 if any(failed(r) for r in rows) else 0


if __name__ == "__main__":
    sys.exit(main())
