"""Run every comparison of terrio with GDAL (and laspy and PDAL) and write a conformance report.

This is the single entry point of terrio's conformance suite. It runs the comparison scripts of
this directory, each on the real-world files of ``scripts/fetch_test_data.sh`` (or on synthetic
rasters for the driver round trips), parses what they print, and writes a Markdown report: for
each suite and each format or operation, how many cases were compared, how many came out
identical to the reference, how many within the suite's tolerance, and how many differ, with the
details of every case that is not identical.

Suites:

    summaries         ``terrio info`` and the pixels of every file vs GDAL (compare_with_gdal.py)
    subdatasets       netCDF groups and HDF5 datasets, one subdataset at a time (compare_with_gdal.py)
    histograms        ``terrio info --hist`` vs ``gdalinfo -hist`` (compare_hist_with_gdal.py)
    vector-roundtrip  vector files converted by terrio, read back by GDAL (compare_with_gdal.py)
    raster-roundtrip  raster drivers, GDAL writing and terrio reading and back (raster_roundtrip.py)
    windows           windowed and overview reads of GeoTIFFs and JPEG 2000 (compare_windows_with_gdal.py)
    ops               translate, warp, mosaic vs GDAL's tools (compare_ops_with_gdal.py)
    catalogs          indexed real DEM mosaics and HTTP window I/O vs GDAL GTI
                      (compare_catalogs_with_gdal.py)
    vrt               serialized warped/derived/kernel VRTs, masks and lazy windows
    overviews         source overview selection and HTTP COG bytes vs gdalwarp -ovr
    dem-calc          dem, calc, proximity, sieve vs GDAL's tools (compare_dem_calc_with_gdal.py)
    vector-ops        ogr2ogr options, rasterize, polygonize (compare_vector_ops_with_gdal.py)
    analysis-output   zonal/point-sampling output reprojection and decimal rounding
    footprint         raster footprints, source/custom fields and singleparts vs GDAL
    topology          buffer, polygon overlay/dissolve, validity, repair and cleanup vs GEOS
    vector-geom       simplify, segmentize, hulls, swap-xy, set-geom-type, combine and sort vs
                      gdal vector and GEOS (compare_vector_geom_with_gdal.py)
    arrow             Arrow and GeoArrow export and import (compare_arrow_with_gdal.py)
    gpkg              GeoPackage updates and R-tree filters (compare_gpkg_with_gdal.py)
    streams           streamed vector conversions vs whole ones and ogr2ogr
                      (compare_streams_with_gdal.py)
    tiles             PMTiles, MBTiles and MVT read and written vs GDAL (compare_tiles_with_gdal.py)
    grib              GRIB1 and GRIB2 files band by band vs GDAL's GRIB driver (compare_grib_with_gdal.py)
    pointclouds       LAS, LAZ and COPC vs laspy and PDAL (compare_pointclouds.py)
    crs               every EPSG CRS's transformations vs GDAL's (tests/epsg_accuracy.rs)
    crs-grids         the same with datum shift grids, then grids, geoid models and coordinate
                      epochs (tests/epsg_accuracy_grids.rs, compare_crs_grids_with_gdal.py)

Usage:
    python scripts/conformance.py ../terrio-data [--output docs/conformance.md] [--json FILE]
        [--only SUITE,...] [--skip SUITE,...] [--terrio target/release/terrio] [--jobs N]
        [--remote] [--strict]

Needs a release build of the command-line tool (``cargo build --release``), GDAL's command-line
tools and Python bindings (``osgeo``) with NumPy; the point cloud suite needs the terrio Python
package, laspy (with lazrs) and PDAL; the Arrow suite pyarrow, GeoPandas and Shapely; the CRS
suite needs cargo. Suites whose requirements are missing are reported as skipped. Exits non-zero
when a suite cannot run, and with ``--strict`` when any case differs.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import datetime
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections import OrderedDict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"

IDENTICAL = "identical"
TOLERANCE = "within tolerance"
DIFFERS = "differs"


@dataclasses.dataclass
class Case:
    """One compared case.

    Attributes:
        group: The format or operation the case belongs to, as the report groups them.
        name: The case's name.
        status: ``identical``, ``within tolerance`` or ``differs``.
        detail: What differs, or how close the case is, when it is not identical.
    """

    group: str
    name: str
    status: str
    detail: str = ""


@dataclasses.dataclass
class Suite:
    """A comparison suite and how it went.

    Attributes:
        key: The suite's command-line name.
        title: The suite's title in the report.
        reference: What terrio is compared with.
        criteria: How the cases are judged, for the report.
        cases: The compared cases.
        error: Why the suite could not run (or its last lines of output when it crashed).
        skipped: Why the suite was skipped, when it was.
        seconds: How long it ran.
    """

    key: str
    title: str
    reference: str
    criteria: str
    cases: list = dataclasses.field(default_factory=list)
    error: str = ""
    skipped: str = ""
    seconds: float = 0.0


def run(cmd, env=None, timeout=7200):
    """Runs a command and returns its exit code and output.

    Args:
        cmd: The command line.
        env: Extra environment variables.
        timeout: Seconds before the command is abandoned.

    Returns:
        A ``(returncode, stdout, stderr)`` tuple.
    """
    full_env = dict(os.environ)
    full_env.update(env or {})
    p = subprocess.run(cmd, check=False, capture_output=True, text=True, env=full_env, timeout=timeout, cwd=ROOT)
    return p.returncode, p.stdout, p.stderr


def tail(text, n=15):
    """The last lines of some output.

    Args:
        text: The output.
        n: How many lines to keep.

    Returns:
        The last ``n`` non-empty lines, joined.
    """
    lines = [line for line in text.splitlines() if line.strip() and not line.startswith("Warning 1:")]
    return "\n".join(lines[-n:])


def missing_modules(*names):
    """The Python modules among ``names`` that cannot be imported.

    Args:
        *names: Module names.

    Returns:
        The missing ones.
    """
    return [n for n in names if importlib.util.find_spec(n) is None]


def format_names(terrio):
    """Maps file extensions to terrio's driver names, from ``terrio formats --json``.

    Args:
        terrio: The terrio executable.

    Returns:
        A dict from lowercase extensions (without the dot) to driver names.
    """
    code, out, _ = run([terrio, "formats", "--json"])
    names = {}
    if code == 0:
        for d in json.loads(out):
            for ext in d["extensions"]:
                names.setdefault(ext, d["name"])
    names.update({"tif": "GTiff", "tiff": "GTiff", "zip": "zip archives", "pbf": "OSM", "img": "HFA", "grd": "GSBG"})
    return names


def group_of_file(name, names):
    """The format a test file belongs to, by its extension.

    Args:
        name: The file name.
        names: Extension-to-driver names, from ``format_names``.

    Returns:
        The driver name.
    """
    ext = name.rsplit(".", 1)[-1].lower() if "." in name else ""
    return names.get(ext, ext or "other")


# Suites: each runs its script and turns its output into cases.


def suite_summaries(ctx):
    """``terrio info`` and the pixels of every file of the test data, against GDAL."""
    s = Suite(
        "summaries",
        "Dataset summaries and pixels",
        "GDAL (`gdalinfo`, `ogrinfo`, Python bindings)",
        "Every raster and vector file of the test data: size, type, CRS, geotransform, nodata, band "
        "statistics, layers, fields, feature counts and extents as GDAL reports them (numbers within "
        "1e-6 relative, statistics within 1e-4), and every raster pixel exactly.",
    )
    _code, out, err = run([sys.executable, str(SCRIPTS / "compare_with_gdal.py"), "--pixels", ctx.data], {"TERRIO_BIN": ctx.terrio})
    for line in out.splitlines():
        m = re.match(r"^(OK|DIFF|FAIL)\s+([^:]+?)(?::\s*(.*))?$", line)
        if m:
            status = IDENTICAL if m[1] == "OK" else DIFFERS
            s.cases.append(Case(group_of_file(m[2], ctx.names), m[2], status, m[3] or ""))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_subdatasets(ctx):
    """netCDF groups and HDF5 datasets, subdataset by subdataset, against GDAL."""
    s = Suite(
        "subdatasets",
        "NetCDF and HDF5 subdatasets",
        "GDAL's netCDF and HDF5 drivers",
        "Every netCDF and HDF5 file of the test data (netCDF-4 groups, HDF5 products, GDAL's "
        "HDF-EOS5, complex, COSMO-SkyMed and netCDF geolocation test files, NASA HDF-EOS5 grids and "
        "swaths when fetched): the subdataset list (names and descriptions) and, for HDF5, the "
        "metadata as GDAL reports them, then each subdataset's summary (geolocation arrays "
        "included) and every pixel.",
    )
    out, err = "", ""
    for sub in ("", "gdal_hdf5", "gdal_netcdf", "hdfeos5"):
        d = os.path.join(ctx.data, sub)
        if not os.path.isdir(d):
            continue
        _code, o, e = run([sys.executable, str(SCRIPTS / "compare_with_gdal.py"), "--subdatasets", d], {"TERRIO_BIN": ctx.terrio})
        out += o
        err += e
        for line in o.splitlines():
            m = re.match(r"^(OK|DIFF)\s+(\S+) (.+?)(?::\s+(.*))?$", line)
            if m:
                name = m[2].lower()
                group = "HDF-EOS5" if sub == "hdfeos5" else "HDF5" if name.endswith((".h5", ".he5")) else "netCDF"
                label = f"{sub}/{m[2]}" if sub else m[2]
                s.cases.append(Case(group, f"{label} {m[3]}", IDENTICAL if m[1] == "OK" else DIFFERS, m[4] or ""))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_histograms(ctx):
    """``terrio info --hist`` against ``gdalinfo -hist``."""
    s = Suite(
        "histograms",
        "Band histograms",
        "`gdalinfo -hist`, `gdalinfo -json -hist`",
        "Rasters of every integer and floating-point type (GeoTIFF strips, tiles and overviews, "
        "statistics in the metadata, and formats read by scanline): the histogram lines of "
        "`gdalinfo -hist` byte for byte, the JSON histograms (with `min` and `max` as GDAL prints "
        "them), with `-stats`, and the histograms GDAL stored in the `.aux.xml` sidecar.",
    )
    _code, out, err = run([sys.executable, str(SCRIPTS / "compare_hist_with_gdal.py"), ctx.data, "--terrio", ctx.terrio, "--json"])
    try:
        results = json.loads(out)
    except json.JSONDecodeError:
        s.error = tail(err or out)
        return s
    for r in results:
        status = IDENTICAL if not r["differences"] else DIFFERS
        s.cases.append(Case(r["case"], r["file"], status, "; ".join(r["differences"])))
    return s


VECTOR_ROUNDTRIPS = [
    ("countries.geojson", ["geojson", "fgb", "gpkg", "shp", "parquet", "csv", "gml", "kml", "mif", "tab", "topojson"]),
    ("cables.geojson", ["fgb", "gpkg", "parquet", "shp"]),
    ("ne_places.parquet", ["geojson", "fgb", "gpkg", "csv", "kml"]),
    ("hike.gpx", ["gpx", "gpkg", "kml"]),
]


def suite_vector_roundtrip(ctx):
    """Vector files converted by terrio into each format, read back by GDAL."""
    s = Suite(
        "vector-roundtrip",
        "Vector conversions",
        "GDAL reading terrio's output",
        "A file converted by `terrio convert` into each format, then read by GDAL: feature counts, "
        "fields, CRS, extent and a sample of geometries and attribute values must match what GDAL "
        "reads from the original, but for the differences GDAL's reading of `ogr2ogr`'s own "
        "conversion into that format has too (KML naming the `name` field `Name`, shapefiles "
        "truncating field names, MapInfo rounding coordinates to its grid, CSV files without a "
        "CRS).",
    )
    errors = []
    for src, exts in VECTOR_ROUNDTRIPS:
        if not os.path.exists(os.path.join(ctx.data, src)):
            continue
        _code, out, err = run(
            [sys.executable, str(SCRIPTS / "compare_with_gdal.py"), "--roundtrip", ctx.data, src, *exts], {"TERRIO_BIN": ctx.terrio}
        )
        found = False
        for line in out.splitlines():
            m = re.match(r"^(OK|DIFF|FAIL)\s+(\S+) -> (\S+?)(?::\s*(.*))?$", line)
            if m:
                found = True
                group = ctx.names.get(m[3], m[3])
                s.cases.append(Case(group, f"{m[2]} to {m[3]}", IDENTICAL if m[1] == "OK" else DIFFERS, m[4] or ""))
        if not found:
            errors.append(f"{src}: {tail(err or out, 5)}")
    s.error = "\n".join(errors) if not s.cases else ""
    return s


def suite_raster_roundtrip(ctx):
    """Every raster driver, on synthetic rasters: GDAL writes, terrio reads, and back."""
    s = Suite(
        "raster-roundtrip",
        "Raster drivers",
        "GDAL writing what terrio reads, and reading what terrio writes",
        "Synthetic rasters of every data type a format holds (several bands, NaN and finite nodata, "
        "geographic, projected and user-defined CRSs). Read: GDAL writes, terrio reads; write: "
        "terrio writes, GDAL reads. Pixels, type, CRS and nodata must match exactly, geotransforms "
        "within 1e-9.",
    )
    _code, out, err = run([sys.executable, str(SCRIPTS / "raster_roundtrip.py"), "-v"], {"TERRIO_BIN": ctx.terrio})
    failures = {}
    for line in out.splitlines():
        m = re.match(r"^  (\S+) (read|write) (.+?): (.*)$", line)
        if m:
            failures.setdefault((m[1], m[2]), []).append((m[3], m[4]))
    for line in out.splitlines():
        m = re.match(r"^(\S+)\s+(\d+/\d+|-)\s+(\d+/\d+|-)$", line)
        if not m:
            continue
        fmt = m[1]
        for kind, cell in (("read", m[2]), ("write", m[3])):
            if cell == "-":
                continue
            ok, total = map(int, cell.split("/"))
            failed = failures.get((fmt, kind), [])
            failed += [(f"raster {k + 1} of the failures", "") for k in range(len(failed), total - ok)]
            for name, detail in failed:
                s.cases.append(Case(f"{fmt} ({kind})", name, DIFFERS, detail))
            for k in range(ok):
                s.cases.append(Case(f"{fmt} ({kind})", f"raster {k + 1}", IDENTICAL))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_windows(ctx):
    """Windowed and overview reads of the GeoTIFFs and JPEG 2000 files, locally (and remote files
    with --remote)."""
    s = Suite(
        "windows",
        "Windowed and overview reads",
        "GDAL reading the same windows",
        "Windows of every GeoTIFF and JPEG 2000 file of the test data, at full resolution and from each "
        "overview (JPEG 2000 resolution levels), read "
        "in place by `terrio convert --srcwin --overview`: pixels identical, geotransforms within "
        "1e-9." + (" Remote: the same on public Cloud Optimized GeoTIFFs over HTTP." if ctx.remote else ""),
    )
    modes = [("local", [ctx.data])] + ([("remote", [])] if ctx.remote else [])
    for mode, extra in modes:
        _code, out, err = run([sys.executable, str(SCRIPTS / "compare_windows_with_gdal.py"), mode, *extra], {"TERRIO_BIN": ctx.terrio})
        for line in out.splitlines():
            m = re.match(r"^(.+?): (full|ovr \d+)\s+(\(.*?\)): (.*)$", line)
            if m:
                level = "full resolution" if m[2] == "full" else "overviews"
                fmt = "JPEG 2000" if m[1].endswith(".jp2") or "JPEG 2000" in m[1] else "GTiff"
                group = f"{fmt} {mode}, {level}"
                ok = m[4].strip() == "ok"
                s.cases.append(Case(group, f"{m[1]} {m[2]} {m[3]}", IDENTICAL if ok else DIFFERS, "" if ok else m[4]))
                continue
            m = re.match(r"^(.+?): info (ok|DIFFERENT)", line)
            if m:
                ok = m[2] == "ok"
                fmt = "JPEG 2000" if "JPEG 2000" in m[1] else "GTiff"
                s.cases.append(Case(f"{fmt} {mode}, headers", m[1], IDENTICAL if ok else DIFFERS))
        if not s.cases:
            s.error = tail(err or out)
    return s


def classify_raster(r):
    """Classifies a raster comparison of the operation scripts' JSON output.

    Args:
        r: One result.

    Returns:
        A ``(status, detail)`` tuple.
    """
    if "error" in r:
        return DIFFERS, f"error: {r['error']}"
    if not r.get("same_size"):
        return DIFFERS, f"size differs (GDAL {r.get('size')})"
    same = [k for k in ("same_type", "same_crs", "same_nodata", "same_metadata") if r.get(k) is False]
    if same:
        return DIFFERS, ", ".join(k.replace("same_", "") + " differs" for k in same)
    if r.get("valid_mismatch"):
        return DIFFERS, f"{r['valid_mismatch']} pixels valid in one output only"
    n = r.get("differing", 0)
    if n == 0:
        return IDENTICAL, ""
    detail = f"{n} pixels differ ({100 * (1 - r['exact']):.4g} %)"
    if r.get("max_diff") == float("inf") or r.get("inf_diff"):
        detail += ", nodata (or NaN) in one output only"
    else:
        detail += f", max difference {r['max_diff']:.3g}"
    if r.get("within_tol") == 1.0:
        return TOLERANCE, detail
    return DIFFERS, detail


def suite_json_ops(ctx, key, script, title, reference, criteria, group_words):
    """A suite of raster operations from a comparison script's JSON output.

    Args:
        ctx: The run's context.
        key: The suite's name.
        script: The script, in this directory.
        title: The report title.
        reference: What terrio is compared with.
        criteria: How cases are judged.
        group_words: How many words of a case's name name its operation.

    Returns:
        The suite.
    """
    s = Suite(key, title, reference, criteria)
    _code, out, err = run([sys.executable, str(SCRIPTS / script), ctx.data, "--terrio", ctx.terrio, "--json"])
    try:
        results = json.loads(out)
    except json.JSONDecodeError:
        s.error = tail(err or out)
        return s
    for r in results:
        status, detail = classify_raster(r)
        words = r["name"].split()
        s.cases.append(Case(" ".join(words[:group_words]), r["name"], status, detail))
    return s


def suite_shapefile_archives(ctx):
    """Filesystem-free Shapefile ZIPs cross-read with GDAL on real public geometry."""
    suite = Suite("shapefile-archives", "Shapefile ZIP archives", "GDAL/OGR",
                  "16 Natural Earth places, Las Vegas roads and Tennessee counties each; "
                  "normalized geometry coordinates exactly (including the single-part polygon wrapper), "
                  "scalar attributes/nulls and EPSG:4326; Z without invented M and asterisk text.")
    missing = missing_modules("terrio", "osgeo", "shapely")
    if missing:
        suite.skipped = "needs the Python modules " + ", ".join(missing)
        return suite
    code, out, err = run([sys.executable, str(SCRIPTS / "compare_shapefile_archives.py"), ctx.data, "--json"])
    try:
        results = json.loads(out)
    except json.JSONDecodeError:
        suite.error = tail(err or out)
        return suite
    for result in results:
        suite.cases.append(Case("Shapefile ZIP", result["name"], result["status"], result["detail"]))
    if code and not any(case.status == DIFFERS for case in suite.cases):
        suite.error = tail(err or out) or f"exit {code}"
    return suite


def suite_cad(ctx):
    """CAD geometry interchange against public GDAL drawings and real OSM roads."""
    suite = Suite(
        "cad", "DWG and DXF", "GDAL DXF and libopencad CAD drivers",
        "Feature counts, geometry types, layer names and drawing coordinates. Linear geometry: "
        "1e-8; curves: Hausdorff distance 0.001 times geometry diagonal and relative length/area "
        "0.002. Z bounds checked separately. Independent R2000 DWG reference coordinates were "
        "checked with libopencad; generated DWG is cross-read via DXF. GDAL uses unmerged "
        "blocks and a UTF-8 override for the mislabeled Russian hatch fixture.",
    )
    missing = missing_modules("osgeo", "numpy", "shapely")
    if missing:
        suite.skipped = "needs the Python modules " + ", ".join(missing)
        return suite
    required = [Path(ctx.data) / "las_vegas_roads.geojson"] + [
        Path(ctx.data) / "cad" / name
        for name in (
            "3dface.dxf", "LWPOLYLINE-OCS.dxf", "circle.dxf", "closed_polyline_with_bulge.dxf",
            "solid-vertex-ordering.dxf", "hatch_as_multipolygon.dxf", "bin_dxf_r2000.dxf",
            "block-basepoint.dxf", "Line.dwg", "Point.dwg",
        )
    ]
    if any(not path.is_file() for path in required):
        suite.skipped = "needs CAD fixtures and Las Vegas roads (scripts/fetch_test_data.sh)"
        return suite
    code, out, err = run([
        sys.executable, str(SCRIPTS / "compare_cad_with_gdal.py"), ctx.data,
        "--terrio", ctx.terrio, "--json",
    ])
    try:
        results = json.loads(out)
    except json.JSONDecodeError:
        suite.error = tail(err or out)
        return suite
    for result in results:
        suite.cases.append(Case(result["name"].split()[0], result["name"], result["status"], result["detail"]))
    if code and not any(r["status"] == DIFFERS for r in results):
        suite.error = tail(err or out)
    return suite


def suite_ops(ctx):
    """translate, warp and mosaic against gdal_translate, gdalwarp, gdalbuildvrt and gdal_merge."""
    return suite_json_ops(
        ctx,
        "ops",
        "compare_ops_with_gdal.py",
        "Raster operations",
        "`gdal_translate`, `gdalwarp`, `gdalbuildvrt`, `gdal_merge.py`",
        "The outputs of the same command lines on real files: size, type, CRS and geotransform must "
        "match, then pixels are compared. Identical: every pixel equal. Within tolerance: every "
        "pixel within the case's tolerance (1e-2 for interpolating resamplings).",
        1,
    )


def suite_benchmark_metadata(ctx):
    """Full real-data regressions for the metadata/schema defects from #253."""
    return suite_json_ops(
        ctx,
        "benchmark-metadata",
        "compare_benchmark_metadata_with_gdal.py",
        "Benchmark color and schema preservation",
        "GDAL Zarr, gdalbuildvrt and ogr2ogr",
        "GCP-only COGs retain 1,599 points and their CRS without an invented affine. "
        "Real SRTM Zarr, four-tile USGS base-resolution VRT and Natural Earth places "
        "GeoPackage: exact color interpretation, schemas, pixels, masks and normalized features.",
        1,
    )


