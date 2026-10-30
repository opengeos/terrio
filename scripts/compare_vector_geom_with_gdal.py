"""Compare terrio's vector geometry commands with GDAL/GEOS on real data.

Usage: python scripts/compare_vector_geom_with_gdal.py ../terrio-data --terrio target/release/terrio

Commands: simplify, segmentize, swap-xy and set-geom-type against ``gdal vector`` (GDAL 3.12
has them); convex-hull and concave-hull against OGR's ``ConvexHull``/``ConcaveHull`` and GEOS's
``GEOSConcaveHullOfPolygons_r`` (through ctypes), the functions GDAL 3.13's commands call;
combine and sort, which GDAL 3.12 lacks, against reference implementations of GDAL 3.13's
algorithms: grouping in Python with OGR geometries, Hilbert codes ported from GDAL's
``alg/hilbert.cpp`` and GEOS's own STR tree (``GEOSSTRtree_iterate_r``).

Data from scripts/fetch_test_data.sh: Tennessee counties (projected to EPSG:5070), US counties
(EPSG:4269), Las Vegas roads (EPSG:32611), Natural Earth countries (110m) and populated places.
Geometries are compared coordinate by coordinate (as WKB); single and multi-part storage of the
same polygons or lines counts as identical, as terrio's GeoPackage writer stores the layer's
declared multi type. Hulls are compared by their sets of undirected edges, which ignores ring
start vertices and orientation. Hulls must be valid; differing edges require relative
symmetric-difference area and scaled Hausdorff distance both at most 1e-10. Sorted orders are compared position by position; positions where the sort keys
tie are not counted as differences (GDAL's sorts are not stable). Writes a JSON report.
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.util
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys

from osgeo import gdal, ogr
import shapely

gdal.UseExceptions()
ogr.UseExceptions()

IDENTICAL, TOLERANCE, DIFFERS = "identical", "within tolerance", "differs"


def load(path, layer=None):
    """Reads the features of a layer.

    Args:
        path: The dataset.
        layer: The layer name; by default the first layer.

    Returns:
        A list of ``(geometry or None, attributes)`` tuples.
    """
    ds = ogr.Open(str(path))
    lyr = ds.GetLayerByName(layer) if layer else ds.GetLayer(0)
    return [
        (f.GetGeometryRef().Clone() if f.GetGeometryRef() else None, f.items())
        for f in lyr
    ]


def layer_type(path):
    """The declared geometry type name of a dataset's first layer.

    Args:
        path: The dataset.

    Returns:
        The OGR type name.
    """
    ds = ogr.Open(str(path))
    return ogr.GeometryTypeToName(ds.GetLayer(0).GetGeomType())


def canonical(g):
    """WKB of a geometry, polygons and lines stored as their multi type.

    Args:
        g: The geometry, or None.

    Returns:
        The ISO WKB bytes, or None.
    """
    if g is None:
        return None
    flat = ogr.GT_Flatten(g.GetGeometryType())
    if flat == ogr.wkbPolygon:
        g = ogr.ForceToMultiPolygon(g.Clone())
    elif flat == ogr.wkbLineString:
        g = ogr.ForceToMultiLineString(g.Clone())
    return bytes(g.ExportToIsoWkb())


def edges(g):
    """The undirected edges of a geometry's linework, with point geometries as vertices.

    Args:
        g: The geometry.

    Returns:
        A set of frozensets of coordinate tuples.
    """
    out = set()

    def visit(h):
        n = h.GetGeometryCount()
        if n:
            for i in range(n):
                visit(h.GetGeometryRef(i))
            return
        pts = h.GetPoints() or []
        if len(pts) == 1:
            out.add(frozenset([pts[0]]))
        for a, b in zip(pts, pts[1:]):
            out.add(frozenset([a, b]))

    visit(g)
    return out


def relative_difference(a, b):
    """The symmetric-difference area of two polygonal geometries relative to ``a``'s area.

    Args:
        a: The reference geometry.
        b: The compared geometry.

    Returns:
        The relative area, NaN when it cannot be computed.
    """
    try:
        va, vb = a.MakeValid(), b.MakeValid()
        return va.SymDifference(vb).GetArea() / max(va.GetArea(), 1e-300)
    except RuntimeError:
        return float("nan")


class Runner:
    """Runs terrio and GDAL and records the cases."""

    def __init__(self, terrio, work):
        self.terrio, self.work, self.cases = terrio, work, []

    def terrio_run(self, args, name):
        """Runs a terrio command writing ``name``.gpkg.

        Args:
            args: The command and its input, before the output.
            name: The output name.

        Returns:
            The output path.
        """
        out = self.work / f"{name}-terrio.gpkg"
        if out.exists():
            out.unlink()
        cmd, src, *options = args
        subprocess.run(
            [self.terrio, cmd, str(src), str(out), *options],
            check=True,
            capture_output=True,
            text=True,
        )
        return out

    def gdal_run(self, args, name):
        """Runs ``gdal vector`` writing ``name``.gpkg.

        Args:
            args: The subcommand and its input, before the output.
            name: The output name.

        Returns:
            The output path.
        """
        out = self.work / f"{name}-gdal.gpkg"
        if out.exists():
            out.unlink()
        cmd, src, *options = args
        subprocess.run(
            ["gdal", "vector", cmd, *options, str(src), str(out)],
            check=True,
            capture_output=True,
            text=True,
        )
        return out

    def record(self, name, status, **detail):
        """Records a case and prints it.

        Args:
            name: The case name.
            status: identical, within tolerance or differs.
            **detail: Numbers describing the comparison.
        """
        self.cases.append({"name": name, "status": status, **detail})
        print(
            f"{'OK' if status != DIFFERS else 'DIFF'} {name}: {status} {detail}",
            flush=True,
        )

    def compare_features(self, name, actual, expected, attributes=True):
        """Compares two feature lists coordinate by coordinate.

        Args:
            name: The case name.
            actual: terrio's features.
            expected: The reference features.
            attributes: Whether attributes must be equal too.
        """
        assert len(actual) == len(expected), (
            f"{name}: {len(actual)} != {len(expected)} features"
        )
        same = 0
        for (ga, aa), (gb, ab) in zip(actual, expected):
            if attributes:
                assert aa == ab, f"{name}: attributes differ: {aa} != {ab}"
            if canonical(ga) == canonical(gb):
                same += 1
        invalid = [
            sum(1 for g, _ in fs if g is not None and not g.IsValid())
            for fs in (actual, expected)
        ]
        status = IDENTICAL if same == len(actual) else DIFFERS
        self.record(
            name,
            status,
            features=len(actual),
            identical=same,
            invalid_terrio=invalid[0],
            invalid_reference=invalid[1],
        )

    def compare_hulls(self, name, actual, expected, tolerance):
        """Compares hulls by their edges, then by area.

        Args:
            name: The case name.
            actual: terrio's geometries.
            expected: The reference geometries.
            tolerance: The largest relative symmetric difference accepted for the hulls
                whose edges differ.
        """
        assert len(actual) == len(expected), f"{name}: feature counts differ"
        same, worst, differing = 0, 0.0, 0
        worst_distance = 0.0
        valid = True
        for a, b in zip(actual, expected):
            if (a is None) != (b is None):
                raise AssertionError(f"{name}: null geometry differs")
            if a is None:
                same += 1
                continue
            valid = valid and bool(a.IsValid()) and bool(b.IsValid())
            if edges(a) == edges(b):
                same += 1
                continue
            sa = shapely.from_wkb(bytes(a.ExportToWkb()))
            sb = shapely.from_wkb(bytes(b.ExportToWkb()))
            scale = max(1.0, *(abs(v) for v in sb.bounds))
            distance = sa.hausdorff_distance(sb) / scale
            worst_distance = max(worst_distance, distance if math.isfinite(distance) else math.inf)
            differing += 1
            if b.GetDimension() == 2:
                difference = relative_difference(b, a)
                worst = max(worst, difference if math.isfinite(difference) else math.inf)
            else:
                worst = math.inf
        if not valid or worst_distance > 1e-10:
            status = DIFFERS
        elif same == len(actual):
            status = IDENTICAL
        elif worst <= tolerance:
            status = TOLERANCE
        else:
            status = DIFFERS
        self.record(
            name,
            status,
            features=len(actual),
            same_edges=same,
            valid=valid,
            max_scaled_hausdorff_distance=worst_distance,
            max_relative_symmetric_difference=worst,
            tolerance=tolerance,
        )


def prepare(data, work):
    """Writes the inputs as GeoPackages.

    Args:
        data: The test data directory.
        work: The output directory.

    Returns:
        A dict of input paths.
    """
    paths = {}
    for name, source, srs in [
        ("tn", "TN_Counties.geojson", "EPSG:5070"),
        ("us", "UScounties.fgb", None),
        ("roads", "las_vegas_roads.geojson", "EPSG:32611"),
        ("ne", "/vsizip/" + str(data / "ne_countries.zip"), None),
        ("places", "/vsizip/" + str(data / "ne_places.zip"), None),
    ]:
        path = work / f"{name}.gpkg"
        if path.exists():
            path.unlink()
        src = source if source.startswith("/vsizip/") else str(data / source)
        options = ["-nln", name] + (["-t_srs", srs] if srs else [])
        # Natural Earth declares Polygon despite containing multipart features.
        # Keep that topology when writing the test layer and subsequent hulls.
        if name == "ne":
            options += ["-nlt", "PROMOTE_TO_MULTI"]
        gdal.VectorTranslate(
            str(path), src, options=options + ["-lco", "SPATIAL_INDEX=NO"]
        )
        paths[name] = path
    return paths


class Geos:
    """GEOS's C API through ctypes, for the functions OGR 3.12 does not expose."""

    def __init__(self):
        candidates = [
            os.path.join(sys.prefix, "lib", "libgeos_c.so"),
            os.path.join(sys.prefix, "lib", "libgeos_c.dylib"),
            os.path.join(sys.prefix, "Library", "bin", "geos_c.dll"),
        ]
        found = ctypes.util.find_library("geos_c")
        if found:
            candidates.append(found)
        lib = None
        for candidate in candidates:
            try:
                lib = ctypes.CDLL(candidate)
                break
            except OSError:
                continue
        if lib is None:
            raise OSError("libgeos_c not found")
        vp, sz = ctypes.c_void_p, ctypes.c_size_t
        lib.GEOS_init_r.restype = vp
        for f, res, args in [
            ("GEOSWKBReader_create_r", vp, [vp]),
            ("GEOSWKBWriter_create_r", vp, [vp]),
            ("GEOSWKBReader_read_r", vp, [vp, vp, ctypes.c_char_p, sz]),
            (
                "GEOSWKBWriter_write_r",
                ctypes.POINTER(ctypes.c_ubyte),
                [vp, vp, vp, ctypes.POINTER(sz)],
            ),
            (
                "GEOSConcaveHullOfPolygons_r",
                vp,
                [vp, vp, ctypes.c_double, ctypes.c_uint, ctypes.c_uint],
            ),
            ("GEOSGeom_destroy_r", None, [vp, vp]),
            ("GEOSSTRtree_create_r", vp, [vp, sz]),
            ("GEOSSTRtree_insert_r", None, [vp, vp, vp, vp]),
            ("GEOSSTRtree_build_r", ctypes.c_int, [vp, vp]),
            ("GEOSSTRtree_iterate_r", None, [vp, vp, vp, vp]),
            ("GEOSSTRtree_destroy_r", None, [vp, vp]),
            ("GEOSFree_r", None, [vp, vp]),
        ]:
            getattr(lib, f).restype = res
            getattr(lib, f).argtypes = args
        self.lib = lib
        self.h = lib.GEOS_init_r()
        self.reader = lib.GEOSWKBReader_create_r(self.h)
        self.writer = lib.GEOSWKBWriter_create_r(self.h)

    def read(self, g):
        """Converts an OGR geometry to a GEOS one.

        Args:
            g: The OGR geometry.

        Returns:
            The GEOS geometry handle.
        """
        wkb = bytes(g.ExportToWkb())
        return self.lib.GEOSWKBReader_read_r(self.h, self.reader, wkb, len(wkb))

    def write(self, gg):
        """Converts a GEOS geometry to an OGR one, and frees it.

        Args:
            gg: The GEOS geometry handle.

        Returns:
            The OGR geometry.
        """
        n = ctypes.c_size_t()
        buf = self.lib.GEOSWKBWriter_write_r(self.h, self.writer, gg, ctypes.byref(n))
        out = ogr.CreateGeometryFromWkb(bytes(buf[: n.value]))
        self.lib.GEOSFree_r(self.h, buf)
        self.lib.GEOSGeom_destroy_r(self.h, gg)
        return out

    def concave_hull_of_polygons(self, g, ratio, tight, holes):
        """GEOS's ConcaveHullOfPolygons, as OGR 3.13's ``ConcaveHullOfPolygons``.

        Args:
            g: The polygonal geometry.
            ratio: The edge length ratio.
            tight: Whether the hull follows the outer boundaries.
            holes: Whether holes are allowed.

        Returns:
            The hull.
        """
        gg = self.read(g)
        out = self.lib.GEOSConcaveHullOfPolygons_r(self.h, gg, ratio, tight, holes)
        self.lib.GEOSGeom_destroy_r(self.h, gg)
        if not out:
            raise RuntimeError("GEOS polygon hull returned no geometry")
        return self.write(out)

    def strtree_order(self, envelopes):
        """The order GDAL 3.13's ``gdal vector sort --method strtree`` gives.

        Args:
            envelopes: ``(minx, maxx, miny, maxy)`` per feature, or None.

        Returns:
            The feature indices in order (null envelopes last).
        """
        tree = self.lib.GEOSSTRtree_create_r(self.h, 10)
        keep = []
        for i, e in enumerate(envelopes):
            if e is None:
                continue
            line = ogr.Geometry(ogr.wkbLineString)
            line.AddPoint_2D(e[0], e[2])
            line.AddPoint_2D(e[1], e[3])
            gg = self.read(line)
            keep.append(gg)
            self.lib.GEOSSTRtree_insert_r(self.h, tree, gg, ctypes.c_void_p(i + 1))
        self.lib.GEOSSTRtree_build_r(self.h, tree)
        order = []
        callback = ctypes.CFUNCTYPE(None, ctypes.c_void_p, ctypes.c_void_p)(
            lambda item, _: order.append(item - 1)
        )
        self.lib.GEOSSTRtree_iterate_r(self.h, tree, callback, None)
        self.lib.GEOSSTRtree_destroy_r(self.h, tree)
        for gg in keep:
            self.lib.GEOSGeom_destroy_r(self.h, gg)
        return order + [i for i, e in enumerate(envelopes) if e is None]


