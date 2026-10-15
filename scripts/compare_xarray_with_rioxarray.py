"""Compare terrio.xarray, terrio's xarray integration, with rioxarray.

Usage:
    python scripts/compare_xarray_with_rioxarray.py [DATA_DIR] [--remote] [--output REPORT.md]
        [--only SECTION] [--verbose]

Every check runs the same code twice: once with rioxarray (`rioxarray.open_rasterio`, the `.rio`
accessor, `rioxarray.merge`) and once with terrio (`terrio.xarray.open_rasterio`, the `.terrio`
accessor, `terrio.xarray.merge_arrays`), and compares what the two runs return with
`xarray.testing.assert_identical`: dimensions, coordinates, values (NaNs included), attributes,
the `spatial_ref` grid mapping coordinate and its attributes. Encodings (`_FillValue`, `dtype`,
`scale_factor`, `add_offset`, `grid_mapping`, `preferred_chunks`, `rasterio_dtype`) and dask
chunks are compared as well. Each check states what it expects:

- ``identical``: `assert_identical` passes, and the encodings agree. When the only difference is
  the text of the CRS (terrio and GDAL write the WKT of some GeoTIFF CRSs differently, for the
  same CRS), the check passes and the report says so;
- ``tolerance``: values and coordinates may differ within stated bounds, for a stated reason
  (floating-point arithmetic of resampling kernels and of the suggested output grid).

Files written by `to_raster` are read back by rioxarray, by terrio and by rasterio. Reads of
local files served over HTTP count the bytes fetched, to show that windows read only the tiles
they need.

Checks without DATA_DIR use synthetic GeoTIFFs written with rasterio; DATA_DIR (the directory
``scripts/fetch_test_data.sh`` fills, also read from ``TERRIO_TEST_DATA``) adds checks on real
files: DEMs with nodata, multiband Landsat, several data types, USGS 3DEP tiles to merge.
``--remote`` adds a public Cloud Optimized GeoTIFF read over HTTPS.

Prints a Markdown agreement table; exits non-zero when a result differs more than its check
allows. Needs xarray, rioxarray (with rasterio), dask and terrio; zarr for the Zarr checks and
scipy for `interpolate_na`.
"""

from __future__ import annotations

import argparse
import http.server
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import urllib.request
import warnings
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import numpy as np

warnings.simplefilter("ignore")

REMOTE_COG = (
    "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/"
    "USGS_1_n40w106.tif"
)

# Real files: DEMs with nodata (NaN, -999999, -inf, -32768), multiband Landsat (uint8, nodata 0,
# band descriptions, tags and statistics), float32 and float64, uint8 classes, int16, COGs with
# overviews, NAD83, UTM, Web Mercator and Albers CRSs.
REAL_FILES = [
    "dem.tif",
    "landsat.tif",
    "landsat7.tif",
    "srtm_sub.tif",
    "srtm90.tif",
    "USGS_1_n40w106.tif",
    "OPERA_L3_DSWx_WTR.tif",
    "las_vegas_dem.tif",
    "nlcd_knoxville.tif",
    "cog.tif",
    "ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif",
]
USGS_TILES = [
    "USGS_1_n40w106.tif",
    "USGS_1_n40w107.tif",
    "USGS_1_n41w106.tif",
    "USGS_1_n41w107.tif",
]

# Attributes of the grid mapping derived from the CRS's WKT text.
_WKT_ATTRS = (
    "crs_wkt",
    "spatial_ref",
    "inverse_flattening",
    "semi_minor_axis",
    "geographic_crs_name",
    "projected_crs_name",
    "horizontal_datum_name",
    "reference_ellipsoid_name",
    "prime_meridian_name",
)
_ENCODING_KEYS = (
    "_FillValue",
    "dtype",
    "scale_factor",
    "add_offset",
    "grid_mapping",
    "preferred_chunks",
    "rasterio_dtype",
    "scales",
    "offsets",
)


# ─── The two implementations ────────────────────────────────────────────────────────────────────


class Flavor:
    """One implementation: rioxarray, or terrio.xarray."""

    def __init__(self, name: str) -> None:
        """Imports the implementation.

        Args:
            name: `rioxarray` or `terrio`.
        """
        self.name = name
        if name == "rioxarray":
            import rioxarray
            import rioxarray.merge

            self._open = rioxarray.open_rasterio
            self.merge_arrays = rioxarray.merge.merge_arrays
            self.merge_datasets = rioxarray.merge.merge_datasets
            self.engine = "rasterio"
            import rasterio.windows

            self.Window = rasterio.windows.Window
        else:
            import terrio.rio.windows
            import terrio.xarray

            self._open = terrio.xarray.open_rasterio
            self.merge_arrays = terrio.xarray.merge_arrays
            self.merge_datasets = terrio.xarray.merge_datasets
            self.engine = "terrio"
            self.Window = terrio.rio.windows.Window

    def open(self, path: str, **kw: Any) -> Any:
        """Opens a raster.

        Args:
            path: The path or URL.
            **kw: Options of `open_rasterio`.

        Returns:
            xarray.DataArray | xarray.Dataset: The raster.
        """
        return self._open(path, **kw)

    def acc(self, obj: Any) -> Any:
        """The accessor of an object.

        Args:
            obj: A DataArray or Dataset.

        Returns:
            Any: `.rio` or `.terrio`.
        """
        return obj.rio if self.name == "rioxarray" else obj.terrio


# ─── Checks ─────────────────────────────────────────────────────────────────────────────────────


@dataclass
class Ctx:
    """What checks get besides the implementation: directories, the HTTP server."""

    data: Path | None
    out: Path
    synth: dict[str, str] = field(default_factory=dict)
    server: Any = None

    def path(self, name: str) -> str:
        """The path of a real test file, or of a synthetic one.

        Args:
            name: The file name.

        Returns:
            str: The path.
        """
        if name in self.synth:
            return self.synth[name]
        assert self.data is not None
        return str(self.data / name)


@dataclass
class Check:
    """A comparison: a function run with each implementation, and what agreement it expects."""

    section: str
    name: str
    func: Callable[[Flavor, Ctx], Any]
    expect: str = "identical"
    rtol: float = 0.0
    atol: float = 0.0
    max_frac: float = 0.0
    coord_rtol: float = 0.0
    reason: str = ""
    needs_data: bool = False
    needs_remote: bool = False
    needs: tuple[str, ...] = ()


CHECKS: list[Check] = []


def add(section: str, name: str, func: Callable[[Flavor, Ctx], Any], **kw: Any) -> None:
    """Registers a check.

    Args:
        section: The section of the report.
        name: The check's name.
        func: The function, run with each implementation.
        **kw: `Check` fields.
    """
    CHECKS.append(Check(section, name, func, **kw))


# ─── Comparing results ──────────────────────────────────────────────────────────────────────────


@dataclass
class Outcome:
    """The result of a check."""

    status: str  # identical, tolerance, differs, error
    detail: str = ""


def _grid_mapping_names(obj: Any) -> list[str]:
    """The grid mapping coordinates of an object.

    Args:
        obj: A DataArray or Dataset.

    Returns:
        list: Their names.
    """
    return [str(c) for c in obj.coords if "crs_wkt" in obj.coords[c].attrs]