def suite_benchmark_compat(ctx):
    """GDAL-versioned pyramids, terrain arithmetic and portable codec contracts."""
    return suite_json_ops(
        ctx, "benchmark-compat", "compare_benchmark_compat_with_gdal.py",
        "Benchmark compatibility and codec roundtrips", "Installed GDAL independent decoder",
        "GDAL 3.13 recursive COG pyramids and versioned slope/hillshade match exactly. "
        "WebP lossless pixels/masks and JPEG XL Byte/UInt16 layouts roundtrip exactly; "
        "lossy WebP quality must change valid decoded output, with no cross-encoder pixel equality claim.", 1,
    )


def suite_warp_policy(ctx):
    """Version-specific warp kernels and point-raster metadata on real terrain."""
    return suite_json_ops(
        ctx, "warp-policy", "compare_warp_policy_with_gdal.py", "Warp kernels and raster type",
        "Installed GDAL (3.12 or 3.13 kernel selected explicitly)",
        "Real USGS DEM clips marked PixelIsPoint: GTiff/COG translations and projected/rotated "
        "warps preserve raster type, CRS, transform, nodata and masks. Nearest is exact; "
        "interpolated Float32 values allow 0.002 m.", 2,
    )


def suite_catalogs(ctx):
    """Indexed GeoPackage/FlatGeobuf real DEM mosaics against GDAL GTI."""
    return suite_json_ops(
        ctx, "catalogs", "compare_catalogs_with_gdal.py", "Indexed raster catalogs",
        "GDAL GTI (GDAL 3.12.2)",
        "Real DEM windows indexed by GeoPackage/FlatGeobuf, z-order, nodata, masks, alpha, "
        "mixed CRS and Terrio-written indexes cross-read by GDAL. Grids, masks and pixels "
        "are compared (1e-4 numerical tolerance). A small HTTP window verifies one source "
        "opened from a 1,002-feature index and records actual bytes for both implementations.", 2,
    )


