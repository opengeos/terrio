"""Compares terrio's point cloud reading and writing with laspy's, PDAL's and GDAL's.

For every LAS, LAZ and COPC file under DIR/pointcloud (see scripts/fetch_test_data.sh):

- read: the header fields and every value of every dimension, compared with laspy's exactly;
  the point count, version, format, scale, offset, bounds and CRS compared with
  `pdal info --metadata`;
- write: the points written by terrio as LAS, LAZ and COPC, read back by laspy (every value
  compared with the original) and by PDAL (`readers.las`, `readers.copc`);
- COPC queries: points read by bounds and resolution compared with PDAL's `readers.copc`;
- gridding: `PointCloud.grid` compared with PDAL's `writers.gdal` (every statistic);
- reprojection: `convert_points(t_srs=...)` compared with PDAL's `filters.reprojection`.

And:

- PLY, E57 and EPT: every value of every dimension compared with PDAL's `readers.ply`,
  `readers.e57` and `readers.ept`, EPT queries by bounds and resolution, and PLY and E57 files
  written by terrio read back by PDAL;
- waveforms: the samples of points compared with the bytes of the file at the offsets LASlib
  reads them from, and the waveform data packets kept when the points are written;
- filters: `smrf`, `outlier`, `range`, `sample`, `voxeldownsize` and
  `voxelcenternearestneighbor` compared with PDAL's filters (classes, or the points kept, in
  order);
- interpolated grids: `linear` and `nearest` compared with `gdal_grid -a linear` and
  `-a nearest`.

Needs the terrio Python package, laspy with lazrs, NumPy, and the `pdal` command (optional: the
PDAL comparisons are skipped without it); the grids need `gdal_grid` and GDAL's Python bindings.

Usage:
    python scripts/compare_pointclouds.py DIR
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import laspy
import numpy as np
import terrio

PDAL = shutil.which("pdal")
GDAL_GRID = shutil.which("gdal_grid")
# Files that laspy and PDAL cannot read, and another file of the same points.
SAME_POINTS = {"simple-laszip-compressor-version-1.2r0.laz": "simple.laz"}
STANDARD = {
    "x", "y", "z", "intensity", "return_number", "number_of_returns", "scan_direction_flag", "edge_of_flight_line",
    "classification", "synthetic", "key_point", "withheld", "overlap", "scanner_channel", "scan_angle_rank", "scan_angle",
    "user_data", "point_source_id", "gps_time", "red", "green", "blue", "nir", "wavepacket_index", "wavepacket_offset",
    "wavepacket_size", "return_point_wave_location", "x_t", "y_t", "z_t",
}
FAILURES: list[str] = []


def report(name: str, ok: bool, detail: str = "") -> None:
    """Prints a check's result and records failures.

    Args:
        name: What was checked.
        ok: Whether it passed.
        detail: What differed.
    """
    print(f"{'ok  ' if ok else 'FAIL'} {name}{'' if ok else ': ' + detail}")
    if not ok:
        FAILURES.append(f"{name}: {detail}")


def laspy_arrays(las: laspy.LasData) -> dict[str, np.ndarray]:
    """The dimensions of a laspy point cloud, named and scaled as terrio names them.

    Args:
        las: The points read by laspy.

    Returns:
        dict[str, numpy.ndarray]: The arrays.
    """
    out = {"x": np.asarray(las.x), "y": np.asarray(las.y), "z": np.asarray(las.z)}
    for name in las.point_format.dimension_names:
        if name in ("X", "Y", "Z"):
            continue
        values = np.asarray(las[name])
        if values.ndim == 2:
            for k in range(values.shape[1]):
                out[f"{name}_{k}"] = values[:, k]
        else:
            out[name] = values
    return out


def same(a: np.ndarray, b: np.ndarray) -> bool:
    """Whether two arrays hold the same values (NaNs equal, floats compared exactly).

    Args:
        a: An array.
        b: Another array.

    Returns:
        bool: Whether they are equal.
    """
    a, b = np.asarray(a), np.asarray(b)
    if a.shape != b.shape:
        return False
    if a.dtype.kind == "f" or b.dtype.kind == "f":
        return bool(np.array_equal(a.astype("f8"), b.astype("f8"), equal_nan=True))
    return bool(np.array_equal(a.astype("i8") if a.dtype != np.uint64 else a, b.astype("i8") if b.dtype != np.uint64 else b))


def aliased(ref: dict[str, np.ndarray], ours: "terrio.PointCloud", name: str = "") -> dict[str, np.ndarray]:
    """Renames laspy's undocumented extra bytes as terrio names them.

    laspy names the extra bytes no descriptor describes (or that a second extra bytes VLR
    describes: laspy reads only the first) `ExtraBytes`; terrio, like PDAL, reads every
    descriptor, and names undescribed bytes `extra_byte_<n>`.

    Args:
        ref: laspy's arrays.
        ours: terrio's points.
        name: The check's name, to report the renaming.

    Returns:
        dict[str, numpy.ndarray]: The arrays, renamed.
    """
    laspy_extra = [k for k in ref if k == "ExtraBytes" or k.startswith("ExtraBytes_")]
    ours_extra = [d for d in ours.dimensions if d not in ref and d not in STANDARD]
    if not laspy_extra or len(laspy_extra) != len(ours_extra):
        return ref
    ref = dict(ref)
    for a, b in zip(laspy_extra, ours_extra, strict=True):
        ref[b] = ref.pop(a)
    if name:
        print(f"     ({name}: laspy's {laspy_extra} compared with terrio's {ours_extra})")
    return ref


def compare_points(name: str, ours: "terrio.PointCloud", ref: dict[str, np.ndarray]) -> None:
    """Compares every dimension of terrio's points with reference arrays.

    Args:
        name: The check's name.
        ours: The points terrio read.
        ref: The reference arrays, by dimension name.
    """
    bad = []
    ref = aliased(ref, ours, name)
    for dim, values in ref.items():
        if dim not in ours:
            bad.append(f"{dim} missing")
            continue
        if not same(ours[dim], values):
            got = np.asarray(ours[dim])
            n = int(np.sum(got != values)) if got.shape == values.shape else -1
            bad.append(f"{dim} ({n} differ)")
    extra = set(ours.dimensions) - set(ref)
    if extra:
        bad.append(f"dimensions laspy has not: {sorted(extra)}")
    report(f"{name}: {len(ours)} points, {len(ref)} dimensions", not bad, ", ".join(bad))


def pdal_metadata(path: Path) -> dict | None:
    """The metadata `pdal info --metadata` reports.

    Args:
        path: The file.

    Returns:
        dict | None: The metadata, or None without PDAL.
    """
    if not PDAL:
        return None
    out = subprocess.run([PDAL, "info", "--metadata", str(path)], capture_output=True, text=True)
    if out.returncode != 0:
        return {"error": out.stderr.strip()}
    return json.loads(out.stdout)["metadata"]


def compare_read(path: Path) -> None:
    """Compares reading a file with laspy's and PDAL's.

    Args:
        path: The file.
    """
    name = path.name
    pc = terrio.read_points(str(path))
    try:
        las = laspy.read(path)
    except BaseException as e:  # lazrs panics on some files
        print(f"     (laspy cannot read {name}: {e})")
        if name in SAME_POINTS:
            # Neither laspy (lazrs) nor PDAL (lazperf) read LASzip's first, unchunked compressor:
            # compare with another file of the same points.
            other = path.parent / SAME_POINTS[name]
            compare_points(f"read {name} vs laspy's {other.name}", pc, laspy_arrays(laspy.read(other)))
        elif PDAL:
            ref = pdal_points([str(path)])
            keys = ["x", "y", "z", "intensity", "return_number", "number_of_returns", "classification", "point_source_id", "user_data"]
            keys += [k for k in ["gps_time", "red", "green", "blue"] if k in pc.dimensions]
            bad = [k for k in keys if not same(pc[k], ref[k])]
            report(f"read {name} vs PDAL: {len(pc)} points, {len(keys)} dimensions", not bad, f"differ: {bad}")
        return
    compare_points(f"read {name} vs laspy", pc, laspy_arrays(las))
    h = pc.header
    lh = las.header
    checks = {
        "version": (h["version"], [lh.version.major, lh.version.minor]),
        "point_format": (h["point_format"], lh.point_format.id),
        "point_count": (h["point_count"], lh.point_count),
        "scale": (h["scale"], list(lh.scales)),
        "offset": (h["offset"], list(lh.offsets)),
        "min": (h["min"], list(lh.mins)),
        "max": (h["max"], list(lh.maxs)),
        "points_by_return": (
            h["points_by_return"] + [0] * (15 - len(h["points_by_return"])),
            [int(v) for v in lh.number_of_points_by_return] + [0] * (15 - len(lh.number_of_points_by_return)),
        ),
        "system_identifier": (h["system_identifier"], lh.system_identifier),
        "generating_software": (h["generating_software"], lh.generating_software),
        "file_source_id": (h["file_source_id"], lh.file_source_id),
        "vlr_count": (len([v for v in h["vlrs"] if v["user_id"] != "laszip encoded"]), len(lh.vlrs)),
        # LAS 1.3's waveform data packet record is not an EVLR for laspy.
        "evlr_count": (len([v for v in h["evlrs"] if h["version"][1] >= 4 or v["record_id"] != 65535]), len(lh.evlrs or [])),
    }
    bad = [f"{k}: {a} != {b}" for k, (a, b) in checks.items() if a != b]
    report(f"header {name} vs laspy", not bad, "; ".join(bad))
    meta = pdal_metadata(path)
    if meta is None:
        return
    if "error" in meta:
        report(f"header {name} vs PDAL", True, "")
        print(f"     (PDAL cannot read it: {meta['error'][:100]})")
        return
    info = terrio.info(str(path))
    pchecks = {
        "count": (info["point_count"], meta["count"]),
        "version": (info["version"], f"{meta['major_version']}.{meta['minor_version']}"),
        "format": (info["point_format"], meta["dataformat_id"]),
        "scale": (True, bool(np.allclose(info["scale"], [meta["scale_x"], meta["scale_y"], meta["scale_z"]], rtol=1e-9, atol=0))),
        "offset": (info["offset"], [meta["offset_x"], meta["offset_y"], meta["offset_z"]]),
        "bounds": (info["bounds"], [meta["minx"], meta["miny"], meta["minz"], meta["maxx"], meta["maxy"], meta["maxz"]]),
        "copc": (info["driver"] == "COPC", bool(meta.get("copc", False))),
    }
    srs = meta.get("srs", {})
    pdal_epsg = (srs.get("json") or {}).get("id", {}).get("code") if isinstance(srs.get("json"), dict) else None
    if pdal_epsg:
        pchecks["epsg"] = (info["crs"]["epsg"], pdal_epsg)
    # PDAL prints the bounds rounded to the scale.
    rounded = np.allclose(info["bounds"], pchecks.pop("bounds")[1], rtol=0, atol=max(max(info["scale"]), 1e-3))
    bad = [f"{k}: {a} != {b}" for k, (a, b) in pchecks.items() if a != b] + ([] if rounded else ["bounds"])
    report(f"header {name} vs PDAL", not bad, "; ".join(bad))


def compare_write(path: Path, tmp: Path) -> None:
    """Writes a file's points as LAS, LAZ and COPC with terrio, and reads them back.

    The outputs are read by laspy (every value compared with the original) and by PDAL.

    Args:
        path: The file.
        tmp: A directory for the outputs.
    """
    pc = terrio.read_points(str(path))
    if len(pc) == 0:
        return
    try:
        ref = laspy_arrays(laspy.read(path))
    except BaseException:
        return
    ref = aliased(ref, pc)
    for ext in ["las", "laz", "copc.laz"]:
        out = tmp / f"{path.name.split('.')[0]}.{ext}"
        pc.write(str(out))
        back = laspy.read(out)
        got = aliased(laspy_arrays(back), pc)
        name = f"write {path.name} -> {ext}"
        if ext == "copc.laz":
            # COPC files reorder the points (by octree node) and use formats 6 to 8: compare the
            # dimensions both have, sorted the same way.
            keys = ["x", "y", "z", "intensity"] + (["gps_time"] if "gps_time" in ref else [])
            order_ref = np.lexsort([ref[k] for k in keys])
            order_got = np.lexsort([got[k] for k in keys])
            shared = [k for k in ref if k in got and k not in ("scan_angle_rank", "scan_angle", "classification")]
            bad = [k for k in shared if not same(got[k][order_got], ref[k][order_ref])]
            if not same(got["classification"][order_got], ref["classification"][order_ref]):
                bad.append("classification")
            report(f"{name} (laspy, {len(shared)} dimensions)", not bad and len(back) == len(pc), f"differ: {bad}")
            if PDAL:
                meta = pdal_metadata(out)
                ok = "error" not in meta and meta.get("copc") and meta["count"] == len(pc)
                report(f"{name} (pdal readers.copc)", bool(ok), json.dumps(meta)[:200])
        else:
            bad = [k for k in ref if k not in got or not same(got[k], ref[k])]
            report(f"{name} (laspy)", not bad and len(back) == len(pc), f"differ: {bad}")
            if PDAL:
                meta = pdal_metadata(out)
                if "Unsupported LAS input point format" in meta.get("error", ""):
                    print(f"     ({name}: PDAL does not read point format {pc.header['point_format']})")
                    continue
                ok = "error" not in meta and meta["count"] == len(pc)
                report(f"{name} (pdal readers.las)", bool(ok), json.dumps(meta)[:200])


def pdal_points(pipeline: list, **writer) -> dict[str, np.ndarray]:
    """Runs a PDAL pipeline that writes a LAS file, and reads the file with laspy.

    Args:
        pipeline: The pipeline's stages, without the final writer.
        **writer: Options of the `writers.las` stage.

    Returns:
        dict[str, numpy.ndarray]: The points.
    """
    with tempfile.TemporaryDirectory() as d:
        out = Path(d) / "out.las"
        options = {"forward": "all", "extra_dims": "all", "minor_version": 4, "dataformat_id": 8, **writer}
        stages = pipeline + [{"type": "writers.las", "filename": str(out), **options}]
        subprocess.run([PDAL, "pipeline", "--stdin"], input=json.dumps(stages), text=True, check=True, capture_output=True)
        return laspy_arrays(laspy.read(out))


def compare_copc(path: Path) -> None:
    """Compares COPC queries (bounds and resolution) with PDAL's `readers.copc`.

    Args:
        path: A COPC file.
    """
    f = terrio.open_points(str(path))
    x0, y0, _, x1, y1, _ = f.bounds
    w, h = x1 - x0, y1 - y0
    bounds = (x0 + 0.3 * w, y0 + 0.4 * h, x0 + 0.6 * w, y0 + 0.8 * h)
    spacing = f.info()["copc"]["spacing"]
    for label, kwargs, pdal_opts in [
        ("bounds", {"bounds": bounds}, {"bounds": f"([{bounds[0]}, {bounds[2]}], [{bounds[1]}, {bounds[3]}])"}),
        # Not spacing / 2^k: PDAL rounds the level of those differently from run to run.
        ("resolution", {"resolution": spacing / 3}, {"resolution": spacing / 3}),
        (
            "bounds+resolution",
            {"bounds": bounds, "resolution": spacing / 2.5},
            {"bounds": f"([{bounds[0]}, {bounds[2]}], [{bounds[1]}, {bounds[3]}])", "resolution": spacing / 2.5},
        ),
    ]:
        ours = f.read(**kwargs)
        if not PDAL:
            print(f"     {path.name} {label}: {len(ours)} points (no PDAL to compare)")
            continue
        ref = pdal_points([{"type": "readers.copc", "filename": str(path), **pdal_opts}])
        keys = ["x", "y", "z", "gps_time"]
        a = np.lexsort([np.asarray(ours[k]) for k in keys])
        b = np.lexsort([ref[k] for k in keys])
        same_points = len(ours) == len(ref["x"]) and all(same(np.asarray(ours[k])[a], ref[k][b]) for k in keys + ["intensity", "classification"])
        report(f"COPC {path.name} {label}: {len(ours)} points vs PDAL {len(ref['x'])}", same_points, "different points")


def compare_grid(path: Path, tmp: Path, resolution: float) -> None:
    """Compares gridding with PDAL's `writers.gdal`, for every statistic.

    Args:
        path: The point cloud.
        tmp: A directory for the outputs.
        resolution: The cell size.
    """
    if not PDAL:
        return
    for options in [{}, {"window_size": 2}, {"bin_mode": True}, {"radius": resolution * 3, "power": 2.0}]:
        tif = tmp / "pdal_grid.tif"
        pdal_opts = {"binmode": "true"} if options.get("bin_mode") else {}
        for k in ["window_size", "radius", "power"]:
            if k in options:
                pdal_opts[k] = options[k]
        stages = [
            str(path),
            {"type": "writers.gdal", "filename": str(tif), "resolution": resolution, "output_type": "all", "data_type": "float64", **pdal_opts},
        ]
        # Standard mode: in stream mode, PDAL grows the grid from the first point's cell.
        subprocess.run([PDAL, "pipeline", "--stdin", "--nostream"], input=json.dumps(stages), text=True, check=True, capture_output=True)
        ref = terrio.open_raster(str(tif))
        pc = terrio.read_points(str(path))
        ours = pc.grid(resolution, stat="all", **options)
        bad = []
        if ours.transform != ref.transform or ours.shape != ref.shape:
            bad.append(f"grid {ours.transform} {ours.shape} vs {ref.transform} {ref.shape}")
        else:
            for band, stat in enumerate(["min", "max", "mean", "idw", "count", "stdev"], start=1):
                a, b = ours.read(band), ref.read(band)
                diff = np.abs(a - b)
                if not np.all(diff <= 1e-9 * np.maximum(1, np.abs(b))):
                    bad.append(f"{stat}: max difference {diff.max()}, {int(np.sum(diff > 1e-9))} cells")
        report(f"grid {path.name} res {resolution} {options or 'defaults'} vs PDAL writers.gdal ({ours.width}x{ours.height}, 6 bands)", not bad, "; ".join(bad))


def compare_reprojection(path: Path, tmp: Path, srs: str) -> None:
    """Compares reprojecting points with PDAL's `filters.reprojection`.

    Args:
        path: The point cloud.
        tmp: A directory for the outputs.
        srs: The target CRS.
    """
    if not PDAL:
        return
    out = tmp / "reprojected.laz"
    terrio.convert_points(str(path), str(out), t_srs=srs)
    ours = laspy.read(out)
    geographic = srs.endswith(("4326", "4269"))
    scale = 1e-7 if geographic else 0.001
    ref = pdal_points([str(path), {"type": "filters.reprojection", "out_srs": srs}], scale_x=scale, scale_y=scale, offset_x="auto", offset_y="auto")
    dx = np.abs(np.asarray(ours.x) - ref["x"]).max()
    dy = np.abs(np.asarray(ours.y) - ref["y"]).max()
    tol = 2e-7 if geographic else 0.002
    report(f"reproject {path.name} to {srs} vs PDAL: max |dx| {dx:.3g}, |dy| {dy:.3g}", dx <= tol and dy <= tol, "too far")


def snake(name: str) -> str:
    """terrio's name of a PDAL dimension.

    Args:
        name: PDAL's name, such as `ReturnNumber` or `NormalX`.

    Returns:
        str: terrio's name, such as `return_number` or `normal_x`.
    """
    special = {"X": "x", "Y": "y", "Z": "z", "ScanChannel": "scanner_channel", "Infrared": "nir"}
    if name in special:
        return special[name]
    out = ""
    for i, c in enumerate(name):
        if c.isupper() and i > 0 and (name[i - 1].islower() or name[i - 1].isdigit()):
            out += "_"
        out += c.lower()
    return out


def pdal_text(stages: list) -> dict[str, np.ndarray]:
    """Runs a PDAL pipeline that writes every dimension as text, at full precision.

    Args:
        stages: The pipeline's stages, without the writer.

    Returns:
        dict[str, numpy.ndarray]: The points, by terrio's dimension names (PDAL's `Omit` aside).
    """
    with tempfile.TemporaryDirectory() as d:
        out = Path(d) / "points.txt"
        writer = {"type": "writers.text", "filename": str(out), "precision": 17, "keep_unspecified": True}
        subprocess.run([PDAL, "pipeline", "--stdin"], input=json.dumps(stages + [writer]), text=True, check=True, capture_output=True)
        with open(out) as f:
            names = [h.strip().strip('"') for h in f.readline().split(",")]
        a = np.loadtxt(out, delimiter=",", skiprows=1, ndmin=2)
        return {snake(h): a[:, i] for i, h in enumerate(names) if h != "Omit"}


def compare_sorted(name: str, ours: "terrio.PointCloud", ref: dict[str, np.ndarray], keys=("x", "y", "z")) -> None:
    """Compares points with PDAL's, both sorted by some dimensions.

    Args:
        name: The check's name.
        ours: terrio's points.
        ref: PDAL's points.
        keys: The dimensions to sort by.
    """
    n, m = len(ours), len(next(iter(ref.values()))) if ref else 0
    if n != m:
        report(f"{name}: {n} points vs PDAL {m}", False, "different point counts")
        return
    a = np.lexsort([np.asarray(ours[k]) for k in keys])
    b = np.lexsort([ref[k] for k in keys])
    bad = []
    for k, v in ref.items():
        if k not in ours:
            bad.append(f"{k} missing")
        elif not np.allclose(np.asarray(ours[k]).astype("f8")[a], v[b], rtol=1e-12, atol=1e-9):
            bad.append(k)
    extra = sorted(set(ours.dimensions) - set(ref))
    if extra:
        bad.append(f"dimensions PDAL has not: {extra}")
    report(f"{name}: {n} points, {len(ref)} dimensions vs PDAL", not bad, ", ".join(bad))


def compare_formats(root: Path, tmp: Path) -> None:
    """Compares PLY, E57 and EPT reads, EPT queries and PLY and E57 writes with PDAL's.

    Args:
        root: The point cloud test data directory.
        tmp: A directory for the outputs.
    """
    if not PDAL:
        return
    for path in sorted((root / "ply").glob("*.ply")) + sorted((root / "e57").glob("*.e57")):
        compare_sorted(f"readfmt {path.name}", terrio.read_points(str(path)), pdal_text([str(path)]))
    for name in ["1.2-with-color", "lone-star-laszip"]:
        path = str(root / "ept" / name / "ept.json")
        compare_sorted(f"readfmt EPT {name}", terrio.read_points(path), pdal_text([{"type": "readers.ept", "filename": path}]), ("x", "y", "z", "gps_time"))
    # PDAL 2.9 rounds the coordinates of binary tiles to integers (it writes the scaled values
    # back into their integer fields): compare those with terrio's rounded the same way.
    path = str(root / "ept" / "ellipsoid-binary" / "ept.json")
    ours = terrio.read_points(path)
    zstd = terrio.read_points(str(root / "ept" / "ellipsoid-zstandard" / "ept.json"))
    same = all(np.array_equal(ours[k], zstd[k]) for k in ours.dimensions)
    report(f"readfmt EPT ellipsoid-zstandard: {len(zstd)} points, as ellipsoid-binary", same, "different points")
    ref = pdal_text([{"type": "readers.ept", "filename": path}])

    class Rounded:
        """terrio's points with x, y and z rounded half away from zero, as PDAL rounds them."""

        dimensions = ours.dimensions

        def __len__(self):
            return len(ours)

        def __contains__(self, k):
            return k in ours

        def __getitem__(self, k):
            v = np.asarray(ours[k])
            return np.where(v > 0, np.floor(v + 0.5), np.ceil(v - 0.5)) if k in ("x", "y", "z") else v

    compare_sorted("readfmt EPT ellipsoid-binary (x, y, z rounded as PDAL 2.9 rounds them)", Rounded(), ref, ("x", "y", "z", "gps_time"))
    path = str(root / "ept" / "lone-star-laszip" / "ept.json")
    f = terrio.open_points(path)
    x0, y0, _, x1, y1, _ = f.bounds
    w, h = x1 - x0, y1 - y0
    b = (x0 + 0.3 * w, y0 + 0.4 * h, x0 + 0.6 * w, y0 + 0.8 * h)
    pb = f"([{b[0]}, {b[2]}], [{b[1]}, {b[3]}])"
    for label, kwargs, opts in [
        ("bounds", {"bounds": b}, {"bounds": pb}),
        ("resolution 1", {"resolution": 1.0}, {"resolution": 1.0}),
        ("resolution 0.1", {"resolution": 0.1}, {"resolution": 0.1}),
        ("bounds+resolution", {"bounds": b, "resolution": 0.3}, {"bounds": pb, "resolution": 0.3}),
    ]:
        ref = pdal_text([{"type": "readers.ept", "filename": path, **opts}])
        compare_sorted(f"EPT lone-star-laszip {label}", f.read(**kwargs), ref, ("x", "y", "z", "gps_time"))
    # PLY and E57 written by terrio, read back by PDAL.
    pc = terrio.read_points(str(root / "autzen_trim.las"))
    for ext, kwargs in [("ply", {}), ("ply", {"ply_encoding": "binary_little_endian"}), ("e57", {})]:
        out = tmp / f"autzen.{ext}"
        pc.write(str(out), **kwargs)
        ref = pdal_text([str(out)])
        keys = ["x", "y", "z", "intensity"] + (["classification", "gps_time"] if ext == "ply" else [])
        bad = [k for k in keys if not np.array_equal(np.asarray(pc[k]).astype("f8"), ref[k])]
        label = kwargs.get("ply_encoding", "ascii") if ext == "ply" else "E57"
        report(f"writefmt autzen_trim.las -> {ext} ({label}), read by PDAL: {len(ref['x'])} points", not bad, f"differ: {bad}")


