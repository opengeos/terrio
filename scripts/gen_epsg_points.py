"""Write GDAL reference points for checking terrio's EPSG transformations.

For each EPSG geographic 2D or projected CRS, with GDAL's default coordinate operations, writes
tab-separated lines of three kinds, ``--points`` of each (50 by default):

- ``F code lon lat x y kind unit``: points of the CRS's area of use (its centre, then points drawn
  at random in its bounding box), transformed from WGS 84.
- ``I code x y lon lat``: other random points of the area of use, transformed to WGS 84 (the
  input ``x y`` comes from GDAL's forward transformation of each point).
- ``X src dst x y x2 y2 kind unit``: random points inside the areas of use of both ``src`` and
  another CRS ``dst`` transformed from ``src`` into ``dst``. ``dst`` is one CRS per source CRS
  whose area of use holds the centre of ``src``'s, picked at random with the source code as seed.

``kind`` is ``g`` for geographic output (``unit`` in degrees per unit) or ``p`` for projected
output (``unit`` in metres per unit), so that errors can be measured in metres.

The random points are drawn uniformly in longitude and latitude within the area of use's
bounding box (unwrapped across the antimeridian), from a generator seeded by the code, so that
the file does not depend on the order the processes finish in. The longitudes are Greenwich ones:
GDAL applies the prime meridian of the CRS (Paris, Ferro) itself. A bounding box can exceed the
domain of a projection, and points GDAL cannot transform are left out (their number is printed).

By default no datum shift grid is used, whatever PROJ has installed, so that the reference does
not depend on the environment. With ``--grids DIR``, GDAL uses the grids of that directory (and
only those, the network being off): the reference terrio is checked against when it finds the
same grids (``epsg_points_grids.tsv`` in the test data, with the grids of
``scripts/proj_grids.txt`` in its ``grids`` directory).

With the default 50 points, a file has about 950,000 lines (83 MB) and takes about 10 CPU
minutes, spread over the processors.

Usage: python scripts/gen_epsg_points.py OUT.tsv [--grids DIR] [--points N]
"""

import argparse
import math
import multiprocessing
import os
import random
import tempfile
import warnings

ARGS = argparse.ArgumentParser(description="Write GDAL reference points for terrio's EPSG transformations.")
ARGS.add_argument("out", help="the output file")
ARGS.add_argument("--grids", help="a directory of datum shift grids for GDAL to use")
ARGS.add_argument("--points", type=int, default=50, help="the points of each kind per CRS (default 50)")
OPTIONS = ARGS.parse_args()

# conda's PROJ activation turns PROJ's network access on, which downloads grids (NAD27 and others)
# and makes the transformations depend on the environment: the network stays off, and PROJ
# searches an empty directory (or the grid directory) and its database's directory only.
os.environ["PROJ_NETWORK"] = "OFF"

import numpy as np  # noqa: E402
import pyproj  # noqa: E402
from osgeo import gdal, osr  # noqa: E402
from pyproj.enums import PJType  # noqa: E402

osr.UseExceptions()
PROJ_DB_DIR = pyproj.datadir.get_data_dir().split(os.pathsep)[0]
GRID_DIR = os.path.abspath(OPTIONS.grids) if OPTIONS.grids else tempfile.mkdtemp()
# PROJ's user directory (where projsync and the network cache put grids) is searched too.
os.environ["XDG_DATA_HOME"] = tempfile.mkdtemp()
osr.SetPROJSearchPaths([GRID_DIR, PROJ_DB_DIR])
osr.SetPROJEnableNetwork(False)
pyproj.network.set_network_enabled(False)
gdal.PushErrorHandler("CPLQuietErrorHandler")
warnings.simplefilter("ignore")


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


def unit_of(s):
    """Describe a CRS's output unit.

    Args:
        s: The spatial reference.

    Returns:
        ``("g", degrees per unit)`` or ``("p", metres per unit)``.
    """
    if s.IsGeographic():
        return "g", s.GetAngularUnits() * 180 / 3.141592653589793
    return "p", s.GetLinearUnits()


def wrap(lon):
    """Wrap a longitude into [-180, 180).

    Args:
        lon: The longitude in degrees.

    Returns:
        The wrapped longitude.
    """
    return (lon + 180) % 360 - 180


AREAS = {}


def init(areas):
    """Set up a worker process.

    Args:
        areas: The areas of use of all CRSs, by EPSG code.
    """
    AREAS.update(areas)


def inside(b, lon, lat):
    """Tell whether an area of use holds a point.

    Args:
        b: The area as ``(west, south, east, north)``, west greater than east across the
            antimeridian.
        lon: The longitude in degrees.
        lat: The latitude in degrees.

    Returns:
        Whether the point is inside.
    """
    w, s, e, n = b
    if not s <= lat <= n:
        return False
    return w <= lon <= e if w <= e else (lon >= w or lon <= e)


