"""Validated command wall-time benchmarks against GDAL (CLI and Python bindings required).

Each tool is tuned independently over the requested CPU budgets. Selected settings are
measured again in alternating order, with a warm-up, fresh output directories, raw samples,
median and range. Only equivalent outputs receive a speed ratio. See docs/benchmarks.md.
"""

import argparse
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from functools import cache
from pathlib import Path

TERRIO = os.environ.get(
    "TERRIO_BIN",
    os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"),
)
USGS = "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif"


@cache
def gdal_kernel_version():
    """Match Terrio's existing warp policy to the GDAL used by this benchmark."""
    from osgeo import gdal

    version = int(gdal.VersionInfo("VERSION_NUM"))
    if 3120000 <= version < 3130000:
        return "3.12"
    if 3130000 <= version < 3140000:
        return "3.13"
    raise ValueError("benchmark warp comparisons support GDAL 3.12 and 3.13")


def terrio_warp(binary):
    """Use equal kernel semantics; exact output validation still decides equivalence."""
    return [binary, "warp", "--kernel-version", gdal_kernel_version()]


def terrio_dem(binary, processing):
    """Select the reference terrain arithmetic explicitly."""
    return [binary, "dem", processing, "--kernel-version", gdal_kernel_version()]


def terrio_cog(binary):
    """Build average overviews, recursive and floor-sized when the reference uses GDAL 3.13."""
    # The GDAL COG commands request average overviews, so Terrio must too; its cubic default matches GDAL's default.
    command = [binary, "convert", "--co", "RESAMPLING=AVERAGE"]
    if gdal_kernel_version() == "3.13":
        command += ["--co", "COG_OVERVIEW_POLICY=GDAL_3_13"]
    return command


def warp(threads):
    """Return ``gdalwarp`` with its multithreading on.

    Args:
        threads: The number of threads.

    Returns:
        The start of the command.
    """
    return ["gdalwarp", "-q", "-multi", "-wo", f"NUM_THREADS={threads}"]


def mosaic(tiles, tif):
    """Use GDAL's single-process materialized mosaic command."""
    return ["gdal", "raster", "mosaic", "-q", "--resolution", "average", *tiles, tif]


