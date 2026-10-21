"""Generate terrio's EPSG table from GDAL and PROJ.

Writes ``crates/terrio/data/epsg.tsv.zst``, a zstd-compressed text table with these lines:

- ``O<TAB>id<TAB>west,south,east,north<TAB>accuracy<TAB>flags<TAB>steps``: a datum transformation,
  as the geodetic steps of the PROJ pipeline GDAL uses (``cart ellps=intl;helmert x=-87 y=-98
  z=-121;inv cart ellps=WGS84``, ``hgridshift grids=us_noaa_conus.tif``; empty for a null
  transformation), the area where it applies (in degrees), its accuracy in metres (-1 when
  unknown) and flags for how PROJ ranks, chains and runs it (``o``: offshore area, ``u``:
  unknown area, ``b``: ballpark, ``c``: a concatenation PROJ makes, rather than one the EPSG
  dataset registers, ``h`` and ``g``: one PROJ's database registers between geographic 3D or
  geocentric CRSs, ``p``: one PROJ prefers among those of equal accuracy, ``z``: the pipeline
  keeps heights between 2D CRSs).
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
  without and with grids, in the direction geog1 to geog2. terrio also chains the single ones
  through intermediate datums, as PROJ does for other pairs.
- ``V<TAB>code<TAB>name<TAB>datum<TAB>unit<TAB>direction<TAB>WKT``: a vertical CRS, with its
  unit in metres and axis direction (``up`` or ``down``).
- ``K<TAB>code<TAB>name<TAB>horizontal<TAB>vertical<TAB>WKT``: a compound CRS.
- ``G<TAB>geog3d<TAB>vertical<TAB>ops<TAB>grid ops``: the transformations (geoid models) from a
  geographic 3D CRS to a vertical CRS.
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

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio", "data", "epsg.tsv.zst")

# Steps of a PROJ pipeline that belong to the datum transformation; the others convert units,
# swap axes or (un)project, which terrio does from the CRS's own definition.
DATUM_STEPS = {"cart", "helmert", "molodensky", "molobadekas", "geogoffset"}

# Steps that apply grids.
GRID_STEPS = {"hgridshift", "vgridshift", "gridshift", "xyzgridshift", "deformation"}

# Steps that neither (un)project nor shift datums.
NEUTRAL_STEPS = {"push", "pop", "noop", "unitconvert", "axisswap", "set"}

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


def datum_steps(definition, crs_pm=0.0, target_pm=0.0):
    """Extract the datum transformation from a PROJ pipeline.

    Args:
        definition: The pipeline from a CRS to WGS 84 (or another geographic CRS).
        crs_pm: The CRS's prime meridian in degrees, which terrio applies itself.
        target_pm: The target CRS's prime meridian in degrees, which terrio also applies.

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
        elif name in NEUTRAL_STEPS:
            continue
        elif name in ("tinshift", "affine", "horner", "vertoffset", "molodensky"):
            return None
        else:
            # The CRS's own (inverse) projection.
            projected = not inverse
    return ";".join(out)


def transformations(source, target, pm=0.0, target_pm=0.0, grids=False):
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

    Returns:
        ``[(steps, (west, south, east, north), accuracy, flags)]`` in GDAL's order.
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
        steps = datum_steps(proj.replace("+", ""), pm, target_pm)
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
        # Concatenations PROJ makes (not those the EPSG dataset registers, which PROJ chains
        # further like single transformations).
        flags += "c" if len(transfos) > 1 and "id" not in doc else ""
        # Transformations PROJ's database registers between geographic 3D CRSs, which PROJ does
        # not chain with others between 2D ones, or between geocentric CRSs.
        kinds = {registered_kind(t) for t in transfos}
        flags += ("h" if "geographic 3D" in kinds else "") + ("g" if "geocentric" in kinds else "")
        flags += "p" if any(p in doc.get("name", "") for p in PRIORITY_OPS) else ""
        flags += "z" if re.search(r"\bproj=push v_3\b", proj.replace("+", "")) else ""
        out.append((steps, bounds, accuracy, flags))
    return out


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
            return [("", (-180.0, -90.0, 180.0, 90.0), -1.0, "")]
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
        for steps, bounds, accuracy, flags in transfos:
            key = (steps, bounds, accuracy, flags)
            if key not in ops:
                ops[key] = len(ops)
                new.append(("O", ops[key], ",".join(repr(round(b, 6)) for b in bounds), repr(accuracy), flags, steps))
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
    for (_, _, _, _, (code, name, datum, geog, flags, area, proj, wkt)), transfos, transfos_grids in zip(crss, found, found_grids):
        ids, gids, new = both(transfos, transfos_grids)
        yield from new
        yield ("C", code, name, datum, geog, ",".join(map(str, ids)), ",".join(map(str, gids)), flags, area, proj, wkt)

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
        return [t for t in transformations(f"EPSG:{p[0]}", f"EPSG:{p[1]}", 0.0, 0.0, grids) if "b" not in t[3]]

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