def transform(ct, pts):
    """Transform points with GDAL as ``TransformPoint`` does, many at a time.

    ``TransformPoints`` takes a coordinate epoch of 0 for points without one, which
    time-dependent transformations (such as ITRF96 to GDA94) apply over two thousand years: the
    epoch is left unknown (infinite), as ``TransformPoint`` leaves it.

    Args:
        ct: The coordinate transformation.
        pts: The points, as ``(x, y)`` pairs.

    Returns:
        The transformed ``(x, y)`` of each point, ``None`` where GDAL could not compute it.
    """
    if not pts:
        return []
    out = ct.TransformPoints([(x, y, 0.0, math.inf) for x, y in pts])
    return [(p[0], p[1]) if math.isfinite(p[0]) and math.isfinite(p[1]) else None for p in out]


def points(code):
    """Compute the reference lines of one CRS.

    Args:
        code: The EPSG code.

    Returns:
        ``(lines, dropped)``: the lines, possibly none, and how many points GDAL could not
        transform.
    """
    n_points = OPTIONS.points
    w, s, e, n = AREAS[code]
    span = e - w if w <= e else e - w + 360
    rng = np.random.default_rng(code)

    def draw(k):
        """Draw ``k`` random points in the area of use's bounding box, as (lon, lat) pairs."""
        lon = wrap(w + rng.uniform(0, span, k))
        return list(zip(lon.tolist(), rng.uniform(s, n, k).tolist()))

    lines, dropped = [], 0
    try:
        wgs84 = srs_for(4326)
        dst = srs_for(code)
        kind, unit = unit_of(dst)
        fwd = osr.CoordinateTransformation(wgs84, dst)
        inv = osr.CoordinateTransformation(dst, wgs84)
    except RuntimeError:
        return lines, dropped
    centre = (wrap(w + span / 2), (s + n) / 2)
    geo = [centre] + draw(n_points - 1)
    try:
        xy = transform(fwd, geo)
    except RuntimeError:
        return lines, dropped
    for (lon, lat), p in zip(geo, xy):
        if p:
            lines.append(f"F\t{code}\t{lon!r}\t{lat!r}\t{p[0]!r}\t{p[1]!r}\t{kind}\t{unit!r}\n")
        else:
            dropped += 1
    try:
        keep = [p for p in transform(fwd, draw(n_points)) if p]
        dropped += n_points - len(keep)
        for (x2, y2), q in zip(keep, transform(inv, keep)):
            if q:
                lines.append(f"I\t{code}\t{x2!r}\t{y2!r}\t{q[0]!r}\t{q[1]!r}\n")
            else:
                dropped += 1
    except RuntimeError:
        pass
    others = [c for c, b in AREAS.items() if c != code and inside(b, *centre)]
    if others:
        other = random.Random(code).choice(sorted(others))
        # The points of both areas: draws from the source's, kept when the other's holds them
        # (the centre does, so there is always one).
        both, tries = [centre], 0
        while len(both) < n_points and tries < 20:
            both += [p for p in draw(n_points) if inside(AREAS[other], *p)]
            tries += 1
        both = both[:n_points]
        try:
            o = srs_for(other)
            okind, ounit = unit_of(o)
            keep = [p for p in transform(fwd, both) if p]
            dropped += len(both) - len(keep)
            for (x, y), q in zip(keep, transform(osr.CoordinateTransformation(dst, o), keep)):
                if q:
                    lines.append(f"X\t{code}\t{other}\t{x!r}\t{y!r}\t{q[0]!r}\t{q[1]!r}\t{okind}\t{ounit!r}\n")
                else:
                    dropped += 1
        except RuntimeError:
            pass
    return lines, dropped


def main():
    """Write the reference points."""
    codes = set()
    for kind in (PJType.GEOGRAPHIC_2D_CRS, PJType.PROJECTED_CRS):
        codes.update(int(c) for c in pyproj.get_codes("EPSG", kind, allow_deprecated=True))
    areas = {}
    for code in sorted(codes):
        try:
            aou = pyproj.CRS.from_epsg(code).area_of_use
        except pyproj.exceptions.CRSError:
            continue
        if aou is not None:
            areas[code] = (aou.west, aou.south, aou.east, aou.north)
    dropped = 0
    with multiprocessing.Pool(initializer=init, initargs=(areas,)) as pool, open(OPTIONS.out, "w") as out:
        for lines, d in pool.imap(points, sorted(areas), chunksize=16):
            out.writelines(lines)
            dropped += d
    print(f"{len(areas)} CRSs, {OPTIONS.points} points of each kind; {dropped} points GDAL cannot transform left out")


if __name__ == "__main__":
    main()
