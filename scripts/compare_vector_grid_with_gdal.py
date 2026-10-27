#!/usr/bin/env python3
"""Compare `terrio vector-grid` with `gdal_grid` on real scattered points.

The points are Natural Earth's populated places (geographic; 769 in the United States, more than
GDAL's 100-point quadtree threshold, and 89 in Peru, fewer), GDAL's own gdal_grid test points (a
regular 20 x 20 grid of a test raster's pixels, with many equidistant neighbours) and a subset of
the Autzen lidar (4,400 projected 3D points, gridded from their Z). Every algorithm is compared
with its parameters, search ellipses (rotated or not), point limits and per-quadrant limits, and
the options (Z field and geometry Z, Z offset and factor, WHERE, SQL, several layers, the clipping
rectangle, extent, size, resolution, data types, CRS and band nodata).

GDAL computes the default `invdist` (power 2, no smoothing, no search ellipse) with AVX or SSE in
single precision with approximate reciprocals; it is compared with `GDAL_USE_AVX=NO` and
`GDAL_USE_SSE=NO`, which select GDAL's double-precision code, as terrio always computes. One case
compares the SIMD code too, within a tolerance.
"""

import argparse
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal, osr

from compare_dem_calc_with_gdal import compare, format_row as format_raster_row, run

gdal.UseExceptions()

SCALAR = ["--config", "GDAL_USE_AVX", "NO", "--config", "GDAL_USE_SSE", "NO"]


def prepare(data, work, terrio):
    """Writes the inputs: GeoPackages of GDAL's grid points and of a lidar subset.

    Args:
        data: The test data directory.
        work: A directory for the inputs.
        terrio: The terrio executable.

    Returns:
        A dict of input paths.
    """
    places = str(Path(data) / "ne_places" / "ne_10m_populated_places_simple.shp")
    grid = str(work / "grid.gpkg")
    gdal.VectorTranslate(grid, str(Path(data) / "gdal_utilities" / "grid.vrt"), format="GPKG", layerName="grid")
    lidar_all = str(work / "autzen_all.gpkg")
    run([terrio, "convert", str(Path(data) / "pointcloud" / "autzen_trim.las"), lidar_all])
    lidar = str(work / "autzen.gpkg")
    gdal.VectorTranslate(lidar, lidar_all, format="GPKG", where="fid % 25 = 0", layerName="points", selectFields=[])
    # Two layers, gridded into two bands.
    two = str(work / "two_layers.gpkg")
    gdal.VectorTranslate(two, places, format="GPKG", where="adm0name = 'France'", layerName="france")
    gdal.VectorTranslate(two, places, format="GPKG", where="adm0name = 'Spain'", layerName="spain", accessMode="append")
    return {"places": places, "grid": grid, "lidar": lidar, "two": two}


