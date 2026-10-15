#!/usr/bin/env python3
"""Compare terrio's datum shift grids, geoid models and time-dependent transformations with GDAL.

Runs the same points through ``terrio crs`` and GDAL (``osr.CoordinateTransformation``, with PROJ
searching the same grid directories and its network access off) and reports, per case, how many
points were compared, how many only one of them could transform, and the largest and mean
differences, horizontally (metres) and in height.

Cases:

- every horizontal shift grid (NTv2 ``.gsb`` and PROJ GeoTIFF grids with latitude and longitude
  offsets) of the test data's ``grids`` and ``grids_legacy`` directories, through a PROJ string
  CRS with ``+nadgrids=``, to and from WGS 84 (PROJ's ``hgridshift``, forward and iterated inverse);
- geoid models (GeoTIFF and GTX) through ``+geoidgrids=`` CRSs to WGS 84 3D;
- EPSG transformations using NADCON5 grids (``gridshift``, biquadratic), French geocentric
  translation grids (``xyzgridshift``), compound CRSs with geoid models, and time-dependent
  Helmert transformations with coordinate epochs.

Usage:
    python scripts/compare_crs_grids_with_gdal.py ../terrio-data [--terrio target/release/terrio]
        [--points 500]

Needs GDAL's Python bindings (``osgeo``) with NumPy, and the grids fetched by
``scripts/fetch_test_data.sh`` (``scripts/proj_grids.txt``).
"""

import argparse
import math
import os
import subprocess
import sys
import tempfile

import numpy as np

ARGS = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ARGS.add_argument("data", help="the test data directory, with grids/ and grids_legacy/")
ARGS.add_argument("--terrio", default="target/release/terrio", help="the terrio executable")
ARGS.add_argument("--points", type=int, default=500, help="random points per case")
OPTIONS = ARGS.parse_args()

GRIDS = os.path.abspath(os.path.join(OPTIONS.data, "grids"))
LEGACY = os.path.abspath(os.path.join(OPTIONS.data, "grids_legacy"))
# PROJ (in GDAL) searches the same grid directories as terrio, with its network access off and
# its user directory empty, so that both use the same grids and only them.
os.environ["PROJ_NETWORK"] = "OFF"
os.environ["XDG_DATA_HOME"] = tempfile.mkdtemp()

from osgeo import gdal, osr  # noqa: E402

gdal.UseExceptions()
osr.UseExceptions()
# The directory of PROJ's database: PROJ_DATA's (or PROJ_LIB's) holding proj.db, else PROJ's own.
PROJ_DB_DIRS = [
    d
    for d in (os.environ.get("PROJ_DATA") or os.environ.get("PROJ_LIB") or "").split(os.pathsep) + (osr.GetPROJSearchPaths() or [])
    if d and os.path.exists(os.path.join(d, "proj.db"))
]
osr.SetPROJSearchPaths([GRIDS, LEGACY] + PROJ_DB_DIRS[:1])
osr.SetPROJEnableNetwork(False)


def srs(definition, epoch=None):
    """Make a GDAL spatial reference in traditional GIS axis order.

    Args:
        definition: An EPSG code, ``EPSG:4326+5773`` or a PROJ string.
        epoch: The coordinate epoch, for a dynamic CRS.

    Returns:
        The spatial reference.
    """
    s = osr.SpatialReference()
    s.SetFromUserInput(definition)
    s.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    if epoch is not None:
        s.SetCoordinateEpoch(epoch)
    return s


def gdal_transform(src, dst, points, s_epoch=None, t_epoch=None):
    """Transform points with GDAL.

    Args:
        src: The source CRS.
        dst: The target CRS.
        points: ``(x, y, z)`` tuples.
        s_epoch: The source coordinate epoch.
        t_epoch: The target coordinate epoch.

    Returns:
        The transformed points, ``None`` for those GDAL cannot transform.
    """
    ct = osr.CoordinateTransformation(srs(src, s_epoch), srs(dst, t_epoch))
    out = []
    for p in points:
        try:
            r = ct.TransformPoint(*p)
            out.append(r if all(math.isfinite(v) for v in r) else None)
        except RuntimeError:
            out.append(None)
    return out