def suite_vrt(ctx):
    """Cross-read virtual rasters, including cropped requests and serialized transformers."""
    return suite_json_ops(
        ctx, "vrt", "compare_vrt_with_gdal.py", "VRT interoperability",
        "GDAL VRT driver, gdalwarp and gdal_translate",
        "Real USGS pixels through derived bands, kernel filters, masks and warped VRTs, "
        "including polynomial GCPs, TPS, homographies and RPCs. Whole and cropped reads "
        "must preserve types, nodata, georeferencing and values; Terrio-authored warped "
        "descriptions are also read by GDAL. Datum grid availability is matched.",
        2,
    )


def suite_overviews(ctx):
    """Stored source levels and HTTP COG transfers against gdalwarp."""
    return suite_json_ops(
        ctx, "overviews", "compare_overviews_with_gdal.py", "Warp source overviews",
        "`gdalwarp -ovr`",
        "Full-resolution target grids, source-level selection and output pixels must match GDAL. "
        "Synthetic level markers and real USGS terrain exercise AUTO, AUTO-n, NONE and forced levels. "
        "A real COG served over HTTP must transfer less than half as many raster bytes with AUTO as NONE.",
        2,
    )


def suite_geolocation(ctx):
    """Scientific swaths and subsampled geolocation arrays against GDAL -geoloc."""
    return suite_json_ops(
        ctx, "geolocation", "compare_geolocation_with_gdal.py", "Geolocation-array warping",
        "`gdalwarp -geoloc` (GDAL 3.12.2)",
        "Real Sentinel-5P swaths, both sides of the antimeridian, and extracted real data in "
        "HDF-EOS full/subsampled dimension maps, with and without invalid coordinate holes. "
        "Fixed output grids and exact transforms; every pixel and nodata mask is compared. "
        "Boundary differences from GDAL's inverse backmap are reported, not hidden.", 1,
    )


def suite_dem_calc(ctx):
    """Terrain, viewshed, algebra, proximity and sieve against GDAL tools."""
    return suite_json_ops(
        ctx,
        "dem-calc",
        "compare_dem_calc_with_gdal.py",
        "Terrain analysis and raster algebra",
        "`gdaldem`, `gdal_viewshed`, `gdal_calc.py`, `gdal_proximity.py`, `gdal_sieve.py`, `gdal_fillnodata`",
        "The outputs of the same command lines on real DEMs and land cover: size, type, nodata and "
        "geotransform must match, then pixels are compared. Within tolerance: every pixel within "
        "the case's tolerance (float32 rounding of NumPy's SIMD functions, for example). "
        "Viewshed outside-observer/radius cases independently verify the radius mask, then "
        "compare in-radius pixels, avoiding GDAL's nondeterministic out-of-range boundary race.",
        2,
    )


def suite_multidim(ctx):
    """Native groups, dimensions, slices and snapshots against GDAL MD and xarray."""
    suite = Suite(
        "multidim", "Native multidimensional arrays", "GDAL multidimensional API and xarray",
        "Real NOAA wind, JPSS HDF5, Sentinel-5P and SRTM Zarr arrays: native types, packed values, "
        "named axes, indexing variables and sliced/transposed values must match exactly. "
        "netCDF-Zarr snapshots retain coordinate values, chunk shapes and unlimited-axis metadata. "
        "Damaged unselected chunks must not affect metadata inspection or selected reads.",
    )
    _code, out, err = run([sys.executable, str(SCRIPTS / "compare_multidim_with_gdal.py"), ctx.data, "--terrio", ctx.terrio, "--json"])
    try:
        results = json.loads(out)
    except json.JSONDecodeError:
        suite.error = tail(err or out)
        return suite
    for result in results:
        status = IDENTICAL if result["status"] == "identical" else DIFFERS
        suite.cases.append(Case(result["operation"], result["name"], status, result.get("detail", "")))
    return suite


def suite_pansharpen(ctx):
    """Real Landsat panchromatic/spectral pairs against GDAL's weighted Brovey."""
    return suite_json_ops(
        ctx, "pansharpen", "compare_pansharpen_with_gdal.py", "Pansharpening", "`gdal_pansharpen`",
        "Real Landsat 8 B8/B4/B3/B2 images at unequal resolutions, including all six resamplers, "
        "weights, selected outputs, bit depth, nodata borders, integer clipping and union/intersection "
        "extents. Compare every pixel, dimensions, type, nodata and geotransform; integer cases "
        "are exact, and Float32 allows 0.002 (one ULP at the scene's radiometric scale).", 1,
    )


def suite_focal(ctx):
    """Real imagery and terrain against gdal raster neighbors, nearblack, clean-collar and gdalenhance."""
    return suite_json_ops(
        ctx, "focal", "compare_focal_with_gdal.py", "Focal statistics, collars and enhancement",
        "`gdal raster neighbors`, `nearblack`, `gdal raster clean-collar`, `gdalenhance`",
        "Sentinel-2 true-color and red-band images, the Landsat 7 scene (with and without an alpha "
        "band or mask) and a USGS DEM (also with deliberate nodata holes): every built-in kernel, "
        "matrix kernels, every method, output types and nodata; both collar algorithms with colors, "
        "thresholds, distances, alpha, masks and in-place updates; equalization, LUT file output "
        "and LUT file input. Every pixel, mask, type, nodata and geotransform must match exactly "
        "(tolerance 0); LUT files are compared token by token. For the in-place flood fill of a raster "
        "with an alpha band, GDAL 3.12 writes stale alpha values on modified lines; that case compares "
        "the color bands with GDAL's update and the alpha band with GDAL's new-file output.", 1,
    )