def hilbert(x, y):
    """GDAL's ``GDALHilbertCode`` on 16-bit integer coordinates (alg/hilbert.cpp, MIT).

    Args:
        x: The X cell.
        y: The Y cell.

    Returns:
        The 32-bit code.
    """
    m = 0xFFFFFFFF
    a = x ^ y
    b = 0xFFFF ^ a
    c = 0xFFFF ^ (x | y)
    d = x & (y ^ 0xFFFF)
    A = a | (b >> 1)
    B = (a >> 1) ^ a
    C = ((c >> 1) ^ (b & (d >> 1))) ^ c
    D = ((a & (c >> 1)) ^ (d >> 1)) ^ d
    for s in (2, 4):
        a, b, c, d = A, B, C, D
        A = (a & (a >> s)) ^ (b & (b >> s))
        B = (a & (b >> s)) ^ (b & ((a ^ b) >> s))
        C ^= (a & (c >> s)) ^ (b & (d >> s))
        D ^= (b & (c >> s)) ^ ((a ^ b) & (d >> s))
    a, b, c, d = A, B, C, D
    C ^= (a & (c >> 8)) ^ (b & (d >> 8))
    D ^= (b & (c >> 8)) ^ ((a ^ b) & (d >> 8))
    a, b = C ^ (C >> 1), D ^ (D >> 1)
    i0 = x ^ y
    i1 = (b | (0xFFFF ^ (i0 | a))) & m
    for s, mask in ((8, 0x00FF00FF), (4, 0x0F0F0F0F), (2, 0x33333333), (1, 0x55555555)):
        i0 = (i0 | (i0 << s)) & mask
        i1 = (i1 | (i1 << s)) & mask
    return ((i1 << 1) | i0) & m


