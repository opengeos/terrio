#!/usr/bin/env python3
"""Freeze interrupted Goode homolosine cases against pyproj 3.8.0 / PROJ 9.9.0."""

import json
import math
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

ROOT = Path(__file__).resolve().parents[1]
BOUNDARY = 40 + 44 / 60 + 11.8 / 3600
EPS = 1e-7


def transformed(operation, point):
    result = operation.transform(*point, errcheck=False)
    return list(result) if all(map(math.isfinite, result)) else None


def wrap(lon):
    return (lon + 180) % 360 - 180


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.9.0"
    pyproj.network.set_network_enabled(False)
    cases = []
    for figure in ["+R=6371000", "+datum=WGS84"]:
        configurations = [(0, "m", 0, 0), (147, "m", 0, 0), (-160, "km", 12345, -6789), (20, "us-ft", -8765, 3456)]
        for lon_0, units, x0, y0 in configurations:
            source = "+proj=longlat " + figure
            target = f"+proj=igh +lon_0={lon_0} {figure} +units={units} +x_0={x0} +y_0={y0}"
            source_crs, target_crs = CRS(source), CRS(target)
            forward = Transformer.from_crs(source_crs, target_crs, always_xy=True)
            inverse = Transformer.from_crs(target_crs, source_crs, always_xy=True)
            latitudes = [-90, -75, -60, -45, -30, -15, 0, 15, 30, 45, 60, 75, 90]
            latitudes += [s * (BOUNDARY + d) for s in [-1, 1] for d in [-EPS, EPS]]
            points = [[wrap(lon_0 + lon), lat] for lon in range(-180, 180, 10) for lat in latitudes]
            # Each side of the interruptions and of the antimeridian opposite the centre.
            cuts = [(-100, [-30, 30, 60, -60]), (-40, [30, 60]), (-20, [-30, -60]), (80, [-30, -60]), (180, [-60, -30, 30, 60])]
            points += [[wrap(lon_0 + cut + d), lat] for cut, lats in cuts for lat in lats for d in [-EPS, EPS]]
            samples = []
            for point in points:
                projected = transformed(forward, point)
                back = transformed(inverse, projected) if projected else None
                samples.append([*point, projected, back])
            factor = target_crs.axis_info[0].unit_conversion_factor
            radius = target_crs.ellipsoid.semi_major_metre
            # Points of the interruptions, past the poles and on PROJ's southern bound. The
            # pole's northing in metres comes from PROJ itself.
            y90 = transformed(forward, [lon_0, 90])[1] * factor - y0
            unit = [
                [0, 1.01 * y90 / radius * 180 / math.pi],
                [0, -1.01 * y90 / radius * 180 / math.pi],
                [0, -y90 / radius * 180 / math.pi],
                [30, y90 / radius * 180 / math.pi],
                [181, 0],
            ]
            inverse_points = [
                [(x0 + math.radians(x) * radius) / factor, (y0 + math.radians(y) * radius) / factor] for x, y in unit
            ]
            # The middle of each interruption, between the lobes' edges.
            for cut, lat in [(-40, 75), (-100, -75), (-20, -75), (80, -75)]:
                west = transformed(forward, [wrap(lon_0 + cut - EPS), lat])
                east = transformed(forward, [wrap(lon_0 + cut + EPS), lat])
                inverse_points.append([(west[0] + east[0]) / 2, northing(west, east)])
            inverse_samples = [[point, transformed(inverse, point)] for point in inverse_points]
            cases.append(
                dict(
                    name=f"igh {figure} lon_0 {lon_0} {units}",
                    source=source,
                    target=target,
                    metres_per_unit=factor,
                    wkt1=target_crs.to_wkt("WKT1_GDAL"),
                    wkt1_esri=target_crs.to_wkt("WKT1_ESRI"),
                    wkt2=target_crs.to_wkt("WKT2_2019"),
                    samples=samples,
                    inverse_samples=inverse_samples,
                )
            )
    output = dict(
        environment=dict(
            pyproj=pyproj.__version__,
            proj=pyproj.proj_version_str,
            network=False,
            forward_tolerance_metres=0.001,
            inverse_tolerance_degrees=1e-8,
        ),
        cases=cases,
    )
    path = ROOT / "crates/terrio-crs/tests/data/igh_reference.json"
    path.write_text(json.dumps(output, separators=(",", ":")) + "\n")
    print(f"{len(cases)} configurations, {sum(len(c['samples']) for c in cases)} forward samples, {path.stat().st_size} bytes")


def northing(west, east):
    """The northing of two points on one parallel."""
    assert abs(west[1] - east[1]) < 1e-6 * max(1, abs(west[1]))
    return west[1]


if __name__ == "__main__":
    main()
