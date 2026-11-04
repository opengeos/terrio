"""Zonal statistics benchmark: wall time, CPU time and peak memory against exactextract and
terrio's previous engine, on Sentinel-2 L2A COGs and crop fields.

Data are cached under --data (never committed): the first N dates of one MGRS tile from
Earth Search, 10 bands each, and seeded synthetic fields (or --fields-file). Every tool runs
in a fresh process, measured from its own resource usage, with the same thread count, three times
interleaved; equivalence with terrio is checked before any speed ratio is reported.

    python scripts/benchmark_zonal.py --data ~/zonal-bench --dates 10 --fields 10000 \\
        --tools terrio,exactextract --json docs/benchmarks/zonal.json

exactextract runs in --gdal-python (an environment with GDAL 3.12+ and exactextract). See
docs/benchmarks.md.
"""

import argparse
import json
import math
import os
import platform
import random
import re
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

STAC = "https://earth-search.aws.element84.com/v1/search"
BANDS = ["blue", "green", "red", "nir", "rededge1", "rededge2", "rededge3", "nir08", "swir16", "swir22"]
STAT_SETS = {
    "mean": ["mean"],
    "moments": ["count", "mean", "stddev"],
    "distribution": ["count", "mean", "stddev", "median", "mode"],
}
HERE = Path(__file__).resolve().parent


def log(*args):
    print(*args, file=sys.stderr, flush=True)


# --- Data -------------------------------------------------------------------------------------


