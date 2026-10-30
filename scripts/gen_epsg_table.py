# SPDX-FileCopyrightText: Copyright (c) 2018-2024, Even Rouault, <even.rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (translated to Python and modified):
#   PROJ 9.8.1:
#     src/coord_operation.cpp
#     src/datums.cpp

"""Generate terrio's EPSG table from GDAL and PROJ.

Writes ``crates/terrio-crs/data/epsg.tsv.zst``, a zstd-compressed text table with these lines:

- ``O<TAB>id<TAB>west,south,east,north<TAB>accuracy<TAB>flags<TAB>steps<TAB>name<TAB>steps accuracy<TAB>steps area``: a datum
  transformation, as the geodetic steps of the PROJ pipeline GDAL uses (``cart ellps=intl;helmert
  x=-87 y=-98 z=-121;inv cart ellps=WGS84``, ``hgridshift grids=us_noaa_conus.tif``; empty for a
  null transformation), the area where it applies (in degrees), its accuracy in metres (-1 when
  unknown), flags for how PROJ ranks, chains and runs it (``o``: offshore area, ``u``: unknown
  area, ``b``: ballpark, ``c``: a concatenation PROJ makes, rather than one PROJ's database
  registers, ``h`` and ``g``: one PROJ's database registers between geographic 3D or
  geocentric CRSs, ``p``: one PROJ prefers among those of equal accuracy, ``z``: the pipeline
  keeps heights between 2D CRSs) and PROJ's name for it, the names of its steps joined by `` + ``
  (``NTF (Paris) to NTF (1) + NTF to WGS 84 (1)``), by which PROJ breaks the last ties when it
  sorts the chains it makes, and for a concatenation PROJ's database registers with another
  accuracy or area than its steps', the sum of their accuracies and the intersection of their
  areas, which PROJ gives it when it concatenates it with other operations (in chains, and with
  the projections of projected CRSs; empty when the same).
- ``C<TAB>code<TAB>name<TAB>datum<TAB>geog<TAB>ops<TAB>grid ops<TAB>flags<TAB>area<TAB>PROJ string<TAB>WKT``:
  an EPSG geographic (2D or 3D) or projected CRS with its official name, EPSG datum code, the
  code of its geographic 2D CRS, its datum transformations to WGS 84 in the order GDAL lists them
  when no grid is installed (PROJ picks among them for each point), the list GDAL uses when the
  grids are available (empty when it is the same), flags (``d``: a dynamic CRS, ``3``: a
  geographic 3D CRS), its area of use (``west,south,east,north`` in degrees, which PROJ intersects
  with the transformations' areas), the PROJ string of its coordinate conversion and the WKT
  GDAL writes for it.
- ``P<TAB>geog1<TAB>geog2<TAB>ops<TAB>grid ops``: the transformations GDAL lists between two
  geographic CRSs whose datums have direct transformations between them in the EPSG dataset,
  without and with grids, in the direction geog1 to geog2.
- ``E<TAB>code<TAB>datum<TAB>type<TAB>name``: a geodetic CRS (``2`` or ``3`` for geographic 2D
  or 3D, ``g`` for geocentric) of the EPSG dataset or of NKG's (numbered from 2,000,000,000,
  as their datums are), which PROJ chains transformations through.
- ``R<TAB>key<TAB>source<TAB>target<TAB>op<TAB>replaced by<TAB>flags<TAB>via``: a transformation
  PROJ's database registers between geodetic CRSs of two datums (``EPSG:1321``), from and to the
  CRSs it registers it for, with the ``O`` row that runs it between the geographic coordinates of
  the two datums (framed by the conversions to and from geocentric coordinates for geocentric
  CRSs; its area is its first in PROJ's database), the keys of the transformations that replace
  it between the same CRSs, flags (``t``: time-dependent, ``e``: PROJ's null transformation
  between a datum ensemble and one of its members) and the CRSs a concatenation goes through.
  terrio chains them through intermediate CRSs and datums as PROJ does between CRSs whose datums
  have no direct transformation.
- ``V<TAB>code<TAB>name<TAB>datum<TAB>unit<TAB>direction<TAB>WKT``: a vertical CRS, with its
  unit in metres and axis direction (``up`` or ``down``).
- ``K<TAB>code<TAB>name<TAB>horizontal<TAB>vertical<TAB>WKT``: a compound CRS.
- ``G<TAB>geog3d<TAB>vertical<TAB>ops<TAB>grid ops``: the transformations (geoid models) from a
  geographic 3D CRS to a vertical CRS.
- ``D<TAB>datum<TAB>date``: the publication date of a datum (``2007-01-01``), by which PROJ
  chooses the datums it chains through.
- ``A<TAB>grid<TAB>old name``: the name a PROJ-data grid had before PROJ 7 (``conus`` for
  ``us_noaa_conus.tif``), which terrio also looks for.

The PROJ strings are normalized for terrio: datum shifts (``+towgs84``, ``+nadgrids``) are left
out since the transformations carry them, ``+datum`` is replaced by its ellipsoid, prime meridians
are in degrees, ``+axis`` gives the axis order of GDAL's traditional GIS order and ``+to_deg``
the angular unit of geographic CRSs in other units than degrees.

The lists "with grids" are PROJ's as if every grid of the PROJ CDN (https://cdn.proj.org) were
installed (``--grid-check known_available``); terrio uses them, without the operations whose
grids it cannot find, when at least one operation needing grids can be used.

Deprecated codes get the definition of their replacement, as GDAL substitutes it by default.

Usage: python scripts/gen_epsg_table.py

Needs pyproj, GDAL's Python bindings and the ``zstd`` command.
"""

import json
import os
import re
import sqlite3
import subprocess
import warnings
from concurrent.futures import ThreadPoolExecutor

import pyproj
from osgeo import gdal, osr
from pyproj.enums import PJType

osr.UseExceptions()
gdal.PushErrorHandler("CPLQuietErrorHandler")
warnings.simplefilter("ignore")

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio-crs", "data", "epsg.tsv.zst")

# Steps of a PROJ pipeline that belong to the datum transformation; the others convert units,
# swap axes or (un)project, which terrio does from the CRS's own definition.
DATUM_STEPS = {"cart", "helmert", "molodensky", "molobadekas", "geogoffset", "vertoffset"}

# Steps that apply grids.
GRID_STEPS = {"hgridshift", "vgridshift", "gridshift", "xyzgridshift", "deformation"}

# Steps that neither (un)project nor shift datums.
NEUTRAL_STEPS = {"push", "pop", "noop", "unitconvert", "axisswap", "set"}

# The method of the conversions between geographic and geocentric CRSs.
GEOCENTRIC = "Geographic/geocentric conversions"

# Operations PROJ prefers among those of equal accuracy (``isPriorityOp`` in PROJ's
# coord_operation.cpp), by name.
PRIORITY_OPS = (
    "NAD83 to NAD83(HARN) (47)",
    "NAD83 to NAD83(HARN) (48)",
    "NAD83 to NAD83(HARN) (49)",
    "NAD83 to NAD83(HARN) (50)",
    "GDA94 to WGS 84 (1)",
    "GDA2020 to WGS 84 (2)",
)