def _normalize_crs_text(a: Any, b: Any) -> tuple[Any, str]:
    """Gives b's grid mapping a's CRS text when both describe the same CRS.

    Args:
        a: The rioxarray result.
        b: The terrio result.

    Returns:
        tuple: b (normalized) and a note, empty when nothing was changed.
    """
    from terrio.rio.crs import CRS

    notes = []
    for name in _grid_mapping_names(a):
        if name not in b.coords or "crs_wkt" not in b.coords[name].attrs:
            continue
        ga, gb = a.coords[name].attrs, b.coords[name].attrs
        if ga["crs_wkt"] == gb["crs_wkt"]:
            continue
        try:
            same = CRS.from_wkt(ga["crs_wkt"]) == CRS.from_wkt(gb["crs_wkt"])
        except Exception:  # noqa: BLE001
            same = False
        inv_a, inv_b = ga.get("inverse_flattening"), gb.get("inverse_flattening")
        if not same and inv_a is not None and inv_b is not None:
            # When the CRSs do not compare equal, every CF attribute but those derived from the
            # WKT text (names, the inverse flattening's digits) must match.
            rest_a = {k: v for k, v in ga.items() if k not in _WKT_ATTRS}
            rest_b = {k: v for k, v in gb.items() if k not in _WKT_ATTRS}
            same = (
                bool(rest_a)
                and repr(rest_a) == repr(rest_b)
                and math.isclose(inv_a, inv_b, rel_tol=1e-9)
            )
        if not same:
            continue
        b = b.copy()
        attrs = dict(b.coords[name].attrs)
        for key in _WKT_ATTRS:
            if key in ga:
                attrs[key] = ga[key]
            else:
                attrs.pop(key, None)
        b.coords[name].attrs = attrs
        notes.append("CRS WKT text differs (same CRS)")
    return b, "; ".join(sorted(set(notes)))


def _encodings(obj: Any) -> dict[str, Any]:
    """The compared encoding items of an object and its variables.

    Args:
        obj: A DataArray or Dataset.

    Returns:
        dict: Items by variable.
    """
    import xarray as xr

    def pick(enc: dict[str, Any]) -> dict[str, Any]:
        return {k: repr(enc[k]) for k in _ENCODING_KEYS if k in enc}

    if isinstance(obj, xr.Dataset):
        return {str(v): pick(obj[v].encoding) for v in obj.data_vars}
    return {"": pick(obj.encoding)}


def _values_close(a: Any, b: Any, c: Check) -> tuple[bool, str]:
    """Whether two arrays agree within a check's tolerance.

    Args:
        a: An array.
        b: Another.
        c: The check.

    Returns:
        tuple: Whether they agree, and the differences.
    """
    va, vb = np.asarray(a, dtype=float), np.asarray(b, dtype=float)
    if va.shape != vb.shape:
        return False, f"shapes {va.shape} and {vb.shape}"
    both_nan = np.isnan(va) & np.isnan(vb)
    with np.errstate(invalid="ignore"):
        close = np.isclose(va, vb, rtol=c.rtol, atol=c.atol) | both_nan
    frac = 1.0 - float(close.mean()) if close.size else 0.0
    diff = np.abs(va - vb)[~both_nan & ~np.isnan(va - vb)]
    maxd = float(diff.max()) if diff.size else 0.0
    nan_mismatch = int((np.isnan(va) != np.isnan(vb)).sum())
    msg = f"max |diff| {maxd:.3g}, {frac:.4%} of values beyond tolerance"
    if nan_mismatch:
        msg += f", {nan_mismatch} NaN mismatches"
    return frac <= c.max_frac, msg


def compare_xr(a: Any, b: Any, c: Check) -> Outcome:
    """Compares two xarray objects.

    Args:
        a: The rioxarray result.
        b: The terrio result.
        c: The check.

    Returns:
        Outcome: The comparison.
    """
    import xarray as xr

    a = a.load() if hasattr(a, "load") else a
    b = b.load() if hasattr(b, "load") else b
    b, note = _normalize_crs_text(a, b)
    ea, eb = _encodings(a), _encodings(b)
    enc_note = "" if ea == eb else f"encodings differ: {ea} vs {eb}"
    try:
        xr.testing.assert_identical(a, b)
        if enc_note:
            return Outcome("differs", enc_note)
        return Outcome("identical", note)
    except AssertionError as err:
        first = str(err).splitlines()
    if c.expect != "tolerance":
        return Outcome("differs", " / ".join(line.strip() for line in first[:12]))
    # Within tolerance: the same structure and metadata, values and coordinates close.
    if isinstance(a, xr.Dataset):
        pairs = [(a[v], b[v]) for v in a.data_vars]
        if set(a.data_vars) != set(b.data_vars):
            return Outcome("differs", "different variables")
    else:
        pairs = [(a, b)]
    details = []
    for da, db in pairs:
        if da.dims != db.dims or da.shape != db.shape:
            return Outcome("differs", f"dims {da.sizes} vs {db.sizes}")
        if set(da.coords) != set(db.coords):
            return Outcome("differs", f"coords {set(da.coords)} vs {set(db.coords)}")
        for name in da.coords:
            ca, cb = da.coords[name], db.coords[name]
            if ca.ndim and ca.dtype.kind == "f":
                if not np.allclose(ca.values, cb.values, rtol=c.coord_rtol, atol=0):
                    return Outcome("differs", f"coordinate {name} beyond rtol {c.coord_rtol}")
            elif ca.ndim and not np.array_equal(ca.values, cb.values):
                return Outcome("differs", f"coordinate {name} differs")
            attrs_a, attrs_b = dict(ca.attrs), dict(cb.attrs)
            gta, gtb = attrs_a.pop("GeoTransform", None), attrs_b.pop("GeoTransform", None)
            if (gta is None) != (gtb is None):
                return Outcome("differs", f"GeoTransform of {name}")
            if gta is not None:
                ta = [float(v) for v in gta.split()]
                tb = [float(v) for v in gtb.split()]
                if not np.allclose(ta, tb, rtol=c.coord_rtol, atol=0):
                    return Outcome("differs", f"GeoTransform {gta} vs {gtb}")
            if repr(attrs_a) != repr(attrs_b) and attrs_a != attrs_b:
                return Outcome("differs", f"attributes of {name} differ")
        if not _attrs_equal(da.attrs, db.attrs):
            return Outcome("differs", f"attributes differ: {da.attrs} vs {db.attrs}")
        ok, msg = _values_close(da.values, db.values, c)
        if not ok:
            return Outcome("differs", msg)
        details.append(msg)
    if enc_note:
        return Outcome("differs", enc_note)
    return Outcome("tolerance", "; ".join(d for d in [note, *details] if d))


def _attrs_equal(a: dict[Any, Any], b: dict[Any, Any]) -> bool:
    """Whether two attribute dicts are equal (arrays compared by value).

    Args:
        a: Attributes.
        b: Attributes.

    Returns:
        bool: True when equal.
    """
    if set(a) != set(b):
        return False
    for k in a:
        va, vb = a[k], b[k]
        if isinstance(va, (np.ndarray, list, tuple)) or isinstance(vb, (np.ndarray, list, tuple)):
            try:
                if not np.array_equal(np.asarray(va), np.asarray(vb), equal_nan=True):
                    return False
            except TypeError:
                if list(va) != list(vb):
                    return False
        elif _is_nan(va) and _is_nan(vb):
            continue
        elif va != vb:
            return False
    return True


def _is_nan(v: Any) -> bool:
    """Whether a value is a NaN number.

    Args:
        v: The value.

    Returns:
        bool: True for NaN.
    """
    try:
        return bool(np.isnan(v))
    except (TypeError, ValueError):
        return False


def norm(v: Any) -> Any:
    """A plain value to compare: CRSs as WKT-equivalence keys, tuples as lists.

    Args:
        v: The value.

    Returns:
        Any: The comparable value.
    """
    if hasattr(v, "to_epsg") and hasattr(v, "to_wkt"):
        return ("CRS", v.to_epsg())
    if hasattr(v, "to_gdal"):
        return ("Affine", tuple(v)[:6])
    if isinstance(v, (tuple, list)):
        return [norm(x) for x in v]
    if isinstance(v, dict):
        return {k: norm(v[k]) for k in sorted(v, key=str)}
    if isinstance(v, np.generic):
        return v.item()
    return v


