#!/usr/bin/env python3
"""Freeze general oblique transformation (ob_tran) cases against pyproj 3.8.0 / PROJ 9.8.1."""

import json
import math
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

ROOT = Path(__file__).resolve().parents[1]

# The rotated projection, its own parameters, the new pole in each of PROJ's three forms, the
# central meridian, units and false origin.
CONFIGURATIONS = [
    ("moll", "", "+o_lon_p=0 +o_lat_p=45", 147, "m", 0, 0),
    ("merc", "+lat_ts=20", "+o_lon_p=-40 +o_lat_p=32.5", 10, "km", 12345, -6789),
    ("eqc", "", "+o_lon_p=20 +o_lat_p=0", -60, "m", 0, 0),
    ("sinu", "", "+o_lon_c=20 +o_lat_c=50 +o_alpha=30", 0, "us-ft", -8765, 3456),
    ("ortho", "+lat_0=30", "+o_lon_p=100 +o_lat_p=60", -100, "m", 0, 0),
    ("robin", "", "+o_lon_1=-30 +o_lat_1=20 +o_lon_2=60 +o_lat_2=-10", 30, "m", 0, 0),
    ("lcc", "+lat_1=30 +lat_2=60 +lat_0=40", "+o_lon_p=-120 +o_lat_p=50", 170, "m", 1000, 2000),
    ("utm", "+zone=33", "+o_lon_p=15 +o_lat_p=80", 15, "m", 0, 0),
]

# Definitions PROJ refuses.
REFUSED = [
    "+proj=ob_tran +o_proj=ob_tran +o_lat_p=45 +R=6371000",
    "+proj=ob_tran +o_proj=merc +o_lon_c=20 +o_lat_c=90 +o_alpha=30 +R=6371000",
    "+proj=ob_tran +o_proj=merc +o_lon_1=-30 +o_lat_1=0 +o_lon_2=60 +o_lat_2=-10 +R=6371000",
    "+proj=ob_tran +o_proj=merc +o_lon_1=-30 +o_lat_1=20 +o_lon_2=60 +o_lat_2=20 +R=6371000",
    "+proj=ob_tran +o_proj=merc +o_lon_1=-30 +o_lat_1=90 +o_lon_2=60 +o_lat_2=20 +R=6371000",
]


def transformed(operation, point):
    result = operation.transform(*point, errcheck=False)
    return list(result) if all(map(math.isfinite, result)) else None


def wrap(lon):
    return (lon + 180) % 360 - 180


def agree(a, b, tolerance):
    if a is None or b is None:
        return a is None and b is None
    return all(abs(x - y) <= tolerance for x, y in zip(a, b))


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.8.1"
    pyproj.network.set_network_enabled(False)
    cases = []
    for figure in ["+R=6371000", "+datum=WGS84"]:
        for o_proj, own, pole, lon_0, units, x0, y0 in CONFIGURATIONS:
            if o_proj == "utm" and figure.startswith("+R="):
                continue
            source = "+proj=longlat " + figure
            target = (
                f"+proj=ob_tran +o_proj={o_proj} {own} {pole} +lon_0={lon_0} {figure} +units={units} +x_0={x0} +y_0={y0}"
            ).replace("  ", " ")
            source_crs, target_crs = CRS(source), CRS(target)
            forward = Transformer.from_crs(source_crs, target_crs, always_xy=True)
            inverse = Transformer.from_crs(target_crs, source_crs, always_xy=True)
            # The CRS route must be PROJ's operation itself.
            operation = Transformer.from_pipeline(
                f"+proj=pipeline +step +proj=unitconvert +xy_in=deg +xy_out=rad +step {target}"
            )
            latitudes = [-90, -75, -60, -45, -30, -15, 0, 15, 30, 45, 60, 75, 90]
            points = [[wrap(lon_0 + lon), lat] for lon in range(-180, 180, 15) for lat in latitudes]
            samples = []
            for point in points:
                projected = transformed(forward, point)
                assert agree(projected, transformed(operation, point), 1e-6), (target, point)
                back = transformed(inverse, projected) if projected else None
                samples.append([*point, projected, back])
            factor = target_crs.axis_info[0].unit_conversion_factor
            radius = target_crs.ellipsoid.semi_major_metre
            # Points of the rotated projection's own domain and beyond it.
            inverse_points = [
                [(x0 + x * radius) / factor, (y0 + y * radius) / factor]
                for x, y in [(0, 0), (0.5, -0.25), (-1.5, 0.75), (3.5, 0), (0, 3.5), (2.5, 2.5)]
            ]
            inverse_samples = [[point, transformed(inverse, point)] for point in inverse_points]
            cases.append(
                dict(
                    name=f"ob_tran {o_proj} {figure} {units}",
                    source=source,
                    target=target,
                    metres_per_unit=factor,
                    radius_metres=radius,
                    false_northing_metres=y0,
                    # PROJ's spherical sinu inverse returns latitudes past a pole; terrio refuses them.
                    refuses_past_pole=o_proj == "sinu" and figure.startswith("+R="),
                    wkt1=target_crs.to_wkt("WKT1_GDAL"),
                    wkt2=target_crs.to_wkt("WKT2_2019"),
                    samples=samples,
                    inverse_samples=inverse_samples,
                )
            )
    for definition in REFUSED:
        try:
            CRS(definition)
        except pyproj.exceptions.CRSError:
            continue
        raise AssertionError(f"PROJ accepts {definition}")
    output = dict(
        environment=dict(
            pyproj=pyproj.__version__,
            proj=pyproj.proj_version_str,
            network=False,
            forward_tolerance_metres=0.001,
            inverse_tolerance_degrees=1e-8,
        ),
        cases=cases,
        refused=REFUSED,
    )
    path = ROOT / "crates/terrio-crs/tests/data/ob_tran_reference.json"
    path.write_text(json.dumps(output, separators=(",", ":")) + "\n")
    samples = sum(len(c["samples"]) for c in cases)
    refused = sum(s[2] is None for c in cases for s in c["samples"])
    print(f"{len(cases)} configurations, {samples} forward samples ({refused} refused), {path.stat().st_size} bytes")


if __name__ == "__main__":
    main()