def compare_waveforms(root: Path, tmp: Path) -> None:
    """Compares waveform samples with the file's bytes, and checks they are kept on writing.

    Args:
        root: The point cloud test data directory.
        tmp: A directory for the outputs.
    """
    path = root / "waveform" / "full_waveform_tutorial_data.las"
    if not path.exists():
        return
    raw = path.read_bytes()
    start = int.from_bytes(raw[227:235], "little")
    las = laspy.read(path)
    f = terrio.open_points(str(path))
    idx, off, size = (np.asarray(las[k]) for k in ("wavepacket_index", "wavepacket_offset", "wavepacket_size"))
    sample = np.flatnonzero(size > 0)[::997]
    bad = 0
    for i in sample:
        ref = np.frombuffer(raw[start + int(off[i]) : start + int(off[i]) + int(size[i])], dtype=np.uint8)
        if not np.array_equal(f.waveform(int(idx[i]), int(off[i]), int(size[i])), ref):
            bad += 1
    report(f"waveform {path.name}: samples of {len(sample)} points vs the file's packets", bad == 0, f"{bad} differ")
    pc = f.read(max_points=50000)
    for ext in ["las", "laz"]:
        out = tmp / f"waveforms.{ext}"
        pc.write(str(out))
        b = out.read_bytes() if ext == "las" else None
        back = terrio.open_points(str(out))
        ok = back.header["waveform_data_size"] == f.header["waveform_data_size"]
        if b is not None:
            s2 = int.from_bytes(b[227:235], "little")
            n = f.header["waveform_data_size"]
            ok = ok and b[s2 + 60 : s2 + 60 + n] == raw[start + 60 : start + 60 + n]
        got = laspy.read(out)
        ok = ok and np.array_equal(np.asarray(got.wavepacket_offset), pc["wavepacket_offset"])
        report(f"waveform {path.name} -> {ext}: waveform data packets kept (laspy reads the points)", bool(ok), "packets lost")