def compare_plain(a: Any, b: Any, c: Check) -> Outcome:
    """Compares two plain results.

    Args:
        a: The rioxarray result.
        b: The terrio result.
        c: The check.

    Returns:
        Outcome: The comparison.
    """
    na, nb = norm(a), norm(b)
    if repr(na) == repr(nb):
        return Outcome("identical", repr(na)[:120])
    if c.expect == "tolerance":
        try:
            fa = np.asarray(_flatten(na), dtype=float)
            fb = np.asarray(_flatten(nb), dtype=float)
            if fa.shape == fb.shape and np.allclose(
                fa, fb, rtol=c.rtol, atol=c.atol, equal_nan=True
            ):
                return Outcome("tolerance", f"max |diff| {np.nanmax(np.abs(fa - fb)):.3g}")
        except (TypeError, ValueError):
            pass
    if isinstance(na, dict) and isinstance(nb, dict):
        keys = sorted(set(na) | set(nb), key=str)
        diffs = [f"{k}: {na.get(k)!r} vs {nb.get(k)!r}" for k in keys if na.get(k) != nb.get(k)]
        return Outcome("differs", "; ".join(diffs)[:600])
    return Outcome("differs", f"{na!r} vs {nb!r}"[:600])


def _flatten(v: Any) -> list[Any]:
    """The numbers of a nested value.

    Args:
        v: The value.

    Returns:
        list: Its numbers.
    """
    if isinstance(v, dict):
        return [x for k in sorted(v, key=str) for x in _flatten(v[k])]
    if isinstance(v, (list, tuple)):
        return [x for item in v for x in _flatten(item)]
    return [v]


def compare(a: Any, b: Any, c: Check) -> Outcome:
    """Compares two results.

    Args:
        a: The rioxarray result.
        b: The terrio result.
        c: The check.

    Returns:
        Outcome: The comparison.
    """
    import xarray as xr

    if isinstance(a, (xr.DataArray, xr.Dataset)):
        return compare_xr(a, b, c)
    if isinstance(a, dict) and any(isinstance(v, (xr.DataArray, xr.Dataset)) for v in a.values()):
        notes = []
        worst = "identical"
        for k in a:
            o = compare(a[k], b[k], c)
            if o.status == "differs":
                return Outcome("differs", f"{k}: {o.detail}")
            if o.status == "tolerance":
                worst = "tolerance"
            if o.detail:
                notes.append(f"{k}: {o.detail}")
        return Outcome(worst, "; ".join(notes)[:400])
    return compare_plain(a, b, c)


def run_check(c: Check, data: Path | None, out: Path, server: Any = None) -> Outcome:
    """Runs a check with both implementations and compares the results.

    Args:
        c: The check.
        data: The real-data directory, or None.
        out: A scratch directory.
        server: The local HTTP server, for laziness checks.

    Returns:
        Outcome: The comparison.
    """
    ctx_r = Ctx(data, out / "rioxarray", server=server)
    ctx_t = Ctx(data, out / "terrio", server=server)
    for ctx in (ctx_r, ctx_t):
        ctx.out.mkdir(parents=True, exist_ok=True)
        ctx.synth = synthetic_files(out)
    err_a: Exception | None = None
    err_b: Exception | None = None
    try:
        a = c.func(Flavor("rioxarray"), ctx_r)
    except Exception as err:  # noqa: BLE001
        err_a = err
    try:
        b = c.func(Flavor("terrio"), ctx_t)
    except Exception as err:  # noqa: BLE001
        err_b = err
    if err_a is not None and err_b is not None:
        if type(err_a).__name__ == type(err_b).__name__:
            return Outcome("identical", f"both raise {type(err_a).__name__}")
        return Outcome(
            "differs",
            f"rioxarray raises {type(err_a).__name__}: {err_a}; terrio raises "
            f"{type(err_b).__name__}: {err_b}"[:300],
        )
    if err_a is not None:
        return Outcome("differs", f"rioxarray raises {type(err_a).__name__}: {err_a}"[:300])
    if err_b is not None:
        return Outcome("differs", f"terrio raises {type(err_b).__name__}: {err_b}"[:300])
    if isinstance(a, Lazy):
        return compare_lazy(a, b, c)
    return compare(a, b, c)


def failed(c: Check, o: Outcome) -> bool:
    """Whether an outcome is worse than its check allows.

    Args:
        c: The check.
        o: The outcome.

    Returns:
        bool: True when the check fails.
    """
    if o.status in ("differs", "error"):
        return True
    return c.expect == "identical" and o.status != "identical"


# ─── Synthetic files ────────────────────────────────────────────────────────────────────────────


_SYNTH_CACHE: dict[str, dict[str, str]] = {}


def synthetic_files(out: Path) -> dict[str, str]:
    """Writes synthetic GeoTIFFs with rasterio (GDAL), once per output directory.

    Args:
        out: The directory.

    Returns:
        dict: Paths by name.
    """
    key = str(out)
    if key in _SYNTH_CACHE:
        return _SYNTH_CACHE[key]
    import rasterio
    from rasterio.enums import Resampling
    from rasterio.transform import from_origin

    d = out / "synth"
    d.mkdir(parents=True, exist_ok=True)
    rng = np.random.default_rng(42)
    files: dict[str, str] = {}
    yy, xx = np.mgrid[0:300, 0:400]
    base = (np.sin(xx / 23.0) * np.cos(yy / 17.0) * 1000 + 1500).astype("float64")

    def write(
        name: str,
        arr: np.ndarray,
        *,
        crs: str,
        transform: Any,
        nodata: Any = None,
        scales: Any = None,
        offsets: Any = None,
        descriptions: Any = None,
        ovr: bool = False,
        tags: dict[str, str] | None = None,
        band_tags: dict[str, str] | None = None,
        **kw: Any,
    ) -> None:
        arr = arr if arr.ndim == 3 else arr[np.newaxis]
        p = str(d / name)
        profile = dict(
            driver="GTiff",
            width=arr.shape[2],
            height=arr.shape[1],
            count=arr.shape[0],
            dtype=arr.dtype,
            crs=crs,
            transform=transform,
            nodata=nodata,
            **kw,
        )
        with rasterio.open(p, "w", **profile) as dst:
            dst.write(arr)
            if scales is not None:
                dst.scales = scales
            if offsets is not None:
                dst.offsets = offsets
            if descriptions is not None:
                dst.descriptions = descriptions
            if tags:
                dst.update_tags(**tags)
            if band_tags:
                dst.update_tags(1, **band_tags)
            if ovr:
                dst.build_overviews([2, 4], Resampling.average)
        files[name] = p

    # uint8, three bands, nodata 0, band descriptions and tags, tiled, overviews.
    rgb = np.stack([(base / 12 + i * 20) % 250 + 1 for i in range(3)]).astype("uint8")
    rgb[:, 10:40, 20:80] = 0
    write(
        "synth_rgb.tif",
        rgb,
        crs="EPSG:32613",
        transform=from_origin(480000, 4400000, 30, 30),
        nodata=0,
        descriptions=["red", "green", "blue"],
        tags={"SENSOR": "synthetic", "YEAR": "2024"},
        ovr=True,
        tiled=True,
        blockxsize=128,
        blockysize=128,
        compress="deflate",
    )
    # int16 with a scale and an offset, nodata -32768, units, overviews.
    dem = (base - 1500).astype("int16")
    dem[200:230, 300:360] = -32768
    write(
        "synth_scaled.tif",
        dem,
        crs="EPSG:4326",
        transform=from_origin(-105.5, 40.0, 0.001, 0.001),
        nodata=-32768,
        scales=[0.5],
        offsets=[100.0],
        descriptions=["elevation"],
        ovr=True,
        band_tags={"units_note": "half metres"},
        tiled=True,
        blockxsize=64,
        blockysize=64,
    )
    # float32 with NaN nodata, strips.
    f32 = base.astype("float32")
    f32[rng.random(f32.shape) < 0.01] = np.nan
    write(
        "synth_f32.tif",
        f32,
        crs="EPSG:3857",
        transform=from_origin(-11700000, 4860000, 50, 50),
        nodata=float("nan"),
    )
    # uint16, two bands, no nodata, a polar stereographic CRS (on the WGS 84 datum: GDAL's choice
    # of a NAD83 to WGS 84 transformation varies between builds).
    u16 = np.stack([base, base[::-1]]).astype("uint16")
    write(
        "synth_u16.tif",
        u16,
        crs="EPSG:3413",
        transform=from_origin(-760000, 1900000, 100, 100),
        tiled=True,
        blockxsize=256,
        blockysize=256,
    )
    # float64 DEM with -9999 nodata, for clipping and reprojection.
    f64 = base.copy()
    f64[:15, :] = -9999
    write(
        "synth_f64.tif",
        f64,
        crs="EPSG:32613",
        transform=from_origin(450000, 4430000, 10, 10),
        nodata=-9999.0,
        tiled=True,
        blockxsize=128,
        blockysize=128,
    )
    # A larger tiled uint16 raster of noise, for laziness checks: 2048 x 2048, 256 x 256 tiles.
    noise = rng.integers(0, 65535, size=(2048, 2048), dtype="uint16")
    write(
        "synth_big.tif",
        noise,
        crs="EPSG:32613",
        transform=from_origin(400000, 4500000, 5, 5),
        tiled=True,
        blockxsize=256,
        blockysize=256,
    )
    _SYNTH_CACHE[key] = files
    return files