def epsg_codes():
    """List the EPSG geographic (2D and 3D) and projected CRS codes, deprecated ones included.

    Returns:
        The sorted codes.
    """
    codes = set()
    for kind in (PJType.GEOGRAPHIC_2D_CRS, PJType.GEOGRAPHIC_3D_CRS, PJType.PROJECTED_CRS):
        codes.update(int(c) for c in pyproj.get_codes("EPSG", kind, allow_deprecated=True))
    return sorted(codes)


def gis_axis(srs, crs):
    """Give the axis directions of a CRS in GDAL's traditional GIS order.

    Args:
        srs: The GDAL spatial reference.
        crs: The pyproj CRS.

    Returns:
        Two letters among ``enws``, such as ``en`` or ``ws``.
    """
    srs = srs.Clone()
    srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    letters = {"east": "e", "west": "w", "north": "n", "south": "s"}
    out = ""
    for m in srs.GetDataAxisToSRSAxisMapping()[:2]:
        d = letters.get(crs.axis_info[abs(m) - 1].direction.lower(), "?")
        if m < 0:
            d = {"e": "w", "w": "e", "n": "s", "s": "n"}.get(d, d)
        out += d
    return out


def normalize_proj(proj, crs, axis):
    """Normalize a PROJ string exported by GDAL for terrio.

    Args:
        proj: The PROJ string from ``ExportToProj4``.
        crs: The pyproj CRS it describes.
        axis: The axis directions in GDAL's traditional GIS order, from ``gis_axis``.

    Returns:
        The PROJ string without datum shifts, with the ellipsoid spelled out, the prime meridian
        in degrees and the angular unit of geographic CRSs.
    """
    tokens = [t for t in proj.split() if t.startswith("+")]
    out = []
    for t in tokens:
        key = t[1:].split("=", 1)[0]
        if key in ("towgs84", "nadgrids", "wktext", "no_defs", "type", "datum", "pm", "axis"):
            continue
        out.append(t)
    # Polar CRSs' axes point "north along 90°E" and the like: those stay east-north.
    if axis != "en" and sorted(axis) in (["e", "n"], ["e", "s"], ["n", "w"], ["s", "w"]):
        out.insert(1, f"+axis={axis}u")
    if "+datum=" in proj and not any(t.startswith(("+ellps=", "+a=")) for t in out):
        e = crs.ellipsoid
        if e.inverse_flattening:
            out.append(f"+a={e.semi_major_metre!r} +rf={e.inverse_flattening!r}")
        else:
            out.append(f"+a={e.semi_major_metre!r} +b={e.semi_major_metre!r}")
    pm = crs.prime_meridian
    if pm.longitude:
        out.append(f"+pm={float(f'{pm.longitude * pm.unit_conversion_factor * 180 / 3.141592653589793:.12g}')!r}")
    if crs.is_geographic:
        unit = crs.axis_info[0].unit_conversion_factor * 180 / 3.141592653589793
        if abs(unit - 1) > 1e-12:
            out.append(f"+to_deg={float(f'{unit:.12g}')!r}")
    return " ".join(out)


def parse_steps(definition):
    """Split a PROJ pipeline into steps.

    Args:
        definition: A PROJ pipeline or single operation, without ``+`` signs.

    Returns:
        A list of ``(inverse, name, params)`` where params is the list of the step's other
        ``key=value`` tokens.
    """
    parts = re.split(r"\bstep\b", definition)
    steps = []
    for part in parts:
        toks = part.split()
        if not toks:
            continue
        inverse = "inv" in toks
        name = next((t[5:] for t in toks if t.startswith("proj=")), None)
        if name is None or name == "pipeline":
            continue
        params = [t for t in toks if t != "inv" and not t.startswith("proj=")]
        steps.append((inverse, name, params))
    return steps


# PROJ's named prime meridians, in degrees.
PRIME_MERIDIANS = {
    "greenwich": 0.0,
    "lisbon": -9.131906111111,
    "paris": 2.337229166667,
    "bogota": -74.080916666667,
    "madrid": -3.687938888889,
    "rome": 12.452333333333,
    "bern": 7.439583333333,
    "jakarta": 106.807719444444,
    "ferro": -17.666666666667,
    "brussels": 4.367975,
    "stockholm": 18.058277777778,
    "athens": 23.7163375,
    "oslo": 10.722916666667,
    "copenhagen": 12.57788,
}


def datum_steps(definition, crs_pm=0.0, target_pm=0.0, z_flips=False):
    """Extract the datum transformation from a PROJ pipeline.

    Args:
        definition: The pipeline from a CRS to WGS 84 (or another geographic CRS).
        crs_pm: The CRS's prime meridian in degrees, which terrio applies itself.
        target_pm: The target CRS's prime meridian in degrees, which terrio also applies.
        z_flips: Keep the axis swaps that turn heights into depths (``axisswap order=1,2,-3``),
            which transformations between vertical CRSs apply between their steps.

    Returns:
        The geodetic steps, ``;``-separated, or ``None`` when the pipeline holds a step terrio
        cannot run (such as a grid applied to projected coordinates).
    """
    steps = parse_steps(definition)
    rotations = [i for i, (_, name, params) in enumerate(steps) if name in ("longlat", "latlong") and any(p.startswith("pm=") for p in params)]
    out = []
    projected = False
    for i, (inverse, name, params) in enumerate(steps):
        if name in DATUM_STEPS or name in GRID_STEPS:
            if projected and name in GRID_STEPS:
                return None
            if name == "helmert" and any(p.startswith("theta=") for p in params):
                return None
            out.append(("inv " if inverse else "") + " ".join([name] + params))
        elif name in ("longlat", "latlong"):
            if i not in rotations:
                continue
            pm = next(p[3:] for p in params if p.startswith("pm="))
            pm = PRIME_MERIDIANS[pm] if pm in PRIME_MERIDIANS else float(pm)
            # "inv longlat" adds the meridian to longitudes, "longlat" subtracts it. The first
            # such step includes the source's prime meridian and the last the target's, which
            # terrio applies itself: what remains is a longitude rotation between datums.
            rotation = pm if inverse else -pm
            if i == rotations[0]:
                rotation -= crs_pm
            if i == rotations[-1]:
                rotation += target_pm
            if abs(rotation) > 1e-7:
                out.append(f"geogoffset dlon={rotation * 3600!r} dlat=0")
        elif name == "axisswap" and z_flips and "order=1,2,-3" in params:
            out.append("axisswap order=1,2,-3")
        elif name in NEUTRAL_STEPS:
            continue
        elif name in ("tinshift", "affine", "horner", "molodensky"):
            return None
        else:
            # The CRS's own (inverse) projection.
            projected = not inverse
    return ";".join(out)


