#!/usr/bin/env python3
"""Compare terrio's chains of datum transformations through intermediate datums with GDAL's (PROJ).

Sweeps pairs of EPSG geographic 2D CRSs on two datums that no transformation of PROJ's database
links directly, whose areas of use overlap, so that PROJ chains transformations through one or
several intermediate datums (ED79 to Monte Mario (Rome) through WGS 84 and Monte Mario, AGD66 to
ITRF2014 through GDA94's geographic and geocentric CRSs). With
``--projected``, the pairs of a projected CRS of each datum are swept too. For each pair, points
of the common area (its centre, then random points) are transformed with GDAL
(``osr.CoordinateTransformation``, as ``OGRCreateCoordinateTransformation`` picks the operation)
and with ``terrio crs``, and the distances between the results are reported, with the pairs of
the largest ones and the operation PROJ (pyproj) uses for them.

PROJ's network access is off and both search the same grid directory (an empty one by default,
``--grids DIR`` for the grids of ``scripts/proj_grids.txt``), so that both have the same grids.

Usage:
    python scripts/compare_chains_with_proj.py [--terrio target/release/terrio] [--grids DIR]
        [--points 5] [--projected] [--report pairs.tsv] [--pairs FILE] [--limit N]

It prints, per kind of pair, how many pairs have every point within 1 µm, 1 mm, 1 cm and 1 m of
GDAL's, then the pairs beyond 1 cm. The report has a line per pair: the codes, the number of
points, the largest distance in metres, the point where it is and PROJ's name for the
operation there (a hint: the first operation pyproj lists whose area holds the point).

Needs GDAL's Python bindings, pyproj and NumPy, with GDAL built on PROJ 9.8 (the version
terrio's EPSG table comes from).
"""

import argparse
import math
import multiprocessing
import os
import sqlite3
import subprocess
import tempfile
import warnings

ARGS = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ARGS.add_argument("--terrio", default="target/release/terrio", help="the terrio executable")
ARGS.add_argument("--grids", help="a directory of datum shift grids for both to use")
ARGS.add_argument("--points", type=int, default=5, help="points per pair (default 5)")
ARGS.add_argument("--projected", action="store_true", help="also sweep pairs of projected CRSs")
ARGS.add_argument("--report", help="write every pair's largest distance to this file")
ARGS.add_argument("--pairs", help="sweep only the pairs of this file (lines of 'source target' codes)")
ARGS.add_argument("--limit", type=int, help="sweep at most this many pairs (for a quick run)")
OPTIONS = ARGS.parse_args()

os.environ["PROJ_NETWORK"] = "OFF"
os.environ["XDG_DATA_HOME"] = tempfile.mkdtemp()

import numpy as np  # noqa: E402
import pyproj  # noqa: E402
from osgeo import gdal, osr  # noqa: E402
from pyproj.enums import PJType  # noqa: E402

osr.UseExceptions()
gdal.PushErrorHandler("CPLQuietErrorHandler")
warnings.simplefilter("ignore")
PROJ_DB_DIR = pyproj.datadir.get_data_dir().split(os.pathsep)[0]
GRID_DIR = os.path.abspath(OPTIONS.grids) if OPTIONS.grids else tempfile.mkdtemp()
osr.SetPROJSearchPaths([GRID_DIR, PROJ_DB_DIR])
osr.SetPROJEnableNetwork(False)
pyproj.datadir.set_data_dir(PROJ_DB_DIR)
pyproj.datadir.append_data_dir(GRID_DIR)
pyproj.network.set_network_enabled(False)
TERRIO = os.path.abspath(OPTIONS.terrio)
TERRIO_ENV = dict(os.environ, TERRIO_GRID_DIR=GRID_DIR, TERRIO_NETWORK="OFF")
TERRIO_ENV.pop("PROJ_DATA", None)
TERRIO_ENV.pop("PROJ_LIB", None)
# Distances between the results (metres) the summary counts the pairs within.
TOLERANCES = (1e-6, 1e-3, 1e-2, 1.0)


def srs_for(code):
    """Make a GDAL spatial reference for an EPSG code, in traditional GIS axis order.

    Args:
        code: The EPSG code.

    Returns:
        The spatial reference.
    """
    s = osr.SpatialReference()
    s.ImportFromEPSG(code)
    s.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    return s


