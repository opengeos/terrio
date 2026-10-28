#!/usr/bin/env python3
"""Freeze ISEA cases against pyproj 3.8.0 / PROJ 9.8.1."""

import json
import math
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

ROOT = Path(__file__).resolve().parents[1]

# Method parameters, units, false origin, and whether PROJ's inverse is defined: only for the
# default grid in its two orientations, without a central meridian of its own (+lon_0=11.25 is
# the default orientation's).
CONFIGURATIONS = [
    ("", "m", 0, 0, True),
    ("+orient=pole", "km", 12345, -6789, True),
    ("+lon_0=10", "m", 0, 0, False),
    ("+orient=pole +lon_0=-40 +lat_0=60", "us-ft", -8765, 3456, False),
    ("+azi=30", "m", 1000, 2000, False),
    ("+resolution=5 +aperture=4", "m", 0, 0, False),
    ("+lon_0=11.25", "m", 0, 0, True),
]
# Options PROJ refuses, which tests/isea.rs expects terrio to refuse too.
REFUSED = [
    "+orient=foo",
    "+orient",
    "+mode=foo",
    "+mode",
    "+resolution=4.0",
    "+aperture=-3",
    "+resolution=99999999999999999999x",
    "+azi=30x",
]
# The authalic sphere of WGS 84, and WGS 84: PROJ's forward takes the geodetic latitude on the
# unit sphere and scales it by the semi-major axis, its inverse uses the authalic radius.
FIGURES = ["+R=6371007.1809184747", "+datum=WGS84"]


def transformed(operation, point):
    result = operation.transform(*point, errcheck=False)
    return list(result) if all(map(math.isfinite, result)) else None


def agree(a, b, factor, tolerance):
    """Whether two projected points agree within a tolerance in metres."""
    if a is None or b is None:
        return a is None and b is None
    return all(abs(x - y) * factor <= tolerance for x, y in zip(a, b))


def scattered(count):
    """Points spread over the sphere, from a fixed linear congruential sequence."""
    state, points = 12345, []
    for _ in range(count):
        state = (1103515245 * state + 12345) % 2**31
        u = state / 2**31
        state = (1103515245 * state + 12345) % 2**31
        v = state / 2**31
        points.append([round(360 * u - 180, 6), round(math.degrees(math.asin(2 * v - 1)), 6)])
    return points


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.8.1"
    pyproj.network.set_network_enabled(False)
    for option in REFUSED:
        try:
            CRS(f"+proj=isea +R=1 {option}")
        except pyproj.exceptions.CRSError:
            continue
        raise AssertionError(f"PROJ accepts {option}")
    # The face centres' latitudes, and their vertices' (the icosahedron in the default
    # orientation has vertices at +-26.565 and +-90 degrees of its own frame).
    e_lat = math.degrees(math.atan((3 + math.sqrt(5)) / 4))
    f_lat = math.degrees(math.atan((3 - math.sqrt(5)) / 4))
    latitudes = [-90, -75, -60, -45, -30, -15, 0, 15, 30, 45, 60, 75, 90]
    latitudes += [s * x for s in [-1, 1] for x in [e_lat, f_lat, 26.565051177077994]]
    grid = [[lon, lat] for lon in range(-180, 180, 15) for lat in latitudes]
    cases = []
    for figure in FIGURES:
        for method, units, x0, y0, invertible in CONFIGURATIONS:
            source = "+proj=longlat " + figure
            target = f"+proj=isea {method} {figure} +units={units} +x_0={x0} +y_0={y0}".replace("  ", " ")
            source_crs, target_crs = CRS(source), CRS(target)
            forward = Transformer.from_crs(source_crs, target_crs, always_xy=True)
            inverse = Transformer.from_crs(target_crs, source_crs, always_xy=True)
            # The CRS route must be PROJ's operation itself.
            operation = Transformer.from_pipeline(
                f"+proj=pipeline +step +proj=unitconvert +xy_in=deg +xy_out=rad +step {target}"
            )
            factor = target_crs.axis_info[0].unit_conversion_factor
            samples = []
            for point in grid + scattered(100):
                projected = transformed(forward, point)
                assert projected is not None, (target, point)
                assert agree(projected, transformed(operation, point), factor, 1e-6), (target, point)
                samples.append([*point, projected, transformed(inverse, projected)])
            # PROJ's inverse is defined exactly in the expected configurations: on the sphere at every
            # sample, poles included. On the ellipsoid the forward takes the semi-major axis and the
            # inverse the authalic radius, so some projected points fall outside the triangles and
            # are refused; the others are not.
            defined = [s[3] is not None for s in samples]
            if not invertible:
                assert not any(defined), target
            elif figure.startswith("+R="):
                assert all(defined), target
            else:
                assert sum(defined) > len(defined) // 2, target
            # Points of the plane in units of the radius, inside the unfolded icosahedron, on its
            # edges and outside it; fewer where PROJ refuses them all.
            radius = target_crs.ellipsoid.semi_major_metre
            step = 4 if invertible else 2
            x_max, y_max = 14 * step // 4, 6 * step // 4
            unit = [(x / step, y / step) for x in range(-x_max, x_max + 1) for y in range(-y_max, y_max + 1)]
            inverse_points = [[(x0 + x * radius) / factor, (y0 + y * radius) / factor] for x, y in unit]
            inverse_samples = [[point, transformed(inverse, point)] for point in inverse_points]
            cases.append(
                dict(
                    name=f"isea {method} {figure} {units}".replace("  ", " "),
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
    path = ROOT / "crates/terrio-crs/tests/data/isea_reference.json"
    path.write_text(json.dumps(output, separators=(",", ":")) + "\n")
    samples = sum(len(c["samples"]) for c in cases)
    refused = sum(e is None for c in cases for _, e in c["inverse_samples"])
    print(f"{len(cases)} configurations, {samples} forward samples, {refused} inverse refusals, {path.stat().st_size} bytes")


if __name__ == "__main__":
    main()