def cases(d, out, threads):
    """Return the benchmark cases.

    Args:
        d: The test data directory.
        out: A scratch directory for outputs.
        threads: The CPU budget for this tuning candidate.

    Returns:
        A list of ``(group, description, terrio_command, gdal_command)`` tuples.
    """
    p = lambda name: os.path.join(d, name)
    o = lambda name: os.path.join(out, name)
    t = TERRIO
    tiles = [p(f"USGS_1_{n}.tif") for n in ("n40w106", "n40w107", "n41w106", "n41w107")]
    return [
        ("Summaries", "Raster info: USGS 3DEP tile (3612×3612)", [t, "info", "--json", p("usgs_dem.tif")], ["gdalinfo", "-json", p("usgs_dem.tif")]),
        (
            # SRTM has no statistics stored in its metadata, so GDAL computes them too.
            "Summaries",
            "Raster statistics: SRTM 90 m (4269×2465)",
            [t, "info", "--json", "--stats", p("srtm90.tif")],
            ["gdalinfo", "-json", "-stats", p("srtm90.tif")],
        ),
        (
            "Summaries",
            "Vector info: US counties GeoPackage",
            [t, "info", "--json", p("UScounties.gpkg")],
            ["ogrinfo", "-json", "-so", "-al", p("UScounties.gpkg")],
        ),
        (
            "Raster conversion",
            "GeoTIFF → COG (Deflate): USGS 3DEP tile",
            [*terrio_cog(t), p("usgs_dem.tif"), o("a.tif"), "-f", "COG", "--compress", "DEFLATE"],
            [
                "gdal_translate",
                "-q",
                "-of",
                "COG",
                "-co",
                "COMPRESS=DEFLATE",
                "-co",
                "OVERVIEWS=IGNORE_EXISTING",
                "-co",
                "RESAMPLING=AVERAGE",
                p("usgs_dem.tif"),
                o("a.tif"),
            ],
        ),
        (
            "Raster conversion",
            "GeoTIFF → GeoTIFF (LZW, tiled): Landsat (4 bands)",
            [t, "convert", p("landsat.tif"), o("b.tif"), "--compress", "LZW", "--tile-size", "256"],
            ["gdal_translate", "-q", "-co", "COMPRESS=LZW", "-co", "TILED=YES", p("landsat.tif"), o("b.tif")],
        ),
        (
            "Raster conversion",
            "GeoTIFF → Zarr: SRTM 90 m",
            [t, "convert", p("srtm90.tif"), o("c.zarr")],
            ["gdal_translate", "-q", "-of", "Zarr", p("srtm90.tif"), o("c.zarr")],
        ),
        (
            "Raster operations",
            "Translate: window + Int16: USGS 3DEP tile",
            [t, "translate", p("usgs_dem.tif"), o("d.tif"), "--srcwin", "500", "500", "2000", "2000", "--ot", "Int16"],
            ["gdal_translate", "-q", "-srcwin", "500", "500", "2000", "2000", "-ot", "Int16", p("usgs_dem.tif"), o("d.tif")],
        ),
        (
            "Raster operations",
            "Warp to NAD83 / UTM 13N, nearest: USGS 3DEP tile",
            [*terrio_warp(t), p("usgs_dem.tif"), o("e.tif"), "--t-srs", "EPSG:26913"],
            [*warp(threads), "-t_srs", "EPSG:26913", p("usgs_dem.tif"), o("e.tif")],
        ),
        (
            "Raster operations",
            "Warp to NAD83 / UTM 13N, bilinear: USGS 3DEP tile",
            [*terrio_warp(t), p("usgs_dem.tif"), o("f.tif"), "--t-srs", "EPSG:26913", "-r", "bilinear"],
            [*warp(threads), "-t_srs", "EPSG:26913", "-r", "bilinear", p("usgs_dem.tif"), o("f.tif")],
        ),
        (
            "Raster operations",
            "Mosaic 4 USGS 3DEP tiles → GeoTIFF",
            [t, "mosaic", *tiles, o("g.tif")],
            mosaic(tiles, o("g.tif")),
        ),
        (
            "Raster operations",
            "Mosaic 4 USGS 3DEP tiles → VRT",
            [t, "mosaic", *tiles, o("h.vrt")],
            ["gdal", "raster", "mosaic", "--config", "VRT_VIRTUAL_OVERVIEWS", "NO", "-q", "--resolution", "average", "-f", "VRT", *tiles, o("h.vrt")],
        ),
        (
            "Vector conversion",
            "Shapefile → GeoPackage: 7,342 populated places",
            [t, "convert", p("ne_places/ne_10m_populated_places_simple.shp"), o("i.gpkg")],
            ["ogr2ogr", o("i.gpkg"), p("ne_places/ne_10m_populated_places_simple.shp")],
        ),
        (
            "Vector conversion",
            "GeoPackage → FlatGeobuf: US counties",
            [t, "convert", p("UScounties.gpkg"), o("j.fgb")],
            ["ogr2ogr", o("j.fgb"), p("UScounties.gpkg")],
        ),
        (
            "Vector conversion",
            "GeoPackage → GeoParquet: US counties",
            [t, "convert", p("UScounties.gpkg"), o("k.parquet")],
            ["ogr2ogr", o("k.parquet"), p("UScounties.gpkg")],
        ),
        (
            "Vector conversion",
            "GeoJSON → Shapefile: world countries (176 features)",
            [t, "convert", p("countries.geojson"), o("l.shp")],
            ["ogr2ogr", "-lco", "ENCODING=UTF-8", o("l.shp"), p("countries.geojson")],
        ),
        (
            "Vector conversion",
            "Filter + reproject: US counties → EPSG:5070",
            [t, "convert", p("UScounties.gpkg"), o("m.gpkg"), "--where", "STATE_FIPS = '47'", "--t-srs", "EPSG:5070"],
            ["ogr2ogr", "-where", "STATE_FIPS = '47'", "-t_srs", "EPSG:5070", o("m.gpkg"), p("UScounties.gpkg")],
        ),
        (
            "Raster ↔ vector",
            "Rasterize US counties at 0.01°",
            [t, "rasterize", p("UScounties.gpkg"), o("n.tif"), "--burn", "1", "--tr", "0.01", "0.01", "--ot", "Byte"],
            ["gdal_rasterize", "-q", "-burn", "1", "-tr", "0.01", "0.01", "-ot", "Byte", p("UScounties.gpkg"), o("n.tif")],
        ),
        (
            "Raster ↔ vector",
            "Polygonize NLCD land cover (966×731)",
            [t, "polygonize", p("nlcd_knoxville.tif"), o("q.gpkg")],
            ["gdal", "raster", "polygonize", "-q", p("nlcd_knoxville.tif"), o("q.gpkg")],
        ),
    ]