SYNTH = ["synth_rgb.tif", "synth_scaled.tif", "synth_f32.tif", "synth_u16.tif", "synth_f64.tif"]


# ─── Laziness: a local HTTP server counting bytes ───────────────────────────────────────────────


def serve(root: str) -> None:
    """Serves files with range requests, counting requests and bytes; prints the port.

    `GET /__stats__` returns the counts (as JSON) and `GET /__reset__` resets them; neither is
    counted.

    Args:
        root: The directory.
    """
    lock = threading.Lock()
    counts = {"requests": 0, "bytes": 0}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args: Any) -> None:
            pass

        def _path(self) -> str:
            return os.path.join(root, self.path.lstrip("/").split("?")[0])

        def _json(self, value: Any) -> None:
            body = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_HEAD(self) -> None:
            p = self._path()
            if not os.path.isfile(p):
                self.send_response(404)
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Content-Length", str(os.path.getsize(p)))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()

        def do_GET(self) -> None:
            if self.path == "/__stats__":
                with lock:
                    self._json(dict(counts))
                return
            if self.path == "/__reset__":
                with lock:
                    counts.update(requests=0, bytes=0)
                self._json({})
                return
            p = self._path()
            if not os.path.isfile(p):
                self.send_response(404)
                self.end_headers()
                return
            size = os.path.getsize(p)
            start, end = 0, size - 1
            m = re.match(r"bytes=(\d+)-(\d*)$", (self.headers.get("Range") or "").strip())
            if m:
                start = int(m.group(1))
                end = min(int(m.group(2)) if m.group(2) else size - 1, size - 1)
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            else:
                self.send_response(200)
            n = end - start + 1
            self.send_header("Content-Length", str(n))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            with open(p, "rb") as f:
                f.seek(start)
                self.wfile.write(f.read(n))
            with lock:
                counts["requests"] += 1
                counts["bytes"] += n

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(srv.server_address[1], flush=True)
    srv.serve_forever()


class RangeServer:
    """A local HTTP server with range requests, in its own process (GDAL holds the GIL)."""

    def __init__(self, root: str) -> None:
        """Starts the server.

        Args:
            root: The directory served.
        """
        self.root = root
        self._proc = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "--serve", root],
            stdout=subprocess.PIPE,
            text=True,
        )
        assert self._proc.stdout is not None
        self.port = int(self._proc.stdout.readline())

    def _get(self, path: str) -> Any:
        """A control request.

        Args:
            path: `/__stats__` or `/__reset__`.

        Returns:
            Any: The JSON response.
        """
        with urllib.request.urlopen(f"http://127.0.0.1:{self.port}{path}", timeout=30) as r:
            return json.loads(r.read())

    def url(self, path: str) -> str:
        """The URL of a file.

        Args:
            path: The file's path, under the root.

        Returns:
            str: The URL.
        """
        rel = os.path.relpath(path, self.root)
        # A query string of its own, so that GDAL's cache of earlier reads is not used.
        self._n = getattr(self, "_n", 0) + 1
        return f"http://127.0.0.1:{self.port}/{rel}?n={self._n}"

    def reset(self) -> None:
        """Resets the counters."""
        self._get("/__reset__")

    @property
    def requests(self) -> int:
        """Requests served since the last reset.

        Returns:
            int: The count.
        """
        return int(self._get("/__stats__")["requests"])

    @property
    def bytes(self) -> int:
        """Bytes served since the last reset.

        Returns:
            int: The count.
        """
        return int(self._get("/__stats__")["bytes"])

    def close(self) -> None:
        """Stops the server."""
        self._proc.terminate()
        self._proc.wait()


@dataclass
class Lazy:
    """A window read over HTTP: the pixels, and what was fetched."""

    values: Any
    bytes: int
    requests: int
    size: int
    opened: int = 0


def compare_lazy(a: Lazy, b: Lazy, c: Check) -> Outcome:
    """Compares two window reads, and checks terrio fetched a small part of the file.

    Args:
        a: rioxarray's read.
        b: terrio's.
        c: The check.

    Returns:
        Outcome: The comparison.
    """
    o = compare(a.values, b.values, c)
    detail = (
        f"file {b.size / 1e6:.1f} MB; opening fetched terrio {b.opened / 1e6:.2f} MB, rioxarray "
        f"{a.opened / 1e6:.2f} MB; the window fetched terrio {b.bytes / 1e6:.2f} MB in "
        f"{b.requests} requests, rioxarray {a.bytes / 1e6:.2f} MB in {a.requests}"
    )
    if o.status != "differs" and b.bytes + b.opened > 0.5 * b.size:
        return Outcome("differs", "terrio read most of the file: " + detail)
    return Outcome(o.status, "; ".join(x for x in (o.detail, detail) if x))


# ─── Opening ────────────────────────────────────────────────────────────────────────────────────


def _open(name: str, **kw: Any) -> Callable[[Flavor, Ctx], Any]:
    """A check opening a file.

    Args:
        name: The file.
        **kw: Options of `open_rasterio`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        da = fl.open(ctx.path(name), **kw)
        if "chunks" in kw:
            return {"array": da, "chunks": repr(da.chunks)}
        return da

    return f


def _engine(name: str, **kw: Any) -> Callable[[Flavor, Ctx], Any]:
    """A check opening a file with `xr.open_dataset`.

    Args:
        name: The file.
        **kw: Options of `open_dataset`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        import xarray as xr

        ds = xr.open_dataset(ctx.path(name), engine=fl.engine, **kw)
        if "chunks" in kw:
            return {"dataset": ds, "chunks": repr(dict(ds.chunks))}
        return ds

    return f


def _add_open_checks(names: list[str], needs_data: bool) -> None:
    """Registers the opening checks of files.

    Args:
        names: The files.
        needs_data: Whether they are real files.
    """
    for name in names:
        for kw in ({}, {"masked": True}, {"mask_and_scale": True}):
            add("open", f"{name} {kw or ''}".strip(), _open(name, **kw), needs_data=needs_data)
    variants: list[tuple[str, dict[str, Any]]] = [
        ("chunks=True", {"chunks": True}),
        ("chunks=auto masked", {"chunks": "auto", "masked": True}),
        ("chunks dict", {"chunks": {"x": 100, "y": 70}}),
        ("band_as_variable", {"band_as_variable": True}),
        (
            "band_as_variable mask_and_scale chunks",
            {"band_as_variable": True, "mask_and_scale": True, "chunks": True},
        ),
        ("parse_coordinates=False", {"parse_coordinates": False}),
        ("default_name", {"default_name": "elevation"}),
    ]
    for name in names[:2] if needs_data else names:
        for label, kw in variants:
            add("open", f"{name} {label}", _open(name, **kw), needs_data=needs_data)
        add("open", f"{name} xr.open_dataset engine", _engine(name), needs_data=needs_data)
        add(
            "open",
            f"{name} xr.open_dataset engine chunks={{}}",
            _engine(name, chunks={}),
            needs_data=needs_data,
        )
        add(
            "open",
            f"{name} xr.open_dataset masked band_as_variable",
            _engine(name, masked=True, band_as_variable=True),
            needs_data=needs_data,
        )