def interpolation_crs(doc):
    """The geographic CRS a transformation interpolates in, as PROJ chains it into compound CRSs.

    Args:
        doc: The transformation's PROJJSON.

    Returns:
        The EPSG code of its last step's interpolation CRS, else of the geographic CRS that
        step goes from or to, or ``""``: PROJ looks at the last step of a concatenation only.
    """
    steps = [d for d in flatten(doc) if d.get("type") == "Transformation"]
    if not steps:
        return ""
    last = steps[-1]
    crs = last.get("interpolation_crs") or {}
    if crs.get("id", {}).get("authority") == "EPSG":
        return crs["id"]["code"]
    for side in ("source_crs", "target_crs"):
        crs = last.get(side) or {}
        if crs.get("type") == "GeographicCRS" and crs.get("id", {}).get("authority") == "EPSG":
            return crs["id"]["code"]
    return ""


def transformations(source, target, pm=0.0, target_pm=0.0, grids=False, vertical=False):
    """List the datum transformations GDAL would try between two CRSs.

    Runs ``projinfo`` with the options ``proj_create_crs_to_crs`` (which GDAL uses) passes to
    PROJ: operations whose area intersects the CRS's, without those needing grids that are not
    installed (which makes PROJ look for operations through intermediate CRSs) or, with
    ``grids``, as if every grid of the PROJ CDN were installed.

    Args:
        source: The source CRS, such as ``EPSG:4267``.
        target: The target CRS.
        pm: The source's prime meridian in degrees.
        target_pm: The target's prime meridian in degrees.
        grids: Whether to list the operations GDAL uses when grids are available.
        vertical: For transformations between vertical CRSs: keep their axis swaps to depths
            and add each one's interpolation CRS (see ``interpolation_crs``) to its row.

    Returns:
        ``[(steps, (west, south, east, north), accuracy, flags, name, steps accuracy, steps
        bounds)]`` in GDAL's order (with the interpolation CRS last when ``vertical``).
    """
    args = ["projinfo", "-s", source, "-t", target, "--spatial-test", "intersects"]
    args += ["--grid-check", "known_available" if grids else "discard_missing", "-o", "PROJ,PROJJSON", "--single-line"]
    env = dict(os.environ, PROJ_NETWORK="ON" if grids else "OFF")
    text = subprocess.run(args, capture_output=True, text=True, env=env).stdout
    out = []
    for block in text.split("Operation No. ")[1:]:
        lines = [line for line in block.splitlines() if line.strip()]
        summary = lines[1] if len(lines) > 1 else ""
        m = re.search(r", ([0-9.]+) m, ", summary)
        accuracy = float(m.group(1)) if m else -1.0
        proj = next((line for line in lines if line.startswith("+proj=")), None)
        js = next((line for line in lines if line.startswith("{")), None)
        if proj is None or js is None:
            continue
        steps = datum_steps(proj.replace("+", ""), pm, target_pm, z_flips=vertical)
        if steps is None:
            continue
        doc = json.loads(js)
        usage = (doc.get("usages") or [doc])[0]
        b = usage.get("bbox")
        name = usage.get("area", "")
        if b is None:
            bounds = (-180.0, -90.0, 180.0, 90.0)
        else:
            bounds = (b["west_longitude"], b["south_latitude"], b["east_longitude"], b["north_latitude"])
        transfos = [d for d in flatten(doc) if d.get("type") == "Transformation" and not pm_change(d)]
        flags = ("o" if "- offshore" in name else "") + ("u" if name in ("", "unknown") else "")
        # A ballpark between CRSs on other prime meridians is "altered to use prime meridian" too.
        ballparks = [d for d in flatten(doc) if d.get("type") == "Transformation" and d.get("name", "").startswith("Ballpark")]
        flags += "b" if ballparks else ""
        # Concatenations PROJ makes (not those its database registers, which PROJ chains further
        # like single transformations; PROJJSON writes their steps without their id), counting
        # the longitude rotations the EPSG dataset registers (CH1903 (Bern) to CH1903 (1)) but
        # not those PROJ makes: PROJ chains CH1903 (Bern) through CH1903 to WGS 84 by itself, but
        # does not chain that further through WGS 84.
        registered = [d for d in flatten(doc) if d.get("type") == "Transformation" and (not pm_change(d) or "id" in d)]
        flags += "c" if len(registered) > 1 and "id" not in doc and step_key(registered) not in REGISTERED_CONCATENATIONS else ""
        # Transformations PROJ's database registers between geographic 3D CRSs, which PROJ does
        # not chain with others between 2D ones, or between geocentric CRSs.
        kinds = {registered_kind(t) for t in transfos}
        flags += ("h" if "geographic 3D" in kinds else "") + ("g" if "geocentric" in kinds else "")
        flags += "p" if any(p in doc.get("name", "") for p in PRIORITY_OPS) else ""
        flags += "z" if re.search(r"\bproj=push v_3\b", proj.replace("+", "")) else ""
        # PROJ's name for the operation: its steps' names, as PROJ names the concatenations it
        # makes (those PROJ chains it into too), without the (inverse) projection of a projected
        # source, which PROJ does not chain. Ballparks, which terrio does not chain, go without.
        named = [d for d in flatten(doc) if d.get("type") != "Conversion" or d.get("method", {}).get("name") == GEOCENTRIC]
        name = "" if ballparks else " + ".join(d.get("name", "") for d in named).replace("\t", " ")
        # PROJ chains a concatenation its database registers by its steps, with their summed
        # accuracy rather than the one registered for it (1.5 m for NAD27 to WGS 84 (52), whose
        # steps add up to 2.15 m): kept when it differs.
        # (Those PROJ makes already have theirs.)
        steps_accuracy, steps_bounds = "", ""
        if "c" not in flags:
            parts = [d for d in flatten(doc) if d.get("type") == "Transformation"]
            steps_accuracy = step_accuracy(parts)
            steps_accuracy = "" if steps_accuracy == accuracy else repr(steps_accuracy)
            # Likewise the intersection of its steps' areas, rather than the one registered for it.
            steps_bounds = step_bounds(parts)
            steps_bounds = "" if steps_bounds in (None, bounds) else ",".join(repr(round(v, 6)) for v in steps_bounds)
        row = (steps, bounds, accuracy, flags, name, steps_accuracy, steps_bounds)
        if vertical:
            row += (interpolation_crs(doc),)
        out.append(row)
    return out


def step_accuracy(transfos):
    """Add up the accuracies of transformations, as PROJ does for the concatenations it makes.

    Args:
        transfos: The PROJJSON transformations.

    Returns:
        The sum of their accuracies in metres, -1 when one is unknown.
    """
    total = 0.0
    for t in transfos:
        try:
            total += float(t["accuracy"])
        except (KeyError, TypeError, ValueError):
            return -1.0
    return total