def suite_vector_ops(ctx):
    """ogr2ogr's options, gdal_rasterize and gdal_polygonize."""
    s = Suite(
        "vector-ops",
        "Vector operations",
        "`ogr2ogr`, `gdal_rasterize`, `gdal_polygonize.py`",
        "Conversions with `ogr2ogr`'s options (where, spat, select, sql, nlt...) compared feature "
        "by feature; rasterized pixels compared exactly; polygons compared by count, holes and "
        "validity, and by rasterizing them back.",
    )
    _code, out, err = run([sys.executable, str(SCRIPTS / "compare_vector_ops_with_gdal.py"), ctx.data, "--terrio", ctx.terrio])
    section = None
    for line in out.splitlines():
        if line.startswith("## "):
            section = line[3:].split(" vs ")[0].split(" --")[0]
            section = {"convert": "convert (ogr2ogr options)"}.get(section, section)
            if "append" in line:
                section = "convert --append/--overwrite"
            continue
        if not section or not line.strip() or line.startswith(("Warning", "More than")):
            continue
        name = re.split(r"\s{2,}", line.strip())[0]
        if " DIFF " in line or line.rstrip().endswith(" DIFF") or " ERROR " in line:
            detail = line.split(" DIFF ", 1)[-1] if " DIFF " in line else line.split(" ERROR ", 1)[-1]
            s.cases.append(Case(section, name, DIFFERS, detail.strip()))
        elif re.search(r"\sOK(\s|$)", line):
            s.cases.append(Case(section, name, IDENTICAL))
        else:
            m = re.search(r"(\d+) of (\d+) pixels differ", line)
            if m:
                n = int(m[1])
                status = IDENTICAL if n == 0 else TOLERANCE
                s.cases.append(Case(section, name, status, "" if n == 0 else f"{n} of {m[2]} pixels differ (reprojected input)"))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_pointclouds(ctx):
    """LAS, LAZ, COPC, EPT, PLY and E57 against laspy, PDAL and gdal_grid."""
    s = Suite(
        "pointclouds",
        "Point clouds",
        "laspy (every value), PDAL (headers, PLY, E57 and EPT, COPC and EPT queries, filters, grids, "
        "reprojection) and gdal_grid (TIN and nearest-neighbour grids)",
        "Every dimension of every point equal to laspy's; headers equal to laspy's and PDAL's; "
        "written files read back identically by laspy and PDAL; PLY, E57 and EPT points equal to "
        "PDAL's; COPC and EPT queries returning the same points as PDAL's; waveform samples equal "
        "to the file's packets; filters giving the same classes or points as PDAL's; grids equal "
        "to PDAL's `writers.gdal`. Reprojections are within 2e-7 degrees or 2 mm of PDAL's; TIN "
        "grids differ from `gdal_grid`'s where points share an x and y.",
    )
    missing = missing_modules("terrio", "laspy", "numpy")
    if missing:
        s.skipped = "needs the Python modules " + ", ".join(missing)
        return s
    # CI runs compare_pointclouds.py as its own (failing) step first; its output is reused here
    # rather than running the comparison again.
    log = os.environ.get("TERRIO_POINTCLOUDS_LOG")
    if log and os.path.exists(log):
        out, err = Path(log).read_text(), ""
    else:
        _code, out, err = run([sys.executable, str(SCRIPTS / "compare_pointclouds.py"), ctx.data])
    for line in out.splitlines():
        m = re.match(r"^(ok  |FAIL) (.+)$", line)
        if not m:
            continue
        text = m[2]
        name, _, detail = text.partition(": ") if m[1] == "FAIL" else (text, "", "")
        tolerance = re.match(r"^(.*) \[within tolerance: (.*)\]$", name)
        if tolerance:
            name, detail = tolerance[1], tolerance[2]
        first = name.split()[0]
        against = "PDAL" if "PDAL" in name or "pdal" in name else "laspy"
        group = {
            "read": f"read vs {against}",
            "header": f"header vs {against}",
            "COPC": "COPC queries vs PDAL",
            "EPT": "EPT queries vs PDAL",
            "readfmt": "PLY, E57 and EPT vs PDAL",
            "writefmt": "PLY, E57 and EPT written, read back",
            "waveform": "waveforms",
            "filter": "filters vs PDAL",
            "grid": "grid vs PDAL writers.gdal",
            "interp": "TIN and nearest grids vs gdal_grid",
            "reproject": "reproject vs PDAL",
        }.get(first, f"write and read back ({against})")
        if m[1] == "FAIL":
            s.cases.append(Case(group, name, DIFFERS, detail))
        elif tolerance:
            s.cases.append(Case(group, name, TOLERANCE, detail))
        elif first == "reproject":
            s.cases.append(Case(group, name.split(" vs PDAL")[0], TOLERANCE, name.split(": ", 1)[-1]))
        else:
            s.cases.append(Case(group, name, IDENTICAL))
    if not s.cases:
        s.error = tail(err or out)
    return s


def epsg_cases(s, ctx, test, points, suffix=""):
    """Runs an EPSG accuracy test and adds a case per CRS (per pair of CRSs from CRS to CRS),
    judged by its worst reference point.

    Args:
        s: The suite the cases go to.
        ctx: The run's context.
        test: The test (``epsg_accuracy`` or ``epsg_accuracy_grids``).
        points: The reference file, in the test data.
        suffix: Appended to the group names.

    Returns:
        Whether the test ran.
    """
    with tempfile.TemporaryDirectory() as tmp:
        report = os.path.join(tmp, "epsg.tsv")
        env = {"TERRIO_TEST_DATA": os.path.abspath(ctx.data), "TERRIO_EPSG_REPORT": report}
        _code, out, err = run(["cargo", "test", "--release", "-p", "terrio-crs", "--features", "wbprojection-fallback", "--test", test, "--", "--nocapture"], env)
        if not os.path.exists(report):
            s.error = tail(err or out)
            return False
        groups = {"F": "forward (WGS 84 to CRS)", "I": "inverse (CRS to WGS 84)", "X": "CRS to CRS"}
        with open(report) as f:
            for line in f:
                # kind, the code (source and target codes from CRS to CRS), the number of points,
                # how many terrio cannot transform, the largest error and that point's input.
                parts = line.rstrip("\n").split("\t")
                kind = parts[0]
                codes = parts[1:3] if kind == "X" else parts[1:2]
                n, failed, value, x, y = parts[len(codes) + 1 :]
                name = " to ".join(f"EPSG:{c}" for c in codes)
                group = groups[kind] + suffix
                tolerance = 1.0 if kind == "X" else 0.01
                at = f"({float(x):.10g}, {float(y):.10g})"
                if value == "fail":
                    detail = f"terrio cannot transform {failed} of {n} points, the first at {at}"
                    s.cases.append(Case(group, name, DIFFERS, detail))
                    continue
                e = float(value)
                detail = f"{e:.4f} m at {at}, the largest of {n} points"
                if e <= 0.001:
                    s.cases.append(Case(group, name, IDENTICAL))
                elif e <= tolerance:
                    s.cases.append(Case(group, name, TOLERANCE, detail))
                else:
                    s.cases.append(Case(group, name, DIFFERS, detail))
    return True


def suite_iau(ctx):
    """All IAU_2015 projected definitions against live GDAL/PROJ."""
    suite = Suite("iau", "IAU_2015 planetary CRSs", "GDAL/PROJ",
                  "2,074 registered pairs plus unit/offset cases, planetary latitudes and axes. "
                  "Component tolerances: 1 mm projected; 1e-8 degrees geographic.")
    if missing_modules("osgeo"):
        suite.skipped = "needs GDAL Python bindings"
        return suite
    code, out, err = run([sys.executable, str(SCRIPTS / "compare_iau_with_gdal.py"), ctx.data, "--terrio", ctx.terrio, "--json"])
    try:
        cases = json.loads(out)
    except json.JSONDecodeError:
        suite.error = tail(err or out)
        return suite
    for case in cases:
        suite.cases.append(Case("IAU_2015", case["name"], case["status"], case["detail"]))
    if code and not any(c.status == DIFFERS for c in suite.cases):
        suite.error = tail(err or out)
    return suite


def suite_perspective(ctx):
    """Native near-sided and tilted perspective, including visibility/domain limits."""
    suite = Suite("perspective", "Perspective projections", "GDAL/PROJ",
                  "20 spherical/WGS84 configurations and 2,500 geographic samples; "
                  "all aspects, horizon sides, tilt/azimuth, units and offsets. "
                  "Projected allowance 1 mm; inverse 1e-8 degrees, longitude weighted "
                  "by cos(latitude). PROJ strings and WKT2 are checked.")
    if missing_modules("osgeo"):
        suite.skipped = "needs GDAL Python bindings"
        return suite
    code, out, err = run([sys.executable, str(SCRIPTS / "compare_perspective_with_gdal.py"), ctx.data, "--terrio", ctx.terrio, "--json"])
    try:
        cases = json.loads(out)
    except json.JSONDecodeError:
        suite.error = tail(err or out)
        return suite
    for case in cases:
        suite.cases.append(Case("Perspective", case["name"], case["status"], case["detail"]))
    if code and not any(c.status == DIFFERS for c in suite.cases):
        suite.error = tail(err or out)
    return suite


def suite_projjson(ctx):
    """PROJJSON units, axes and bound transformations, with and without IDs."""
    suite = Suite("projjson", "PROJJSON CRS definitions", "GDAL/PROJ",
                  "Supported two-dimensional geographic, projected and bound definitions. "
                  "Component tolerances: 1 mm projected; 1e-8 degrees geographic.")
    if missing_modules("osgeo"):
        suite.skipped = "needs GDAL Python bindings"
        return suite
    code, out, err = run([sys.executable, str(SCRIPTS / "compare_projjson_with_gdal.py"), ctx.data, "--terrio", ctx.terrio, "--json"])
    try:
        cases = json.loads(out)
    except json.JSONDecodeError:
        suite.error = tail(err or out)
        return suite
    for case in cases:
        suite.cases.append(Case("PROJJSON", case["name"], case["status"], case["detail"]))
    if code and not any(c.status == DIFFERS for c in suite.cases):
        suite.error = tail(err or out)
    return suite


def suite_geocentric(ctx):
    """XYZ conversions, units, poles, datum shifts and projected composition."""
    suite = Suite("geocentric", "Geocentric XYZ endpoints", "GDAL/PROJ",
                  "All three ordinates: 1 mm for Cartesian/projected values and heights; "
                  "1e-8 degrees for geographic angles. Includes explicit declared Helmert "
                  "pipelines where PROJ 9.8.1's geocentric BoundCRS factory drops the shift.")
    if missing_modules("osgeo"):
        suite.skipped = "needs GDAL Python bindings"
        return suite
    code, out, err = run([sys.executable, str(SCRIPTS / "compare_geocentric_with_gdal.py"), ctx.data, "--terrio", ctx.terrio, "--json"])
    try:
        cases = json.loads(out)
    except json.JSONDecodeError:
        suite.error = tail(err or out)
        return suite
    for case in cases:
        suite.cases.append(Case("XYZ", case["name"], case["status"], case["detail"]))
    if code and not any(c.status == DIFFERS for c in suite.cases):
        suite.error = tail(err or out)
    return suite