def hilbert_order(envelopes):
    """The order of GDAL 3.13's ``gdal vector sort --method hilbert``, with its codes.

    Args:
        envelopes: ``(minx, maxx, miny, maxy)`` per feature, or None.

    Returns:
        ``(order, codes)``: indices in order (stable), and the code of each feature.
    """
    valid = [e for e in envelopes if e is not None]
    x0, x1 = min(e[0] for e in valid), max(e[1] for e in valid)
    y0, y1 = min(e[2] for e in valid), max(e[3] for e in valid)
    top = (1 << 16) - 2
    codes = []
    for e in envelopes:
        if e is None:
            codes.append(0xFFFFFFFF)
            continue
        cx, cy = (e[0] + e[1]) / 2, (e[2] + e[3]) / 2
        hx = round(top * (cx - x0) / (x1 - x0)) if x1 != x0 else 0
        hy = round(top * (cy - y0) / (y1 - y0)) if y1 != y0 else 0
        codes.append(hilbert(int(hx), int(hy)))
    return sorted(range(len(envelopes)), key=lambda i: codes[i]), codes


def field_string(v):
    """A value as OGR's GetFieldAsString formats it (the types of the test data).

    Args:
        v: The value.

    Returns:
        Its text.
    """
    if v is None:
        return ""
    if isinstance(v, float):
        return "%.15g" % v
    return str(v)