def large_cases(d, out, threads):
    """Return the cases on large files (see ``scripts/fetch_large_data.sh``).

    Args:
        d: The large data directory.
        out: A scratch directory for outputs.
        threads: The CPU budget for this tuning candidate.

    Returns:
        A list of ``(group, description, terrio_command, gdal_command)`` tuples.
    """
    p = lambda name: os.path.join(d, name)
    o = lambda name: os.path.join(out, name)
    t = TERRIO
    dem, s2, gpkg, pbf = p("dem16.tif"), p("s2_b04.tif"), p("colorado.gpkg"), p("colorado.osm.pbf")
    # The 16 tiles scripts/fetch_large_data.sh downloads, not other tiles the directory may hold.
    tiles = [p(f"USGS_1_n{lat}w{lon}.tif") for lat in range(39, 43) for lon in range(104, 108)]
    missing = [t for t in tiles if not os.path.exists(t)]
    if missing:
        sys.exit(f"missing large-data files (run scripts/fetch_large_data.sh): {', '.join(missing)}")
    r, v = "Large rasters", "Large vectors"
    return [
        (r, "Statistics: 14412² Float32 mosaic", [t, "info", "--json", "--stats", dem], ["gdalinfo", "-json", "-stats", dem]),
        (
            r,
            "GeoTIFF → COG (Deflate): the mosaic",
            [*terrio_cog(t), dem, o("a.tif"), "-f", "COG", "--compress", "DEFLATE"],
            ["gdal_translate", "-q", "-of", "COG", "-co", "COMPRESS=DEFLATE", "-co", "OVERVIEWS=IGNORE_EXISTING", "-co", "RESAMPLING=AVERAGE", dem, o("a.tif")],
        ),
        (
            r,
            "Window 2048² of the mosaic",
            [t, "translate", "--srcwin", "6000", "6000", "2048", "2048", dem, o("b.tif")],
            ["gdal_translate", "-q", "-srcwin", "6000", "6000", "2048", "2048", dem, o("b.tif")],
        ),
        (
            r,
            "Warp the mosaic to NAD83 / UTM 13N, bilinear",
            [*terrio_warp(t), "--t-srs", "EPSG:26913", "-r", "bilinear", dem, o("c.tif")],
            [*warp(threads), "-t_srs", "EPSG:26913", "-r", "bilinear", dem, o("c.tif")],
        ),
        (r, "Mosaic 16 USGS 3DEP tiles → GeoTIFF", [t, "mosaic", *tiles, o("d.tif")], mosaic(tiles, o("d.tif"))),
        (r, "Hillshade of the mosaic", [*terrio_dem(t, "hillshade"), dem, o("e.tif")], ["gdaldem", "hillshade", "-q", dem, o("e.tif")]),
        (r, "Slope of the mosaic", [*terrio_dem(t, "slope"), dem, o("f.tif")], ["gdaldem", "slope", "-q", dem, o("f.tif")]),
        (
            r,
            "Sentinel-2 band: JPEG 2000 → GeoTIFF",
            [t, "convert", p("s2_l1c_B04.jp2"), o("g.tif")],
            ["gdal_translate", "-q", p("s2_l1c_B04.jp2"), o("g.tif")],
        ),
        (r, "Statistics: Sentinel-2 band (15-bit samples)", [t, "info", "--json", "--stats", s2], ["gdalinfo", "-json", "-stats", s2]),
        (v, "Info: Colorado OpenStreetMap GeoPackage (6.2 M features)", [t, "info", "--json", gpkg], ["ogrinfo", "-json", "-so", "-al", gpkg]),
        (
            v,
            "Lines (2.0 M) → FlatGeobuf",
            [t, "convert", gpkg, o("h.fgb"), "-l", "lines"],
            ["ogr2ogr", "-f", "FlatGeobuf", o("h.fgb"), gpkg, "lines"],
        ),
        (
            v,
            "Polygons (2.3 M) → GeoParquet",
            [t, "convert", gpkg, o("i.parquet"), "-l", "multipolygons"],
            ["ogr2ogr", "-f", "Parquet", o("i.parquet"), gpkg, "multipolygons"],
        ),
        (v, "Lines → GeoJSON", [t, "convert", gpkg, o("j.geojson"), "-l", "lines"], ["ogr2ogr", "-f", "GeoJSON", o("j.geojson"), gpkg, "lines"]),
        (
            v,
            "Lines → Shapefile",
            [t, "convert", gpkg, o("k.shp"), "-l", "lines"],
            ["ogr2ogr", "-f", "ESRI Shapefile", o("k.shp"), gpkg, "lines"],
        ),
        (
            v,
            "Filter + reproject lines → GeoPackage",
            [t, "convert", gpkg, o("l.gpkg"), "-l", "lines", "--where", "highway IS NOT NULL", "--t-srs", "EPSG:3857"],
            ["ogr2ogr", "-f", "GPKG", o("l.gpkg"), gpkg, "lines", "-where", "highway IS NOT NULL", "-t_srs", "EPSG:3857"],
        ),
        (v, "Polygons → GeoPackage", [t, "convert", gpkg, o("m.gpkg"), "-l", "multipolygons"], ["ogr2ogr", "-f", "GPKG", o("m.gpkg"), gpkg, "multipolygons"]),
        (v, "GeoPackage (5 layers) → GeoPackage", [t, "convert", gpkg, o("n.gpkg")], ["ogr2ogr", "-f", "GPKG", o("n.gpkg"), gpkg]),
        (v, "OpenStreetMap PBF (Colorado) → GeoPackage", [t, "convert", pbf, o("p.gpkg")], ["ogr2ogr", "-f", "GPKG", o("p.gpkg"), pbf]),
    ]