def cases(inputs):
    """The comparison cases.

    Args:
        inputs: The input paths from ``prepare``.

    Returns:
        A list of dicts with the case name, the GDAL and terrio command lines (with ``{out}``
        standing for the output path) and the tolerance.
    """
    out = []

    def add(name, src, alg, gdal_opts, terrio_opts, tol=0.0, scalar=True, terrio_alg=None):
        cfg = SCALAR if scalar else []
        method = alg.split(":")[0]
        out.append(
            {
                "name": f"{method} {name}",
                "gdal": ["gdal_grid", "-q", *cfg, "-a", alg, *gdal_opts, src, "{out}"],
                "terrio": ["vector-grid", terrio_alg or alg, src, "{out}", *terrio_opts],
                "tol": tol,
            }
        )

    def extent_size(extent, size):
        g = ["-txe", repr(extent[0]), repr(extent[2]), "-tye", repr(extent[1]), repr(extent[3]), "-outsize", str(size[0]), str(size[1])]
        t = ["--extent", *map(repr, extent), "--size", str(size[0]), str(size[1])]
        return g, t

    # Populated places of the United States (769, a quadtree in GDAL), population as the value.
    us = ["-zfield", "pop_max", "-where", "adm0name = 'United States of America'", "-ot", "Float64"]
    us_t = ["--zfield", "pop_max", "--where", "adm0name = 'United States of America'"]
    g, t = extent_size((-125.0, 24.0, -66.0, 50.0), (120, 80))
    us_algorithms = [
        "invdist",
        "invdist:power=3:smoothing=0.1",
        "invdist:radius1=3:radius2=2:max_points=5:min_points=2",
        "invdistnn:radius=4:max_points=8",
        "invdistnn:radius=4:min_points=3:smoothing=0.5",
        "invdistnn:radius=4:min_points_per_quadrant=1:max_points_per_quadrant=3",
        "invdist:radius=4:max_points_per_quadrant=2",
        "average",
        "minimum",
        "maximum",
        "range",
        "count",
        "average:radius1=3:radius2=2",
        "average:radius=3:max_points_per_quadrant=2:max_points=6",
        "average:radius=3:min_points_per_quadrant=1",
        "nearest",
        "nearest:radius=2",
        "nearest:radius1=3:radius2=1:angle=45",
        "minimum:radius=3",
        "maximum:radius1=4:radius2=2:angle=30",
        "range:radius=3:min_points=3",
        "count:radius=3",
        "count:radius=3:min_points_per_quadrant=1",
        "average_distance:radius=3",
        "average_distance:radius=3:max_points_per_quadrant=2",
        "average_distance_pts:radius=3",
        "maximum:radius=4:max_points_per_quadrant=1",
    ]
    for alg in us_algorithms:
        add(f"places US {alg.partition(':')[2] or 'defaults'}", inputs["places"], alg, us + g, us_t + t)
    # Linear interpolation: qhull's and spade's triangles are the same for scattered points,
    # but the order of their vertices (and so the rounding of the interpolation) can differ.
    for alg in ["linear", "linear:radius=0", "linear:radius=2"]:
        add(f"places US {alg.partition(':')[2] or 'defaults'}", inputs["places"], alg, us + g, us_t + t, tol=1e-6)
    # Float32 output rounds those differences away.
    add("places US Float32", inputs["places"], "linear", [*us[:-1], "Float32", *g], us_t + t + ["--ot", "Float32"])
    # GDAL 3.12 scans every point for a rotated ellipse (3.13, as terrio, searches its quadtree):
    # sums are taken in another order.
    for alg in ["average:radius1=3:radius2=2:angle=20", "average_distance:radius1=3:radius2=2:angle=20"]:
        add(f"places US rotated {alg.partition(':')[2]}", inputs["places"], alg, us + g, us_t + t, tol=1e-6)
    # GDAL 3.13 applies the rotation in its quadtree and per-quadrant searches (3.12 refuses
    # rotation with per-quadrant limits).
    if int(gdal.VersionInfo()) >= 3130000:
        for alg in [
            "average:radius1=3:radius2=2:angle=20:max_points_per_quadrant=2",
            "range:radius1=4:radius2=2:angle=50:min_points_per_quadrant=1",
            "average_distance:radius1=4:radius2=2:angle=-30:max_points_per_quadrant=3",
            "count:radius1=4:radius2=1:angle=80:min_points_per_quadrant=1",
        ]:
            add(f"places US rotated {alg.partition(':')[2]}", inputs["places"], alg, us + g, us_t + t)
    # Keyword overrides must remap invdist to invdistnn before checking the radius.
    add(
        "places US keyword quadrant defaults", inputs["places"],
        "invdist:min_points_per_quadrant=1:max_points=5", us + g,
        us_t + t + ["--min-points-per-quadrant", "1", "--max-points", "5"],
        terrio_alg="invdist",
    )
    # GDAL's default SIMD code: single precision with approximate reciprocals.
    add("places US SIMD (GDAL default)", inputs["places"], "invdist", us + g, us_t + t, tol=2000.0, scalar=False)
    # Peru's 89 places: GDAL scans every point, in their order.
    pe = ["-zfield", "pop_max", "-where", "adm0name = 'Peru'"]
    pe_t = ["--zfield", "pop_max", "--where", "adm0name = 'Peru'"]
    g, t = extent_size((-82.0, -19.0, -68.0, 0.0), (70, 95))
    for alg in [
        "invdist:radius1=2:radius2=1:angle=30:max_points=4",
        "average:radius1=2:radius2=1:angle=60",
        "nearest:radius=1",
        "nearest:radius1=2:radius2=1:angle=15",
        "minimum:radius=2",
        "count:radius1=2:radius2=1:angle=75",
        "average_distance:radius1=2:radius2=1:angle=40",
        "average_distance_pts:radius1=2:radius2=1:angle=40",
        "average_distance_pts",
    ]:
        add(f"places Peru {alg.partition(':')[2] or 'defaults'}", inputs["places"], alg, pe + g, pe_t + t)
    # GDAL's grid points: on the nodes, shifted off them and finer, with many equidistant points.
    for label, extent, size in [
        ("on nodes", (440720.0, 3750120.0, 441920.0, 3751320.0), (20, 20)),
        ("shifted", (440721.0, 3750120.0, 441920.0, 3751321.0), (20, 20)),
        ("fine", (440700.0, 3750100.0, 441950.0, 3751350.0), (57, 43)),
    ]:
        g, t = extent_size(extent, size)
        for alg in [
            "invdist",
            "invdist:radius=100:max_points=3",
            "invdistnn:radius=130:max_points_per_quadrant=1",
            "average:radius=61",
            "average:radius=130:max_points_per_quadrant=1:max_points=3",
            "nearest",
            "nearest:radius1=90:radius2=40:angle=10",
            "range:radius=100",
            "count:radius=100",
            "average_distance_pts:radius=100",
        ]:
            add(f"grid points {label} {alg.partition(':')[2] or 'defaults'}", inputs["grid"], alg, ["-ot", "Float64", *g], t)
        # A regular grid's Delaunay triangulation is degenerate: each square's diagonal is a tie,
        # which qhull and spade break differently off the nodes.
        add(f"grid points {label} defaults", inputs["grid"], "linear", ["-ot", "Float64", *g], t, tol=1e-9)
    # The lidar points, valued by their Z.
    g, t = extent_size((636000.0, 848930.0, 637180.0, 849500.0), (236, 114))
    for alg in [
        "invdist",
        "invdist:power=3:radius1=40:radius2=25:angle=35:max_points=10",
        "invdistnn:radius=30",
        "invdistnn:radius=30:min_points_per_quadrant=1:max_points_per_quadrant=2",
        "average:radius=20",
        "nearest",
        "nearest:radius=10",
        "nearest:radius1=15:radius2=8:angle=30",
        "minimum:radius=20",
        "range:radius=25:min_points_per_quadrant=1",
        "count:radius=20",
        "average_distance:radius=20",
        "average_distance_pts:radius=15",
    ]:
        add(f"lidar Z {alg.partition(':')[2] or 'defaults'}", inputs["lidar"], alg, ["-ot", "Float64", *g], t)
    add("lidar Z defaults", inputs["lidar"], "linear", ["-ot", "Float64", *g], t, tol=1e-9)
    add("lidar Z nodata option", inputs["lidar"], "minimum:radius=5:nodata=-9999", ["-ot", "Float64", *g], ["--nodata", "-9999", "--radius", "5", *t], terrio_alg="minimum")
    # Options: Z offset and factor, data types, band nodata, CRS, resolution, default extent and
    # size, SQL, clipping rectangle, and two layers in two bands.
    add(
        "lidar Z offset factor Int16 nodata",
        inputs["lidar"],
        "average:radius=15:nodata=-1",
        ["-z_increase", "-100", "-z_multiply", "10", "-ot", "Int16", *g],
        ["--zoffset", "-100", "--zmultiply", "10", "--ot", "Int16", *t],
    )
    add("lidar Z Byte", inputs["lidar"], "nearest", ["-ot", "Byte", *g], ["--ot", "Byte", *t])
    add("lidar Z Float32 crs", inputs["lidar"], "count:radius=10", ["-ot", "Float32", "-a_srs", "EPSG:2992", *g], ["--ot", "Float32", "--crs", "EPSG:2992", *t])
    add(
        "lidar Z resolution",
        inputs["lidar"],
        "maximum:radius=12",
        ["-ot", "Float64", "-txe", "636000", "636800", "-tye", "848930", "849500", "-tr", "7", "9"],
        ["--extent", "636000", "848930", "636800", "849500", "--resolution", "7", "9"],
    )
    add(
        "places default extent and size",
        inputs["places"],
        "nearest",
        ["-zfield", "scalerank", "-where", "adm0name = 'Peru'", "-ot", "Float64"],
        ["--zfield", "scalerank", "--where", "adm0name = 'Peru'"],
    )
    add(
        "places SQL",
        inputs["places"],
        "average:radius=3",
        # gdal_grid takes the CRS of named layers only, none for -sql: both are given one.
        ["-sql", "SELECT pop_min, pop_max FROM ne_10m_populated_places_simple WHERE adm0name = 'Brazil'", "-zfield", "pop_min", "-ot", "Float64",
         "-a_srs", "EPSG:4326",
         "-txe", "-75", "-33", "-tye", "-35", "6", "-outsize", "60", "60"],
        ["--sql", "SELECT pop_min, pop_max FROM ne_10m_populated_places_simple WHERE adm0name = 'Brazil'", "--zfield", "pop_min", "--crs", "EPSG:4326",
         "--extent", "-75", "-35", "-33", "6", "--size", "60", "60"],
    )
    add(
        "places clipping rectangle",
        inputs["places"],
        "invdist:power=1",
        ["-zfield", "pop_max", "-clipsrc", "-10", "35", "30", "60", "-ot", "Float64", "-txe", "-12", "32", "-tye", "33", "62", "-outsize", "88", "58"],
        ["--zfield", "pop_max", "--bbox", "-10", "35", "30", "60", "--extent", "-12", "33", "32", "62", "--size", "88", "58"],
    )
    add(
        "two layers in two bands",
        inputs["two"],
        "invdistnn:radius=3:max_points=6",
        ["-l", "france", "-l", "spain", "-zfield", "pop_max", "-ot", "Float32", "-txe", "-10", "10", "-tye", "35", "52", "-outsize", "80", "68"],
        ["-l", "france", "-l", "spain", "--zfield", "pop_max", "--ot", "Float32", "--extent", "-10", "35", "10", "52", "--size", "80", "68"],
    )
    # GDAL preserves the declared nodata even if integer conversion saturates empty cells.
    for dtype, nodata in [("Byte", "-1"), ("Int16", "nan"), ("Float32", "-9999.1")]:
        add(
            f"empty {dtype} nodata {nodata}", inputs["grid"], f"nearest:radius=0.001:nodata={nodata}",
            ["-ot", dtype, "-txe", "-200", "-199", "-tye", "-80", "-79", "-outsize", "1", "1"],
            ["--ot", dtype, "--extent", "-200", "-80", "-199", "-79", "--size", "1", "1"],
        )
    add(
        "ignored malformed option", inputs["grid"], "nearest:power=abc:max_points=abc",
        ["-ot", "Float64", "-txe", "0", "20", "-tye", "0", "20", "-outsize", "8", "8"],
        ["--extent", "0", "0", "20", "20", "--size", "8", "8"],
    )
    add(
        "case-insensitive GPKG layer", inputs["two"], "nearest",
        ["-l", "FRANCE", "-zfield", "pop_max", "-ot", "Float64", "-txe", "-10", "10", "-tye", "35", "52", "-outsize", "8", "8"],
        ["-l", "FRANCE", "--zfield", "pop_max", "--extent", "-10", "35", "10", "52", "--size", "8", "8"],
    )
    return out