_add_open_checks(SYNTH, needs_data=False)
for _n in ("synth_rgb.tif", "synth_scaled.tif"):
    add("open", f"{_n} overview_level=0", _open(_n, overview_level=0))
    add(
        "open",
        f"{_n} overview_level=1 mask_and_scale",
        _open(_n, overview_level=1, mask_and_scale=True),
    )
_add_open_checks(REAL_FILES, needs_data=True)
for _n in ("dem.tif", "landsat.tif", "USGS_1_n40w106.tif", "srtm90.tif"):
    add("open", f"{_n} overview_level=0", _open(_n, overview_level=0), needs_data=True)
    add(
        "open",
        f"{_n} overview_level=1 masked chunks",
        _open(_n, overview_level=1, masked=True, chunks=True),
        needs_data=True,
    )


# ─── Slicing and laziness ───────────────────────────────────────────────────────────────────────


def _slices(name: str, **kw: Any) -> Callable[[Flavor, Ctx], Any]:
    """A check slicing an opened file with isel and sel.

    Args:
        name: The file.
        **kw: Options of `open_rasterio`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        da = fl.open(ctx.path(name), **kw)
        h, w = da.sizes["y"], da.sizes["x"]
        x0, x1 = float(da.x[w // 4]), float(da.x[w // 2])
        y0, y1 = float(da.y[h // 3]), float(da.y[2 * h // 3])
        return {
            "isel": da.isel(x=slice(w // 5, w // 2), y=slice(h // 4, h // 2 + 7)),
            "isel step": da.isel(x=slice(3, w - 5, 3), y=slice(h - 2, 1, -4)),
            "isel point": da.isel(x=w // 3, y=h // 2),
            "isel fancy": da.isel(x=[1, 5, w // 2, w - 1], y=[h - 1, 0, 3]),
            "isel band": da.isel(band=-1),
            "sel": da.sel(x=slice(x0, x1), y=slice(y0, y1)),
            "sel nearest": da.sel(x=x0 + 1e-9, y=y1, method="nearest"),
        }

    return f


def _lazy(name: str, window: tuple[float, float, float, float], **kw: Any) -> Callable[..., Any]:
    """A check reading a window of a file over HTTP, counting the bytes fetched.

    Args:
        name: The file.
        window: The window, as fractions `(x0, y0, x1, y1)` of the raster.
        **kw: Options of `open_rasterio`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        path = ctx.path(name)
        if ctx.server is None:
            raise RuntimeError("no HTTP server")
        ctx.server.reset()
        da = fl.open(ctx.server.url(path), **kw)
        opened = ctx.server.bytes
        h, w = da.sizes["y"], da.sizes["x"]
        sel = da.isel(
            x=slice(int(w * window[0]), int(w * window[2])),
            y=slice(int(h * window[1]), int(h * window[3])),
        )
        ctx.server.reset()
        values = sel.load()
        # Over HTTP, GDAL reads `.aux.xml` sidecar files (band statistics) and terrio does not:
        # compare pixels and coordinates.
        values.attrs = {}
        return Lazy(values, ctx.server.bytes, ctx.server.requests, os.path.getsize(path), opened)

    return f


for _n in SYNTH[:3]:
    add("slicing", f"{_n} isel/sel", _slices(_n))
    add(
        "slicing",
        f"{_n} isel/sel masked chunks",
        _slices(_n, masked=True, chunks={"x": 64, "y": 64}),
    )
for _n in ("dem.tif", "landsat.tif", "USGS_1_n40w106.tif"):
    add("slicing", f"{_n} isel/sel", _slices(_n), needs_data=True)
    add(
        "slicing",
        f"{_n} isel/sel mask_and_scale chunks",
        _slices(_n, mask_and_scale=True, chunks=True),
        needs_data=True,
    )
add(
    "laziness",
    "synth_big.tif window over HTTP",
    _lazy("synth_big.tif", (0.1, 0.1, 0.25, 0.2)),
    needs=("server",),
)
add(
    "laziness",
    "synth_big.tif window over HTTP (dask)",
    _lazy("synth_big.tif", (0.5, 0.6, 0.6, 0.7), chunks=True),
    needs=("server",),
)
for _n, _win in (
    ("dem.tif", (0.35, 0.3, 0.4, 0.36)),
    ("landsat.tif", (0.5, 0.5, 0.6, 0.6)),
    ("USGS_1_n40w106.tif", (0.2, 0.7, 0.25, 0.75)),
):
    add("laziness", f"{_n} window over HTTP", _lazy(_n, _win), needs_data=True, needs=("server",))
    add(
        "laziness",
        f"{_n} window over HTTP (dask)",
        _lazy(_n, _win, chunks=True, masked=True),
        needs_data=True,
        needs=("server",),
    )
    add(
        "laziness",
        f"{_n} window over HTTP (dask, lock=True)",
        _lazy(_n, _win, chunks=True, lock=True),
        needs_data=True,
        needs=("server",),
    )


# ─── Properties ─────────────────────────────────────────────────────────────────────────────────


def _props(name: str, **kw: Any) -> Callable[[Flavor, Ctx], Any]:
    """A check of accessor properties.

    Args:
        name: The file.
        **kw: Options of `open_rasterio`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        da = fl.open(ctx.path(name), **kw)
        a = fl.acc(da)
        nodata = a.nodata
        return {
            "crs": a.crs,
            "transform": a.transform(),
            "transform recalc": a.transform(recalc=True),
            "bounds": a.bounds(),
            "bounds recalc": a.bounds(recalc=True),
            "resolution": a.resolution(),
            "shape": a.shape,
            "width": a.width,
            "height": a.height,
            "count": a.count,
            "x_dim": a.x_dim,
            "y_dim": a.y_dim,
            "nodata": "nan" if nodata is not None and np.isnan(nodata) else norm(nodata),
            "encoded_nodata": repr(a.encoded_nodata),
            "grid_mapping": a.grid_mapping,
            "estimate_utm_crs": a.estimate_utm_crs(),
            "transform_bounds 4326": [round(v, 9) for v in a.transform_bounds("EPSG:4326")],
        }

    return f


def _writes(name: str) -> Callable[[Flavor, Ctx], Any]:
    """A check of the metadata writers.

    Args:
        name: The file.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        da = fl.open(ctx.path(name))
        a = fl.acc(da)
        renamed = da.rename(x="lon", y="lat")
        ra = fl.acc(renamed)
        ra.set_spatial_dims(x_dim="lon", y_dim="lat", inplace=True)
        return {
            "write_nodata": a.write_nodata(7),
            "write_nodata encoded": a.write_nodata(-5, encoded=True),
            "write_nodata None": a.write_nodata(None),
            "write_crs": a.write_crs("EPSG:4269"),
            "write_crs grid mapping name": a.write_crs("EPSG:4326", grid_mapping_name="crs"),
            "write_transform": a.write_transform(a.transform() * a.transform().translation(1, 1)),
            "write_coordinate_system": a.write_coordinate_system(),
            "set_spatial_dims bounds": ra.bounds(),
            "isel_window": a.isel_window(fl.Window(10.5, 20.2, 50, 60)),
            "isel_window pad": a.isel_window(fl.Window(-10, -20, 50, 60), pad=True),
        }

    return f


for _n in SYNTH:
    add("properties", f"{_n}", _props(_n))
add("properties", "synth_f32.tif masked", _props("synth_f32.tif", masked=True))
add("properties", "synth_rgb.tif masked", _props("synth_rgb.tif", masked=True))
add("properties", "synth_f64.tif write_*", _writes("synth_f64.tif"))
add("properties", "synth_scaled.tif write_*", _writes("synth_scaled.tif"))
for _n in REAL_FILES[:6]:
    add("properties", _n, _props(_n), needs_data=True)


# ─── Clipping and padding ───────────────────────────────────────────────────────────────────────


def _box(a: Any, f: tuple[float, float, float, float]) -> tuple[float, float, float, float]:
    """A box inside an array's bounds, from fractions.

    Args:
        a: The accessor.
        f: Fractions `(x0, y0, x1, y1)`.

    Returns:
        tuple: The box.
    """
    left, bottom, right, top = a.bounds()
    dx, dy = right - left, top - bottom
    return left + dx * f[0], bottom + dy * f[1], left + dx * f[2], bottom + dy * f[3]


