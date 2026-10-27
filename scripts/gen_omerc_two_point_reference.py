#!/usr/bin/env python3
"""Freeze two-point oblique Mercator cases against GDAL 3.12.2 / PROJ 9.8.1."""

import json
import math
from pathlib import Path
from osgeo import gdal, osr

ROOT = Path(__file__).resolve().parents[1]

# name, (lat_1, lon_1), (lat_2, lon_2), lat_0, k, units, x_0, y_0
LINES = [
    ("Tasmania", (-40, 140), (-44, 150), 0, 1, "m", 0, 0),
    ("the points swapped", (-44, 150), (-40, 140), 0, 1, "m", 0, 0),
    ("Italy", (46, 7), (38, 18), 42, 0.9996, "m", 500000, 0),
    ("across the antimeridian", (50, 170), (60, -170), 55, 1, "km", 0, 0),
    ("south, US feet", (-30, -70), (-20, -60), -25, 0.9999, "us-ft", 12345, -6789),
    ("near a meridian", (10, 20), (40, 21), 0, 1, "m", 0, 0),
]

# Definitions PROJ refuses, from omerc.cpp's checks, and how PROJ 9.8.1 takes each.
INVALID = [
    "+proj=omerc +lat_1=90 +lon_1=0 +lat_2=40 +lon_2=10",
    "+proj=omerc +lat_1=40 +lon_1=0 +lat_2=-90 +lon_2=10",
    "+proj=omerc +lat_1=40 +lon_1=0 +lat_2=40 +lon_2=10",
    "+proj=omerc +lat_1=0 +lon_1=0 +lat_2=40 +lon_2=10",
    "+proj=omerc +lat_0=90 +lat_1=40 +lon_1=0 +lat_2=45 +lon_2=10",
    "+proj=omerc",
]


def transformed(operation, point):
    try:
        result = operation.TransformPoint(*point)[:2]
        return list(result) if all(map(math.isfinite, result)) else None
    except RuntimeError:
        return None


def refused(definition):
    srs = osr.SpatialReference()
    try:
        srs.SetFromUserInput(definition + " +datum=WGS84")
        source = osr.SpatialReference()
        source.SetFromUserInput("+proj=longlat +datum=WGS84")
        osr.CoordinateTransformation(source, srs)
        return False
    except RuntimeError:
        return True


def main():
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    version = ".".join(map(str, (osr.GetPROJVersionMajor(), osr.GetPROJVersionMinor(), osr.GetPROJVersionMicro())))
    assert version == "9.8.1"
    assert gdal.VersionInfo("RELEASE_NAME") == "3.12.2"
    gdal.PushErrorHandler("CPLQuietErrorHandler")
    cases = []
    for figure in ["+R=6371000", "+datum=WGS84"]:
        for name, (lat1, lon1), (lat2, lon2), lat0, k, units, x0, y0 in LINES:
            source = "+proj=longlat " + figure
            target = (
                f"+proj=omerc +lat_0={lat0} +lat_1={lat1} +lon_1={lon1} +lat_2={lat2} +lon_2={lon2} +k={k} "
                f"{figure} +units={units} +x_0={x0} +y_0={y0}"
            )
            definitions = []
            for text in [source, target]:
                srs = osr.SpatialReference()
                srs.SetFromUserInput(text)
                srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                definitions.append(srs)
            forward = osr.CreateCoordinateTransformation(*definitions)
            inverse = osr.CreateCoordinateTransformation(*reversed(definitions))
            # A world grid, which reaches the projection's limits 90° away from the line, and the
            # neighbourhood of the line.
            points = [[x, y] for x in range(-180, 180, 30) for y in [-89, -75, -45, -15, 0, 15, 45, 75, 89]]
            mid = (lon1 + ((lon2 - lon1 + 180) % 360 - 180) / 2 + 180) % 360 - 180
            points += [[lon1, lat1], [lon2, lat2], [mid, (lat1 + lat2) / 2]]
            points += [[(mid + dx + 180) % 360 - 180, max(-89, min(89, (lat1 + lat2) / 2 + dy))] for dx in [-20, -5, 5, 20] for dy in [-20, -5, 5, 20]]
            samples = []
            for point in points:
                projected = transformed(forward, point)
                back = transformed(inverse, projected) if projected else None
                samples.append([*point, projected, back])
            factor = definitions[1].GetLinearUnits()
            inverse_points = [[x0 / factor, y0 / factor], [1e9, 1e9], [-1e9, 1e9], [0, 1e9]]
            inverse_samples = [[point, transformed(inverse, point)] for point in inverse_points]
            cases.append(
                dict(
                    name=f"{name}, {'sphere' if figure.startswith('+R') else 'WGS 84'}, {units}",
                    source=source,
                    target=target,
                    metres_per_unit=factor,
                    wkt1=definitions[1].ExportToWkt(["FORMAT=WKT1_GDAL"]),
                    wkt1_esri=definitions[1].ExportToWkt(["FORMAT=WKT1_ESRI"]),
                    wkt2=definitions[1].ExportToWkt(["FORMAT=WKT2_2019"]),
                    samples=samples,
                    inverse_samples=inverse_samples,
                )
            )
    invalid = [dict(definition=d, proj_refuses=refused(d)) for d in INVALID]
    gdal.PopErrorHandler()
    output = dict(
        environment=dict(gdal="3.12.2", proj=version, network=False, forward_tolerance_metres=0.001, inverse_tolerance_degrees=1e-8),
        cases=cases,
        invalid=invalid,
    )
    path = ROOT / "crates/terrio-crs/tests/data/omerc_two_point_reference.json"
    path.write_text(json.dumps(output, separators=(",", ":")) + "\n")
    print(f"{len(cases)} configurations, {sum(len(c['samples']) for c in cases)} forward samples, {path.stat().st_size} bytes")
    print("refused by PROJ:", [i["proj_refuses"] for i in invalid])


if __name__ == "__main__":
    main()