def items(data, tile, season, dates):
    """The first `dates` items (by date, one per day) of `tile`, cached in items.json."""
    cache = data / ("items-" + re.sub(r"[^0-9A-Za-z]+", "_", f"{tile}-{season}") + ".json")
    if not cache.exists():
        body = {
            "collections": ["sentinel-2-l2a"],
            "datetime": season,
            "query": {"grid:code": {"eq": f"MGRS-{tile}"}, "eo:cloud_cover": {"lt": 20}},
            "limit": 100,
            "sortby": [{"field": "properties.datetime", "direction": "asc"}],
        }
        request = urllib.request.Request(STAC, json.dumps(body).encode(), {"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=60) as response:
            features = json.load(response)["features"]
        cache.write_text(json.dumps(features))
    features, seen = [], set()
    for f in json.loads(cache.read_text()):
        day = f["properties"]["datetime"][:10]
        if day not in seen:
            seen.add(day)
            features.append(f)
    if len(features) < dates:
        raise SystemExit(f"only {len(features)} dates of {tile} in {season}")
    return features[:dates]


def download(data, features):
    """Local copies of each item's bands: rows of (path, date, band, url)."""
    rows = []
    for f in features:
        day = f["properties"]["datetime"][:10]
        for band in BANDS:
            url = f["assets"][band]["href"]
            path = data / "s2" / day / f"{band}.tif"
            if not path.exists():
                path.parent.mkdir(parents=True, exist_ok=True)
                log(f"downloading {day} {band}")
                urllib.request.urlretrieve(url, path.with_suffix(".part"))
                path.with_suffix(".part").rename(path)
            rows.append((path, day, band, url))
    return rows


def repeat_path(data, path, r):
    """`path` itself for r = 0, otherwise a symlink to it under s2-repeat/r{r}/ (created once)."""
    if r == 0:
        return path
    link = data / "s2-repeat" / f"r{r}" / path.relative_to(data / "s2")
    if not link.is_symlink():
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to(path.resolve())
    return link


def fields(data, n, reference, aoi_km=None, seed=20261104):
    """`n` non-overlapping rotated rectangles (fields) over the raster `reference`, or over a
    centered `aoi_km` square of it, 5% with a hole, in its CRS: a jittered grid of cells, each
    field 20–80% of its cell."""
    path = data / (f"fields-{n}.gpkg" if aoi_km is None else f"fields-{n}-aoi{aoi_km:g}km.gpkg")
    if path.exists():
        return path
    from osgeo import gdal, ogr

    ds = gdal.Open(str(reference))
    gt, w, h = ds.GetGeoTransform(), ds.RasterXSize, ds.RasterYSize
    x0, y1 = gt[0], gt[3]
    x1, y0 = x0 + w * gt[1], y1 + h * gt[5]
    if aoi_km is not None:
        cx, cy, half = (x0 + x1) / 2, (y0 + y1) / 2, aoi_km * 500
        if 2 * half > min(x1 - x0, y1 - y0):
            raise SystemExit(f"--aoi-km {aoi_km:g} is larger than the tile")
        x0, x1, y0, y1 = cx - half, cx + half, cy - half, cy + half
    side = math.ceil(math.sqrt(n))
    cw, ch = (x1 - x0) / side, (y1 - y0) / side
    rng = random.Random(seed)
    out = ogr.GetDriverByName("GPKG").CreateDataSource(str(path))
    layer = out.CreateLayer("fields", ds.GetSpatialRef(), ogr.wkbPolygon)
    layer.CreateField(ogr.FieldDefn("field_id", ogr.OFTInteger))
    layer.StartTransaction()
    cells = rng.sample(range(side * side), n)
    for k, cell in enumerate(cells):
        cx, cy = x0 + (cell % side + 0.5) * cw, y0 + (cell // side + 0.5) * ch
        fraction = rng.uniform(0.2, 0.8)
        aspect = rng.uniform(0.5, 2.0)
        a = math.sqrt(fraction * cw * ch * aspect) / 2
        b = math.sqrt(fraction * cw * ch / aspect) / 2
        scale = min(1.0, 0.45 * cw / max(a, b))  # stay inside the cell when rotated
        a, b = a * scale, b * scale
        angle = rng.uniform(0, math.pi)
        ca, sa = math.cos(angle), math.sin(angle)

        def ring(u, v):
            r = ogr.Geometry(ogr.wkbLinearRing)
            for px, py in [(-u, -v), (u, -v), (u, v), (-u, v), (-u, -v)]:
                r.AddPoint_2D(cx + px * ca - py * sa, cy + px * sa + py * ca)
            return r

        polygon = ogr.Geometry(ogr.wkbPolygon)
        polygon.AddGeometry(ring(a, b))
        if rng.random() < 0.05:
            polygon.AddGeometry(ring(a / 3, b / 3))
        feature = ogr.Feature(layer.GetLayerDefn())
        feature.SetField("field_id", k)
        feature.SetGeometry(polygon)
        layer.CreateFeature(feature)
    layer.CommitTransaction()
    out = None
    return path


# --- Tools ------------------------------------------------------------------------------------

EXACTEXTRACT = r"""
import json, sys
from exactextract import exact_extract
spec = json.loads(sys.argv[1])
names = {"stddev": "stdev"}
out = {}
for path, day, band in spec["rasters"]:
    result = exact_extract(path, spec["zones"], [names.get(s, s) for s in spec["stats"]], output="pandas")
    for s in spec["stats"]:
        out[f"{day}_{band}_{s}"] = [None if x != x else float(x) for x in result[names.get(s, s)]]
json.dump(out, open(spec["output"], "w"))
"""


def timed(cmd, env=None):
    """Runs `cmd`: wall seconds, CPU seconds, peak RSS MiB (from the child's own rusage)."""
    with tempfile.TemporaryFile() as stderr:
        start = time.perf_counter()
        child = subprocess.Popen(list(map(str, cmd)), stdout=subprocess.DEVNULL, stderr=stderr, env=env)
        _, status, usage = os.wait4(child.pid, 0)
        wall = time.perf_counter() - start
        child.returncode = os.waitstatus_to_exitcode(status)
        if child.returncode:
            stderr.seek(0)
            raise RuntimeError(f"{cmd[0]} failed: {stderr.read().decode(errors='replace')[-2000:]}")
    rss_unit = 1 if sys.platform == "darwin" else 1024  # ru_maxrss: bytes on macOS, KiB on Linux
    return dict(wall=wall, cpu=usage.ru_utime + usage.ru_stime, rss_mib=usage.ru_maxrss * rss_unit / 2**20)


def terrio_columns(path, stats):
    """Statistic columns of a terrio output, keyed DATE_BAND_STAT with ISO dates."""
    from osgeo import ogr

    ds = ogr.Open(str(path))
    layer = ds.GetLayer(0)
    defn = layer.GetLayerDefn()
    names = [defn.GetFieldDefn(i).GetName() for i in range(defn.GetFieldCount())]
    out = {n: [] for n in names if n.split("_")[-1] in stats}
    features = list(layer)
    # FlatGeobuf rows are in spatial-index order; other tools keep the input order.
    # ponytail: assumes a field_id in input order (generated fields); --fields-file needs one.
    if "field_id" in names:
        features.sort(key=lambda f: f.GetField("field_id"))
    for f in features:
        for n in out:
            out[n].append(f.GetField(n))
    # 2024_07_01_red_mean -> 2024-07-01_red_mean
    return {re.sub(r"^(\d{4})_(\d{2})_(\d{2})_", r"\1-\2-\3_", n): v for n, v in out.items()}


def compare(reference, other, stats, rel=1e-6, abs_=1e-6):
    """Maximum absolute and relative differences of `other` from `reference` for shared keys."""
    worst_abs, worst_rel, compared = 0.0, 0.0, 0
    for key, values in other.items():
        if key not in reference:
            raise AssertionError(f"missing column {key}")
        for i, (a, b) in enumerate(zip(reference[key], values, strict=True)):
            if a is None or b is None:
                if (a is None) != (b is None) and not (key.endswith("_count") and (a or 0) == (b or 0)):
                    raise AssertionError(f"{key}[{i}]: null mismatch {a} vs {b}")
                continue
            d = abs(a - b)
            worst_abs = max(worst_abs, d)
            worst_rel = max(worst_rel, d / max(abs(a), 1e-300))
            compared += 1
    return dict(max_abs=worst_abs, max_rel=worst_rel, values=compared,
                equivalent=all(worst_abs <= abs_ or worst_rel <= rel for _ in [0]))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--data", type=Path, required=True)
    p.add_argument("--tile", default="15TVG")
    p.add_argument("--season", default="2023-04-01T00:00:00Z/2023-10-31T23:59:59Z")
    p.add_argument("--dates", default="10")
    p.add_argument("--repeat-dates", type=int, default=1,
                   help="list the cached dates this many times, each repeat a year later through its own "
                        "symlinked paths (simulates more dates; local files only)")
    p.add_argument("--fields", default="10000")
    p.add_argument("--aoi-km", type=float, help="generate the fields inside a centered square of this side")
    p.add_argument("--fields-file", type=Path)
    p.add_argument("--bands", help="comma-separated subset of the 10 bands")
    p.add_argument("--stats", default="mean,moments,distribution")
    p.add_argument("--method", choices=["center", "fractional"], default="center")
    p.add_argument("--tools", default="terrio,exactextract")
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--threads", type=int, default=16)
    p.add_argument("--terrio", default=str(HERE.parent / "target/release/terrio"))
    p.add_argument("--terrio-engine", default=str(HERE.parent / "target/release/examples/zonal_engine"))
    p.add_argument("--terrio-engine-baseline", help="another zonal_engine build to compare (tool terrio-engine-baseline)")
    p.add_argument("--terrio-main", default=os.environ.get("TERRIO_MAIN_BIN"))
    p.add_argument("--remote", action="store_true", help="terrio tools read the COG URLs instead of the local copies")
    p.add_argument("--gdal-python", default=sys.executable)
    p.add_argument("--json", type=Path)
    args = p.parse_args()
    if args.remote and args.repeat_dates > 1:
        p.error("--repeat-dates needs local files: repeated URLs would share one read")
    args.data.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, RAYON_NUM_THREADS=str(args.threads), OMP_NUM_THREADS=str(args.threads),
               GDAL_NUM_THREADS=str(args.threads), DASK_NUM_WORKERS=str(args.threads))
    tools = args.tools.split(",")
    results = []
    for dates in map(int, args.dates.split(",")):
        rows = download(args.data, items(args.data, args.tile, args.season, dates))
        if args.bands:
            rows = [r for r in rows if r[2] in args.bands.split(",")]
        # Repeat r is labelled r years later and read through its own symlink: terrio shares one
        # read among rows naming the same path, and repeats must cost what distinct dates cost.
        rows = [(repeat_path(args.data, path, r), f"{int(day[:4]) + r}{day[4:]}", band, url)
                for r in range(args.repeat_dates) for path, day, band, url in rows]
        for n in map(int, args.fields.split(",")):
            zones = args.fields_file or fields(args.data, n, rows[0][0], args.aoi_km)
            for set_name in args.stats.split(","):
                stats = STAT_SETS[set_name]
                case = dict(dates=dates * args.repeat_dates, distinct_dates=dates, fields=n, aoi_km=args.aoi_km,
                            stats=stats, method=args.method, rasters=len(rows), threads=args.threads,
                            remote=args.remote)
                log(f"case {case}")
                with tempfile.TemporaryDirectory(dir=args.data) as work:
                    work = Path(work)
                    inputs = work / "inputs.csv"
                    inputs.write_text("path,time,label\n" + "".join(f"{u if args.remote else p},{d},{b}\n" for p, d, b, u in rows))
                    commands = {}
                    stat_list = ",".join(stats)
                    extra = ["--fractional"] if args.method == "fractional" else []
                    if "terrio" in tools:
                        commands["terrio"] = ([args.terrio, "zonal-stats", zones, work / "terrio.fgb", "--inputs", inputs,
                                               "--stats", stat_list, *extra], work / "terrio.fgb")
                    if "terrio-engine" in tools:
                        # The engine alone: results discarded (crates/terrio/examples/zonal_engine.rs).
                        commands["terrio-engine"] = ([args.terrio_engine, zones, inputs, stat_list, args.method], work / "none")
                    if "terrio-engine-baseline" in tools and args.terrio_engine_baseline:
                        commands["terrio-engine-baseline"] = ([args.terrio_engine_baseline, zones, inputs, stat_list, args.method], work / "none")
                    if "terrio-main" in tools and args.terrio_main:
                        flags = [x for p, d, b, _ in rows for x in ("--raster", p, "--prefix", f"{d.replace('-', '_')}_{b}")]
                        commands["terrio-main"] = ([args.terrio_main, "zonal-stats", zones, work / "main.fgb", *flags,
                                                    "--stats", stat_list, *extra], work / "main.fgb")
                    if "exactextract" in tools:
                        spec = dict(zones=str(zones), rasters=[(str(p_), d, b) for p_, d, b, _ in rows],
                                    stats=stats, output=str(work / "ee.json"))
                        commands["exactextract"] = ([args.gdal_python, "-c", EXACTEXTRACT, json.dumps(spec)], work / "ee.json")
                    samples = {name: [] for name in commands}
                    for run in range(args.runs):
                        for name, (cmd, _) in commands.items():
                            try:
                                samples[name].append(timed(cmd, env))
                                log(f"  {name} run {run + 1}: {samples[name][-1]}")
                            except RuntimeError as error:
                                samples[name].append(dict(error=str(error)))
                                log(f"  {name} failed: {error}")
                    reference = terrio_columns(commands["terrio"][1], stats) if "terrio" in commands else None
                    for name, (cmd, output) in commands.items():
                        ok = [s for s in samples[name] if "error" not in s]
                        entry = dict(case, tool=name, runs=samples[name])
                        if ok:
                            for metric in ("wall", "cpu", "rss_mib"):
                                values = [s[metric] for s in ok]
                                entry[metric] = dict(median=statistics.median(values), min=min(values), max=max(values))
                        if reference and name != "terrio" and output.exists() and ok:
                            other = (terrio_columns(output, stats) if output.suffix == ".fgb"
                                     else json.loads(output.read_text()))
                            try:
                                entry["equivalence"] = compare(reference, other, stats,
                                                               rel=2e-7 if args.method == "fractional" else 1e-6)
                            except AssertionError as error:
                                entry["equivalence"] = dict(equivalent=False, detail=str(error))
                        results.append(entry)
    summary = dict(machine=platform.node(), cpu=platform.processor() or platform.machine(), cores=os.cpu_count(),
                   python=platform.python_version(), results=results)
    text = json.dumps(summary, indent=2)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(text)
    print(text)


if __name__ == "__main__":
    main()