def terrio_transform(src, dst, points, s_epoch=None, t_epoch=None):
    """Transform points with ``terrio crs``, one point per line on its standard input.

    Args:
        src: The source CRS.
        dst: The target CRS.
        points: ``(x, y, z)`` tuples.
        s_epoch: The source coordinate epoch.
        t_epoch: The target coordinate epoch.

    Returns:
        The transformed points, ``None`` for those terrio cannot transform.
    """
    env = dict(os.environ, TERRIO_GRID_DIR=os.pathsep.join([GRIDS, LEGACY]), TERRIO_NETWORK="OFF")
    env.pop("PROJ_DATA", None)
    env.pop("PROJ_LIB", None)
    cmd = [OPTIONS.terrio, "crs", src, "--to", dst]
    if s_epoch is not None:
        cmd += ["--s_coord_epoch", repr(s_epoch)]
    if t_epoch is not None:
        cmd += ["--t_coord_epoch", repr(t_epoch)]
    text = "".join(f"{x!r} {y!r} {z!r}\n" for x, y, z in points)
    p = subprocess.run(cmd, input=text, capture_output=True, text=True, env=env, check=True)
    out = []
    for line in p.stdout.splitlines():
        v = line.split()
        out.append(tuple(float(x) for x in v) if len(v) == 3 else None)
    return out


def distance(a, b, geographic):
    """The horizontal distance between two points, in metres.

    Args:
        a: A point.
        b: Another point.
        geographic: Whether the coordinates are longitudes and latitudes in degrees.

    Returns:
        The distance.
    """
    if geographic:
        dx = (a[0] - b[0]) * math.cos(math.radians(b[1])) * 111320.0
        dy = (a[1] - b[1]) * 111320.0
        return math.hypot(dx, dy)
    return math.hypot(a[0] - b[0], a[1] - b[1])


ROWS = []


def compare(name, src, dst, points, geographic=True, s_epoch=None, t_epoch=None):
    """Compare terrio and GDAL on points and record a report row.

    Args:
        name: The case's name.
        src: The source CRS.
        dst: The target CRS.
        points: ``(x, y, z)`` tuples.
        geographic: Whether the target coordinates are degrees.
        s_epoch: The source coordinate epoch.
        t_epoch: The target coordinate epoch.
    """
    g = gdal_transform(src, dst, points, s_epoch, t_epoch)
    t = terrio_transform(src, dst, points, s_epoch, t_epoch)
    both = [(a, b) for a, b in zip(t, g) if a is not None and b is not None]
    only = sum(1 for a, b in zip(t, g) if (a is None) != (b is None))
    dh = [distance(a, b, geographic) for a, b in both]
    dz = [abs(a[2] - b[2]) for a, b in both if len(a) > 2]
    row = (name, len(both), only, max(dh, default=0.0), float(np.mean(dh)) if dh else 0.0, max(dz, default=0.0))
    ROWS.append(row)
    print(f"{name:58s} {row[1]:5d} {row[2]:4d} {row[3]:11.3e} {row[4]:11.3e} {row[5]:11.3e}", flush=True)


def grid_extent(path):
    """The extent of a grid's first image, in degrees.

    Args:
        path: The grid file.

    Returns:
        ``(west, south, east, north)``, or ``None`` for a projected or unreadable grid.
    """
    try:
        ds = gdal.Open(path)
    except RuntimeError:
        return None
    gt = ds.GetGeoTransform()
    if ds.GetSpatialRef() is not None and ds.GetSpatialRef().IsProjected():
        return None
    w, n = gt[0], gt[3]
    e, s = w + gt[1] * ds.RasterXSize, n + gt[5] * ds.RasterYSize
    return (w, min(s, n), e, max(s, n))