def _clip_box(name: str, **kw: Any) -> Callable[[Flavor, Ctx], Any]:
    """A check of clip_box.

    Args:
        name: The file.
        **kw: Options of `open_rasterio`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        import rasterio.warp

        da = fl.open(ctx.path(name), **kw)
        a = fl.acc(da)
        # The same box for both: the bounds in EPSG:4326 as rasterio computes them.
        g = rasterio.warp.transform_bounds(a.crs.to_wkt(), "EPSG:4326", *a.bounds())
        gx, gy = g[2] - g[0], g[3] - g[1]
        gbox = (g[0] + gx * 0.3, g[1] + gy * 0.3, g[0] + gx * 0.6, g[1] + gy * 0.65)
        return {
            "native": a.clip_box(*_box(a, (0.3, 0.4, 0.55, 0.7))),
            "edge": a.clip_box(*_box(a, (-0.2, -0.1, 0.33, 0.21))),
            "crs=EPSG:4326": a.clip_box(*gbox, crs="EPSG:4326"),
            "auto_expand": a.clip_box(*_box(a, (0.5, 0.5, 0.5001, 0.5001)), auto_expand=True),
        }

    return f


def _poly(a: Any) -> dict[str, Any]:
    """A diamond in an array's bounds.

    Args:
        a: The accessor.

    Returns:
        dict: The GeoJSON polygon.
    """
    # Vertices off pixel edges: a vertex on an edge makes all_touched depend on float rounding.
    x0, y0, x1, y1 = _box(a, (0.2037, 0.2511, 0.6973, 0.8029))
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    ring = [(x0, cy), (cx, y0), (x1, cy), (cx, y1), (x0, cy)]
    return {"type": "Polygon", "coordinates": [ring]}


def _clip(name: str, **kw: Any) -> Callable[[Flavor, Ctx], Any]:
    """A check of clip.

    Args:
        name: The file.
        **kw: Options of `open_rasterio`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        from terrio.rio.warp import transform_geom

        da = fl.open(ctx.path(name), **kw)
        a = fl.acc(da)
        poly = _poly(a)
        poly4326 = transform_geom(a.crs.to_wkt(), "EPSG:4326", poly)
        return {
            "polygon": a.clip([poly]),
            "all_touched": a.clip([poly], all_touched=True),
            "invert drop=False": a.clip([poly], invert=True, drop=False),
            "drop=False": a.clip([poly], drop=False),
            "from_disk": fl.acc(fl.open(ctx.path(name), **kw)).clip([poly], from_disk=True),
            "crs=EPSG:4326": a.clip([poly4326], crs="EPSG:4326"),
        }

    return f


def _pad(name: str, **kw: Any) -> Callable[[Flavor, Ctx], Any]:
    """A check of pad_box and pad_xy.

    Args:
        name: The file.
        **kw: Options of `open_rasterio`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        da = fl.open(ctx.path(name), **kw)
        a = fl.acc(da)
        bounds = a.bounds()
        sub = a.clip_box(*_box(a, (0.3, 0.3, 0.6, 0.6)))
        sa = fl.acc(sub)
        return {
            "pad_box": sa.pad_box(*bounds),
            "pad_box constant": sa.pad_box(*bounds, constant_values=3),
            "pad_xy": sa.pad_xy(*_box(a, (0.1, 0.2, 0.9, 0.7))),
        }

    return f


for _n in SYNTH:
    add("clip", f"{_n} clip_box", _clip_box(_n))
    add("clip", f"{_n} clip", _clip(_n))
    add("pad", f"{_n} pad_box", _pad(_n))
add("clip", "synth_rgb.tif clip masked", _clip("synth_rgb.tif", masked=True))
add("clip", "synth_scaled.tif clip mask_and_scale", _clip("synth_scaled.tif", mask_and_scale=True))
add("clip", "synth_rgb.tif clip_box chunks", _clip_box("synth_rgb.tif", chunks=True))
for _n in ("dem.tif", "landsat.tif", "srtm90.tif", "USGS_1_n40w106.tif"):
    add("clip", f"{_n} clip_box", _clip_box(_n), needs_data=True)
    add("clip", f"{_n} clip", _clip(_n), needs_data=True)
    add("clip", f"{_n} clip masked", _clip(_n, masked=True), needs_data=True)
    add("pad", f"{_n} pad_box", _pad(_n), needs_data=True)


# ─── Reprojection ───────────────────────────────────────────────────────────────────────────────

_GRID_REASON = (
    "the output grid is GDAL's suggested warp grid, computed by terrio's own arithmetic: "
    "transforms agree to ~1e-15 (relative), which moves pixel-centre coordinates by as much"
)
_KERNEL_REASON = (
    _GRID_REASON + "; bilinear and cubic kernels sum weights in a different order than GDAL, "
    "so values differ by float rounding (~1e-9 relative)"
)


def _reproject(name: str, dst: str | None, resampling: str, **kw: Any) -> Callable[..., Any]:
    """A check of reproject.

    Args:
        name: The file.
        dst: The target CRS, or None for the estimated UTM zone.
        resampling: The resampling method.
        **kw: Options of `reproject`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        import rasterio.enums

        da = fl.open(ctx.path(name))
        a = fl.acc(da)
        sub = fl.acc(a.clip_box(*_box(a, (0.2, 0.2, 0.7, 0.7))))
        crs = dst or a.estimate_utm_crs()
        rs: Any = resampling
        if fl.name == "rioxarray":
            rs = getattr(rasterio.enums.Resampling, resampling)
        return sub.reproject(crs, resampling=rs, **kw)

    return f


def _reproject_match(name: str, resampling: str = "nearest") -> Callable[..., Any]:
    """A check of reproject_match onto a grid both computed by rioxarray.

    Args:
        name: The file.
        resampling: The resampling method.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        import rasterio.enums
        import rioxarray  # noqa: F401 - the match grid comes from rioxarray

        da = fl.open(ctx.path(name))
        ref = Flavor("rioxarray").open(ctx.path(name))
        match = ref.rio.clip_box(*_box(ref.rio, (0.25, 0.25, 0.6, 0.6))).rio.reproject("EPSG:4326")
        rs: Any = resampling
        if fl.name == "rioxarray":
            rs = getattr(rasterio.enums.Resampling, resampling)
        return fl.acc(da).reproject_match(match, resampling=rs)

    return f


for _n in ("synth_f64.tif", "synth_rgb.tif", "synth_f32.tif", "synth_u16.tif"):
    # synth_f32.tif has 1% of its pixels NaN, scattered: GDAL's warp kernels next to nodata
    # pixels (which renormalize the weights of the valid neighbours) differ from terrio's.
    _frac = 0.02 if _n == "synth_f32.tif" else 0.0
    _why = _KERNEL_REASON + ("; GDAL's warp kernels next to nodata pixels" if _frac else "")
    # Integer outputs: kernel results within float rounding of x.5 may round either way.
    _int = _n in ("synth_rgb.tif", "synth_u16.tif")
    _katol = 1.0 if _int else 1e-6
    if _int:
        _why += "; integer outputs: results within float rounding of x.5 round either way (by 1)"
    add(
        "reproject",
        f"{_n} EPSG:4326 nearest",
        _reproject(_n, "EPSG:4326", "nearest"),
        expect="tolerance",
        coord_rtol=1e-12,
        reason=_GRID_REASON,
    )
    add(
        "reproject",
        f"{_n} EPSG:4326 bilinear",
        _reproject(_n, "EPSG:4326", "bilinear"),
        expect="tolerance",
        coord_rtol=1e-12,
        rtol=1e-6,
        atol=_katol,
        max_frac=_frac,
        reason=_why,
    )
    add(
        "reproject",
        f"{_n} UTM cubic resolution",
        _reproject(_n, None, "cubic", resolution=60),
        expect="tolerance",
        coord_rtol=1e-12,
        rtol=1e-6,
        atol=_katol,
        max_frac=_frac,
        reason=_why,
    )
    add(
        "reproject",
        f"{_n} EPSG:3857 shape",
        _reproject(_n, "EPSG:3857", "nearest", shape=(120, 150)),
        expect="tolerance",
        coord_rtol=1e-12,
        reason=_GRID_REASON,
    )
    add("reproject", f"{_n} reproject_match", _reproject_match(_n))
    add(
        "reproject",
        f"{_n} reproject_match bilinear",
        _reproject_match(_n, "bilinear"),
        expect="tolerance",
        rtol=1e-6,
        atol=_katol,
        max_frac=_frac,
        reason=_why,
    )
for _n in ("dem.tif", "landsat.tif", "USGS_1_n40w106.tif"):
    add(
        "reproject",
        f"{_n} EPSG:4326 nearest",
        _reproject(_n, "EPSG:4326", "nearest"),
        expect="tolerance",
        coord_rtol=1e-12,
        reason=_GRID_REASON,
        needs_data=True,
    )
    add(
        "reproject",
        f"{_n} UTM bilinear",
        _reproject(_n, None, "bilinear"),
        expect="tolerance",
        coord_rtol=1e-12,
        rtol=1e-6,
        atol=1.0 if _n == "landsat.tif" else 1e-6,
        max_frac=0.01,
        reason=_KERNEL_REASON + "; GDAL's warp kernels next to nodata pixels",
        needs_data=True,
    )
    add("reproject", f"{_n} reproject_match", _reproject_match(_n), needs_data=True)


# ─── Merging ────────────────────────────────────────────────────────────────────────────────────


def _merge(names: list[str], **kw: Any) -> Callable[[Flavor, Ctx], Any]:
    """A check of merge_arrays and merge_datasets.

    Args:
        names: The files.
        **kw: Options of `open_rasterio`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        arrays = [fl.open(ctx.path(n), **kw) for n in names]
        return {
            "merge_arrays": fl.merge_arrays(arrays),
            "merge_arrays method=max nodata=0": fl.merge_arrays(arrays, method="max", nodata=0),
            "merge_datasets": fl.merge_datasets([x.to_dataset(name="v") for x in arrays]),
        }

    return f