def remote_cases():
    """Return cases reading a remote Cloud Optimized GeoTIFF.

    Returns:
        A list of ``(group, description, terrio_command, gdal_command)`` tuples.
    """
    return [
        ("Remote data", "Info of a remote COG (USGS 3DEP on S3)", [TERRIO, "info", "--json", USGS], ["gdalinfo", "-json", "/vsicurl/" + USGS]),
    ]


def clean(out):
    """Empty the output directory.

    Args:
        out: The directory.
    """
    for name in os.listdir(out):
        path = os.path.join(out, name)
        shutil.rmtree(path) if os.path.isdir(path) else os.remove(path)


def busy_cpu_seconds():
    """Linux machine CPU time, excluding idle/iowait and double-counted guest time."""
    if not Path("/proc/stat").exists():
        return None
    counters = [int(x) for x in Path("/proc/stat").read_text().splitlines()[0].split()[1:]]
    return sum(counters[i] for i in (0, 1, 2, 5, 6, 7)) / os.sysconf("SC_CLK_TCK")


def run_once(cmd, env, stdout=None, metrics=None):
    """Measure one fresh process, including startup; capture output outside Python memory.

    Linux CPU affinity bounds *all* child threads, including terrio's non-Rayon workers.
    On other platforms the environment variables are pool hints, not CPU limits.
    """
    limit = int(env.get("TERRIO_BENCH_CPU_LIMIT", "1"))
    if hasattr(os, "sched_getaffinity"):
        cpus = sorted(os.sched_getaffinity(0))[:limit]
        cmd = ["taskset", "--cpu-list", ",".join(map(str, cpus)), *cmd]
    with tempfile.TemporaryFile() as err:
        busy_start = busy_cpu_seconds()
        start = time.perf_counter()
        p = subprocess.Popen(
            cmd,
            stdout=stdout if stdout is not None else subprocess.DEVNULL,
            stderr=err,
            env=env,
        )
        if hasattr(os, "wait4"):
            _, status, usage = os.wait4(p.pid, 0)
            elapsed = time.perf_counter() - start
            code = os.waitstatus_to_exitcode(status)
            p.returncode = code
            peak = usage.ru_maxrss / (1024 * 1024 if platform.system() == "Darwin" else 1024)
        else:
            code = p.wait()
            elapsed, peak = time.perf_counter() - start, None
        busy_end = busy_cpu_seconds()
        if metrics is not None and busy_start is not None and hasattr(os, "wait4"):
            metrics["background_cpus"] = max(0.0, (busy_end - busy_start - usage.ru_utime - usage.ru_stime) / elapsed)
        err.seek(0)
        return elapsed, peak, code, err.read().decode(errors="replace")