def combine_reference(features, group_by, multi):
    """GDAL 3.13's ``gdal vector combine`` (without --keep-nested), in Python.

    Args:
        features: ``(geometry, attributes)`` tuples.
        group_by: The grouping field names.
        multi: The output collection type (an OGR wkb type).

    Returns:
        ``(geometry, attributes)`` per group, in GDAL's order.
    """
    groups = {}
    for g, attrs in features:
        fields = {k.casefold(): k for k in attrs}
        actual = [fields[k.casefold()] for k in group_by]
        key = tuple(field_string(attrs[k]) for k in actual)
        if key not in groups:
            groups[key] = (ogr.Geometry(multi), {k: attrs[k] for k in actual})
        out = groups[key][0]
        if g is None or g.IsEmpty():
            continue
        if ogr.GT_IsSubClassOf(ogr.GT_Flatten(g.GetGeometryType()), ogr.wkbGeometryCollection):
            for i in range(g.GetGeometryCount()):
                out.AddGeometry(g.GetGeometryRef(i))
        else:
            out.AddGeometry(g)
    return [groups[k] for k in sorted(groups, key=lambda k: [s.encode() for s in k])]


def envelopes_of(features):
    """The envelopes of features' geometries.

    Args:
        features: ``(geometry, attributes)`` tuples.

    Returns:
        ``(minx, maxx, miny, maxy)`` per feature, or None for null or empty geometries.
    """
    return [g.GetEnvelope() if g is not None and not g.IsEmpty() else None for g, _ in features]