def source_types():
    """List the kind of source CRS of every transformation of PROJ's database.

    Returns:
        ``{(authority, code): type}``, such as ``"geographic 3D"``.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    out = {}
    for table in ("helmert_transformation", "grid_transformation", "other_transformation", "concatenated_operation"):
        query = f"""
            SELECT t.auth_name, t.code, s.type FROM {table} t
            JOIN geodetic_crs s ON s.auth_name = t.source_crs_auth_name AND s.code = t.source_crs_code
        """
        out.update({(a, str(c)): kind for a, c, kind in db.execute(query)})
    return out


SOURCE_TYPES = source_types()


def operation_extents():
    """List the area of every coordinate operation of PROJ's database.

    Returns:
        ``{(authority, code): (west, south, east, north)}``, from its first usage.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    query = """
        SELECT u.object_auth_name, u.object_code, e.west_lon, e.south_lat, e.east_lon, e.north_lat
        FROM usage u JOIN extent e ON e.auth_name = u.extent_auth_name AND e.code = u.extent_code
        WHERE u.object_table_name IN ('helmert_transformation', 'grid_transformation',
            'other_transformation', 'concatenated_operation')
        ORDER BY u.auth_name, u.code
    """
    out = {}
    for a, c, w, s, e, n in db.execute(query):
        out.setdefault((a, str(c)), (w, s, e, n))
    return out


OPERATION_EXTENTS = operation_extents()


def intersection(a, b):
    """Intersect two areas as PROJ's ``GeographicBoundingBox::intersection``.

    Args:
        a: ``(west, south, east, north)`` in degrees, west beyond east across the antimeridian.
        b: The other area.

    Returns:
        The intersection, the larger part when it has two across the antimeridian, ``None``
        when they do not overlap.
    """
    w, s, e, n = a
    ow, os_, oe, on = b
    if n < os_ or s > on:
        return None
    s2, n2 = max(s, os_), min(n, on)
    if w == -180 and e == 180 and ow > oe:
        return (ow, s2, oe, n2)
    if ow == -180 and oe == 180 and w > e:
        return (w, s2, e, n2)
    if w <= e:
        if ow <= oe:
            rw, re_ = max(w, ow), min(e, oe)
            return (rw, s2, re_, n2) if rw < re_ else None
        if ow > 180 or oe < -180:
            return None
        x, y = intersection(a, (ow, os_, 180, on)), intersection(a, (-180, os_, oe, on))
        if x and y:
            return x if x[2] - x[0] > y[2] - y[0] else y
        return x or y
    if ow <= oe:
        return intersection(b, a)
    return (max(w, ow), s2, min(e, oe), n2)


def step_bounds(transfos):
    """Intersect the areas of transformations, as PROJ does for the concatenations it makes.

    Args:
        transfos: The PROJJSON transformations.

    Returns:
        The intersection of the areas PROJ's database gives them, ``None`` when there is a
        single one, one has no area or they do not overlap.
    """
    if len(transfos) < 2:
        return None
    out = None
    for t in transfos:
        ident = t.get("id", {})
        authority = ident.get("authority", "")
        while m := re.fullmatch(r"(?:INVERSE|DERIVED_FROM)\((.*)\)", authority):
            authority = m.group(1)
        extent = OPERATION_EXTENTS.get((authority, str(ident.get("code"))))
        if extent is None:
            return None
        out = extent if out is None else intersection(out, extent)
        if out is None:
            return None
    return tuple(float(v) for v in out)


def registered_concatenations():
    """List the steps of the concatenated operations PROJ's database registers.

    Returns:
        A set of sorted ``((authority, code), ...)`` tuples, one per concatenated operation (the
        EPSG dataset's and PROJ's own, such as ``WGS 84 (G1762) to WGS 84 (G2296)``).
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    query = """
        SELECT s.operation_auth_name, s.operation_code, s.step_auth_name, s.step_code
        FROM concatenated_operation_step s
        JOIN concatenated_operation c ON c.auth_name = s.operation_auth_name AND c.code = s.operation_code
        WHERE c.deprecated = 0
    """
    steps = {}
    for auth, code, step_auth, step_code in db.execute(query):
        steps.setdefault((auth, code), []).append((step_auth, str(step_code)))
    return {tuple(sorted(s)) for s in steps.values()}


REGISTERED_CONCATENATIONS = registered_concatenations()


def step_key(transfos):
    """Key the transformations of an operation as ``registered_concatenations`` does.

    Args:
        transfos: The PROJJSON transformations of the operation, in order.

    Returns:
        The sorted ``(authority, code)`` of their ids, inverses as the operations they invert.
    """
    out = []
    for t in transfos:
        ident = t.get("id", {})
        authority = ident.get("authority", "")
        while m := re.fullmatch(r"(?:INVERSE|DERIVED_FROM)\((.*)\)", authority):
            authority = m.group(1)
        out.append((authority, str(ident.get("code"))))
    return tuple(sorted(out))


def registered_kind(doc):
    """Tell the kind of CRSs PROJ's database registers a transformation between.

    PROJJSON gives the CRSs PROJ uses it between (2D ones for 2D CRSs), not those it is
    registered for.

    Args:
        doc: The PROJJSON transformation.

    Returns:
        The type of its source CRS in the database (``"geographic 2D"``, ``"geographic 3D"`` or
        ``"geocentric"``), ``None`` for one it is not in.
    """
    ident = doc.get("id", {})
    # PROJ writes INVERSE(EPSG) for an inverse, DERIVED_FROM(EPSG) for one it adapted to other
    # CRSs.
    authority = ident.get("authority", "")
    while m := re.fullmatch(r"(?:INVERSE|DERIVED_FROM)\((.*)\)", authority):
        authority = m.group(1)
    return SOURCE_TYPES.get((authority, str(ident.get("code"))))


def pm_change(doc):
    """Tell whether a PROJJSON transformation only changes the prime meridian.

    Args:
        doc: The PROJJSON transformation.

    Returns:
        True for longitude rotations, which PROJ chains like conversions.
    """
    name = doc.get("name", "")
    return doc.get("method", {}).get("name") == "Longitude rotation" or "altered to use prime meridian" in name


def flatten(doc):
    """List the single operations of a PROJJSON coordinate operation.

    Args:
        doc: The PROJJSON operation.

    Returns:
        The operation itself, or the steps of a concatenated operation, flattened.
    """
    if doc.get("type") == "ConcatenatedOperation":
        return [d for step in doc.get("steps", []) for d in flatten(step)]
    return [doc]


def datum_dates():
    """List the publication dates of the EPSG datums that have one.

    Returns:
        ``("D", code, date)`` rows in order of code.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    query = """
        SELECT code, publication_date FROM geodetic_datum
        WHERE auth_name = 'EPSG' AND publication_date IS NOT NULL
    """
    return sorted((("D", int(c), d) for c, d in db.execute(query)), key=lambda r: r[1])