def horizontal_grid(path):
    """Tell whether a grid holds horizontal offsets PROJ's ``hgridshift`` applies.

    Args:
        path: The grid file.

    Returns:
        Whether it does.
    """
    if path.endswith(".gsb"):
        return True
    try:
        ds = gdal.Open(path)
    except RuntimeError:
        return False
    md = ds.GetMetadata()
    descriptions = [ds.GetRasterBand(i + 1).GetDescription() for i in range(ds.RasterCount)]
    return md.get("TYPE") == "HORIZONTAL_OFFSET" and "latitude_offset" in descriptions


def random_points(rng, extent, n, z=0.0, margin=0.02):
    """Random points within an extent.

    Args:
        rng: The random generator.
        extent: ``(west, south, east, north)``.
        n: The number of points.
        z: The height of the points.
        margin: The fraction of the extent left out along each edge.

    Returns:
        ``(x, y, z)`` tuples.
    """
    w, s, e, nn = extent
    dx, dy = (e - w) * margin, (nn - s) * margin
    xs = rng.uniform(w + dx, e - dx, n)
    ys = rng.uniform(max(s + dy, -89.0), min(nn - dy, 89.0), n)
    return [(float(x), float(y), z) for x, y in zip(xs, ys)]


def main():
    """Run every case and print the report."""
    rng = np.random.default_rng(42)
    print(f"{'case':58s} {'pts':>5s} {'only':>4s} {'max (m)':>11s} {'mean (m)':>11s} {'max dz (m)':>11s}")
    files = sorted(os.listdir(GRIDS)) + sorted(f"legacy/{f}" for f in os.listdir(LEGACY))
    for f in files:
        path = os.path.join(LEGACY, f[7:]) if f.startswith("legacy/") else os.path.join(GRIDS, f)
        name = os.path.basename(path)
        if not horizontal_grid(path):
            continue
        extent = grid_extent(path)
        if extent is None:
            continue
        proj = f"+proj=longlat +ellps=WGS84 +nadgrids={name} +no_defs +type=crs"
        pts = random_points(rng, extent, OPTIONS.points)
        compare(f"hgridshift {name}", proj, "EPSG:4326", pts)
        compare(f"hgridshift {name} (inverse)", "EPSG:4326", proj, pts)
    # Geoid models.
    for name in ("us_nga_egm96_15.tif", "us_noaa_g2018u0.tif", "uk_os_OSGM15_GB.tif", "egm96_15.gtx"):
        path = os.path.join(LEGACY if name.endswith(".gtx") else GRIDS, name)
        extent = (-179.9, -85.0, 179.9, 85.0) if name.endswith(".gtx") else grid_extent(path)
        proj = f"+proj=longlat +datum=WGS84 +geoidgrids={name} +vunits=m +no_defs +type=crs"
        pts = random_points(rng, extent, OPTIONS.points, z=100.0)
        compare(f"vgridshift {name}", proj, "EPSG:4979", pts)
        compare(f"vgridshift {name} (inverse)", "EPSG:4979", proj, pts)
    # EPSG transformations with grids, compound CRSs and coordinate epochs.
    cases = [
        ("NADCON5 NAD83(HARN) to NAD83(NSRS2007) (EPSG:4152 to 4759)", "EPSG:4152", "EPSG:4759", (-124, 25, -67, 49), 0.0, None, None),
        ("NADCON5 NAD27 to NAD83(2011) (EPSG:4267 to 6318)", "EPSG:4267", "EPSG:6318", (-124, 25, -67, 49), 0.0, None, None),
        ("NTF to RGF93 (xyzgridshift, EPSG:4275 to 4171)", "EPSG:4275", "EPSG:4171", (-4.5, 42.5, 8, 51), 0.0, None, None),
        ("NAD27 to NAD83 (EPSG:4267 to 4269)", "EPSG:4267", "EPSG:4269", (-124, 25, -67, 49), 0.0, None, None),
        ("OSGB36 to ETRS89 (OSTN15, EPSG:4277 to 4258)", "EPSG:4277", "EPSG:4258", (-5.5, 50.2, 1.5, 58.5), 0.0, None, None),
        ("DHDN to ETRS89 (BETA2007, EPSG:4314 to 4258)", "EPSG:4314", "EPSG:4258", (6, 47.3, 15, 55), 0.0, None, None),
        ("WGS 84 3D to EGM96 height (EPSG:4979 to 4326+5773)", "EPSG:4979", "EPSG:4326+5773", (-180, -85, 180, 85), 100.0, None, None),
        ("EGM96 height to WGS 84 3D (EPSG:4326+5773 to 4979)", "EPSG:4326+5773", "EPSG:4979", (-180, -85, 180, 85), 100.0, None, None),
        ("NAD83(2011) 3D to NAVD88 (GEOID18, EPSG:6319 to 6318+5703)", "EPSG:6319", "EPSG:6318+5703", (-124, 25, -67, 49), 100.0, None, None),
        ("NAVD88 to NAD83(2011) 3D (GEOID18, EPSG:6318+5703 to 6319)", "EPSG:6318+5703", "EPSG:6319", (-124, 25, -67, 49), 100.0, None, None),
        ("ETRS89 3D to OSGB36 + ODN height (EPSG:4937 to 7405)", "EPSG:4937", "EPSG:7405", (-5.5, 50.2, 1.5, 58.5), 100.0, None, None),
        ("ITRF2014 at 2010.0 to GDA2020 (EPSG:7912 to 7843)", "EPSG:7912", "EPSG:7843", (113, -43, 153, -11), 0.0, 2010.0, None),
        ("ITRF2014 at 2030.5 to GDA2020 (EPSG:9000 to 7844)", "EPSG:9000", "EPSG:7844", (113, -43, 153, -11), 0.0, 2030.5, None),
        ("GDA2020 to ITRF2014 at 2015.0 (EPSG:7843 to 7912)", "EPSG:7843", "EPSG:7912", (113, -43, 153, -11), 0.0, None, 2015.0),
        ("ITRF2014 to ITRF2020 at 2010.0 (EPSG:7912 to 9989)", "EPSG:7912", "EPSG:9989", (-180, -85, 180, 85), 0.0, 2010.0, 2010.0),
        ("ITRF2014 to ITRF2020 at 2022.0 (EPSG:9000 to 9990)", "EPSG:9000", "EPSG:9990", (-180, -85, 180, 85), 0.0, 2022.0, 2022.0),
        ("ITRF2014 to ETRF2000 at 2020.0 (EPSG:7912 to 7931)", "EPSG:7912", "EPSG:7931", (-10, 35, 30, 70), 0.0, 2020.0, 2020.0),
        ("ITRF2014 to ITRF2020 without epoch (EPSG:7912 to 9989)", "EPSG:7912", "EPSG:9989", (-180, -85, 180, 85), 0.0, None, None),
    ]
    for name, src, dst, extent, z, s_epoch, t_epoch in cases:
        geographic = not srs(dst).IsProjected()
        compare(name, src, dst, random_points(rng, extent, OPTIONS.points, z=z), geographic, s_epoch, t_epoch)
    worst = max(ROWS, key=lambda r: max(r[3], r[5]))
    print(f"\n{len(ROWS)} cases, {sum(r[1] for r in ROWS)} points; largest difference {max(worst[3], worst[5]):.3e} m ({worst[0]})")
    if any(r[2] for r in ROWS):
        print("points only one of terrio and GDAL transformed: " + ", ".join(f"{r[0]} ({r[2]})" for r in ROWS if r[2]))
    sys.exit(0)


if __name__ == "__main__":
    main()
