"""Write GDAL reference points for checking terrio's EPSG transformations.

For each EPSG geographic 2D or projected CRS, with GDAL's default coordinate operations, writes
tab-separated lines of three kinds:

- ``F code lon lat x y kind unit``: the centre of the CRS's area of use, transformed from WGS 84.
- ``I code x y lon lat``: a point at 30 % of the area of use, transformed to WGS 84 (the input
  ``x y`` comes from GDAL's forward transformation of that point).
- ``X src dst x y x2 y2 kind unit``: the area-of-use centre of ``src`` transformed into another
  CRS ``dst`` whose area of use holds it (one pair per source CRS, picked at random with the
  source code as seed).

``kind`` is ``g`` for geographic output (``unit`` in degrees per unit) or ``p`` for projected
output (``unit`` in metres per unit), so that errors can be measured in metres.

By default no datum shift grid is used, whatever PROJ has installed, so that the reference does
not depend on the environment. With ``--grids DIR``, GDAL uses the grids of that directory (and
only those, the network being off): the reference terrio is checked against when it finds the
same grids (``epsg_points_grids.tsv`` in the test data, with the grids of
``scripts/proj_grids.txt`` in its ``grids`` directory).

Usage: python scripts/gen_epsg_points.py OUT.tsv [--grids DIR]
"""

import argparse
import multiprocessing
import os
import random
import tempfile
import warnings

ARGS = argparse.ArgumentParser(description="Write GDAL reference points for terrio's EPSG transformations.")
ARGS.add_argument("out", help="the output file")
ARGS.add_argument("--grids", help="a directory of datum shift grids for GDAL to use")
OPTIONS = ARGS.parse_args()

# conda's PROJ activation turns PROJ's network access on, which downloads grids (NAD27 and others)
# and makes the transformations depend on the environment: the network stays off, and PROJ
# searches an empty directory (or the grid directory) and its database's directory only.
os.environ["PROJ_NETWORK"] = "OFF"

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


def points(code):
    """Compute the reference lines of one CRS.

    Args:
        code: The EPSG code.

    Returns:
        The lines, possibly none.
    """
    w, s, e, n = AREAS[code]
    span = e - w if w <= e else e - w + 360
    lines = []
    try:
        wgs84 = srs_for(4326)
        dst = srs_for(code)
        kind, unit = unit_of(dst)
        fwd = osr.CoordinateTransformation(wgs84, dst)
        inv = osr.CoordinateTransformation(dst, wgs84)
        lon, lat = wrap(w + span / 2), (s + n) / 2
        x, y, _ = fwd.TransformPoint(lon, lat)
    except RuntimeError:
        return lines
    lines.append(f"F\t{code}\t{lon!r}\t{lat!r}\t{x!r}\t{y!r}\t{kind}\t{unit!r}\n")
    try:
        x2, y2, _ = fwd.TransformPoint(wrap(w + span * 0.3), s + (n - s) * 0.3)
        lon2, lat2, _ = inv.TransformPoint(x2, y2)
        lines.append(f"I\t{code}\t{x2!r}\t{y2!r}\t{lon2!r}\t{lat2!r}\n")
    except RuntimeError:
        pass
    others = [c for c, b in AREAS.items() if c != code and inside(b, lon, lat)]
    if others:
        other = random.Random(code).choice(sorted(others))
        try:
            o = srs_for(other)
            okind, ounit = unit_of(o)
            xo, yo, _ = osr.CoordinateTransformation(dst, o).TransformPoint(x, y)
            lines.append(f"X\t{code}\t{other}\t{x!r}\t{y!r}\t{xo!r}\t{yo!r}\t{okind}\t{ounit!r}\n")
        except RuntimeError:
            pass
    return lines


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
    with multiprocessing.Pool(initializer=init, initargs=(areas,)) as pool, open(OPTIONS.out, "w") as out:
        for lines in pool.imap(points, sorted(areas), chunksize=16):
            out.writelines(lines)


if __name__ == "__main__":
    main()