def datum_pairs():
    """List the pairs of datums with direct transformations, besides WGS 84.

    Returns:
        Sorted ``(datum1, datum2)`` EPSG code pairs, each pair once.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    query = """
        SELECT DISTINCT g1.datum_code, g2.datum_code FROM (
            SELECT source_crs_auth_name sa, source_crs_code sc, target_crs_auth_name ta, target_crs_code tc
                FROM helmert_transformation WHERE deprecated = 0
            UNION ALL SELECT source_crs_auth_name, source_crs_code, target_crs_auth_name, target_crs_code
                FROM other_transformation WHERE deprecated = 0
            UNION ALL SELECT source_crs_auth_name, source_crs_code, target_crs_auth_name, target_crs_code
                FROM concatenated_operation WHERE deprecated = 0
            UNION ALL SELECT source_crs_auth_name, source_crs_code, target_crs_auth_name, target_crs_code
                FROM grid_transformation WHERE deprecated = 0) t
        JOIN geodetic_crs g1 ON g1.auth_name = t.sa AND g1.code = t.sc
        JOIN geodetic_crs g2 ON g2.auth_name = t.ta AND g2.code = t.tc
        WHERE t.sa = 'EPSG' AND t.ta = 'EPSG' AND g1.datum_auth_name = 'EPSG' AND g2.datum_auth_name = 'EPSG'
    """
    wgs84 = {"6326"}
    pairs = set()
    for a, b in db.execute(query):
        a, b = str(a), str(b)
        if a != b and a not in wgs84 and b not in wgs84:
            pairs.add((min(a, b, key=int), max(a, b, key=int)))
    return sorted(pairs)


# The authorities whose operations PROJ chains between two EPSG CRSs (its database's
# ``authority_to_authority_preference`` for EPSG to EPSG).
CHAIN_AUTHORITIES = ("EPSG", "PROJ", "NKG")


# The authorities of the geodetic CRSs PROJ chains through between EPSG CRSs.
CHAIN_CRS_AUTHORITIES = ("EPSG", "NKG")

# Codes for the CRSs and datums of other authorities than EPSG (NKG:ETRF14): from this number on.
OTHER_CODES = 2_000_000_000


def other_codes():
    """Number the geodetic CRSs and datums of other chained authorities than EPSG.

    Returns:
        ``{(authority, code): number}`` for the CRSs and the datums, in order of authority and
        code: EPSG codes stand for themselves.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    keys = set()
    for auth, code, dauth, dcode in db.execute("SELECT auth_name, code, datum_auth_name, datum_code FROM geodetic_crs"):
        if auth in CHAIN_CRS_AUTHORITIES:
            keys.update(k for k in ((auth, str(code)), (dauth, str(dcode))) if k[0] != "EPSG")
    return {k: OTHER_CODES + i for i, k in enumerate(sorted(keys))}


OTHER = other_codes()


def code_of(auth, code):
    """The number terrio's table gives a CRS or datum.

    Args:
        auth: The authority.
        code: The code.

    Returns:
        The EPSG code, or the number ``other_codes`` gives another authority's, ``None`` for
        one of no chained authority.
    """
    return int(code) if auth == "EPSG" else OTHER.get((auth, str(code)))