def intersection(a, b):
    """Intersect two areas of use, ignoring those across the antimeridian.

    Args:
        a: ``(west, south, east, north)`` in degrees.
        b: The same.

    Returns:
        The common area, or ``None`` when it is empty or crosses the antimeridian.
    """
    if a[0] > a[2] or b[0] > b[2]:
        return None
    w, s, e, n = max(a[0], b[0]), max(a[1], b[1]), min(a[2], b[2]), min(a[3], b[3])
    return (w, s, e, n) if w < e and s < n else None


def crss():
    """List the EPSG geographic 2D CRSs with their datums and areas of use.

    Returns:
        ``(geog, datum_links, projected)``: ``{code: (datum, area)}``, the set of datum pairs a
        transformation of PROJ's database links directly (both orders), and a projected CRS of
        each geographic CRS ``{geog: (code, area)}``.
    """
    db = sqlite3.connect(os.path.join(PROJ_DB_DIR, "proj.db"))
    geog = {}
    for code, datum, w, s, e, n in db.execute(
        "SELECT g.code, g.datum_code, a.west_lon, a.south_lat, a.east_lon, a.north_lat FROM geodetic_crs g "
        "JOIN usage u ON u.object_table_name = 'geodetic_crs' AND u.object_auth_name = g.auth_name "
        "AND u.object_code = g.code JOIN extent a ON a.auth_name = u.extent_auth_name AND a.code = u.extent_code "
        "WHERE g.auth_name = 'EPSG' AND g.type = 'geographic 2D' AND g.deprecated = 0 AND g.datum_auth_name = 'EPSG'"
    ):
        geog[int(code)] = (f"EPSG:{datum}", (w, s, e, n))
    links = set()
    for a, b in db.execute(
        "SELECT gs.datum_auth_name || ':' || gs.datum_code, gt.datum_auth_name || ':' || gt.datum_code FROM coordinate_operation_view v "
        "JOIN geodetic_crs gs ON gs.auth_name = v.source_crs_auth_name AND gs.code = v.source_crs_code "
        "JOIN geodetic_crs gt ON gt.auth_name = v.target_crs_auth_name AND gt.code = v.target_crs_code "
        "WHERE v.deprecated = 0"
    ):
        links.add((a, b))
        links.add((b, a))
    projected = {}
    for code, base, w, s, e, n in db.execute(
        "SELECT p.code, p.geodetic_crs_code, a.west_lon, a.south_lat, a.east_lon, a.north_lat FROM projected_crs p "
        "JOIN usage u ON u.object_table_name = 'projected_crs' AND u.object_auth_name = p.auth_name "
        "AND u.object_code = p.code JOIN extent a ON a.auth_name = u.extent_auth_name AND a.code = u.extent_code "
        "WHERE p.auth_name = 'EPSG' AND p.deprecated = 0 AND p.geodetic_crs_auth_name = 'EPSG' ORDER BY p.code"
    ):
        projected.setdefault(int(base), (int(code), (w, s, e, n)))
    return geog, links, projected


def pairs():
    """List the pairs to sweep.

    Returns:
        ``(source, target, area)`` triples: geographic 2D pairs on datums no transformation links
        directly, then (with ``--projected``) the projected CRSs of those pairs, with the area
        their points are drawn in.
    """
    geog, links, projected = crss()
    out = []
    codes = sorted(c for c in geog if c != 4326)
    for i, a in enumerate(codes):
        for b in codes[i + 1 :]:
            (da, aa), (db_, ab) = geog[a], geog[b]
            if da == db_ or (da, db_) in links:
                continue
            area = intersection(aa, ab)
            if area is None:
                continue
            out.append((a, b, area))
    if OPTIONS.projected:
        for a, b, area in list(out):
            if a in projected and b in projected:
                (pa, aa), (pb, ab) = projected[a], projected[b]
                common = intersection(area, aa) and intersection(intersection(area, aa), ab)
                if common:
                    out.append((pa, pb, common))
    if OPTIONS.pairs:
        with open(OPTIONS.pairs) as f:
            keep = {tuple(int(c) for c in line.split()[:2]) for line in f if line.strip()}
        out = [p for p in out if p[:2] in keep]
    return out[: OPTIONS.limit] if OPTIONS.limit else out


def metres(s, a, b):
    """The distance between two points of a CRS, in metres.

    Args:
        s: The CRS's GDAL spatial reference.
        a: ``(x, y)``.
        b: ``(x, y)``.

    Returns:
        The distance (an equirectangular approximation for geographic CRSs).
    """
    if s.IsGeographic():
        deg = s.GetAngularUnits() * 180 / math.pi
        dlat = (a[1] - b[1]) * deg * 111_320.0
        dlon = (a[0] - b[0]) * deg * 111_320.0 * math.cos(math.radians(a[1] * deg))
        return math.hypot(dlon, dlat)
    return math.hypot(a[0] - b[0], a[1] - b[1]) * s.GetLinearUnits()