def format_row(result):
    """Formats raster agreement together with the separately checked CRS."""
    row = format_raster_row(result)
    if "error" in result:
        return row
    return row + (" crs=ok" if result.get("same_crs") else " crs=DIFF")


def main():
    """Runs and reports the comparisons."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("data", help="the test data directory (see scripts/fetch_test_data.sh)")
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--only", help="only the cases whose name contains this")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--keep", help="keep the outputs in this directory")
    args = parser.parse_args()
    work = Path(args.keep or tempfile.mkdtemp(prefix="terrio_vector_grid_"))
    work.mkdir(parents=True, exist_ok=True)
    results = []
    try:
        inputs = prepare(args.data, work, args.terrio)
        for i, case in enumerate(cases(inputs)):
            if args.only and args.only not in case["name"]:
                continue
            gout, tout = str(work / f"gdal_{i}.tif"), str(work / f"terrio_{i}.tif")
            for output in (gout, tout):
                if os.path.exists(output):
                    os.remove(output)
            try:
                run([v.replace("{out}", gout) for v in case["gdal"]])
                run([args.terrio, *[v.replace("{out}", tout) for v in case["terrio"]]])
                result = compare(gout, tout, case["tol"])
                expected, actual = gdal.Open(gout), gdal.Open(tout)
                a, b = expected.GetProjection(), actual.GetProjection()
                result["same_crs"] = (not a and not b) or bool(osr.SpatialReference(a).IsSame(osr.SpatialReference(b)))
                expected = actual = None
            except (RuntimeError, subprocess.CalledProcessError) as exc:
                result = {"error": str(exc)}
            result["name"] = case["name"]
            results.append(result)
            if not args.json:
                print(format_row(result), flush=True)
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    if args.json:
        print(json.dumps(results, indent=1))
    return int(any("error" in result for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
