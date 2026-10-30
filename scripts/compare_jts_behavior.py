"""Black-box geometry comparisons for the JTS provenance replacements.

Requires Shapely 2.1+, GDAL Python bindings and GEOS 3.14. Run the geometry_probe
example as --probe. Optionally compare JTS 1.20 with --jts-classpath (directory
containing compiled JtsOracle plus jts-core.jar), using --java. No reference
implementation source is read. --fixtures writes replayable GEOS observations;
--output records exact, tightly tolerant and differing results separately.
"""

from __future__ import annotations
import argparse
from collections import Counter, defaultdict
import json
import math
from pathlib import Path
import random
import subprocess

import shapely
from shapely import from_wkb, to_wkb
from shapely.geometry import (
    GeometryCollection,
    LineString,
    MultiLineString,
    MultiPoint,
    MultiPolygon,
    Point,
    Polygon,
    box,
)
from osgeo import gdal, ogr
from compare_vector_geom_with_gdal import Geos, edges, valid_str_order


def cases():
    rng = random.Random(0x4A5453)

    def case(name, op, g, ratio=0.0, holes=False, tight=False):
        return dict(
            name=name,
            operation=op,
            input=to_wkb(g).hex(),
            ratio=ratio,
            holes=holes,
            tight=tight,
        )

    # Overflow must not turn an unmeasurable section into a within-tolerance
    # simplification. GEOS retains the middle coordinate of this original input.
    yield case(
        "overflow-distance",
        "simplify",
        LineString([(0, 0), (1e155, 1e155), (2e155, 0)]),
        0.3,
    )

    # Observed empty-result contract, including invalid zero-area polygons.
    for name, polygon in [
        ("point", Polygon([(1, 1)] * 4)),
        ("collinear", Polygon([(0, 0), (1, 1), (2, 2)])),
        ("bowtie", Polygon([(0, 0), (2, 2), (0, 2), (2, 0)])),
        ("underflow", Polygon([(0, 0), (1e-200, 0), (0, 1e-200)])),
    ]:
        for tight in (False, True):
            for holes in (False, True):
                yield case(
                    f"zero-area-{name}-{tight}-{holes}",
                    "polygon_hull",
                    polygon,
                    0.3,
                    holes,
                    tight,
                )

    # A valid hole touching its shell at a point away from the traversal start
    # must remain a separate ring. Reflect and cycle the exterior independently.
    shell = [(0, 0), (10, 0), (10, 10), (0, 10), (0, 5)]
    for reflected in (False, True):
        sign = -1 if reflected else 1
        for start in range(len(shell)):
            exterior = shell[start:] + shell[:start]
            polygon = Polygon(
                [(sign * x, y) for x, y in exterior],
                [[(0, 5), (sign * 3, 3), (sign * 3, 7)]],
            )
            for tight in (False, True):
                for holes in (False, True):
                    yield case(
                        f"pinched-hole-{reflected}-{start}-{tight}-{holes}",
                        "polygon_hull",
                        polygon,
                        0.3,
                        holes,
                        tight,
                    )

    # Observed component-jump probes: processed and unprocessed components use
    # different representative points. Cycle ring starts and reverse part order.
    triangle = [(0, 0), (1, 0), (-43, 2)]
    quadrilateral = [(776, -657), (21, -127), (1, 0), (14, 655)]
    for reflected in [False, True]:
        sign = -1 if reflected else 1
        for reversed_parts in [False, True]:
            for start in range(3):
                ring = triangle[start:] + triangle[:start]
                parts = [ring, quadrilateral]
                if reversed_parts:
                    parts.reverse()
                g = MultiPolygon(
                    [Polygon([(sign * x, sign * y) for x, y in part]) for part in parts]
                )
                yield case(
                    f"component-point-{reflected}-{reversed_parts}-{start}",
                    "simplify",
                    g,
                    100,
                )

    # Two disjoint triangles can expose two erosion edges at once. The first
    # exposed edge remains the candidate even if the other side appears later.
    for reflected in [False, True]:
        sign = -1 if reflected else 1
        for reversed_parts in [False, True]:
            parts = [[(18, -5), (15, -6), (12, -2)], [(8, 3), (13, 3), (10, 4)]]
            if reversed_parts:
                parts.reverse()
            g = MultiPolygon(
                [Polygon([(sign * x, sign * y) for x, y in p]) for p in parts]
            )
            for ratio in [0.05, 0.1, 0.2]:
                yield case(
                    f"border-choice-{reflected}-{reversed_parts}-{ratio}",
                    "polygon_hull",
                    g,
                    ratio,
                )

    # Non-cocircular random sites, duplicates, collinear and closed-line inputs.
    for n in range(40):
        points = [
            (rng.uniform(-100, 100), rng.uniform(-100, 100)) for _ in range(5 + n % 31)
        ]
        g = MultiPoint(points)
        yield case(f"sites-{n}", "convex", g)
        for ratio in (0.0, 0.15, 0.4, 0.8, 1.0):
            for holes in (False, True):
                yield case(f"sites-{n}-{ratio}-{holes}", "concave", g, ratio, holes)
    for seed in range(5):
        large_rng = random.Random(0x4A5453 ^ (seed + 100))
        g = MultiPoint(
            [
                (large_rng.uniform(-100, 100), large_rng.uniform(-100, 100))
                for _ in range(80 + 20 * seed)
            ]
        )
        for ratio in (0.3, 0.6):
            for holes in (False, True):
                yield case(
                    f"large-sites-{seed}-{ratio}-{holes}", "concave", g, ratio, holes
                )
    for n, g in enumerate(
        (
            MultiPoint([]),
            Point(1, 2),
            MultiPoint([(1, 2), (1, 2)]),
            MultiPoint([(0, 10), (10, 0)]),
            MultiPoint([(i, 2 * i) for i in range(20)]),
        )
    ):
        for op in ("convex", "concave"):
            yield case(f"degenerate-{n}", op, g, 0.5)
    for n in range(60):
        points = [(i, rng.uniform(-2, 2)) for i in range(30)]
        g = LineString(points)
        if n % 3 == 0:
            g = MultiLineString([points, [(0, 5), (15, 0), (30, 5)]])
        if n % 3 == 1:
            g = Polygon(
                [
                    (
                        10 * math.cos(i * math.tau / 30) * (1 + rng.random() * 0.3),
                        10 * math.sin(i * math.tau / 30) * (1 + rng.random() * 0.3),
                    )
                    for i in range(30)
                ],
                [[(0, 0), (0, 1), (1, 1), (1, 0)]],
            )
        for tol in (0.0, 0.1, 1.0, 5.0, 1000.0):
            yield case(f"simplify-{n}-{tol}", "simplify", g, tol)
    special = [
        Polygon([(0, 0), (10, 0), (10, 10), (0, 10)]),
        Polygon([(0, 0), (10, 0), (10, 10), (0, 10)], [[(0, 5), (5, 2), (5, 8)]]),
        Polygon(
            [(0, 0), (10, 0), (10, 10), (0, 10)], [[(0, 5), (5, 0), (10, 5), (5, 8)]]
        ),
        Polygon([(0, 0), (10, 10), (10, 0), (0, 10)]),
        Polygon([(0, 0), (10, 0), (5, 5), (10, 10), (0, 10), (5, 5)]),
        Polygon(
            [(0, 0), (20, 0), (20, 20), (0, 20)],
            [
                [(0, 10), (5, 5), (10, 10), (5, 15)],
                [(10, 10), (15, 5), (20, 10), (15, 15)],
            ],
        ),
        Polygon(
            [(0, 0), (20, 0), (20, 20), (0, 20)],
            [[(5, 10), (10, 5), (15, 10), (10, 15)], [(5, 10), (0, 5), (0, 15)]],
        ),
    ]
    for n, g in enumerate(special):
        if g.is_valid:
            for tol in (0.0, 1.0, 20.0):
                yield case(f"contact-{n}-{tol}", "simplify", g, tol)
        yield case(f"valid-contact-{n}", "valid", g)
    for n in range(300):
        pts = [
            (rng.randrange(-20, 21), rng.randrange(-20, 21)) for _ in range(3 + n % 12)
        ]
        g = Polygon(pts)
        if n % 4 == 0:
            g = g.convex_hull
        if n % 4 == 1:
            g = MultiPolygon(
                [
                    box(0, 0, 10, 10),
                    box(rng.randrange(8, 13), rng.randrange(8, 13), 20, 20),
                ]
            )
        if n % 4 == 2:
            x, y = rng.randrange(-2, 9), rng.randrange(-2, 9)
            g = Polygon(
                box(0, 0, 10, 10).exterior.coords,
                [box(x, y, x + 3, y + 3).exterior.coords],
            )
        if isinstance(g, (Polygon, MultiPolygon)):
            yield case(f"valid-{n}", "valid", g)
    # Full-range integer predicates and repeated adjacent vertices.
    for g in [
        box(-2147483648, -2147483648, 2147483647, 2147483647),
        Polygon([(0, 0), (0, 0), (10, 0), (10, 10), (0, 10)]),
        MultiPolygon([box(0, 0, 10, 10), box(10, 10, 20, 20)]),
        MultiPolygon(
            [
                Polygon(
                    box(0, 0, 20, 20).exterior.coords,
                    [box(2, 2, 18, 18).exterior.coords],
                ),
                box(5, 5, 10, 10),
            ]
        ),
    ]:
        yield case("valid-extreme-or-contact", "valid", g)
    for n in range(25):
        # Disjoint non-grid polygons with non-cocircular shell sites.
        polys = []
        for i in range(3 + n % 4):
            x = (i % 3) * 30 + rng.random()
            y = (i // 3) * 30 + rng.random()
            polys.append(
                Polygon(
                    [
                        (x, y),
                        (x + 10 + rng.random(), y),
                        (x + 10, y + 10 + rng.random()),
                        (x, y + 10),
                    ]
                )
            )
        g = MultiPolygon(polys)
        for ratio in (0.0, 0.2, 0.6, 1.0):
            for tight in (False, True):
                for holes in (False, True):
                    yield case(
                        f"polygons-{n}-{ratio}-{tight}-{holes}",
                        "polygon_hull",
                        g,
                        ratio,
                        holes,
                        tight,
                    )
    # Grid / ties are retained as adversarial observations rather than removed.
    for n in range(5):
        g = MultiPoint([(x, y) for x in range(6 + n) for y in range(5)])
        for ratio in (0.0, 0.5, 1.0):
            for holes in (False, True):
                yield case(f"grid-{n}-{ratio}-{holes}", "concave", g, ratio, holes)
    for n in (0, 1, 2, 9, 10, 11, 21, 100, 101, 1001):
        g = GeometryCollection(
            [
                box(
                    rng.random() * 100,
                    rng.random() * 100,
                    100 + rng.random() * 10,
                    100 + rng.random() * 10,
                )
                for _ in range(n)
            ]
        )
        yield case(f"str-{n}", "strtree", g)

    yield case(
        "str-overflow-centres",
        "strtree",
        GeometryCollection([Point(1e308 + i * 1e306, i) for i in reversed(range(24))]),
    )

    for name, parts in [
        ("str-ties", [Point((i % 3) * 10, (i // 3) % 3) for i in range(35)]),
        ("str-overflow-centres", [Point(1e308, 1e308) for _ in range(25)]),
        (
            "str-signed-zero",
            [Point(-0.0 if i % 2 else 0.0, float(i % 3)) for i in range(25)],
        ),
    ]:
        yield case(name, "strtree", GeometryCollection(parts))


def reference(c, geos):
    g = from_wkb(bytes.fromhex(c["input"]))
    op = c["operation"]
    if op == "valid":
        return bool(g.is_valid)
    if op == "strtree":
        return geos.strtree_order(
            [(p.bounds[0], p.bounds[2], p.bounds[1], p.bounds[3]) for p in g.geoms]
        )
    if op == "simplify":
        result = shapely.simplify(g, c["ratio"], preserve_topology=True)
    elif op == "convex":
        result = g.convex_hull
    elif op == "concave":
        result = shapely.concave_hull(g, ratio=c["ratio"], allow_holes=c["holes"])
    else:
        result = from_wkb(
            bytes(
                geos.concave_hull_of_polygons(
                    ogr.CreateGeometryFromWkb(bytes.fromhex(c["input"])),
                    c["ratio"],
                    c["tight"],
                    c["holes"],
                ).ExportToWkb()
            )
        )
    return to_wkb(result).hex()


def compare(c, actual, expected):
    if c["operation"] == "valid":
        return ("exact" if actual == expected else "differs"), 0.0
    if c["operation"] == "strtree":
        if actual == expected:
            return "exact", 0.0
        g = from_wkb(bytes.fromhex(c["input"]))
        envs = [(p.bounds[0], p.bounds[2], p.bounds[1], p.bounds[3]) for p in g.geoms]
        return (
            "ties"
            if sorted(actual) == sorted(expected) and valid_str_order(actual, envs)
            else "differs"
        ), 0.0
    a = ogr.CreateGeometryFromWkb(bytes.fromhex(actual))
    b = ogr.CreateGeometryFromWkb(bytes.fromhex(expected))
    if c["operation"] == "simplify":
        return ("exact" if a.ExportToIsoWkb() == b.ExportToIsoWkb() else "differs"), 0.0
    if a.IsEmpty() or b.IsEmpty():
        return ("exact" if a.IsEmpty() and b.IsEmpty() else "differs"), 0.0
    sa = from_wkb(bytes.fromhex(actual))
    sb = from_wkb(bytes.fromhex(expected))
    if not sa.is_valid or not sb.is_valid:
        return "differs", None
    if edges(a) == edges(b):
        return "exact", 0.0
    if sa.geom_type not in ("Polygon", "MultiPolygon") or sb.area == 0:
        return "differs", None
    area = sa.symmetric_difference(sb).area / max(sb.area, 1e-300)
    distance = sa.hausdorff_distance(sb)
    scale = max(1.0, *(abs(v) for v in sb.bounds))
    return (
        "tolerance" if area <= 1e-10 and distance <= 1e-10 * scale else "differs"
    ), area


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--probe", type=Path)
    p.add_argument("--fixtures", type=Path)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--java", default="java")
    p.add_argument("--jts-classpath")
    args = p.parse_args()
    corpus = list(cases())
    geos = Geos()
    for c in corpus:
        c["expected"] = reference(c, geos)
    if args.jts_classpath:
        version = subprocess.check_output(
            [args.java, "-cp", args.jts_classpath, "JtsOracle", "--version"], text=True
        ).strip()
        if version != "1.20.0":
            raise RuntimeError(f"expected JTS 1.20.0, got {version}")
        lines = [
            "\t".join(
                [
                    c["operation"],
                    str(c["ratio"]),
                    str(c["tight"]).lower(),
                    str(c["holes"]).lower(),
                    c["input"],
                ]
            )
            for c in corpus
        ]
        result = subprocess.run(
            [args.java, "-cp", args.jts_classpath, "JtsOracle"],
            input="\n".join(lines) + "\n",
            text=True,
            capture_output=True,
            check=True,
        )
        outputs = result.stdout.splitlines()
        assert len(outputs) == len(corpus), (len(outputs), len(corpus), result.stderr)
        for c, out in zip(corpus, outputs):
            if out.startswith("ERROR"):
                c["jts_error"] = out
            else:
                c["jts_expected"] = (
                    json.loads(out)
                    if c["operation"] in ("valid", "strtree")
                    else out.lower()
                )
    jts_summary = defaultdict(Counter)
    for c in corpus:
        if "jts_expected" in c:
            status, error = compare(c, c["jts_expected"], c["expected"])
            c["jts_status"] = status
            jts_summary[c["operation"]][status] += 1
            if status == "exact":
                del c["jts_expected"]
        elif "jts_error" in c:
            jts_summary[c["operation"]]["error"] += 1
    if args.fixtures:
        args.fixtures.mkdir(parents=True, exist_ok=True)
        grouped = defaultdict(list)
        for c in corpus:
            grouped[c["operation"]].append(c)
        for op, group in grouped.items():
            reference_name = (
                f"GEOS {shapely.geos_version_string}; GDAL {gdal.VersionInfo('RELEASE_NAME')}"
                + ("; JTS 1.20.0" if args.jts_classpath else "")
            )
            (args.fixtures / f"{op}.json").write_text(
                '{"reference":'
                + json.dumps(reference_name)
                + ',"cases":[\n'
                + ",\n".join(json.dumps(c, separators=(",", ":")) for c in group)
                + "\n]}\n"
            )
    summary = defaultdict(Counter)
    differences = []
    if args.probe:
        selected = [c for c in corpus if c["operation"] != "valid"]
        result = subprocess.run(
            [str(args.probe.resolve())],
            input="\n".join(json.dumps(c) for c in selected) + "\n",
            text=True,
            capture_output=True,
            check=True,
        )
        outputs = [json.loads(line) for line in result.stdout.splitlines()]
        assert len(outputs) == len(selected)
        for c, out in zip(selected, outputs):
            status, error = (
                compare(c, out["output"], c["expected"])
                if "output" in out
                else ("error", None)
            )
            summary[c["operation"]][status] += 1
            if status in ("differs", "error"):
                differences.append(
                    {
                        "name": c["name"],
                        "operation": c["operation"],
                        "relative_area": error,
                        "actual": out,
                        **c,
                    }
                )
    report = {
        "reference": f"GEOS {shapely.geos_version_string}, GDAL {gdal.VersionInfo('RELEASE_NAME')}",
        "cases": len(corpus),
        "summary": summary,
        "jts_summary": jts_summary,
        "differences": differences,
    }
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "differences"}, indent=2))
    return int(bool(differences))


if __name__ == "__main__":
    raise SystemExit(main())