def suite_lazy_geotiff(ctx):
    """Pinned GDAL metadata, sample and external-overview regressions through Rust."""
    suite = Suite("lazy-geotiff", "Lazy GeoTIFF metadata and external overviews", "GDAL",
                  "Frozen GDAL reference checks: sample bits, nodata, band properties, RPCs, "
                  "file lists and external overview dimensions, values and masks. Exact assertions; "
                  "this suite does not regenerate reference files with the installed GDAL.")
    if not shutil.which("cargo"):
        suite.skipped = "needs cargo"
        return suite
    tests = ["external_overviews", "geotiff_nodata", "geotiff_band_metadata",
             "lazy_rpc", "lazy_sample_bits", "file_list"]
    command = ["cargo", "test", "--release", "-p", "terrio"]
    for test in tests:
        command += ["--test", test]
    code, out, err = run(command, {"TERRIO_TEST_DATA": os.path.abspath(ctx.data)})
    for name, result in re.findall(r"^test (\S+) \.\.\. (ok|FAILED)$", out, re.MULTILINE):
        suite.cases.append(Case("GeoTIFF", name, IDENTICAL if result == "ok" else DIFFERS,
                                "Pinned reference assertions passed" if result == "ok" else tail(out)))
    if not suite.cases or (code and not any(case.status == DIFFERS for case in suite.cases)):
        suite.error = tail(err or out)
    return suite


def suite_crs(ctx):
    """Every EPSG CRS's transformations against GDAL's (crates/terrio-crs/tests/epsg_accuracy.rs)."""
    s = Suite(
        "crs",
        "Coordinate transformations",
        "GDAL 3.12 and PROJ (`gdaltransform`)",
        "50 reference points per EPSG geographic and projected CRS (the centre of its area of use "
        "and random points of it), transformed from WGS 84 (forward), 50 more back (inverse), and "
        "50 between the CRS and another whose area holds them, compared with GDAL's results, "
        "neither using datum shift grids, a case per CRS (per pair of CRSs from CRS to CRS) "
        "judged by its worst point; then "
        "projections EPSG has no code for, against PROJ "
        "(crates/terrio/tests/geostationary.rs: points from nadir to near the limb, forward "
        "within 1 mm and inverse within 1e-8°, and points the satellite cannot see refused; "
        "crates/terrio-crs/tests/ellipsoidal_projections.rs: points over the globe on WGS 84 and on "
        "a sphere, forward within 1 mm and inverse within 1e-8°, and for the gnomonic points "
        "beyond the horizon refused; crates/terrio-crs/tests/proj_strings.rs: every PROJ projection "
        "with typical parameters, on WGS 84 and on a sphere, as a PROJ string and as pyproj's "
        "WKT1_GDAL and WKT2_2019, 20 points each, matching PROJ or refused; "
        "crates/terrio-crs/tests/crs_benchmark.rs: five 91-point graticules pinned to PROJ "
        "9.10.0 development commit 5fa31c2, forward within 1 mm and inverse within "
        "1e-7 degrees with longitude weighted by cos(latitude) (1e-6 degrees at exact "
        "input poles for platform asin rounding); PROJ-refused inverse "
        "boundary points may be refused or recover the original input). "
        "Identical: within 1 mm. Within tolerance: within 1 cm (forward, inverse) or 1 m (CRS to "
        "CRS, where the datum transformations GDAL picks can differ by area of use). Known "
        "differences: deprecated CRSs whose own datum PROJ uses, where terrio uses their "
        "replacement's (#153).",
    )
    if not shutil.which("cargo"):
        s.skipped = "needs cargo"
        return s
    if not os.path.exists(os.path.join(ctx.data, "epsg_points.tsv")):
        s.skipped = "epsg_points.tsv is missing from the test data (scripts/gen_epsg_points.py)"
        return s
    epsg_cases(s, ctx, "epsg_accuracy", "epsg_points.tsv")
    proj_cases(s)
    return s


# The PROJ definitions without an EPSG code that the tests check, by test file and test.
PROJ_CASES = {
    "crs_benchmark": {
        "unterminated_quoted_proj_values_are_refused": "Malformed quoted PROJ values are refused before parameters or +init can be used, as GDAL 3.12.2 / PROJ 9.8.1",
        "web_mercator_keeps_its_origin_and_false_origin": (
            "Web Mercator (webmerc): lon_0, x_0 and y_0, without the fallback", TOLERANCE,
            "within 1e-9 degrees of GDAL 3.13.3 / PROJ 9.9.0",
        ),
        "versioned_authority_references_match_proj": (
            "Versioned EPSG URN/URL references", TOLERANCE,
            "EPSG:4326 resolution; EPSG:3857 coordinates within 1e-7 m of PROJ 9.8.1",
        ),
        "lcc_omitted_second_parallel_is_zero_in_a_crs": (
            "Lambert Conic Conformal: omitted 2nd parallel is 0 (CRS semantics), PROJ string and WKT", TOLERANCE,
            "within 1e-9 degrees of GDAL 3.13.3 / PROJ 9.9.0",
        ),
        "winkel_tripel_omitted_parallel_preserves_crs_default": (
            "Winkel Tripel: omitted lat_1 equals explicit zero (CRS semantics)", TOLERANCE,
            "omitted and explicit-zero CRS parallel give equal coordinates",
        ),
        "polar_stereographic_hemisphere_follows_lat_ts_in_a_crs": (
            "Polar stereographic: the pole is on lat_ts's side (CRS semantics), PROJ string and WKT", TOLERANCE,
            "within 1e-9 degrees of GDAL 3.13.3 / PROJ 9.9.0",
        ),
        "orthographic_roundtrips_across_origins_scales_and_rotations": (
            "Orthographic: round trips across origins, figures, scales and azimuths", TOLERANCE,
            "round trip <1e-5 degrees at the ill-conditioned limb",
        ),
        "stereographic_origins_near_a_pole_stay_accurate": (
            "Stereographic and Oblique Stereographic: origins within 1e-6 to 1e-12 degrees of a pole", TOLERANCE,
            "within the origin's shift of the pole's answer; sterea within 2 mm of GDAL 3.13.3 at lat_0=89.99999999 (PROJ: 12,714 km at 89.9999999)",
        ),
        **{
            test: (name, TOLERANCE, "forward <1 mm; inverse <1e-7 degrees, or 1e-6 at exact poles (longitude weighted by cos(latitude))")
            for test, name in {
                "mollweide": "Mollweide: pinned PROJ 9.10 operation graticule, including the finite pole approximation",
                "wagner_iv": "Wagner IV: pinned PROJ 9.10 operation graticule, including poles",
                "wagner_v": "Wagner V: pinned PROJ 9.10 operation graticule, including poles",
                "orthographic": "Orthographic: pinned PROJ 9.10 WGS84 oblique graticule, forward/inverse",
                "winkel_tripel_explicit_parallel_matches_operation_oracle": "Winkel Tripel: explicit lat_1=acos(2/pi) against PROJ's operation default",
            }.items()
        },
        "quoted_proj_string_values_read_as_in_proj": (
            "PROJ strings with values in double quotes", TOLERANCE,
            "within 1e-9 degrees of GDAL 3.13.3 / PROJ 9.9.0",
        ),
    },
    "operations": {
        "reports_the_transformation_used": "Operation report: the name and accuracy of PROJ 9.9's first candidate without grids (27700 both ways, 31467, `+towgs84`)",
        "reports_ballparks_and_no_operation": "Operation report: ballparks (4610, EGM96 without its grid) and no operation for one datum",
    },
    "crs_policy": {
        "promoted_vector_heights_match_gdal_and_keep_geometry_dimensions": (
            "Promoted vector heights and preserved geometry dimensions", TOLERANCE,
            "GDAL 3.12.2 / PROJ 9.8.1: RD New to WGS84 3D, 1e-8 degrees / 1 mm; 2D stays 2D",
        ),
    },
    "promote_to_3d": {
        "heights_of_promoted_2d_crss_go_through_the_datum_transformation": (
            "PromoteTo3D: heights of 2D CRSs through the datum transformation (RD New, sterea, sphere)", TOLERANCE,
            "within 1e-8 degrees and 1 mm of GDAL 3.13.3 after OGRSpatialReference::PromoteTo3D",
        ),
    },
    "refusal_kinds": {
        "global_latitude_of_origin_validation_matches_proj": "PROJ 9.8.1 global lat_0 validation for longlat, UTM, geocentric, Web Mercator and ob_tran",
        "strict_policy_allows_vertical_unit_conversion": (
            "Strict NAVD88 metres to US survey feet", TOLERANCE,
            "GDAL 3.12.2 / PROJ 9.8.1 under ONLY_BEST and ALLOW_BALLPARK=NO; 1e-10 height units",
        ),
        "strict_refusals_have_their_own_variants": "ONLY_BEST and ALLOW_BALLPARK refuse where GDAL 3.13.3 does without grids, horizontal and vertical (CH1903+, Tokyo, WGS 84 + EGM96)",
    },
    "geostationary": {
        "goes16_sweep_x_matches_proj": "GOES-16: `+proj=geos +h=35786023 +lon_0=-75 +sweep=x +ellps=GRS80`",
        "seviri_sweep_y_matches_proj": "Meteosat SEVIRI: `+proj=geos +h=35785831 +lon_0=0 +sweep=y +a=6378169 +b=6356583.8`",
    },
    "ellipsoidal_projections": {
        "eqc_matches_proj": "Equidistant Cylindrical: `+proj=eqc +lat_ts=30 +lat_0=10 +lon_0=-20`, WGS 84 and sphere",
        "sinu_matches_proj": "Sinusoidal: `+proj=sinu +lon_0=15`, WGS 84 and sphere",
        "bonne_matches_proj": "Bonne: `+proj=bonne +lat_1=45` and `+lat_1=-30`, WGS 84 and sphere",
        "mill_matches_proj": "Miller Cylindrical: `+proj=mill +lon_0=-90`, WGS 84 and sphere",
        "vandg_matches_proj": "Van der Grinten: `+proj=vandg`, WGS 84 and sphere",
        "gnom_matches_proj": "Gnomonic: `+proj=gnom` oblique, polar and equatorial, WGS 84 and sphere",
        "gnom_refuses_points_beyond_the_horizon": "Gnomonic near and beyond the horizon: `+proj=gnom +lat_0=40 +lon_0=-100`",
        "cass_matches_proj": "Cassini-Soldner: `+proj=cass +lat_0=48 +lon_0=10` (WGS 84 and sphere) and `+hyperbolic`",
    },
    "proj_strings": {
        "earth_wkt2_negative_axes_match_gdal": (
            "Earth WKT2 west/south geographic axes", TOLERANCE,
            "GDAL 3.12.2/PROJ 9.8.1 traditional GIS order: forward/inverse within 1e-12 degrees for west/south, south/west and north/east controls",
        ),
        "earth_meridian_and_inverse_values_match_proj": (
            "Earth meridian series and inverse normalization", TOLERANCE,
            "nine pinned GDAL 3.12.2/PROJ 9.8.1 points: UTM 10N, equidistant conic and sinusoidal; forward <1 micrometre, inverse <1e-11 degrees",
        ),
        "proj_strings_match_proj_or_are_refused": "Every PROJ projection as a PROJ string (117, WGS 84 and sphere): matches PROJ or is refused",
        "unsupported_projections_are_refused_in_every_encoding": "The projections terrio does not implement, refused as WKT too",
        "pyproj_wkt_matches_proj": "The same CRSs as pyproj's WKT1_GDAL and WKT2_2019 (known differences: #119, #156)",
        "own_wkt_reads_back_as_the_same_crs": "The same CRSs through the WKT terrio writes, read back by terrio",
        "longlat_longitudes_are_relative_to_lon_0": "Geographic PROJ strings with `+lon_0` (`longlat`, `latlong`, `+pm`, `+towgs84`): seven GDAL 3.13.3 points",
    },
}