def positive_int(value):
    """argparse type for counts that cannot be zero."""
    n = int(value)
    if n < 1:
        raise ValueError("must be at least 1")
    return n


def apply_background_gate(result, threshold=1.0):
    """Average CPU estimates over paired series, avoiding tick noise on short commands.

    /proc/stat has clock-tick granularity. A single 15 ms command cannot resolve
    sub-core background use reliably; aggregate its CPU seconds with its paired tool.
    This analysis can also be reproduced from saved raw samples without retiming.
    """

    def average(background, samples):
        pairs = [(cpu, seconds) for values, durations in zip(background, samples) for cpu, seconds in zip(values, durations)]
        duration = sum(seconds for _, seconds in pairs)
        return sum(cpu * seconds for cpu, seconds in pairs) / duration if duration else None

    tuning = [average(t["background_cpus"], t["samples"]) for t in result["tuning"]]
    measured = average(result["background_cpus"], result["samples"])
    result["background_series_averages"] = {"tuning": tuning, "measurement": measured}
    noisy = any(value > threshold for value in [*tuning, measured] if value is not None)
    result["measurement_issues"] = [f"background CPU use exceeded {threshold:g} CPUs; rerun on an idle machine"] if noisy else []
    a, b = result["medians"]
    result["ratio"] = None if result["validation"] or noisy else b / a
    return result


def measure_pair(commands, root, budgets, runs, tune_runs, env, validate):
    """Tune separately, then measure interleaved, retaining final outputs for validation.

    ``commands(out, budget)`` returns both commands with separate, disposable outputs.
    Validation is deliberately outside the timed region. Failures never receive ratios.
    """
    dirs = [os.path.join(root, name) for name in ("terrio", "gdal")]
    for d in dirs:
        os.makedirs(d, exist_ok=True)
    errors = []
    background = [[], []]

    def execute(tool, budget):
        clean(dirs[tool])
        command = commands(dirs[tool], budget)[tool]
        process_env = dict(
            env,
            GDAL_NUM_THREADS=str(budget),
            RAYON_NUM_THREADS=str(budget),
            TERRIO_BENCH_CPU_LIMIT=str(budget),
        )
        with open(os.path.join(dirs[tool], "stdout.json"), "wb") as stdout:
            metrics = {}
            elapsed, peak, code, err = run_once(command, process_env, stdout, metrics)
        if code:
            failure = f"{'terrio' if tool == 0 else 'GDAL'} exit {code}: {err[-1000:]}"
            if failure not in errors:
                errors.append(failure)
        return elapsed, peak, metrics.get("background_cpus")

    tuning = []
    for budget in budgets:
        samples, background_tuning = [[], []], [[], []]
        for i in range(tune_runs + 1):
            for tool in [0, 1] if i % 2 == 0 else [1, 0]:
                elapsed, _, busy = execute(tool, budget)
                if i:
                    samples[tool].append(elapsed)
                    if busy is not None:
                        background_tuning[tool].append(busy)
        tuning.append({"budget": budget, "samples": samples, "background_cpus": background_tuning})
    selected = [min(tuning, key=lambda t: statistics.median(t["samples"][tool]))["budget"] for tool in (0, 1)]
    samples = [[], []]
    for tool in (0, 1):
        execute(tool, selected[tool])  # independent warm-up at selected settings
    for i in range(runs):
        for tool in [0, 1] if i % 2 == 0 else [1, 0]:
            elapsed, _, busy = execute(tool, selected[tool])
            samples[tool].append(elapsed)
            if busy is not None:
                background[tool].append(busy)
    try:
        differences = errors or validate(*dirs)
    except Exception as exc:  # noqa: BLE001 - validation errors must suppress scoring
        differences = [f"validation failed: {exc}"]
    medians = [statistics.median(s) for s in samples]
    result = {
        "budgets": selected,
        "tuning": tuning,
        "samples": samples,
        "medians": medians,
        "validation": differences,
        "timing_notes": ["wide timing range"] if any(max(s) > 2 * statistics.median(s) for s in samples) else [],
        "background_cpus": background,
    }
    return apply_background_gate(result, float(env.get("TERRIO_BENCH_MAX_BACKGROUND_CPUS", "1")))


