#!/usr/bin/env python3
"""Freeze HEALPix and rHEALPix cases against pyproj 3.8.0 / PROJ 9.8.1."""

import json
import math
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

ROOT = Path(__file__).resolve().parents[1]
EPS = 1e-7
# The latitude where the polar caps meet the equatorial region, asin(2/3).
CAP = math.degrees(math.asin(2 / 3))

# HEALPix: central meridian, rot_xy, units and false origin.
HEALPIX = [
    (0, 0, "m", 0, 0),
    (20, 0, "m", 0, 0),
    (-170, 30, "km", 12345, -6789),
    (90, -45, "us-ft", -8765, 3456),
    (0, 90, "m", 1000, 2000),
]
# rHEALPix: central meridian, north and south polar squares, units and false origin.
RHEALPIX = [
    (0, 0, 0, "m", 0, 0),
    (0, 1, 2, "m", 0, 0),
    (20, 3, 1, "km", 12345, -6789),
    (-150, 2, 3, "us-ft", -8765, 3456),
    (100, 0, 3, "m", 1000, 2000),
]
# A sphere and WGS 84 for each configuration, and for the second of each method an ellipsoid
# flattened enough (|n| >= 0.01) that PROJ computes the authalic latitude in closed form and
# refines its inverse.
FIGURES = ["+R=6371000", "+datum=WGS84"]
FLATTENED = "+a=6378137 +rf=40"


def transformed(operation, point):
    result = operation.transform(*point, errcheck=False)
    return list(result) if all(map(math.isfinite, result)) else None


def wrap(lon):
    return (lon + 180) % 360 - 180


def agree(a, b, factor, tolerance):
    """Whether two projected points agree within a tolerance in metres."""
    if a is None or b is None:
        return a is None and b is None
    return all(abs(x - y) * factor <= tolerance for x, y in zip(a, b))


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.8.1"
    pyproj.network.set_network_enabled(False)
    configurations = [
        (f"+proj=healpix +lon_0={lon_0} +rot_xy={rot}", lon_0, units, x0, y0)
        for lon_0, rot, units, x0, y0 in HEALPIX
    ] + [
        (f"+proj=rhealpix +lon_0={lon_0} +north_square={n} +south_square={s}", lon_0, units, x0, y0)
        for lon_0, n, s, units, x0, y0 in RHEALPIX
    ]
    runs = [(f, c) for f in FIGURES for c in configurations]
    runs += [(FLATTENED, configurations[1]), (FLATTENED, configurations[len(HEALPIX) + 1])]
    cases = []
    for figure, (method, lon_0, units, x0, y0) in runs:
        source = "+proj=longlat " + figure
        target = f"{method} {figure} +units={units} +x_0={x0} +y_0={y0}"
        source_crs, target_crs = CRS(source), CRS(target)
        forward = Transformer.from_crs(source_crs, target_crs, always_xy=True)
        inverse = Transformer.from_crs(target_crs, source_crs, always_xy=True)
        # The CRS route must be PROJ's operation itself.
        operation = Transformer.from_pipeline(
            f"+proj=pipeline +step +proj=unitconvert +xy_in=deg +xy_out=rad +step {target}"
        )
        latitudes = [-90, -75, -60, -45, -30, -15, 0, 15, 30, 45, 60, 75, 90]
        latitudes += [s * (CAP + d) for s in [-1, 1] for d in [-EPS, EPS]]
        points = [[wrap(lon_0 + lon), lat] for lon in range(-180, 180, 30) for lat in latitudes]
        # Each side of the polar caps' edges and on their middles, in the caps.
        points += [
            [wrap(lon_0 + e + d), lat]
            for e in range(-180, 180, 45)
            for d in [-EPS, EPS]
            for lat in [-85, -70, -50, 50, 70, 85]
        ]
        factor = target_crs.axis_info[0].unit_conversion_factor
        # Points on the edge of the image: the poles (the caps' tips), the antimeridian of the
        # central meridian, and in the caps the meridians of their edges. There the last bit of
        # a unit moves the inverse in or out of the image.
        edge = []
        samples = []
        for point in points:
            relative = wrap(point[0] - lon_0)
            if (
                abs(point[1]) == 90
                or relative == -180
                or (abs(point[1]) > CAP and relative % 90 == 0)
            ):
                edge.append(len(samples))
            projected = transformed(forward, point)
            assert agree(projected, transformed(operation, point), factor, 1e-6), (target, point)
            back = transformed(inverse, projected) if projected else None
            samples.append([*point, projected, back])
        radius = target_crs.ellipsoid.semi_major_metre
        # Points in the image, on its edges and outside it.
        unit = [
            (0, 0),
            (3, 0.5),
            (-3, -0.7),
            (0.5, 1.2),
            (2.2, 1.5),
            (0, 1.2),
            (3.3, 0),
            (-2, 2),
            (0, 2),
            (2.5, -2),
            (-0.5, -2.3),
            (1, 2.5),
            (0.3, -0.75),
            # Left of the image: PROJ's rHEALPix inverse accepts it (see docs/crs.md).
            (-4, 0),
        ]
        inverse_points = [[(x0 + x * radius) / factor, (y0 + y * radius) / factor] for x, y in unit]
        inverse_samples = [[point, transformed(inverse, point)] for point in inverse_points]
        # PROJ projects every point; it refuses only points outside the image.
        assert all(s[2] is not None for s in samples), target
        cases.append(
            dict(
                name=f"{method[6:]} {figure} {units}",
                source=source,
                target=target,
                metres_per_unit=factor,
                wkt1=target_crs.to_wkt("WKT1_GDAL"),
                wkt1_esri=target_crs.to_wkt("WKT1_ESRI"),
                wkt2=target_crs.to_wkt("WKT2_2019"),
                samples=samples,
                edge_samples=edge,
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
    path = ROOT / "crates/terrio-crs/tests/data/healpix_reference.json"
    path.write_text(json.dumps(output, separators=(",", ":")) + "\n")
    samples = sum(len(c["samples"]) for c in cases)
    refused = sum(e is None for c in cases for _, e in c["inverse_samples"])
    print(
        f"{len(cases)} configurations, {samples} forward samples, {refused} inverse refusals, {path.stat().st_size} bytes"
    )


if __name__ == "__main__":
    main()