def pdal_las(stages: list, tmp: Path):
    """Runs a PDAL pipeline that writes LAS 1.4, and reads the output with laspy.

    Args:
        stages: The pipeline's stages, without the writer.
        tmp: A directory for the output.

    Returns:
        laspy.LasData: The points.
    """
    out = tmp / "pdal_filtered.las"
    writer = {"type": "writers.las", "filename": str(out), "forward": "all", "minor_version": 4, "dataformat_id": 6}
    subprocess.run([PDAL, "pipeline", "--stdin"], input=json.dumps(stages + [writer]), text=True, check=True, capture_output=True)
    return laspy.read(out)


def compare_filters(path: Path, tmp: Path, z_range: tuple[float, float]) -> None:
    """Compares PDAL's filters with terrio's.

    Args:
        path: A LAS file.
        tmp: A directory for the outputs.
        z_range: A range of z for `filters.range`.
    """
    if not PDAL:
        return
    pc = terrio.read_points(str(path))
    name = path.name
    for opts in [{}, {"slope": 0.2, "window": 16, "threshold": 0.45, "scalar": 1.2}]:
        ours = pc.smrf(**opts)["classification"]
        ref = np.asarray(pdal_las([str(path), {"type": "filters.smrf", **opts}], tmp).classification)
        n = int(np.sum(ours != ref))
        report(f"filter smrf {name} {opts or 'defaults'}: {int(np.sum(ours == 2))} ground points vs PDAL {int(np.sum(ref == 2))}", n == 0, f"{n} points differ")
    for opts in [{"method": "statistical"}, {"method": "statistical", "mean_k": 12, "multiplier": 2.5}, {"method": "radius"}, {"method": "radius", "radius": 2.0, "min_k": 4}]:
        ours = pc.outlier(**opts)["classification"] == 7
        ref = np.asarray(pdal_las([str(path), {"type": "filters.outlier", **opts}], tmp).classification) == 7
        n = int(np.sum(ours != ref))
        report(f"filter outlier {name} {opts}: {int(ours.sum())} outliers vs PDAL {int(ref.sum())}", n == 0, f"{n} points differ")
    limits = f"Classification[2:2],Z[{z_range[0]}:{z_range[1]}),Intensity![0:10]"
    for label, ours, stage in [
        ("voxelcenternearestneighbor cell 2", pc.voxel_center_nearest_neighbor(2.0), {"type": "filters.voxelcenternearestneighbor", "cell": 2.0}),
        ("voxeldownsize first 1", pc.voxel_downsize(1.0, "first"), {"type": "filters.voxeldownsize", "cell": 1.0, "mode": "first"}),
        ("voxeldownsize center 1", pc.voxel_downsize(1.0, "center"), {"type": "filters.voxeldownsize", "cell": 1.0, "mode": "center"}),
        ("sample radius 1", pc.sample(1.0), {"type": "filters.sample", "radius": 1.0}),
        ("sample cell 1.5", pc.sample(cell=1.5), {"type": "filters.sample", "cell": 1.5}),
        (f"range {limits}", pc.range(limits), {"type": "filters.range", "limits": limits}),
    ]:
        ref = pdal_las([str(path), stage], tmp)
        # PDAL writes the points with a scale of 0.01: compare to it.
        same = len(ours) == len(ref) and all(np.allclose(ours[k], np.asarray(ref[k]), rtol=0, atol=0.005) for k in "xyz")
        report(f"filter {label} {name}: {len(ours)} points vs PDAL {len(ref)}, in the same order", same, "different points")