def _merge_synth(fl: Flavor, ctx: Ctx) -> Any:
    """Merges overlapping pieces of a synthetic raster.

    Args:
        fl: The implementation.
        ctx: The context.

    Returns:
        dict: The merged arrays.
    """
    da = fl.open(ctx.path("synth_f64.tif"))
    a = fl.acc(da)
    parts = [
        a.clip_box(*_box(a, (0.0, 0.0, 0.6, 0.6))),
        a.clip_box(*_box(a, (0.4, 0.3, 1.0, 1.0))),
        a.clip_box(*_box(a, (0.1, 0.5, 0.5, 0.9))),
    ]
    b = fl.open(ctx.path("synth_rgb.tif"), masked=True)
    ba = fl.acc(b)
    bparts = [
        ba.clip_box(*_box(ba, (0.0, 0.0, 0.7, 0.5))),
        ba.clip_box(*_box(ba, (0.3, 0.4, 1, 1))),
    ]
    return {
        "first": fl.merge_arrays(parts),
        "last": fl.merge_arrays(parts, method="last"),
        "bounds res": fl.merge_arrays(parts, bounds=_box(a, (0.1, 0.1, 0.8, 0.8)), res=(20, 20)),
        "multiband masked": fl.merge_arrays(bparts),
    }


add("merge", "synthetic pieces", _merge_synth)
add("merge", "USGS 3DEP tiles (overview 2)", _merge(USGS_TILES, overview_level=2), needs_data=True)
add(
    "merge",
    "USGS 3DEP tiles (overview 2) masked",
    _merge(USGS_TILES, overview_level=2, masked=True),
    needs_data=True,
)


# ─── Writing ────────────────────────────────────────────────────────────────────────────────────


