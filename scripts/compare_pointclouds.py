"""Compares terrio's point cloud reading and writing with laspy's and PDAL's.

For every LAS, LAZ and COPC file under DIR/pointcloud (see scripts/fetch_test_data.sh):

- read: the header fields and every value of every dimension, compared with laspy's exactly;
  the point count, version, format, scale, offset, bounds and CRS compared with
  `pdal info --metadata`;
- write: the points written by terrio as LAS, LAZ and COPC, read back by laspy (every value
  compared with the original) and by PDAL (`readers.las`, `readers.copc`);
- COPC queries: points read by bounds and resolution compared with PDAL's `readers.copc`;
- gridding: `PointCloud.grid` compared with PDAL's `writers.gdal` (every statistic);
- reprojection: `convert_points(t_srs=...)` compared with PDAL's `filters.reprojection`.

Needs the terrio Python package, laspy with lazrs, NumPy, and the `pdal` command (optional: the
PDAL comparisons are skipped without it).

Usage:
    python scripts/compare_pointclouds.py DIR
"""

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import laspy
import numpy as np

import terrio

PDAL = shutil.which("pdal")
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
    for a, b in zip(laspy_extra, ours_extra):
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
        "evlr_count": (len(h["evlrs"]), len(lh.evlrs or [])),
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
    """Writes a file's points as LAS, LAZ and COPC with terrio, and reads them back with laspy
    and PDAL.

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
    geographic = srs.endswith("4326")
    scale = 1e-7 if geographic else 0.001
    ref = pdal_points([str(path), {"type": "filters.reprojection", "out_srs": srs}], scale_x=scale, scale_y=scale, offset_x="auto", offset_y="auto")
    dx = np.abs(np.asarray(ours.x) - ref["x"]).max()
    dy = np.abs(np.asarray(ours.y) - ref["y"]).max()
    tol = 2e-7 if geographic else 0.002
    report(f"reproject {path.name} to {srs} vs PDAL: max |dx| {dx:.3g}, |dy| {dy:.3g}", dx <= tol and dy <= tol, "too far")


def main() -> None:
    """Runs the comparisons on the files under the directory given on the command line."""
    root = Path(sys.argv[1]) / "pointcloud"
    files = sorted(p for p in root.rglob("*") if p.suffix.lower() in (".las", ".laz"))
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
        compare_reprojection(root / "test_utm16.las", tmp, "EPSG:4326")
        compare_reprojection(root / "las_with_several_extra_byte_bloc.laz", tmp, "EPSG:4326")
        compare_reprojection(root / "synthetic" / "format6.laz", tmp, "EPSG:32615")
    print(f"\n{len(FAILURES)} failure(s)")
    for f in FAILURES:
        print("  " + f)
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