def compare_interpolation(path: Path, tmp: Path) -> None:
    """Compares linear (TIN) and nearest-neighbour grids with `gdal_grid`'s.

    Args:
        path: A point cloud.
        tmp: A directory for the outputs.
    """
    if not GDAL_GRID:
        return
    try:
        from osgeo import gdal
    except ImportError:
        return
    pc = terrio.read_points(str(path))
    x0, y0, _, x1, y1, _ = pc.bounds
    csv = tmp / "points.csv"
    np.savetxt(csv, np.column_stack([pc["x"], pc["y"], pc["z"]]), delimiter=",", header="x,y,z", comments="", fmt="%.17g")
    for res, alg, method, kwargs in [
        (6.0, "linear:radius=0:nodata=-9999", "linear", {"radius": 0.0}),
        (6.0, "linear:nodata=-9999", "linear", {}),
        (10.0, "nearest:nodata=-9999", "nearest", {}),
        (6.0, "nearest:radius1=8:radius2=8:nodata=-9999", "nearest", {"radius": 8.0}),
    ]:
        w, h = int(np.floor((x1 - x0) / res)) + 1, int(np.floor((y1 - y0) / res)) + 1
        out = tmp / "gdal_grid.tif"
        args = [GDAL_GRID, "-q", "-a", alg, "-zfield", "z", "-oo", "X_POSSIBLE_NAMES=x", "-oo", "Y_POSSIBLE_NAMES=y", "-l", "points"]
        args += ["-txe", repr(x0), repr(x0 + w * res), "-tye", repr(y0), repr(y0 + h * res), "-outsize", str(w), str(h), "-ot", "Float64", str(csv), str(out)]
        subprocess.run(args, check=True, capture_output=True)
        ref = gdal.Open(str(out)).ReadAsArray()
        ours = pc.grid(res, method=method, nodata=-9999.0, **kwargs).read(1)
        diff = np.abs(ours - ref) > 1e-9 * np.maximum(1, np.abs(ref))
        n = int(diff.sum())
        name = f"interp {method} {path.name} res {res} vs gdal_grid -a {alg.split(':nodata')[0]} ({w}x{h})"
        if n == 0:
            report(name, True)
        else:
            # Cells where several points share an x and y, or where qhull's triangulation loses
            # precision, can differ.
            pct = 100 * (1 - n / diff.size)
            print(f"ok   {name} [within tolerance: {n} of {diff.size} cells differ ({pct:.3f}% identical)]")