def _to_raster(name: str, open_kw: dict[str, Any], write_kw: dict[str, Any]) -> Callable[..., Any]:
    """A check of to_raster: the file is read back by rioxarray, terrio and rasterio.

    Args:
        name: The file.
        open_kw: Options of `open_rasterio`.
        write_kw: Options of `to_raster`.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        import rasterio

        da = fl.open(ctx.path(name), **open_kw)
        out = str(ctx.out / f"to_raster_{len(os.listdir(ctx.out))}_{Path(name).stem}.tif")
        fl.acc(da).to_raster(out, **write_kw)
        read_kw = {k: v for k, v in open_kw.items() if k != "chunks"}
        back = {}
        for reader in ("rioxarray", "terrio"):
            arr = Flavor(reader).open(out, **read_kw).load()
            arr.encoding.pop("source", None)
            back[f"read by {reader}"] = arr
        with rasterio.open(out) as src:
            profile = {k: v for k, v in src.profile.items() if k != "nodata"}
            nd = src.nodata
            back["rasterio profile"] = {
                **{k: repr(v) for k, v in profile.items()},
                "nodata": "nan" if nd is not None and np.isnan(nd) else nd,
                "tags": src.tags(),
                "band tags": [src.tags(b) for b in src.indexes],
                "descriptions": src.descriptions,
                "scales": src.scales,
                "offsets": src.offsets,
            }
        return back

    return f


_WRITES: list[tuple[str, dict[str, Any], dict[str, Any]]] = [
    ("synth_rgb.tif", {}, {}),
    ("synth_rgb.tif", {"masked": True}, {"compress": "deflate"}),
    ("synth_scaled.tif", {"mask_and_scale": True}, {}),
    ("synth_scaled.tif", {}, {"tiled": True, "blockxsize": 128, "blockysize": 128}),
    ("synth_f32.tif", {"chunks": {"x": 100, "y": 100}}, {"driver": "COG", "compress": "deflate"}),
    ("synth_f64.tif", {}, {"windowed": True}),
    ("synth_u16.tif", {"chunks": True}, {"tags": {"EXTRA": "yes"}}),
]
for _n, _o, _w in _WRITES:
    add(
        "to_raster",
        f"{_n} {_o or ''} {_w or ''}".replace("  ", " ").strip(),
        _to_raster(_n, _o, _w),
    )
for _n, _o, _w in [
    ("dem.tif", {}, {}),
    ("landsat.tif", {"masked": True}, {"compress": "deflate"}),
    ("dem.tif", {"chunks": True}, {"driver": "COG", "compress": "deflate"}),
    ("landsat.tif", {}, {"windowed": True, "tiled": True}),
    ("srtm_sub.tif", {"mask_and_scale": True}, {}),
]:
    add(
        "to_raster",
        f"{_n} {_o or ''} {_w or ''}".replace("  ", " ").strip(),
        _to_raster(_n, _o, _w),
        needs_data=True,
    )


def _dataset_to_raster(fl: Flavor, ctx: Ctx) -> Any:
    """Writes a Dataset of bands and reads it back.

    Args:
        fl: The implementation.
        ctx: The context.

    Returns:
        xarray.DataArray: The file, read by rioxarray.
    """
    ds = fl.open(ctx.path("synth_rgb.tif"), band_as_variable=True)
    out = str(ctx.out / "dataset_to_raster.tif")
    fl.acc(ds).to_raster(out)
    arr = Flavor("rioxarray").open(out).load()
    arr.encoding.pop("source", None)
    return arr


add("to_raster", "Dataset (band_as_variable) to_raster", _dataset_to_raster)


def _zarr(fl: Flavor, ctx: Ctx) -> Any:
    """Writes a reprojected array with xarray's `to_zarr`; reads it back with each library.

    Args:
        fl: The implementation.
        ctx: The context.

    Returns:
        dict: The store read back.
    """
    import rasterio
    import xarray as xr

    da = fl.open(ctx.path("synth_rgb.tif"), masked=True)
    utm = fl.acc(da).reproject("EPSG:4326")
    store = str(ctx.out / "xarray.zarr")
    shutil.rmtree(store, ignore_errors=True)
    utm.to_dataset(name="band_data").to_zarr(store, mode="w", consolidated=False)
    z = xr.open_zarr(store, consolidated=False, decode_coords="all").band_data.load()
    with rasterio.open(f'ZARR:"{store}":/band_data') as src:
        gdal = {"crs": src.crs.to_epsg(), "transform": tuple(src.transform)[:6], "shape": src.shape}
    import terrio

    r = terrio.open_raster(store)
    return {
        "open_zarr": z,
        "rioxarray crs": z.rio.crs,
        "terrio crs": z.terrio.crs,
        "rioxarray transform": tuple(z.rio.transform())[:6],
        "GDAL": gdal,
        "terrio core": {"crs": r.crs, "shape": r.shape},
    }


def _geozarr(fl: Flavor, ctx: Ctx) -> Any:
    """Writes GeoZarr with terrio's `to_raster` (the reference with rioxarray, through GDAL).

    Args:
        fl: The implementation.
        ctx: The context.

    Returns:
        dict: The store read back by rioxarray and terrio.
    """
    da = fl.open(ctx.path("synth_f64.tif"), masked=True)
    store = str(ctx.out / "geozarr.zarr")
    shutil.rmtree(store, ignore_errors=True)
    fl.acc(da).to_raster(store, driver="Zarr")
    out = {}
    for reader in ("rioxarray", "terrio"):
        arr = Flavor(reader).open(store, masked=True).load()
        out[f"{reader} values"] = arr.values
        out[f"{reader} crs"] = Flavor(reader).acc(arr).crs
        out[f"{reader} transform"] = tuple(Flavor(reader).acc(arr).transform())[:6]
    return out


add(
    "zarr",
    "xarray to_zarr of a reprojected array, read by rioxarray, terrio and GDAL",
    _zarr,
    needs=("zarr",),
    expect="tolerance",
    coord_rtol=1e-12,
    rtol=1e-12,
    reason=_GRID_REASON,
)
add("zarr", "to_raster(.zarr) GeoZarr, read by rioxarray and terrio", _geozarr, needs=("zarr",))


def _interp(name: str, method: str) -> Callable[[Flavor, Ctx], Any]:
    """A check of interpolate_na.

    Args:
        name: The file.
        method: The method.

    Returns:
        callable: The check function.
    """

    def f(fl: Flavor, ctx: Ctx) -> Any:
        da = fl.open(ctx.path(name), masked=True)
        sub = da.isel(x=slice(100, 160), y=slice(180, 240)).copy()
        return fl.acc(sub).interpolate_na(method)

    return f


for _m in ("nearest", "linear", "cubic"):
    add(
        "interpolate_na",
        f"synth_scaled.tif {_m}",
        _interp("synth_scaled.tif", _m),
        needs=("scipy",),
    )


# ─── Remote ─────────────────────────────────────────────────────────────────────────────────────


def _remote(fl: Flavor, ctx: Ctx) -> Any:
    """Reads the public COG: metadata, a window, an overview, a clip.

    Args:
        fl: The implementation.
        ctx: The context.

    Returns:
        dict: The results.
    """
    da = fl.open(REMOTE_COG, masked=True, chunks=True)
    return {
        "metadata": da.isel(x=slice(0, 0), y=slice(0, 0)),
        "window": da.isel(x=slice(2000, 2300), y=slice(100, 400)),
        "overview 2": fl.open(REMOTE_COG, overview_level=2),
        "clip_box": fl.acc(da).clip_box(-105.5, 39.5, -105.4, 39.6),
    }


add("remote", "USGS 3DEP COG on S3", _remote, needs_remote=True)


# ─── Running ────────────────────────────────────────────────────────────────────────────────────


@dataclass
class Row:
    """A row of the report."""

    section: str
    name: str
    expect: str
    outcome: Outcome
    reason: str = ""


def available(c: Check, data: Path | None, remote: bool, server: Any) -> str | None:
    """Why a check cannot run, or None.

    Args:
        c: The check.
        data: The real-data directory.
        remote: Whether remote checks run.
        server: The HTTP server, or None.

    Returns:
        str | None: The reason.
    """
    if c.needs_data and data is None:
        return "no DATA_DIR"
    if c.needs_remote and not remote:
        return "no --remote"
    for mod in c.needs:
        if mod == "server":
            if server is None:
                return "no HTTP server"
            continue
        try:
            __import__(mod)
        except ImportError:
            return f"{mod} is not installed"
    return None


def report(rows: list[Row]) -> str:
    """The Markdown report.

    Args:
        rows: The results.

    Returns:
        str: The report.
    """
    lines = ["# terrio.xarray vs rioxarray", ""]
    sections: dict[str, list[Row]] = {}
    for r in rows:
        sections.setdefault(r.section, []).append(r)
    lines += [
        "| Section | Checks | Identical | Identical but the CRS's WKT text | Within tolerance "
        "| Differ |",
        "|---|---|---|---|---|---|",
    ]
    totals = [0, 0, 0, 0, 0]
    for s, rs in sections.items():
        n_wkt = sum(r.outcome.status == "identical" and "CRS WKT" in r.outcome.detail for r in rs)
        n_id = sum(r.outcome.status == "identical" for r in rs) - n_wkt
        n_tol = sum(r.outcome.status == "tolerance" for r in rs)
        n_bad = len(rs) - n_id - n_wkt - n_tol
        row = [len(rs), n_id, n_wkt, n_tol, n_bad]
        totals = [t + v for t, v in zip(totals, row, strict=True)]
        lines.append(f"| {s} | " + " | ".join(str(v) for v in row) + " |")
    lines.append("| **all** | " + " | ".join(f"**{v}**" for v in totals) + " |")
    lines.append("")
    for s, rs in sections.items():
        lines += [f"## {s}", "", "| Check | Expected | Result | Detail |", "|---|---|---|---|"]
        for r in rs:
            detail = r.outcome.detail.replace("|", "\\|").replace("\n", " ")
            if r.reason and r.outcome.status == "tolerance":
                detail = f"{detail} ({r.reason})"
            lines.append(f"| {r.name} | {r.expect} | {r.outcome.status} | {detail} |")
        lines.append("")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Runs the comparison.

    Args:
        argv: Command-line arguments.

    Returns:
        int: The exit status.
    """
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    p.add_argument("data", nargs="?", default=os.environ.get("TERRIO_TEST_DATA"))
    p.add_argument("--remote", action="store_true", help="add the remote COG checks")
    p.add_argument("--output", help="write the Markdown report to this file")
    p.add_argument("--only", help="run one section")
    p.add_argument("--verbose", action="store_true", help="print each check as it runs")
    p.add_argument("--serve", help=argparse.SUPPRESS)
    args = p.parse_args(argv)
    if args.serve:
        serve(args.serve)
        return 0
    data = Path(args.data) if args.data and Path(args.data).is_dir() else None
    out = Path(tempfile.mkdtemp(prefix="terrio-xarray-compare-", dir=os.environ.get("TMPDIR")))
    synthetic_files(out)
    server = RangeServer(str(data)) if data is not None else None
    synth_server = RangeServer(str(out / "synth"))
    rows: list[Row] = []
    try:
        for c in CHECKS:
            if args.only and c.section != args.only:
                continue
            srv = synth_server if c.name.startswith("synth") else server
            why = available(c, data, args.remote, srv)
            if why:
                continue
            o = run_check(c, data, out, srv)
            rows.append(Row(c.section, c.name, c.expect, o, c.reason))
            if args.verbose:
                print(f"[{o.status}] {c.section}: {c.name} {o.detail}"[:300], file=sys.stderr)
    finally:
        if server is not None:
            server.close()
        synth_server.close()
        shutil.rmtree(out, ignore_errors=True)
    text = report(rows)
    if args.output:
        Path(args.output).write_text(text + "\n")
    print(text)
    bad = [
        r for r in rows if failed(Check(r.section, r.name, lambda *_: None, r.expect), r.outcome)
    ]
    for r in bad:
        print(f"FAILED {r.section}: {r.name}: {r.outcome.detail}", file=sys.stderr)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
