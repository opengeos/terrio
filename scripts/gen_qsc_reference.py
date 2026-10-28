#!/usr/bin/env python3
"""Freeze quadrilateralized spherical cube cases against pyproj 3.8.0 / PROJ 9.8.1."""

import json
import math
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

ROOT = Path(__file__).resolve().parents[1]
EPS = 1e-7
# The latitude of the cube's corners, atan(1 / sqrt(2)).
CORNER = math.degrees(math.atan(1 / math.sqrt(2)))

# Each cube face by its centre, centres off the face centres (PROJ picks the face and keeps the
# central meridian), units and false origin.
CONFIGURATIONS = [
    (0, 0, "m", 0, 0),
    (0, 90, "m", 0, 0),
    (0, 180, "km", 12345, -6789),
    (0, -90, "m", 0, 0),
    (90, 0, "us-ft", -8765, 3456),
    (-90, 0, "m", 0, 0),
    (20, 40, "m", 1000, 2000),
    (50, -170, "m", 0, 0),
]


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


def distance(a, b):
    """The angle between two points in degrees."""
    (lon1, lat1), (lon2, lat2) = [(math.radians(lon), math.radians(lat)) for lon, lat in [a, b]]
    cosine = math.sin(lat1) * math.sin(lat2) + math.cos(lat1) * math.cos(lat2) * math.cos(lon1 - lon2)
    return math.degrees(math.acos(max(-1.0, min(1.0, cosine))))


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.8.1"
    pyproj.network.set_network_enabled(False)
    cases = []
    for figure in ["+R=6371000", "+datum=WGS84"]:
        for lat_0, lon_0, units, x0, y0 in CONFIGURATIONS:
            source = "+proj=longlat " + figure
            target = f"+proj=qsc +lat_0={lat_0} +lon_0={lon_0} {figure} +units={units} +x_0={x0} +y_0={y0}"
            source_crs, target_crs = CRS(source), CRS(target)
            forward = Transformer.from_crs(source_crs, target_crs, always_xy=True)
            inverse = Transformer.from_crs(target_crs, source_crs, always_xy=True)
            # The CRS route must be PROJ's operation itself.
            operation = Transformer.from_pipeline(
                f"+proj=pipeline +step +proj=unitconvert +xy_in=deg +xy_out=rad +step {target}"
            )
            latitudes = [-90, -75, -60, -45, -30, -15, 0, 15, 30, 45, 60, 75, 90]
            latitudes += [s * (CORNER + d) for s in [-1, 1] for d in [-EPS, EPS]]
            points = [[wrap(lon_0 + lon), lat] for lon in range(-180, 180, 30) for lat in latitudes]
            # Each side of the face and area edges (multiples of 45 degrees from the centre).
            edges = [[wrap(lon_0 + e + d), lat] for e in range(-180, 180, 45) for d in [-EPS, EPS] for lat in [-50, -10, 10, 50]]
            points += edges
            factor = target_crs.axis_info[0].unit_conversion_factor
            # The face centre and its antipode, where the inverse goes through acos near 180
            # degrees and so moves by about 1e-6 degrees with the last bit of the northing.
            centre = (0, math.copysign(90, lat_0)) if abs(lat_0) >= 67.5 else (lon_0, 0)
            antipodal = []
            samples = []
            for point in points:
                if distance(point, centre) > 179.999:
                    antipodal.append(len(samples))
                projected = transformed(forward, point)
                assert agree(projected, transformed(operation, point), factor, 1e-6), (target, point)
                back = transformed(inverse, projected) if projected else None
                samples.append([*point, projected, back])
            radius = target_crs.ellipsoid.semi_major_metre
            # Points of the face, of its areas' edges and beyond it.
            unit = [(0, 0), (0.5, -0.25), (1, 1), (-1, 0.3), (0.2, -1), (1.5, 0), (3, 2), (-2, -0.5)]
            inverse_points = [[(x0 + x * radius) / factor, (y0 + y * radius) / factor] for x, y in unit]
            inverse_samples = [[point, transformed(inverse, point)] for point in inverse_points]
            # PROJ refuses no qsc point; the Python and Node/WASM tests rely on it.
            assert all(s[2] is not None and s[3] is not None for s in samples), target
            assert all(e is not None for _, e in inverse_samples), target
            cases.append(
                dict(
                    name=f"qsc {figure} lat_0 {lat_0} lon_0 {lon_0} {units}",
                    source=source,
                    target=target,
                    metres_per_unit=factor,
                    wkt1=target_crs.to_wkt("WKT1_GDAL"),
                    wkt1_esri=target_crs.to_wkt("WKT1_ESRI"),
                    wkt2=target_crs.to_wkt("WKT2_2019"),
                    samples=samples,
                    antipodal_samples=antipodal,
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
    path = ROOT / "crates/terrio-crs/tests/data/qsc_reference.json"
    path.write_text(json.dumps(output, separators=(",", ":")) + "\n")
    samples = sum(len(c["samples"]) for c in cases)
    print(f"{len(cases)} configurations, {samples} forward samples, {path.stat().st_size} bytes")


if __name__ == "__main__":
    main()