def geodetic_rows():
    """List the geodetic CRSs PROJ may chain transformations through.

    Returns:
        ``("E", code, datum, type, name)`` rows in order of code, ``type`` being ``2`` or ``3``
        for geographic 2D or 3D CRSs and ``g`` for geocentric ones; with the ``D`` rows of the
        publication dates of other authorities' datums.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    query = "SELECT auth_name, code, datum_auth_name, datum_code, type, name FROM geodetic_crs WHERE deprecated = 0"
    kinds = {"geographic 2D": "2", "geographic 3D": "3", "geocentric": "g"}
    rows = []
    for auth, code, dauth, dcode, kind, name in db.execute(query):
        c, d = code_of(auth, code), code_of(dauth, dcode)
        if auth in CHAIN_CRS_AUTHORITIES and kind in kinds and c is not None and d is not None:
            rows.append(("E", c, d, kinds[kind], name.replace("\t", " ")))
    dates = [
        ("D", OTHER[(a, str(c))], date)
        for a, c, date in db.execute("SELECT auth_name, code, publication_date FROM geodetic_datum WHERE publication_date IS NOT NULL")
        if (a, str(c)) in OTHER
    ]
    return sorted(rows, key=lambda r: r[1]) + sorted(dates, key=lambda r: r[1])


def registered_operation(op):
    """Describe an operation PROJ's database registers, as terrio chains it.

    Args:
        op: ``(auth, code, table, name, method, source, target, accuracy, source type, target
            type)`` from the database.

    Returns:
        ``(row, via)``: the transformation as ``transformations`` lists them, run from the
        geographic coordinates of its source CRS's datum to those of its target's (geocentric
        CRSs' operations framed by the conversions to and from geocentric coordinates), and the
        codes of the CRSs a concatenation goes through; ``None`` when PROJ cannot run it.
    """
    auth, code, _, name, _, source, target, accuracy, source_type, target_type = op
    text = subprocess.run(
        ["projinfo", f"{auth}:{code}", "-k", "operation", "-o", "PROJ,PROJJSON", "--single-line"],
        capture_output=True,
        text=True,
        env=dict(os.environ, PROJ_NETWORK="OFF"),
    ).stdout
    lines = text.splitlines()
    proj = next((line for line in lines if line.startswith("+proj=")), None)
    js = next((line for line in lines if line.startswith("{")), None)
    if proj is None or js is None:
        return None
    src_crs, dst_crs = pyproj.CRS.from_user_input(source), pyproj.CRS.from_user_input(target)

    def pm_of(crs):
        """The prime meridian of a CRS in degrees."""
        pm = crs.prime_meridian
        return pm.longitude * pm.unit_conversion_factor * 180 / 3.141592653589793

    # Steps terrio cannot run (deformation models, ``defmodel``) make the operation one it
    # leaves out, as PROJ does when their grids are missing.
    known = DATUM_STEPS | GRID_STEPS | NEUTRAL_STEPS | {"longlat", "latlong"}
    if any(name not in known for _, name, _ in parse_steps(proj.replace("+", ""))):
        return None
    steps = datum_steps(proj.replace("+", ""), pm_of(src_crs), pm_of(dst_crs))
    if steps is None:
        return None

    def ellipsoid(crs):
        """The ellipsoid of a CRS as a ``cart`` step spells it."""
        e = crs.ellipsoid
        return f"a={e.semi_major_metre!r} rf={e.inverse_flattening!r}" if e.inverse_flattening else f"a={e.semi_major_metre!r} b={e.semi_major_metre!r}"

    parts = [p for p in steps.split(";") if p]
    if source_type == "geocentric":
        parts.insert(0, f"cart {ellipsoid(src_crs)}")
    if target_type == "geocentric":
        parts.append(f"inv cart {ellipsoid(dst_crs)}")
    doc = json.loads(js)
    # The first of its areas as PROJ's database lists them (PROJJSON leaves out some).
    usage = USAGES.get((auth, code))
    bounds, area = usage if usage else ((-180.0, -90.0, 180.0, 90.0), "")
    flags = ("o" if "- offshore" in area else "") + ("u" if area in ("", "unknown") else "")
    flags += ("h" if "geographic 3D" in (source_type, target_type) else "") + ("g" if source_type == "geocentric" else "")
    flags += "p" if any(p in name for p in PRIORITY_OPS) else ""
    # Between geographic 2D CRSs PROJ keeps heights (push v_3 and pop v_3), and around
    # operations between geocentric CRSs.
    flags += "z" if re.search(r"\bproj=push v_3\b", proj.replace("+", "")) or source_type == "geocentric" else ""
    flat = [d for d in flatten(doc) if d.get("type") != "Conversion"]
    full_name = " + ".join(d.get("name", "") for d in flat).replace("\t", " ") or name
    # PROJ's accuracy: the database's, 999 m standing for an unknown one.
    accuracy = -1.0 if accuracy is None or float(accuracy) == 999.0 else float(accuracy)
    steps_accuracy, steps_bounds = "", ""
    if len(flat) > 1:
        # The sum of its steps' accuracies, from the database as PROJ reads them (PROJJSON leaves
        # out some).
        known = [ACCURACIES.get(k) for k in CONCAT_STEPS.get((auth, code), [])]
        steps_accuracy = sum(known) if known and None not in known else -1.0
        steps_accuracy = "" if steps_accuracy == accuracy else repr(steps_accuracy)
        steps_bounds = step_bounds(flat)
        steps_bounds = "" if steps_bounds in (None, bounds) else ",".join(repr(round(v, 6)) for v in steps_bounds)
    ids = [d.get("target_crs", {}).get("id", {}) for d in flatten(doc)[:-1]]
    via = [code_of(i.get("authority", ""), i.get("code", "")) for i in ids if i.get("code") is not None]
    row = (";".join(parts), bounds, accuracy, flags, full_name, steps_accuracy, steps_bounds)
    return row, [str(v) for v in via if v is not None]


def operation_usages():
    """List the first area of use of each operation, as PROJ reads them from its database.

    Returns:
        ``{(authority, code): ((west, south, east, north), description)}``, PROJ taking the
        usages of a large-scale scope first, then in order of usage.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    query = """
        SELECT u.object_auth_name, u.object_code, e.west_lon, e.south_lat, e.east_lon, e.north_lat, e.description
        FROM usage u
        JOIN extent e ON u.extent_auth_name = e.auth_name AND u.extent_code = e.code
        JOIN scope s ON u.scope_auth_name = s.auth_name AND u.scope_code = s.code
        WHERE u.object_table_name IN ('helmert_transformation', 'grid_transformation', 'other_transformation',
            'concatenated_operation')
            AND NOT (u.extent_auth_name = 'PROJ' AND u.extent_code = 'EXTENT_UNKNOWN')
            AND NOT (u.scope_auth_name = 'PROJ' AND u.scope_code = 'SCOPE_UNKNOWN')
        ORDER BY (CASE WHEN s.scope LIKE '%large scale%' THEN 0 ELSE 1 END), u.auth_name, u.code
    """
    out = {}
    for auth, code, w, so, e, n, description in db.execute(query):
        out.setdefault((auth, str(code)), ((w, so, e, n), description or ""))
    return out


USAGES = operation_usages()


def operation_accuracies():
    """Read the accuracies of the operations and the steps of the concatenated ones.

    Returns:
        ``(accuracies, steps)``: ``{(authority, code): metres}`` (``None`` when unknown, as
        999 m stands for in PROJ's database) and ``{(authority, code): [(authority, code)]}``
        in order of step.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    accuracies = {
        (a, str(c)): (None if acc is None or float(acc) == 999.0 else float(acc))
        for a, c, acc in db.execute("SELECT auth_name, code, accuracy FROM coordinate_operation_view")
    }
    steps = {}
    query = "SELECT operation_auth_name, operation_code, step_auth_name, step_code FROM concatenated_operation_step ORDER BY step_number"
    for a, c, sa, sc in db.execute(query):
        steps.setdefault((a, str(c)), []).append((sa, str(sc)))
    return accuracies, steps


ACCURACIES, CONCAT_STEPS = operation_accuracies()


def registered_operations():
    """List the operations PROJ's database registers between EPSG geodetic CRSs of two datums.

    Returns:
        ``(op, replacements, time_dependent, ensemble)`` tuples in order of authority and code:
        the database row (see ``registered_operation``), the keys (``EPSG:1234``) of the
        operations that replace it between the same CRSs, whether it needs the coordinates'
        epochs, and whether it is PROJ's null transformation between a datum ensemble and one
        of its members (WGS 84 to WGS 84 (G1762)).
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    authorities = ",".join(f"'{a}'" for a in CHAIN_AUTHORITIES)
    crs_authorities = ",".join(f"'{a}'" for a in CHAIN_CRS_AUTHORITIES)
    query = f"""
        SELECT v.auth_name, v.code, v.table_name, v.name, v.method_name, v.source_crs_auth_name, v.source_crs_code,
            v.target_crs_auth_name, v.target_crs_code, v.accuracy, s.type, t.type, s.datum_auth_name, s.datum_code,
            t.datum_auth_name, t.datum_code
        FROM coordinate_operation_view v
        JOIN geodetic_crs s ON s.auth_name = v.source_crs_auth_name AND s.code = v.source_crs_code
        JOIN geodetic_crs t ON t.auth_name = v.target_crs_auth_name AND t.code = v.target_crs_code
        WHERE v.deprecated = 0 AND v.auth_name IN ({authorities})
            AND v.source_crs_auth_name IN ({crs_authorities}) AND v.target_crs_auth_name IN ({crs_authorities})
            AND NOT (s.datum_auth_name = t.datum_auth_name AND s.datum_code = t.datum_code)
        ORDER BY v.auth_name, v.code
    """
    replacements = {}
    for table, auth, code, rtable, rauth, rcode in db.execute(
        "SELECT superseded_table_name, superseded_auth_name, superseded_code, replacement_table_name, "
        "replacement_auth_name, replacement_code FROM supersession WHERE same_source_target_crs = 1"
    ):
        if table == rtable:
            replacements.setdefault((auth, str(code)), []).append(f"{rauth}:{rcode}")
    ensembles = {(str(e), str(m)) for e, m in db.execute("SELECT ensemble_code, member_code FROM geodetic_datum_ensemble_member")}
    steps = {}
    for auth, code, sauth, scode in db.execute("SELECT operation_auth_name, operation_code, step_auth_name, step_code FROM concatenated_operation_step"):
        steps.setdefault((auth, str(code)), []).append((sauth, str(scode)))
    methods = {(a, str(c)): m or "" for a, c, m in db.execute("SELECT auth_name, code, method_name FROM coordinate_operation_view")}
    out = []
    for row in db.execute(query):
        auth, code, table, name, method, sauth, source, tauth, target, accuracy, stype, ttype, _, sdatum, _, tdatum = row
        key = (auth, str(code))
        used = [methods.get(s, "") for s in steps.get(key, [])] or [method or ""]
        time_dependent = any("time-dependent" in m.lower() or "point motion" in m.lower() for m in used)
        ensemble = auth == "PROJ" and ((str(sdatum), str(tdatum)) in ensembles or (str(tdatum), str(sdatum)) in ensembles)
        op = (auth, str(code), table, name, method, f"{sauth}:{source}", f"{tauth}:{target}", accuracy, stype, ttype)
        out.append((op, replacements.get(key, []), time_dependent, ensemble))
    return out


def vertical_rows():
    """List the EPSG vertical and compound CRSs.

    Returns:
        ``(verticals, compounds)``: ``("V", code, name, datum, unit, direction, wkt)`` and
        ``("K", code, name, horizontal, vertical, wkt)`` rows.
    """
    verticals, compounds = [], []
    for code in sorted(int(c) for c in pyproj.get_codes("EPSG", PJType.VERTICAL_CRS, allow_deprecated=True)):
        try:
            crs = pyproj.CRS.from_epsg(code)
            srs = osr.SpatialReference()
            srs.ImportFromEPSG(code)
            wkt = srs.ExportToWkt()
        except (pyproj.exceptions.CRSError, RuntimeError):
            continue
        axis = crs.axis_info[0] if crs.axis_info else None
        if axis is None or not wkt:
            continue
        datum = crs.datum.to_json_dict().get("id", {}).get("code", "") if crs.datum else ""
        verticals.append(("V", code, crs.name.replace("\t", " "), datum, repr(axis.unit_conversion_factor), axis.direction.lower(), wkt))
    for code in sorted(int(c) for c in pyproj.get_codes("EPSG", PJType.COMPOUND_CRS, allow_deprecated=True)):
        try:
            crs = pyproj.CRS.from_epsg(code)
            srs = osr.SpatialReference()
            srs.ImportFromEPSG(code)
            wkt = srs.ExportToWkt()
        except (pyproj.exceptions.CRSError, RuntimeError):
            continue
        subs = crs.sub_crs_list
        if len(subs) != 2:
            continue
        h, v = subs[0].to_epsg(min_confidence=100), subs[1].to_epsg(min_confidence=100)
        if h is None or v is None or not subs[1].is_vertical:
            continue
        compounds.append(("K", code, crs.name.replace("\t", " "), h, v, wkt))
    return verticals, compounds


def geoid_pairs():
    """List the pairs of a geographic 3D CRS and a vertical CRS with transformations between them.

    Returns:
        Sorted ``(geographic 3D code, vertical code)`` pairs.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    query = """
        SELECT DISTINCT g.code, v.code FROM (
            SELECT source_crs_auth_name sa, source_crs_code sc, target_crs_auth_name ta, target_crs_code tc
                FROM grid_transformation WHERE deprecated = 0
            UNION ALL SELECT source_crs_auth_name, source_crs_code, target_crs_auth_name, target_crs_code
                FROM other_transformation WHERE deprecated = 0
            UNION ALL SELECT target_crs_auth_name, target_crs_code, source_crs_auth_name, source_crs_code
                FROM grid_transformation WHERE deprecated = 0) t
        JOIN geodetic_crs g ON g.auth_name = t.sa AND g.code = t.sc AND g.type = 'geographic 3D'
        JOIN vertical_crs v ON v.auth_name = t.ta AND v.code = t.tc
        WHERE t.sa = 'EPSG' AND t.ta = 'EPSG'
    """
    return sorted({(int(a), int(b)) for a, b in db.execute(query)})


def grid_alternatives(names):
    """List the old names of PROJ-data grids.

    Args:
        names: The grid names to look up.

    Returns:
        Sorted ``("A", name, old name)`` rows.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    rows = db.execute("SELECT proj_grid_name, old_proj_grid_name FROM grid_alternatives WHERE old_proj_grid_name IS NOT NULL")
    return sorted({("A", a, b) for a, b in rows if a in names and b and b != a})


def geocentric_rows(base):
    """Cartesian endpoint definitions, reusing the datum's geographic 3D operation lists.

    Datum steps in Terrio run in geographic coordinates: Cartesian conversion is
    performed by the endpoint, not duplicated in the stored operations.
    """
    for code in sorted(map(int, pyproj.get_codes("EPSG", PJType.GEOCENTRIC_CRS, allow_deprecated=True))):
        srs = osr.SpatialReference()
        srs.ImportFromEPSG(code)
        requested = pyproj.CRS.from_epsg(code)
        crs = pyproj.CRS.from_epsg(int(srs.GetAuthorityCode(None) or code))
        datum = str(crs.datum.to_json_dict().get("id", {}).get("code", ""))
        candidates = [r for r in base.values() if str(r[3]) == datum and str(r[9]).startswith("+proj=longlat")]
        candidates.sort(key=lambda r: ("3" not in str(r[7]), r[2] != crs.name, int(r[1])))
        proxy = candidates[0] if candidates else None
        area = crs.area_of_use
        bounds = ",".join(repr(round(v, 6)) for v in (area.west, area.south, area.east, area.north)) if area else ""
        yield ("C", code, requested.name.replace("\t", " "), datum, proxy[4] if proxy else "",
               proxy[5] if proxy else "", proxy[6] if proxy else "", "3" + ("d" if srs.IsDynamic() else ""), bounds,
               normalize_proj(srs.ExportToProj4(), crs, "en"), srs.ExportToWkt())


def rows():
    """Yield the table's rows.

    Yields:
        ``O`` rows for each new datum transformation, then the ``C``, ``P``, ``V``, ``K``, ``G``
        and ``A`` rows (see the module documentation).
    """
    crss = []
    for code in epsg_codes():
        try:
            srs = osr.SpatialReference()
            srs.ImportFromEPSG(code)
            proj = srs.ExportToProj4()
            wkt = srs.ExportToWkt()
            # The definition GDAL uses, which is the replacement of a deprecated code.
            actual = int(srs.GetAuthorityCode(None) or code)
            crs = pyproj.CRS.from_epsg(actual)
            name = pyproj.CRS.from_epsg(code).name.replace("\t", " ")
        except (pyproj.exceptions.CRSError, RuntimeError):
            continue
        if not proj or "+proj=" not in proj:
            continue
        datum = srs.GetAuthorityCode("DATUM") or srs.GetAuthorityCode("GEOGCS|DATUM") or ""
        # GDAL's WKT leaves the codes out for some CRSs (those on the WGS 84 ensemble, such as
        # WGS 84 / Equal Earth Greenwich, and others): PROJ's are those of the base CRS.
        if not datum and crs.datum:
            datum = str(crs.datum.to_json_dict().get("id", {}).get("code", ""))
        # WGS 84 itself (its datum ensemble); its realizations (G1762...) have transformations
        # (of known accuracy) to it.
        is_wgs84 = datum == "6326"
        pm = crs.prime_meridian
        pm = pm.longitude * pm.unit_conversion_factor * 180 / 3.141592653589793
        three_d = crs.type_name == "Geographic 3D CRS"
        geog = srs.GetAuthorityCode("GEOGCS") or ""
        if not geog and crs.geodetic_crs:
            geog = str(crs.geodetic_crs.to_2d().to_epsg() or "")
        if three_d:
            geog = str(crs.to_2d().to_epsg() or "")
        flags = ("d" if srs.IsDynamic() else "") + ("3" if three_d else "")
        aou = crs.area_of_use
        area = ",".join(repr(round(v, 6)) for v in (aou.west, aou.south, aou.east, aou.north)) if aou else ""
        line = (code, name, datum, geog, flags, area, normalize_proj(proj, crs, gis_axis(srs, crs)), wkt)
        crss.append((actual, is_wgs84, pm, three_d, line))

    def to_wgs84(c, grids):
        """List the transformations of a CRS to WGS 84.

        Args:
            c: The CRS's entry in ``crss``.
            grids: Whether to list those with grids.

        Returns:
            The transformations.
        """
        actual, is_wgs84, pm, three_d, _ = c
        if is_wgs84:
            return [("", (-180.0, -90.0, 180.0, 90.0), -1.0, "", "", "", "")]
        return transformations(f"EPSG:{actual}", "EPSG:4979" if three_d else "EPSG:4326", pm, 0.0, grids)

    with ThreadPoolExecutor(os.cpu_count()) as pool:
        found = list(pool.map(lambda c: to_wgs84(c, False), crss))
        found_grids = list(pool.map(lambda c: to_wgs84(c, True), crss))
    ops = {}

    def op_ids(transfos):
        """Number transformations, new ones first seen.

        Args:
            transfos: The transformations from ``transformations``.

        Returns:
            Their ids and the ``O`` rows of those not seen before.
        """
        ids = []
        new = []
        for steps, bounds, accuracy, flags, name, steps_accuracy, steps_bounds in transfos:
            key = (steps, bounds, accuracy, flags, name, steps_accuracy, steps_bounds)
            if key not in ops:
                ops[key] = len(ops)
                new.append(("O", ops[key], ",".join(repr(round(b, 6)) for b in bounds), repr(accuracy), flags, steps, name, steps_accuracy, steps_bounds))
            if ops[key] not in ids:
                ids.append(ops[key])
        return ids, new

    def both(transfos, transfos_grids):
        """Number the transformations without and with grids.

        Args:
            transfos: The transformations without grids.
            transfos_grids: The transformations with grids.

        Returns:
            The two id lists (the second empty when it equals the first) and the new ``O``
            rows.
        """
        ids, new = op_ids(transfos)
        gids, gnew = op_ids(transfos_grids)
        return ids, ([] if gids == ids else gids), new + gnew

    grids_used = set()
    base = {}
    for (_, _, _, _, (code, name, datum, geog, flags, area, proj, wkt)), transfos, transfos_grids in zip(crss, found, found_grids):
        ids, gids, new = both(transfos, transfos_grids)
        yield from new
        row = ("C", code, name, datum, geog, ",".join(map(str, ids)), ",".join(map(str, gids)), flags, area, proj, wkt)
        base[code] = row
        yield row
    yield from geocentric_rows(base)

    # Direct transformations between datums other than WGS 84.
    geogs = {}
    for _, _, pm, three_d, (code, _, datum, geog, _, _, _, _) in crss:
        if geog and int(geog) == code and not three_d:
            geogs.setdefault(datum, []).append((code, pm))
    pairs = [(a, b) for da, db in datum_pairs() if da in geogs and db in geogs for a in geogs[da] for b in geogs[db]]

    def pair_ops(p, grids):
        """List the transformations of a pair of geographic CRSs.

        Args:
            p: The pair, as ``((code, pm), (code, pm))``.
            grids: Whether to list those with grids.

        Returns:
            The transformations.
        """
        return transformations(f"EPSG:{p[0][0]}", f"EPSG:{p[1][0]}", p[0][1], p[1][1], grids)

    with ThreadPoolExecutor(os.cpu_count()) as pool:
        found = list(pool.map(lambda p: pair_ops(p, False), pairs))
        found_grids = list(pool.map(lambda p: pair_ops(p, True), pairs))
    for ((a, _), (b, _)), transfos, transfos_grids in zip(pairs, found, found_grids):
        ids, gids, new = both(transfos, transfos_grids)
        yield from new
        if ids or gids:
            yield ("P", a, b, ",".join(map(str, ids)), ",".join(map(str, gids)))

    yield from datum_dates()

    # The operations PROJ's database registers between geodetic CRSs, which terrio chains
    # through intermediate datums as PROJ does, and the CRSs they go between.
    yield from geodetic_rows()
    registered = registered_operations()
    with ThreadPoolExecutor(os.cpu_count()) as pool:
        described = list(pool.map(lambda r: registered_operation(r[0]), registered))
    for (op, replaced_by, time_dependent, ensemble), found in zip(registered, described):
        if found is None:
            continue
        row, via = found
        (oid,), new = op_ids([row])
        yield from new
        flags = ("t" if time_dependent else "") + ("e" if ensemble else "")
        source, target = (code_of(*c.split(":", 1)) for c in (op[5], op[6]))
        yield ("R", f"{op[0]}:{op[1]}", source, target, oid, ",".join(replaced_by), flags, ",".join(via))

    # Vertical and compound CRSs, and the geoid models between geographic 3D and vertical CRSs.
    verticals, compounds = vertical_rows()
    yield from verticals
    yield from compounds
    known = {v[1] for v in verticals}
    gpairs = [(g, v) for g, v in geoid_pairs() if v in known]

    def geoid_ops(p, grids):
        """List the non-ballpark transformations from a geographic 3D CRS to a vertical CRS.

        Args:
            p: The pair of codes.
            grids: Whether to list those with grids.

        Returns:
            The transformations.
        """
        # Without their names, which only rank chains.
        return [t[:4] + ("", "", "") for t in transformations(f"EPSG:{p[0]}", f"EPSG:{p[1]}", 0.0, 0.0, grids) if "b" not in t[3]]

    with ThreadPoolExecutor(os.cpu_count()) as pool:
        found = list(pool.map(lambda p: geoid_ops(p, False), gpairs))
        found_grids = list(pool.map(lambda p: geoid_ops(p, True), gpairs))
    for (g, v), transfos, transfos_grids in zip(gpairs, found, found_grids):
        ids, gids, new = both(transfos, transfos_grids)
        yield from new
        if ids or gids:
            yield ("G", g, v, ",".join(map(str, ids)), ",".join(map(str, gids)))

    for steps, *_ in ops:
        grids_used.update(re.findall(r"grids=@?([^ ;]+)", steps))
    names = {n.lstrip("@") for g in grids_used for n in g.split(",")}
    yield from grid_alternatives(names)


def main():
    """Write the compressed table."""
    # conda's PROJ activation turns PROJ's network access on, with which GDAL fails to import
    # CRSs whose transformations use grids the PROJ CDN no longer has: off, but for the lists
    # "with grids" (see ``transformations``).
    os.environ["PROJ_NETWORK"] = "OFF"
    osr.SetPROJEnableNetwork(False)
    lines = ["\t".join(map(str, r)) + "\n" for r in rows()]
    header = (
        f"# EPSG Geodetic Parameter Dataset (https://epsg.org), via PROJ {pyproj.proj_version_str} "
        f"and GDAL {gdal.__version__}. Generated by scripts/gen_epsg_table.py.\n"
    )
    data = (header + "".join(lines)).encode()
    out = subprocess.run(["zstd", "-19", "-q", "-c"], input=data, capture_output=True, check=True).stdout
    with open(OUT, "wb") as f:
        f.write(out)
    kinds = {}
    for line in lines:
        kinds[line[0]] = kinds.get(line[0], 0) + 1
    print(f"{kinds}, {len(data)} bytes, {len(out)} compressed")


if __name__ == "__main__":
    main()