def proj_cases(s):
    """Runs the tests of projections EPSG has no code for and adds a case per definition.

    Args:
        s: The suite the cases go to.
    """
    group = "CRS references and projection definitions"
    for file, tests in PROJ_CASES.items():
        package = "terrio" if file in {"geostationary", "crs_policy"} else "terrio-crs"
        features = [] if package == "terrio" else ["--features", "wbprojection-fallback"]
        code, out, err = run(["cargo", "test", "--release", "-p", package, *features, "--test", file])
        first_case = len(s.cases)
        for test, description in tests.items():
            name, status, detail = (description, IDENTICAL, "") if isinstance(description, str) else description
            m = re.search(rf"^test {test} \.\.\. (\w+)$", out, re.M)
            if not m:
                s.cases.append(Case(group, name, DIFFERS, "the test did not run: " + tail(err or out, 3)))
            elif m.group(1) == "ok":
                s.cases.append(Case(group, name, status, detail))
            else:
                s.cases.append(Case(group, name, DIFFERS, "differs from PROJ"))
        if code != 0 and not any(case.status == DIFFERS for case in s.cases[first_case:]):
            s.cases.append(Case(group, f"{file}: Cargo test process", DIFFERS, tail(err or out, 5)))


def suite_crs_grids(ctx):
    """Transformations with datum shift grids, geoid models and coordinate epochs vs GDAL's."""
    s = Suite(
        "crs-grids",
        "Datum shift grids, geoid models and coordinate epochs",
        "GDAL 3.12 and PROJ, with the same grids",
        "The EPSG reference points again, GDAL and terrio both using the grids of "
        "`scripts/proj_grids.txt` (crates/terrio-crs/tests/epsg_accuracy_grids.rs); then random "
        "points through every horizontal shift grid (NTv2 and GeoTIFF, forward and inverse), "
        "geoid models (GeoTIFF and GTX), NADCON5 and geocentric translation grids, compound CRSs "
        "and time-dependent transformations with coordinate epochs "
        "(scripts/compare_crs_grids_with_gdal.py), with the largest difference of each case. "
        "Identical: within 1 mm. Within tolerance: within 1 cm, or 1 m from CRS to CRS. Known "
        "differences: deprecated CRSs whose own datum PROJ uses, where terrio uses their "
        "replacement's (#153).",
    )
    if not shutil.which("cargo"):
        s.skipped = "needs cargo"
        return s
    if not os.path.exists(os.path.join(ctx.data, "grids", "us_noaa_conus.tif")):
        s.skipped = "the grids are missing from the test data (scripts/fetch_test_data.sh)"
        return s
    if os.path.exists(os.path.join(ctx.data, "epsg_points_grids.tsv")):
        if not epsg_cases(s, ctx, "epsg_accuracy_grids", "epsg_points_grids.tsv", " with grids"):
            return s
    missing = missing_modules("osgeo", "numpy")
    if missing:
        return s
    _code, out, err = run([sys.executable, str(SCRIPTS / "compare_crs_grids_with_gdal.py"), ctx.data, "--terrio", ctx.terrio])
    row = re.compile(r"^(.+?)\s+(\d+)\s+(\d+)\s+(\S+)\s+(\S+)\s+(\S+)$")
    for line in out.splitlines():
        m = row.match(line)
        if not m or line.startswith("case"):
            continue
        name, n, only, largest, _mean, dz = m.group(1).strip(), int(m.group(2)), int(m.group(3)), float(m.group(4)), m.group(5), float(m.group(6))
        group = "grids, geoids and epochs"
        worst = max(largest, dz)
        detail = f"{n} points, largest difference {worst:.2e} m" + (f", {only} transformed by only one" if only else "")
        if only or worst > 0.01:
            s.cases.append(Case(group, name, DIFFERS, detail))
        elif worst > 0.001:
            s.cases.append(Case(group, name, TOLERANCE, detail))
        else:
            s.cases.append(Case(group, name, IDENTICAL, detail))
    if not any(c.group.startswith("grids") for c in s.cases):
        s.error = tail(err or out)
    return s


def suite_arrow(ctx):
    """Arrow and GeoArrow export and import of vector layers, against GDAL, pyarrow and GeoPandas."""
    s = Suite(
        "arrow",
        "Arrow and GeoArrow",
        "GDAL's Arrow streams, pyarrow, GeoPandas",
        "Every layer of the test data exported with `Layer.to_arrow()` (WKB and native GeoArrow) "
        "against GDAL's Arrow stream of the same layer, through pyarrow and GeoPandas, and imported "
        "back with `terrio.from_arrow()` (and by GDAL's `WriteArrow`): rows, geometries, attribute "
        "values and CRS. Within tolerance: geometries equal up to vertex order.",
    )
    missing = missing_modules("terrio", "pyarrow", "geopandas", "shapely")
    if missing:
        s.skipped = "needs the Python modules " + ", ".join(missing)
        return s
    _code, out, err = run([sys.executable, str(SCRIPTS / "compare_arrow_with_gdal.py"), ctx.data])
    for line in out.splitlines():
        m = re.match(r"^(ok |~ok|BAD) (\S+) (.+?)\s{2,}(rows .*)$", line)
        if not m:
            continue
        status = {"ok ": IDENTICAL, "~ok": TOLERANCE}.get(m[1], DIFFERS)
        s.cases.append(Case(m[3].strip(), m[2], status, "" if status == IDENTICAL else m[4]))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_gpkg(ctx):
    """GeoPackage updates (append, overwrite, added layers) and R-tree spatial filters vs ogr2ogr."""
    s = Suite(
        "gpkg",
        "GeoPackage updates and spatial filters",
        "`ogr2ogr`, GDAL's `validate_gpkg.py`",
        "`--append`, `--overwrite` and added layers compared with `ogr2ogr` (features, extents, "
        "counts, sequences, R-trees, `integrity_check`, the GeoPackage validator, rasters kept, the "
        "file changed in place), `--spat` through the R-tree compared with `ogr2ogr -spat`, appends "
        "to a file of 200,000 points compared with `ogr2ogr -append`, and terrio killed while it "
        "commits an append (SQLite must then find the file intact, with all or none of the new "
        "features).",
    )
    _code, out, err = run(
        [sys.executable, str(SCRIPTS / "compare_gpkg_with_gdal.py"), ctx.data, "--terrio", ctx.terrio, "--points", "200000"]
    )
    current = None
    for line in out.splitlines():
        m = re.match(r"^(ok |BAD) (.+)$", line)
        if m:
            current = Case(m[2].split()[0], m[2], IDENTICAL if m[1] == "ok " else DIFFERS)
            s.cases.append(current)
        elif current is not None and current.status == DIFFERS and line.startswith("      "):
            current.detail = "; ".join(filter(None, [current.detail, line.strip()]))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_streams(ctx):
    """Streamed vector conversions vs whole-layer ones, and a large layer vs ogr2ogr."""
    s = Suite(
        "streams",
        "Streamed vector conversions",
        "terrio's whole-layer conversions, `ogr2ogr`",
        "Vector conversions read and written a chunk of features at a time, compared with the same "
        "conversions of whole layers (identical files; GeoParquet by content) and read back by GDAL; "
        "a synthetic layer of 100,000 polygons converted by terrio and `ogr2ogr`, compared by feature "
        "count and a sample of features.",
    )
    _code, out, err = run(
        [sys.executable, str(SCRIPTS / "compare_streams_with_gdal.py"), ctx.data, "--terrio", ctx.terrio, "--features", "100000"]
    )
    for line in out.splitlines():
        m = re.match(r"^(ok |BAD) (\S+)->(\S+)\s{2}(.*)$", line)
        if m:
            status = IDENTICAL if m[1] == "ok " else DIFFERS
            s.cases.append(Case(f"stream to {m[3]}", m[2], status, "" if status == IDENTICAL else m[4]))
            continue
        m = re.match(r"^\s+(\S+ -> \S+)\s.*\s(same|DIFFERS) \((.*)\)$", line)
        if m:
            status = IDENTICAL if m[2] == "same" else DIFFERS
            s.cases.append(Case("large layer vs ogr2ogr", m[1], status, "" if status == IDENTICAL else m[3]))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_tiles(ctx):
    """PMTiles, MBTiles and MVT: raster and vector tiles read and written, vs GDAL."""
    s = Suite(
        "tiles",
        "Tile archives",
        "GDAL's MBTiles, PMTiles and MVT drivers",
        "Raster tile pyramids written by both tools and read by GDAL (pixels at full resolution and "
        "in each overview), vector tiles read by both tools and written by both (feature counts, "
        "areas and lengths by feature), invalid polygons GDAL repairs with GEOS's `MakeValid` or "
        "fails to clip (tiles read unclipped), and archives updated in place.",
    )
    _code, out, err = run([sys.executable, str(SCRIPTS / "compare_tiles_with_gdal.py"), ctx.data, "--terrio", ctx.terrio])
    group, raster = "", ""
    for line in out.splitlines():
        if not line.startswith(" "):
            header = line.rstrip(":")
            if header.startswith("Vector tiles read"):
                group = "vector tiles read"
            elif header.startswith("Vector tiles written"):
                group = "vector tiles written"
            elif header.startswith("A directory"):
                group = "MVT directories"
            elif header.startswith("Invalid polygons"):
                group = "invalid polygons (MakeValid)"
            elif header.startswith("Updates"):
                group = "archive updates"
            elif header.startswith("AVIF"):
                group = "AVIF tiles"
            else:
                group, raster = "raster tiles", header
            continue
        text = line.strip()
        m = re.match(r"^(.+?): (\d+) vs (\d+) features, area rel\. diff (\S+), length rel\. diff (\S+), (\d+) of (\d+) keys differ", text)
        if m:
            same_count = m[2] == m[3]
            close = float(m[4]) < 1e-9 and float(m[5]) < 1e-9
            status = IDENTICAL if same_count and close and m[6] == "0" else TOLERANCE if same_count and m[6] == "0" else DIFFERS
            s.cases.append(Case(group, m[1], status, "" if status == IDENTICAL else text.split(": ", 1)[1]))
            continue
        m = re.match(r"^(.+?): ([\d.]+)% identical, max diff (\d+), mean ([\d.]+)", text)
        if m:
            status = IDENTICAL if m[3] == "0" else TOLERANCE if float(m[4]) < 1.0 else DIFFERS
            name = f"{raster}: {m[1]}" if group == "raster tiles" else m[1]
            s.cases.append(Case(group, name, status, "" if status == IDENTICAL else text.split(": ", 1)[1]))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_grib(ctx):
    """GRIB files, band by band, against GDAL's GRIB driver."""
    s = Suite(
        "grib",
        "GRIB",
        "GDAL's GRIB driver (degrib and g2clib)",
        "GDAL's GRIB test files, ecCodes' and real forecasts (NOAA's GFS and HRRR, ECMWF's open data): "
        "sizes, geotransforms (1e-9 relative) and CRSs, every band's description, `GRIB_*` metadata "
        "and nodata value exactly, and its pixels (identical, or within one Float32 unit in the last "
        "place), with units normalized (GDAL's default) and as stored.",
    )
    dirs = [os.path.abspath(os.path.join(ctx.data, "grib", d)) for d in ("real", "gdal", "eccodes")]
    dirs = [d for d in dirs if os.path.isdir(d)]
    if not dirs:
        s.skipped = "no GRIB data (scripts/fetch_test_data.sh)"
        return s
    missing = missing_modules("numpy", "osgeo", "terrio")
    if missing:
        s.skipped = f"needs {', '.join(missing)}"
        return s
    code, out, err = run([sys.executable, str(SCRIPTS / "compare_grib_with_gdal.py"), *dirs, "--raw"])
    # Exit code 1 means a file differs; anything else, or a traceback, that the script failed.
    if code not in (0, 1) or "Traceback" in err:
        s.error = tail(err or out)
    for line in out.splitlines():
        # Files neither tool opens (SKIP) are not cases.
        m = re.match(r"^(OK|TOL|DIFF)\s+(\S+?)/(\S+?)( \(units as stored\))?: (.*)$", line)
        if m:
            status = {"OK": IDENTICAL, "TOL": TOLERANCE}.get(m[1], DIFFERS)
            group = {"real": "real forecasts", "gdal": "GDAL's test files", "eccodes": "ecCodes' test files"}.get(m[2], m[2])
            s.cases.append(Case(group, m[3] + (m[4] or ""), status, "" if status == IDENTICAL else m[5]))
    if not s.cases:
        s.error = tail(err or out)
    return s