def compare(pair):
    """Compare one pair's results.

    Args:
        pair: ``(source, target, area)``.

    Returns:
        ``(source, target, compared, largest distance, operation, point)``: the number of points
        both transformed, the largest distance in metres (``inf`` when only one of them
        transformed a point), PROJ's name for the operation of the worst point and that point;
        ``None`` when GDAL cannot make the transformation.
    """
    src, dst, (w, s, e, n) = pair
    rng = np.random.default_rng(src * 100_000 + dst)
    lonlat = [((w + e) / 2, (s + n) / 2)] + list(
        zip(rng.uniform(w, e, OPTIONS.points - 1).tolist(), rng.uniform(s, n, OPTIONS.points - 1).tolist())
    )
    try:
        ss, ds = srs_for(src), srs_for(dst)
        to_src = osr.CoordinateTransformation(srs_for(4326), ss)
        ct = osr.CoordinateTransformation(ss, ds)
    except RuntimeError:
        return None
    pts, expected = [], []
    for lon, lat in lonlat:
        try:
            p = to_src.TransformPoint(lon, lat)[:2] if not ss.IsGeographic() else (lon, lat)
            q = ct.TransformPoint(*p)[:2]
        except RuntimeError:
            continue
        if all(math.isfinite(v) for v in (*p, *q)):
            pts.append(p)
            expected.append(q)
    if not pts:
        return None
    lines = "".join(f"{x!r} {y!r}\n" for x, y in pts)
    out = subprocess.run(
        [TERRIO, "crs", f"EPSG:{src}", "--to", f"EPSG:{dst}"], input=lines, capture_output=True, text=True, env=TERRIO_ENV
    ).stdout.splitlines()
    worst, worst_at = 0.0, 0
    for i, (q, line) in enumerate(zip(expected, out + [""] * len(pts))):
        got = line.split()
        d = metres(ds, q, (float(got[0]), float(got[1]))) if len(got) >= 2 else math.inf
        if d > worst:
            worst, worst_at = d, i
    op = ""
    if worst > 1e-3:
        try:
            # The first operation PROJ lists whose area holds the point.
            lon, lat = lonlat[worst_at] if ss.IsGeographic() else lonlat[0]
            group = pyproj.transformer.TransformerGroup(f"EPSG:{src}", f"EPSG:{dst}", always_xy=True)
            for t in group.transformers:
                a = t.area_of_use
                if a is None or (a.west <= lon <= a.east and a.south <= lat <= a.north):
                    op = t.description
                    break
        except Exception:  # noqa: BLE001 - the name is only a hint
            op = "?"
    return src, dst, len(pts), worst, op, pts[worst_at]


GEOG_CODES = {int(c) for c in pyproj.get_codes("EPSG", PJType.GEOGRAPHIC_2D_CRS, allow_deprecated=True)}


def main():
    """Run the sweep and print the summary."""
    todo = pairs()
    results = []
    with multiprocessing.Pool() as pool:
        for r in pool.imap_unordered(compare, todo, chunksize=8):
            if r:
                results.append(r)
    results.sort()
    geographic = [r for r in results if r[0] in GEOG_CODES]
    groups = [("geographic", geographic)]
    if OPTIONS.projected:
        groups.append(("projected", [r for r in results if r[0] not in GEOG_CODES]))
    for label, rs in groups:
        counts = ", ".join(f"{sum(r[3] <= t for r in rs)} within {t:g} m" for t in TOLERANCES)
        print(f"{label}: {len(rs)} pairs, {sum(r[2] for r in rs)} points: {counts}")
    worst = sorted((r for r in results if r[3] > 1e-2), key=lambda r: -r[3])
    for src, dst, _, d, op, (x, y) in worst[:40]:
        print(f"EPSG:{src} -> EPSG:{dst} at {x!r} {y!r}: {d:.3f} m  {op}")
    if OPTIONS.report:
        with open(OPTIONS.report, "w") as f:
            for src, dst, n, d, op, (x, y) in results:
                f.write(f"{src}\t{dst}\t{n}\t{d!r}\t{x!r}\t{y!r}\t{op}\n")


if __name__ == "__main__":
    main()