def fmt(seconds):
    """Format a duration.

    Args:
        seconds: The duration, or None.

    Returns:
        The duration in seconds or milliseconds.
    """
    if seconds is None:
        return "failed"
    return f"{seconds * 1000:.0f} ms" if seconds < 1 else f"{seconds:.2f} s"


def gdal_version(env):
    """Return the GDAL version string.

    Args:
        env: The environment.

    Returns:
        The output of ``gdalinfo --version``.
    """
    return subprocess.run(["gdalinfo", "--version"], check=True, capture_output=True, text=True, env=env).stdout.strip()


def main():
    """Run validated benchmarks and save Markdown plus reproducible raw measurements."""
    from benchmark_validation import validate_case
    from osgeo import gdal, osr

    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("data", nargs="?", help="test data directory")
    ap.add_argument("--runs", type=positive_int, default=7)
    ap.add_argument("--remote", action="store_true")
    ap.add_argument("--large", help="large data directory")
    ap.add_argument("--large-runs", type=positive_int, default=5)
    ap.add_argument("--tune-runs", type=positive_int, default=3)
    ap.add_argument(
        "--threads",
        type=positive_int,
        help="maximum CPU budget (default: available CPUs)",
    )
    ap.add_argument(
        "--thread-counts",
        help="comma-separated tuning budgets (default: 1,2,4,8,16,max)",
    )
    ap.add_argument("--only", help="case description substring, for focused reruns")
    ap.add_argument(
        "--max-background-cpus",
        type=float,
        default=1.0,
        help="withhold ratios when series-average competing CPU use exceeds this value (Linux)",
    )
    ap.add_argument("--out", help="Markdown output; raw samples go to FILE.json")
    args = ap.parse_args()
    if hasattr(os, "sched_getaffinity") and not shutil.which("taskset"):
        ap.error("Linux CPU affinity requires taskset; install util-linux and ensure taskset is on PATH")
    if not 0 <= args.max_background_cpus < float("inf"):
        ap.error("--max-background-cpus must be finite and nonnegative")
    available = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count() or 1
    maximum = args.threads or available
    if maximum > available:
        ap.error(f"only {available} CPUs available")
    try:
        budgets = (
            sorted({positive_int(x) for x in args.thread_counts.split(",")})
            if args.thread_counts
            else sorted({n for n in (1, 2, 4, 8, 16, maximum) if n <= maximum})
        )
    except ValueError:
        ap.error("--thread-counts must contain positive integers")
    if any(n > maximum for n in budgets):
        ap.error("tuning budgets must not exceed --threads / available CPUs")
    if not args.data and not args.large:
        ap.error("give DATA_DIR, --large DIR or both")
    env = dict(
        os.environ,
        GDAL_PAM_ENABLED="NO",
        GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR",
        PROJ_NETWORK="OFF",
        TERRIO_GRID_DOWNLOAD="NO",
        TERRIO_NETWORK="OFF",
        TERRIO_BENCH_MAX_BACKGROUND_CPUS=str(args.max_background_cpus),
    )
    # The validator runs in this process and must not create PAM statistics caches either.
    gdal.SetConfigOption("GDAL_PAM_ENABLED", "NO")
    gdal.SetConfigOption("GDAL_NUM_THREADS", "1")
    osr.UseExceptions()
    try:
        osr.SpatialReference().ImportFromEPSG(4326)
    except RuntimeError:
        ap.error("PROJ database unavailable: activate the GDAL environment and set PROJ_DATA to its share/proj directory")
    version = gdal_version(env)
    if gdal.VersionInfo("RELEASE_NAME") not in version:
        ap.error("GDAL CLI and Python binding versions must match")
    cpu_model = platform.processor() or platform.machine()
    if Path("/proc/cpuinfo").exists():
        cpu_model = next(
            (line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")),
            cpu_model,
        )
    machine = {
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "command": sys.argv,
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True, cwd=Path(__file__).resolve().parent).strip(),
        "platform": platform.platform(),
        "processor": cpu_model,
        "available_cpus": available,
        "load_average": os.getloadavg() if hasattr(os, "getloadavg") else None,
        "gdal": version,
        "proj_search_paths": osr.GetPROJSearchPaths(),
        "terrio": subprocess.check_output([TERRIO, "--version"], text=True).strip(),
        "budgets": budgets,
        "affinity": hasattr(os, "sched_getaffinity"),
        "runs": args.runs,
        "large_runs": args.large_runs,
        "tune_runs": args.tune_runs,
        "max_background_cpus": args.max_background_cpus,
    }
    lines = [
        f"Machine: {machine['processor']}, {available} available CPUs, {machine['platform']}; {version}; {machine['terrio']}.",
        "",
        (
            f"CPU budgets tested independently: {budgets}; {args.tune_runs} tuning samples per budget after warm-up. "
            f"Fresh measurement: {args.runs} samples ({args.large_runs} for large files), alternating tool order. "
            "Times are median [min–max], including process startup; validation is outside timing. "
            "Ratios require passing output validation. CPU columns show terrio / GDAL selected budgets."
        ),
        "",
    ]
    records = []
    with tempfile.TemporaryDirectory(prefix="terrio-bench-") as root:

        @cache
        def make_cases(out, budget):
            result = [(c, args.runs) for c in (cases(os.path.abspath(args.data), out, budget) if args.data else [])]
            result += [(c, args.runs) for c in (remote_cases() if args.remote else [])]
            result += [(c, args.large_runs) for c in (large_cases(os.path.abspath(args.large), out, budget) if args.large else [])]
            return result

        all_cases = make_cases(root, budgets[0])
        group = None
        for index, ((g, desc, tcmd, gcmd), runs) in enumerate(all_cases):
            if args.only and args.only.lower() not in desc.lower():
                continue
            if g != group:
                group = g
                lines += [
                    "",
                    f"### {g}",
                    "",
                    "| Task | terrio median [range] | GDAL median [range] | CPUs T / G | terrio vs GDAL | Validation |",
                    "|---|---|---|---|---|---|",
                ]
            print(f"Benchmarking: {desc}", flush=True)

            def commands(out, budget, case_index=index):
                c, _ = make_cases(out, budget)[case_index]
                return c[2:]

            def validate(a, b):
                return validate_case(commands(a, budgets[0])[0], commands(b, budgets[0])[1], a, b)

            result = measure_pair(commands, root, budgets, runs, args.tune_runs, env, validate)
            ratio = result["ratio"]
            score = "—" if ratio is None else (f"{ratio:.2f}× faster" if ratio >= 1 else f"{1 / ratio:.2f}× slower")
            durations = [f"{fmt(statistics.median(s))} [{fmt(min(s))}–{fmt(max(s))}]" for s in result["samples"]]
            for tool, name in enumerate(("terrio", "GDAL")):
                if any(difference.startswith(name + " exit") for difference in result["validation"]):
                    durations[tool] = "failed"
            status = "; ".join(result["validation"] + result["measurement_issues"]) or "pass"
            if result["timing_notes"]:
                status += "; " + "; ".join(result["timing_notes"])
            status = status.replace("|", "/").replace("\n", " ")
            if len(status) > 600:
                status = status[:600] + "… (full details in JSON)"
            lines.append(f"| {desc} | {durations[0]} | {durations[1]} | {result['budgets'][0]} / {result['budgets'][1]} | {score} | {status} |")
            print(lines[-1], flush=True)
            records.append(
                dict(
                    group=g,
                    task=desc,
                    commands=[
                        commands(os.path.join(root, "terrio"), result["budgets"][0])[0],
                        commands(os.path.join(root, "gdal"), result["budgets"][1])[1],
                    ],
                    load_average_end=os.getloadavg() if hasattr(os, "getloadavg") else None,
                    **result,
                )
            )
            # Checkpoint complete rows so interrupted large runs preserve their evidence.
            if args.out:
                Path(args.out).write_text("\n".join(lines) + "\n")
                Path(args.out + ".json").write_text(json.dumps({"machine": machine, "cases": records}, indent=2) + "\n")
    if not records:
        ap.error("no cases matched")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