def suite_analysis_output(ctx):
    """Output reprojection and decimal rounding for zonal statistics and point sampling."""
    suite = Suite(
        "analysis-output", "Zonal and point-sampling output", "GDAL, NumPy and OGR",
        "Real Tennessee zones, DEM, Landsat and NLCD rasters. Baseline values are checked "
        "against GDAL/NumPy; output-only reprojection agrees within 1e-5 m in EPSG:3857. "
        "Rounded statistics/samples, nulls, counts and source attributes must agree exactly.",
    )
    if missing_modules("osgeo", "numpy", "shapely"):
        suite.skipped = "needs osgeo, numpy and shapely"
        return suite
    code, out, err = run([
        sys.executable, str(SCRIPTS / "compare_analysis_output_with_gdal.py"), ctx.data,
        "--terrio", ctx.terrio,
    ])
    for line in out.splitlines():
        if line.startswith("PASS "):
            name, _, detail = line[5:].partition(": ")
            suite.cases.append(Case("analysis-output", name, TOLERANCE, detail))
    if code != 0 or len(suite.cases) != 6:
        suite.error = tail(err or out)
    return suite


def suite_footprint(ctx):
    """Source attributes and multipart/singlepart footprints on real rasters."""
    suite = Suite(
        "footprint", "Raster footprints", "GDAL Footprint and OGR",
        "DEM and Landsat footprints with added nodata gaps, union/intersection and multipart/singlepart, "
        "written as Shapefiles. Feature counts, source paths, custom text and CRS must match; "
        "symmetric-difference area <= max(1e-6, reference area * 1e-12).",
    )
    if missing_modules("osgeo"):
        suite.skipped = "needs osgeo"
        return suite
    code, out, err = run([
        sys.executable, str(SCRIPTS / "compare_processing_with_gdal.py"), ctx.data,
        "--terrio", ctx.terrio, "--only-footprint",
    ])
    for line in out.splitlines():
        if line.startswith("PASS "):
            suite.cases.append(Case("footprint", line[5:].split(":", 1)[0], TOLERANCE, line.split(":", 1)[1].strip()))
    if code != 0 or len(suite.cases) != 8:
        suite.error = tail(err or out)
    return suite


def suite_topology(ctx):
    """Vector topology on real counties and roads against GDAL/GEOS."""
    s = Suite(
        "topology",
        "Vector topology",
        "GDAL/OGR with GEOS",
        "Tennessee counties and Las Vegas roads, projected into metre-based CRSs: source "
        "attributes, validity, and geometry coverage by source IDs. Relative symmetric-"
        "difference area <= 1e-7 for topology and <= 5e-4 for tessellated buffers. "
        "Collapsed polygon linework is excluded from the area comparison.",
    )
    if missing_modules("osgeo"):
        s.skipped = "needs osgeo with GEOS"
        return s
    if any(
        not os.path.isfile(os.path.join(ctx.data, name))
        for name in ("TN_Counties.geojson", "las_vegas_roads.geojson")
    ):
        s.skipped = (
            "needs Tennessee counties and Las Vegas roads (scripts/fetch_test_data.sh)"
        )
        return s
    with tempfile.TemporaryDirectory(prefix="terrio-topology-") as tmp:
        report = os.path.join(tmp, "report.json")
        code, out, err = run(
            [
                sys.executable,
                str(SCRIPTS / "compare_topology_with_gdal.py"),
                ctx.data,
                "--terrio",
                ctx.terrio,
                "--work-dir",
                os.path.join(tmp, "outputs"),
                "--json",
                report,
            ]
        )
        if code != 0:
            s.error = tail(err or out)
            return s
        try:
            results = json.loads(Path(report).read_text())["cases"]
        except (OSError, KeyError, json.JSONDecodeError) as e:
            s.error = str(e)
            return s
        for r in results:
            name = r["name"]
            group = (
                "make-valid" if name.startswith("make-valid") else name.split("-")[0]
            )
            detail = ""
            if r["status"] == TOLERANCE:
                detail = f"relative symmetric-difference area {r['max_relative_symmetric_difference']:.3g} (limit {r['tolerance']:.3g})"
            s.cases.append(Case(group, name, r["status"], detail))
    return s


def suite_vector_geom(ctx):
    """Vector geometry commands on real data against gdal vector and GEOS."""
    s = Suite(
        "vector-geom",
        "Vector geometry commands",
        "gdal vector, OGR and GEOS",
        "Tennessee and US counties, Las Vegas roads and Natural Earth countries and places: "
        "simplify, segmentize, swap-xy and set-geom-type against gdal vector, coordinate by "
        "coordinate; convex hulls by canonical WKB, concave hulls against OGR/GEOS by their edges (relative "
        "symmetric-difference area <= 0.2 where erosion order differs); combine and sort "
        "against GDAL 3.13's algorithms (GEOS's STR tree, GDAL's Hilbert code), ties aside.",
    )
    if missing_modules("osgeo"):
        s.skipped = "needs osgeo with GEOS"
        return s
    if not shutil.which("gdal"):
        s.skipped = "needs the gdal command (GDAL >= 3.12)"
        return s
    from osgeo import gdal

    if int(gdal.VersionInfo("VERSION_NUM")) < 3120000:
        s.skipped = "needs GDAL >= 3.12 for the vector geometry commands"
        return s
    needed = ("TN_Counties.geojson", "UScounties.fgb", "las_vegas_roads.geojson", "ne_countries.zip", "ne_places.zip")
    if any(not os.path.isfile(os.path.join(ctx.data, name)) for name in needed):
        s.skipped = "needs " + ", ".join(needed) + " (scripts/fetch_test_data.sh)"
        return s
    with tempfile.TemporaryDirectory(prefix="terrio-vector-geom-") as tmp:
        report = os.path.join(tmp, "report.json")
        code, out, err = run(
            [
                sys.executable,
                str(SCRIPTS / "compare_vector_geom_with_gdal.py"),
                ctx.data,
                "--terrio",
                ctx.terrio,
                "--work-dir",
                os.path.join(tmp, "outputs"),
                "--json",
                report,
            ]
        )
        if code != 0:
            s.error = tail(err or out)
            return s
        try:
            results = json.loads(Path(report).read_text())["cases"]
        except (OSError, KeyError, json.JSONDecodeError) as e:
            s.error = str(e)
            return s
        for r in results:
            name = r["name"]
            group = next(g for g in ("simplify", "segmentize", "swap-xy", "set-geom-type", "convert", "convex-hull", "concave-hull", "combine", "sort") if name.startswith(g))
            detail = ""
            if r["status"] != IDENTICAL:
                detail = ", ".join(f"{k} {v:.3g}" if isinstance(v, float) else f"{k} {v}" for k, v in r.items() if k not in ("name", "status"))
            s.cases.append(Case(group, name, r["status"], detail))
    return s