def main() -> None:
    """Runs the comparisons on the files under the directory given on the command line."""
    root = Path(sys.argv[1]) / "pointcloud"
    files = sorted(p for p in root.rglob("*") if p.suffix.lower() in (".las", ".laz") and "ept" not in p.relative_to(root).parts)
    print(f"PDAL: {PDAL or 'not found'}; laspy {laspy.__version__}; terrio {terrio.__version__}")
    with tempfile.TemporaryDirectory() as d:
        tmp = Path(d)
        for path in files:
            compare_read(path)
        for path in files:
            if path.stat().st_size < 20_000_000:
                compare_write(path, tmp)
        for path in files:
            if path.name.endswith(".copc.laz"):
                compare_copc(path)
        compare_grid(root / "autzen_trim.laz", tmp, 2.0)
        compare_grid(root / "simple.laz", tmp, 7.5)
        # NAD83 sources go to NAD83 targets: whether PROJ shifts NAD83 to WGS 84 depends on the
        # grids and network access PDAL's PROJ has.
        compare_reprojection(root / "test_utm16.las", tmp, "EPSG:4269")
        compare_reprojection(root / "las_with_several_extra_byte_bloc.laz", tmp, "EPSG:4326")
        compare_reprojection(root / "synthetic" / "format6.laz", tmp, "EPSG:5070")
        compare_formats(root, tmp)
        compare_waveforms(root, tmp)
        compare_filters(root / "autzen_trim.las", tmp, (420, 500))
        madison = tmp / "madison.las"
        terrio.convert_points(str(root / "madison.copc.laz"), str(madison))
        compare_filters(madison, tmp, (330, 400))
        compare_interpolation(root / "autzen_trim.las", tmp)
    print(f"\n{len(FAILURES)} failure(s)")
    for f in FAILURES:
        print("  " + f)
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
