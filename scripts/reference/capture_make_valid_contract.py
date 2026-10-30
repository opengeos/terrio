"""Capture public GEOS linework observations for an independent tile repair.

Inputs were authored for this specification, not copied from upstream tests or
Terrio's deferred repair. This script calls public Shapely APIs only. It does not
import Terrio, read implementation source, or implement geometry repair.

Run with Shapely 2.1+; stdout is the complete, reproducible JSON capture. Redirect
to a new scratch file, review differences, then explicitly update any fixture.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import platform
import sys

import shapely
from shapely.geometry import MultiPolygon, Polygon


def closed(points):
    points = list(points)
    return points + [points[0]] if points and points[-1] != points[0] else points


def square(x0, y0, x1, y1):
    return closed([(x0, y0), (x1, y0), (x1, y1), (x0, y1)])


def cases():
    a = square(0, 0, 6, 6)
    inner = square(2, 2, 4, 4)
    bow = closed([(0, 0), (6, 6), (0, 6), (6, 0)])
    fractional = closed([(0, 0), (7, 5), (0, 5), (6, 0)])
    base = [
        ("empty", []),
        ("empty-component", [[]]),
        ("square", [[a]]),
        ("square-hole", [[a, inner]]),
        ("square-hole-reversed", [[a, inner[::-1]]]),
        ("bowtie", [[bow]]),
        ("fractional-crossing", [[fractional]]),
        ("half-integer-crossing", [[closed([(0, 0), (7, 5), (0, 5), (7, 0)])]]),
        ("negative-fractional-crossing", [[closed([(-x, -y) for x, y in fractional])]]),
        ("point-touch-lobes", [[closed([(0, 0), (3, 3), (0, 6), (-3, 3), (0, 0), (3, -3), (6, 0), (3, 3)])]]),
        ("outward-spike", [[closed([(0, 0), (6, 0), (6, 6), (3, 6), (3, 9), (3, 6), (0, 6)])]]),
        ("inward-spike", [[closed([(0, 0), (6, 0), (6, 6), (3, 6), (3, 3), (3, 6), (0, 6)])]]),
        ("collapsed-collinear", [[closed([(0, 0), (3, 0), (6, 0)])]]),
        ("collapsed-point", [[[(2, 2)] * 4]]),
        ("duplicate-consecutive-vertex", [[closed([(0, 0), (6, 0), (6, 0), (6, 6), (0, 6)])]]),
        ("double-wound", [[a[:-1] + a]]),
        ("adjacent-squares", [[a], [square(6, 0, 12, 6)]]),
        ("point-touch-squares", [[a], [square(6, 6, 12, 12)]]),
        ("overlap-crossing", [[a], [square(3, 2, 9, 8)]]),
        ("overlap-coincident-edges", [[a], [square(3, 0, 9, 6)]]),
        ("identical-components", [[a], [a]]),
        ("identical-components-reversed", [[a], [a[::-1]]]),
        ("nested-components", [[square(0, 0, 9, 9)], [square(3, 3, 6, 6)]]),
        ("hole-outside", [[a, square(9, 0, 12, 3)]]),
        ("hole-crosses-shell", [[a, square(3, -3, 9, 3)]]),
        ("hole-shares-shell-edge", [[a, square(0, 2, 3, 4)]]),
        ("hole-touches-shell-point", [[a, closed([(0, 3), (2, 1), (4, 3), (2, 5)])]]),
        ("overlapping-holes", [[square(0, 0, 12, 12), square(2, 2, 7, 7), square(5, 5, 10, 10)]]),
        ("nested-holes", [[square(0, 0, 12, 12), square(2, 2, 10, 10), square(4, 4, 8, 8)]]),
        ("duplicate-holes", [[a, inner, inner]]),
        ("shell-equals-hole", [[a, a]]),
        ("three-nested-rings", [[square(0, 0, 12, 12)], [square(2, 2, 10, 10)], [square(4, 4, 8, 8)]]),
        ("valid-plus-collapsed", [[a], [closed([(9, 0), (12, 0), (15, 0)])]]),
        ("collinear-subdivision-overlap", [[a], [closed([(0, 0), (3, 0), (6, 0), (6, 6), (0, 6)])]]),
        ("wide-i32-bowtie", [[closed([(-2147483648, -2147483648), (2147483647, 2147483647), (-2147483648, 2147483647), (2147483647, -2147483648)])]]),
        ("near-i32-max", [[closed([(2147483639, 2147483639), (2147483646, 2147483644), (2147483639, 2147483644), (2147483645, 2147483639)])]]),
    ]
    yield from base
    for name, polygons in base:
        if name in {"bowtie", "fractional-crossing", "overlap-crossing", "overlap-coincident-edges", "nested-components", "hole-outside"}:
            yield name + "-reverse-rings", [[ring[::-1] for ring in p] for p in polygons]
            yield name + "-rotate-rings", [[ring[1:-1] + ring[:2] for ring in p] for p in polygons]
            if len(polygons) > 1:
                yield name + "-reverse-components", polygons[::-1]


def polygon_parts(geometry):
    if geometry.is_empty:
        return []
    if geometry.geom_type == "Polygon":
        return [geometry]
    if geometry.geom_type in {"MultiPolygon", "GeometryCollection"}:
        return [p for g in geometry.geoms for p in polygon_parts(g)]
    return []


def coordinates(polygon):
    return [list(polygon.exterior.coords)] + [list(r.coords) for r in polygon.interiors]


def binary_manifest():
    """Identify loaded GEOS binaries on Linux without inspecting their source."""
    maps = Path("/proc/self/maps")
    if not maps.exists():
        return {"status": "unavailable; record loaded GEOS binaries separately"}
    paths = sorted({line.split()[-1] for line in maps.read_text().splitlines() if "/libgeos" in line})
    return [{"name": Path(p).name, "sha256": hashlib.sha256(Path(p).read_bytes()).hexdigest()} for p in paths]


def main():
    observations = []
    for name, polygons in cases():
        geometry = MultiPolygon([Polygon(p[0], p[1:]) if p else Polygon() for p in polygons])
        result = shapely.make_valid(geometry, method="linework", keep_collapsed=True)
        parts = polygon_parts(result)
        polygonal = MultiPolygon(parts)
        observations.append({
            "id": name,
            "input_polygons": polygons,
            "input_wkb": shapely.to_wkb(geometry, byte_order=1).hex(),
            "input_valid": bool(shapely.is_valid(geometry)),
            "result_type": result.geom_type,
            "result_wkt": shapely.to_wkt(result, rounding_precision=-1),
            "result_wkb": shapely.to_wkb(result, byte_order=1).hex(),
            "polygonal_wkb": shapely.to_wkb(polygonal, byte_order=1).hex(),
            "polygonal_coordinates": [coordinates(p) for p in parts],
            "polygon_count": len(parts),
            "hole_count": sum(len(p.interiors) for p in parts),
            "polygonal_area": float(polygonal.area),
            "polygonal_valid": bool(shapely.is_valid(polygonal)),
        })
    output = {
        "schema_version": 1,
        "provenance": "Independently authored integer inputs; public executable oracle only; no Terrio implementation imported.",
        "source_baseline": "0281ec0add52008c051cd024ab9a23ca855055d7",
        "reference": {
            "shapely": shapely.__version__,
            "geos": shapely.geos_version_string,
            "geos_capi": shapely.geos_capi_version_string,
            "loaded_geos_binaries": binary_manifest(),
            "python": platform.python_version(),
            "system": platform.system(),
            "machine": platform.machine(),
            "method": "linework",
            "keep_collapsed": True,
            "input_geometry_type": "MultiPolygon",
            "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        },
        "observations": observations,
    }
    json.dump(output, sys.stdout, indent=2, allow_nan=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
