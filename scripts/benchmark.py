"""Time terrio's commands against their GDAL equivalents on real-world files.

Each case runs the terrio command and the GDAL command doing the same work, in a fresh process,
once to warm the file cache and then ``--runs`` times; the median wall time is reported. Results
print as a Markdown table per group.

Usage: python scripts/benchmark.py DATA_DIR [--runs N] [--remote] [--out FILE]

Needs the GDAL command-line tools on PATH and a release build of terrio
(``target/release/terrio``, or the path in ``TERRIO_BIN``).
"""

import argparse
import os
import platform
import shutil
import statistics
import subprocess
import tempfile
import time

TERRIO = os.environ.get("TERRIO_BIN", os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
USGS = "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif"


def cases(d, out):
    """Return the benchmark cases.

    Args:
        d: The test data directory.
        out: A scratch directory for outputs.

    Returns:
        A list of ``(group, description, terrio_command, gdal_command)`` tuples.
    """
    p = lambda name: os.path.join(d, name)  # noqa: E731 - a short local helper
    o = lambda name: os.path.join(out, name)  # noqa: E731
    t = TERRIO
    tiles = [p(f"USGS_1_{n}.tif") for n in ("n40w106", "n40w107", "n41w106", "n41w107")]
    return [
        ("Summaries", "Raster info: USGS 3DEP tile (3612×3612)", [t, "info", p("usgs_dem.tif")], ["gdalinfo", p("usgs_dem.tif")]),
        (
            # SRTM has no statistics stored in its metadata, so GDAL computes them too.
            "Summaries",
            "Raster statistics: SRTM 90 m (4269×2465)",
            [t, "info", "--stats", p("srtm90.tif")],
            ["gdalinfo", "-stats", p("srtm90.tif")],
        ),
        (
            "Summaries",
            "Vector info: US counties GeoPackage",
            [t, "info", p("UScounties.gpkg")],
            ["ogrinfo", "-so", "-al", p("UScounties.gpkg")],
        ),
        (
            "Raster conversion",
            "GeoTIFF → COG (Deflate): USGS 3DEP tile",
            [t, "convert", p("usgs_dem.tif"), o("a.tif"), "-f", "COG", "--compress", "DEFLATE"],
            ["gdal_translate", "-q", "-of", "COG", "-co", "COMPRESS=DEFLATE", p("usgs_dem.tif"), o("a.tif")],
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
            "Warp to UTM 13N, nearest: USGS 3DEP tile",
            [t, "warp", p("usgs_dem.tif"), o("e.tif"), "--t-srs", "EPSG:32613"],
            ["gdalwarp", "-q", "-t_srs", "EPSG:32613", p("usgs_dem.tif"), o("e.tif")],
        ),
        (
            "Raster operations",
            "Warp to UTM 13N, bilinear: USGS 3DEP tile",
            [t, "warp", p("usgs_dem.tif"), o("f.tif"), "--t-srs", "EPSG:32613", "-r", "bilinear"],
            ["gdalwarp", "-q", "-t_srs", "EPSG:32613", "-r", "bilinear", p("usgs_dem.tif"), o("f.tif")],
        ),
        (
            "Raster operations",
            "Mosaic 4 USGS 3DEP tiles → GeoTIFF",
            [t, "mosaic", *tiles, o("g.tif")],
            ["gdalwarp", "-q", *tiles, o("g.tif")],
        ),
        (
            "Raster operations",
            "Mosaic 4 USGS 3DEP tiles → VRT",
            [t, "mosaic", *tiles, o("h.vrt")],
            ["gdalbuildvrt", "-q", o("h.vrt"), *tiles],
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
            "GeoJSON → Shapefile: world cables",
            [t, "convert", p("cables.geojson"), o("l.shp")],
            ["ogr2ogr", o("l.shp"), p("cables.geojson")],
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
            ["gdal_polygonize.py", "-q", p("nlcd_knoxville.tif"), o("q.gpkg")],
        ),
    ]


def remote_cases():
    """Return cases reading a remote Cloud Optimized GeoTIFF.

    Returns:
        A list of ``(group, description, terrio_command, gdal_command)`` tuples.
    """
    return [
        ("Remote data", "Info of a remote COG (USGS 3DEP on S3)", [TERRIO, "info", USGS], ["gdalinfo", "/vsicurl/" + USGS]),
    ]


def clean(out):
    """Empty the output directory.

    Args:
        out: The directory.
    """
    for name in os.listdir(out):
        path = os.path.join(out, name)
        shutil.rmtree(path) if os.path.isdir(path) else os.remove(path)


def run(cmd, out, runs, env):
    """Time a command.

    Args:
        cmd: The command and its arguments.
        out: The output directory, emptied before each run.
        runs: The number of timed runs (after one warm-up run).
        env: The environment.

    Returns:
        The median wall time in seconds, or None when the command fails.
    """
    times = []
    for i in range(runs + 1):
        clean(out)
        start = time.perf_counter()
        r = subprocess.run(cmd, capture_output=True, env=env)
        elapsed = time.perf_counter() - start
        if r.returncode != 0:
            print(f"  failed: {' '.join(cmd)}\n  {r.stderr.decode(errors='replace')[:300]}")
            return None
        if i:
            times.append(elapsed)
    return statistics.median(times)


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
    return subprocess.run(["gdalinfo", "--version"], capture_output=True, text=True, env=env).stdout.strip()


def main():
    """Run the benchmark and print Markdown tables."""
    ap = argparse.ArgumentParser()
    ap.add_argument("data")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--remote", action="store_true", help="include remote reads")
    ap.add_argument("--out", help="also write the tables to this file")
    args = ap.parse_args()
    # No .aux.xml statistics caches: every run computes from the pixels. No directory listing when
    # opening remote files, as recommended for cloud data.
    env = dict(os.environ, GDAL_PAM_ENABLED="NO", GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR")
    out = tempfile.mkdtemp(prefix="terrio-bench-")
    all_cases = cases(os.path.abspath(args.data), out) + (remote_cases() if args.remote else [])
    lines = [
        f"Machine: {platform.processor() or platform.machine()}, {os.cpu_count()} threads, {platform.system()}. "
        f"{gdal_version(env)}; terrio {subprocess.run([TERRIO, '--version'], capture_output=True, text=True).stdout.split()[-1]}. "
        f"Median of {args.runs} runs after a warm-up run; wall time of the whole command.",
        "",
    ]
    group = None
    for g, desc, terrio_cmd, gdal_cmd in all_cases:
        if g != group:
            group = g
            lines += ["", f"### {g}", "", "| Task | terrio | GDAL | terrio vs GDAL |", "|---|---|---|---|"]
        a = run(terrio_cmd, out, args.runs, env)
        b = run(gdal_cmd, out, args.runs, env)
        ratio = f"{b / a:.1f}× faster" if a and b and b >= a else (f"{a / b:.1f}× slower" if a and b else "")
        lines.append(f"| {desc} | {fmt(a)} | {fmt(b)} | {ratio} |")
        print(lines[-1], flush=True)
    shutil.rmtree(out, ignore_errors=True)
    text = "\n".join(lines) + "\n"
    print(text)
    if args.out:
        with open(args.out, "w") as f:
            f.write(text)


if __name__ == "__main__":
    main()