def valid_str_order(order, envelopes):
    """Whether an order is one GEOS's STR tree can give: slices of features by box centre X
    (ties in either slice), each sorted by box centre Y, nulls last.

    Args:
        order: Feature indices.
        envelopes: ``(minx, maxx, miny, maxy)`` per feature, or None.

    Returns:
        True when the order is valid.
    """
    items = [i for i in order if envelopes[i] is not None]
    n = len(items)
    if n <= 1:
        return True
    slices = math.ceil(math.sqrt(math.ceil(n / 10)))
    per = math.ceil(n / slices)

    def centre(i, axis):
        lo, hi = envelopes[i][axis : axis + 2]
        total = lo + hi
        # Finite sums preserve subnormal midpoints; only an overflowing sum
        # needs half-before-add (where both terms are far from underflow).
        return total / 2 if math.isfinite(total) else lo / 2 + hi / 2

    x = lambda i: centre(i, 0)  # noqa: E731
    y = lambda i: centre(i, 2)  # noqa: E731
    chunks = [items[k : k + per] for k in range(0, n, per)]
    for a, b in zip(chunks, chunks[1:]):
        if max(map(x, a)) > min(map(x, b)):
            return False
    return all(y(p) <= y(q) for c in chunks for p, q in zip(c, c[1:]))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("data", type=Path)
    local_terrio = Path(__file__).resolve().parents[1] / "target/release/terrio"
    p.add_argument("--terrio", default=shutil.which("terrio") or str(local_terrio))
    p.add_argument("--work-dir", type=Path)
    p.add_argument("--json", type=Path)
    args = p.parse_args()
    if not shutil.which(args.terrio):
        p.error("pass --terrio with the compiled CLI path")
    data = args.data.resolve()
    work = (args.work_dir or data / "vector-geom-validation").resolve()
    work.mkdir(parents=True, exist_ok=True)
    paths = prepare(data, work)
    r = Runner(args.terrio, work)
    geos = Geos()

    # gdal vector simplify / segmentize / swap-xy / set-geom-type: same command, same output.
    for name, cmd, src, options in [
        ("simplify-tn-500m", "simplify", "tn", ["--tolerance", "500"]),
        ("simplify-tn-5km", "simplify", "tn", ["--tolerance", "5000"]),
        ("simplify-us-0.01deg", "simplify", "us", ["--tolerance", "0.01"]),
        ("simplify-us-0.1deg", "simplify", "us", ["--tolerance", "0.1"]),
        ("simplify-roads-5m", "simplify", "roads", ["--tolerance", "5"]),
        ("simplify-ne-1deg", "simplify", "ne", ["--tolerance", "1"]),
        ("segmentize-tn-1km", "segmentize", "tn", ["--max-length", "1000"]),
        ("segmentize-us-0.05deg", "segmentize", "us", ["--max-length", "0.05"]),
        ("segmentize-roads-20m", "segmentize", "roads", ["--max-length", "20"]),
        ("swap-xy-us", "swap-xy", "us", []),
        ("swap-xy-places", "swap-xy", "places", []),
        ("set-geom-type-tn-single", "set-geom-type", "tn", ["--single"]),
        ("set-geom-type-roads-multi", "set-geom-type", "roads", ["--multi"]),
        ("set-geom-type-tn-multilinestring", "set-geom-type", "tn", ["--geometry-type", "MULTILINESTRING"]),
        ("set-geom-type-us-polygon", "set-geom-type", "us", ["--geometry-type", "POLYGON"]),
        ("set-geom-type-places-xyz", "set-geom-type", "places", ["--dim", "XYZ"]),
        ("set-geom-type-roads-point-skip", "set-geom-type", "roads", ["--geometry-type", "POINT", "--skip"]),
    ]:
        out_t = r.terrio_run([cmd, paths[src], *options], name)
        out_g = r.gdal_run([cmd, paths[src], *options], name)
        r.compare_features(name, load(out_t), load(out_g))
        if cmd == "set-geom-type":
            # The declared type (Z included) as GDAL writes it.
            assert layer_type(out_t) == layer_type(out_g) or not load(out_t), (
                f"{name}: layer type {layer_type(out_t)} != {layer_type(out_g)}"
            )

    # ogr2ogr -simplify / -segmentize through terrio convert.
    for name, option, value, src in [
        ("convert-simplify-us", "-simplify", "0.05", "us"),
        ("convert-segmentize-roads", "-segmentize", "15", "roads"),
    ]:
        out_t = work / f"{name}-terrio.gpkg"
        out_g = work / f"{name}-gdal.gpkg"
        for o in (out_t, out_g):
            if o.exists():
                o.unlink()
        subprocess.run([args.terrio, "convert", str(paths[src]), str(out_t), "--" + option[1:], value], check=True)
        gdal.VectorTranslate(str(out_g), str(paths[src]), options=[option, value])
        r.compare_features(name, load(out_t), load(out_g))

    # Convex hulls: OGR's ConvexHull, which gdal vector convex-hull runs.
    for src in ["tn", "us", "roads", "ne"]:
        name = f"convex-hull-{src}"
        out = r.terrio_run(["convex-hull", paths[src]], name)
        expected = [(g.ConvexHull() if g else None, a) for g, a in load(paths[src])]
        r.compare_features(name, load(out), expected)

    # Concave hulls of points: places combined by country (terrio combine), then OGR's
    # ConcaveHull; and of road vertices.
    combined = r.terrio_run(["combine", paths["places"], "--group-by", "adm0_a3"], "places-by-country")
    for src_path, label in [(combined, "places"), (paths["roads"], "roads")]:
        source = load(src_path)
        for ratio in [0.0, 0.1, 0.3, 0.6, 1.0]:
            for holes in [False, True]:
                name = f"concave-hull-{label}-{ratio:g}" + ("-holes" if holes else "")
                options = ["--ratio", str(ratio)] + (["--allow-holes"] if holes else [])
                out = r.terrio_run(["concave-hull", src_path, *options], name)
                expected = [g.ConcaveHull(ratio, holes) if g else None for g, _ in source]
                r.compare_hulls(name, [g for g, _ in load(out)], expected, 1e-10)
    # Concave hulls of polygons: GEOS's ConcaveHullOfPolygons, which OGR 3.13 calls.
    # prepare() promotes the mixed Natural Earth input to a MultiPolygon layer,
    # so neither writer coerces disjoint hull components into polygon holes.
    ne = load(paths["ne"])
    for ratio, tight, holes in [
        (0.0, False, False),
        (0.1, False, False),
        (0.3, False, False),
        (0.3, True, False),
        (0.3, False, True),
        (0.6, False, False),
        (1.0, False, False),
    ]:
        name = f"concave-hull-ne-{ratio:g}" + ("-tight" if tight else "") + ("-holes" if holes else "")
        options = ["--ratio", str(ratio)] + (["--tight"] if tight else []) + (["--allow-holes"] if holes else [])
        out = r.terrio_run(["concave-hull", paths["ne"], *options], name)
        expected = [geos.concave_hull_of_polygons(g, ratio, tight, holes) for g, _ in ne]
        r.compare_hulls(name, [g for g, _ in load(out)], expected, 1e-10)

    # A notched shell exposes erosion triangles on two sides: GEOS keeps the first
    # exposed edge in its map, rather than replacing it with the longest edge.
    notch = ogr.CreateGeometryFromWkt("MULTIPOLYGON (((0 0,8 0,8 2,2 2,2 8,0 8,0 0)),((5 5,9 5,9 9,5 9,5 5)))")
    notch_path = work / "notched-polygons.geojson"
    notch_path.write_text(json.dumps({"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {}, "geometry": json.loads(notch.ExportToJson())}]}))
    for tight in [False, True]:
        name = "concave-hull-notch" + ("-tight" if tight else "")
        options = ["--ratio", "0.3"] + (["--tight"] if tight else [])
        out = r.terrio_run(["concave-hull", str(notch_path), *options], name)
        expected = [geos.concave_hull_of_polygons(notch, 0.3, tight, False)]
        r.compare_hulls(name, [g for g, _ in load(out)], expected, 1e-10)

    # Four shells touching only at vertices enclose an exterior gap, not an
    # input polygon hole. GEOS fills that gap even without --allow-holes.
    touching = ogr.CreateGeometryFromWkt("MULTIPOLYGON (((0 0,4 0,4 4,0 4,0 0)),((4 4,8 4,8 8,4 8,4 4)),((8 0,12 0,12 4,8 4,8 0)),((4 -4,8 -4,8 0,4 0,4 -4)))")
    touching_path = work / "touching-shells.geojson"
    touching_path.write_text(json.dumps({"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {}, "geometry": json.loads(touching.ExportToJson())}]}))
    for tight in [False, True]:
        for holes in [False, True]:
            name = "concave-hull-touching" + ("-tight" if tight else "") + ("-holes" if holes else "")
            options = ["--ratio", "1"] + (["--tight"] if tight else []) + (["--allow-holes"] if holes else [])
            out = r.terrio_run(["concave-hull", str(touching_path), *options], name)
            expected = [geos.concave_hull_of_polygons(touching, 1, tight, holes)]
            r.compare_hulls(name, [g for g, _ in load(out)], expected, 0)

    # combine: groups ordered by their text values, parts added one by one.
    for name, src, field, multi in [
        ("combine-places-by-country", "places", "adm0_a3", ogr.wkbMultiPoint),
        ("combine-us-by-state", "us", "STATE_FIPS", ogr.wkbGeometryCollection),
        ("combine-roads-by-class", "roads", "class", ogr.wkbMultiLineString),
    ]:
        out = r.terrio_run(["combine", paths[src], "--group-by", field], name)
        r.compare_features(name, load(out), combine_reference(load(paths[src]), [field], multi))
    out = r.terrio_run(["combine", paths["tn"]], "combine-tn-all")
    r.compare_features("combine-tn-all", load(out), combine_reference(load(paths["tn"]), [], ogr.wkbGeometryCollection))

    # sort: the order of the features, by their source attributes.
    for src in ["us", "places", "roads"]:
        # A unique source row ID survives sorting even when all user attributes
        # are duplicated. Keep it separate from the geometries being checked.
        sort_path = work / f"sort-{src}-source.gpkg"
        if sort_path.exists():
            sort_path.unlink()
        ds = gdal.VectorTranslate(str(sort_path), str(paths[src]))
        layer = ds.GetLayer(0)
        row_field = "terrio_validation_row"
        assert layer.FindFieldIndex(row_field, 1) < 0
        layer.CreateField(ogr.FieldDefn(row_field, ogr.OFTInteger64))
        for row, feature in enumerate(layer):
            feature.SetField(row_field, row)
            layer.SetFeature(feature)
        ds = None
        source = load(sort_path)
        envelopes = envelopes_of(source)
        for method in ["hilbert", "strtree"]:
            name = f"sort-{src}-{method}"
            out = r.terrio_run(["sort", sort_path, "--method", method], name)
            got = load(out)
            order = [a[row_field] for _, a in got]
            if method == "hilbert":
                expected, codes = hilbert_order(envelopes)
                tie = lambda i, j: codes[i] == codes[j]  # noqa: E731
            else:
                expected = geos.strtree_order(envelopes)
                tie = lambda i, j: False  # noqa: E731
            same = sum(1 for a, b in zip(order, expected) if a == b or tie(a, b))
            assert sorted(order) == list(range(len(source))), name + ": features lost"
            status = IDENTICAL if same == len(order) else DIFFERS
            if status == DIFFERS and method == "strtree" and valid_str_order(order, envelopes):
                # GEOS sorts with std::sort: features whose box centres tie in X may fall on
                # either side of a slice boundary, which reorders those slices.
                status = TOLERANCE
            r.record(name, status, features=len(order), same_positions=same)

    report = {
        "reference": "GDAL " + gdal.VersionInfo("RELEASE_NAME") + " and GEOS "
        + ".".join(str(v) for v in (ogr.GetGEOSVersionMajor(), ogr.GetGEOSVersionMinor(), ogr.GetGEOSVersionMicro())),
        "cases": r.cases,
    }
    (args.json or work / "report.json").write_text(json.dumps(report, indent=2) + "\n")

if __name__ == "__main__":
    main()