SUITES = OrderedDict(
    [
        ("summaries", suite_summaries),
        ("subdatasets", suite_subdatasets),
        ("histograms", suite_histograms),
        ("vector-roundtrip", suite_vector_roundtrip),
        ("cad", suite_cad),
        ("shapefile-archives", suite_shapefile_archives),
        ("raster-roundtrip", suite_raster_roundtrip),
        ("windows", suite_windows),
        ("ops", suite_ops),
        ("warp-policy", suite_warp_policy),
        ("benchmark-metadata", suite_benchmark_metadata),
        ("benchmark-compat", suite_benchmark_compat),
        ("catalogs", suite_catalogs),
        ("vrt", suite_vrt),
        ("overviews", suite_overviews),
        ("geolocation", suite_geolocation),
        ("dem-calc", suite_dem_calc),
        ("pansharpen", suite_pansharpen),
        ("focal", suite_focal),
        ("vector-ops", suite_vector_ops),
        ("multidim", suite_multidim),
        ("topology", suite_topology),
        ("vector-geom", suite_vector_geom),
        ("footprint", suite_footprint),
        ("analysis-output", suite_analysis_output),
        ("arrow", suite_arrow),
        ("gpkg", suite_gpkg),
        ("streams", suite_streams),
        ("tiles", suite_tiles),
        ("grib", suite_grib),
        ("pointclouds", suite_pointclouds),
        ("lazy-geotiff", suite_lazy_geotiff),
        ("crs", suite_crs),
        ("geocentric", suite_geocentric),
        ("perspective", suite_perspective),
        ("projjson", suite_projjson),
        ("iau", suite_iau),
        ("crs-grids", suite_crs_grids),
    ]
)


def tally(cases):
    """Counts cases by status.

    Args:
        cases: The cases.

    Returns:
        A ``(total, identical, within tolerance, differing)`` tuple.
    """
    n = len(cases)
    same = sum(1 for c in cases if c.status == IDENTICAL)
    tol = sum(1 for c in cases if c.status == TOLERANCE)
    return n, same, tol, n - same - tol


def versions(ctx):
    """The versions of the tools compared, for the report header.

    Args:
        ctx: The run's context.

    Returns:
        A list of ``(name, version)`` pairs.
    """
    out = []
    code, rev, _ = run(["git", "rev-parse", "--short", "HEAD"])
    code_v, tv, _ = run([ctx.terrio, "--version"])
    out.append(("terrio", (tv.strip().split()[-1] if code_v == 0 else "?") + (f" ({rev.strip()})" if code == 0 else "")))
    try:
        from osgeo import gdal, osr

        out.append(("GDAL", gdal.__version__))
        out.append(("PROJ", ".".join(str(v) for v in proj_version(osr))))
    except ImportError:
        pass
    if shutil.which("pdal"):
        code, v, _ = run(["pdal", "--version"])
        m = re.search(r"pdal (\S+)", v)
        if m:
            out.append(("PDAL", m[1]))
    if importlib.util.find_spec("laspy"):
        import laspy

        out.append(("laspy", laspy.__version__))
    return out


def proj_version(osr):
    """PROJ's version as GDAL reports it.

    Args:
        osr: GDAL's ``osr`` module.

    Returns:
        ``(major, minor, patch)``.
    """
    return osr.GetPROJVersionMajor(), osr.GetPROJVersionMinor(), osr.GetPROJVersionMicro()


def md_escape(text):
    """Escapes text for a Markdown table cell.

    Args:
        text: The text.

    Returns:
        The escaped text, on one line.
    """
    return text.replace("|", "\\|").replace("\n", " ").strip()


def markdown(suites, ctx, vers):
    """Writes the report.

    Args:
        suites: The suites that ran.
        ctx: The run's context.
        vers: Tool versions, from ``versions``.

    Returns:
        The Markdown text.
    """
    lines = [
        "# Conformance",
        "",
        "<!-- Generated by scripts/conformance.py: do not edit by hand. -->",
        "",
        (
            "terrio's results compared with GDAL's (and laspy's and PDAL's for point clouds) on real-world "
            "files: the files of `scripts/fetch_test_data.sh` (Natural Earth, USGS 3DEP, Sentinel-2, NASA "
            "ECOSTRESS and OPERA, OpenStreetMap and more, with copies GDAL derives in other formats), and "
            "synthetic rasters for the driver round trips. Each case is **identical** to the reference, "
            "**within tolerance** (the suite says which), or **differs**. Regenerate this page with:"
        ),
        "",
        "```sh",
        "cargo build --release",
        "python scripts/conformance.py ../terrio-data --output docs/conformance.md",
        "```",
        "",
        f"Generated on {datetime.date.today().isoformat()} with " + ", ".join(f"{name} {v}" for name, v in vers) + ".",
        "",
        "## Summary",
        "",
        "| Suite | Reference | Cases | Identical | Within tolerance | Differs |",
        "|---|---|---:|---:|---:|---:|",
    ]
    totals = [0, 0, 0, 0]
    for s in suites:
        link = f"[{s.title}](#{re.sub(r'[^a-z0-9 -]', '', s.title.lower()).replace(' ', '-')})"
        if s.skipped or s.error:
            note = f"skipped: {s.skipped}" if s.skipped else "could not run"
            lines.append(f"| {link} | {s.reference} | {note} | | | |")
            continue
        n, same, tol, diff = tally(s.cases)
        totals = [a + b for a, b in zip(totals, (n, same, tol, diff), strict=True)]
        lines.append(f"| {link} | {s.reference} | {n:,} | {same:,} | {tol:,} | {diff:,} |")
    lines.append(f"| **All** | | **{totals[0]:,}** | **{totals[1]:,}** | **{totals[2]:,}** | **{totals[3]:,}** |")
    for s in suites:
        lines += ["", f"## {s.title}", "", s.criteria, ""]
        if s.skipped:
            lines += [f"Skipped: {s.skipped}."]
            continue
        if s.error:
            lines += ["The suite could not run:", "", "```text", s.error, "```"]
            continue
        lines += ["| Format or operation | Cases | Identical | Within tolerance | Differs |", "|---|---:|---:|---:|---:|"]
        groups = OrderedDict()
        for c in s.cases:
            groups.setdefault(c.group, []).append(c)
        for group, cases in groups.items():
            n, same, tol, diff = tally(cases)
            lines.append(f"| {md_escape(group)} | {n:,} | {same:,} | {tol:,} | {diff:,} |")
        others = [c for c in s.cases if c.status != IDENTICAL]
        if others:
            limit = ctx.max_details
            lines += ["", "Cases that are not identical:", "", "| Format or operation | Case | Result | Details |", "|---|---|---|---|"]
            # Differences first, then the cases within tolerance.
            for c in sorted(others, key=lambda c: c.status != DIFFERS)[:limit]:
                lines.append(f"| {md_escape(c.group)} | {md_escape(c.name)} | {c.status} | {md_escape(c.detail)[:300]} |")
            if len(others) > limit:
                lines.append(f"| | ... and {len(others) - limit:,} more | | |")
    return "\n".join(lines) + "\n"


def main():
    """Runs the suites and writes the report.

    Returns:
        The exit status.
    """
    ap = argparse.ArgumentParser(
        description=__doc__.splitlines()[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="Suites: " + ", ".join(SUITES)
    )
    ap.add_argument("data", help="the test data directory (scripts/fetch_test_data.sh)")
    ap.add_argument("--terrio", default=str(ROOT / "target" / "release" / "terrio"), help="the terrio executable")
    ap.add_argument("--output", "-o", help="write the Markdown report here (default: standard output)")
    ap.add_argument("--json", help="also write every case as JSON here")
    ap.add_argument("--only", help="run only these suites (comma-separated)")
    ap.add_argument("--skip", help="skip these suites (comma-separated)")
    ap.add_argument("--jobs", "-j", type=int, default=3, help="suites run at once (default 3)")
    ap.add_argument("--remote", action="store_true", help="also compare windowed reads of public COGs over HTTP")
    ap.add_argument("--max-details", type=int, default=60, help="non-identical cases listed per suite (default 60)")
    ap.add_argument("--strict", action="store_true", help="exit non-zero when any case differs")
    a = ap.parse_args()

    names = list(SUITES)
    if a.only:
        names = [n.strip() for n in a.only.split(",")]
    if a.skip:
        names = [n for n in names if n not in {s.strip() for s in a.skip.split(",")}]
    unknown = [n for n in names if n not in SUITES]
    if unknown:
        ap.error(f"unknown suite(s) {', '.join(unknown)} (suites: {', '.join(SUITES)})")
    if not os.path.isdir(a.data):
        ap.error(f"{a.data} is not a directory")
    if not os.path.exists(a.terrio):
        ap.error(f"{a.terrio} does not exist: run `cargo build --release` first, or pass --terrio")

    ctx = argparse.Namespace(data=a.data, terrio=os.path.abspath(a.terrio), remote=a.remote, max_details=a.max_details)
    ctx.names = format_names(ctx.terrio)
    gdal_missing = missing_modules("osgeo", "numpy")

    def go(name):
        start = datetime.datetime.now()
        if gdal_missing and name not in ("pointclouds", "crs"):
            fn = SUITES[name]
            s = Suite(name, name, "", fn.__doc__ or "")
            s.skipped = "needs the Python modules " + ", ".join(gdal_missing)
        else:
            try:
                s = SUITES[name](ctx)
            except Exception as e:  # noqa: BLE001 - report the suite as broken and go on
                s = Suite(name, name, "", "")
                s.error = f"{type(e).__name__}: {e}"
        s.seconds = (datetime.datetime.now() - start).total_seconds()
        n, same, tol, diff = tally(s.cases)
        state = (
            f"skipped ({s.skipped})"
            if s.skipped
            else ("ERROR" if s.error else f"{n} cases: {same} identical, {tol} within tolerance, {diff} differ")
        )
        print(f"{name:<17} {state} [{s.seconds:.0f} s]", file=sys.stderr, flush=True)
        return s

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, a.jobs)) as pool:
        suites = list(pool.map(go, names))

    report = markdown(suites, ctx, versions(ctx))
    if a.output:
        Path(a.output).write_text(report)
    else:
        sys.stdout.write(report)
    if a.json:
        data = [
            {
                "suite": s.key,
                "title": s.title,
                "skipped": s.skipped,
                "error": s.error,
                "seconds": s.seconds,
                "cases": [dataclasses.asdict(c) for c in s.cases],
            }
            for s in suites
        ]
        Path(a.json).write_text(json.dumps(data, indent=1))
    if any(s.error for s in suites):
        return 2
    if a.strict and any(c.status == DIFFERS for s in suites for c in s.cases):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
