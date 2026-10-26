#!/usr/bin/env python3
"""Freeze perspective projection/domain cases against GDAL 3.12.2 / PROJ 9.8.1."""

import json
import math
from pathlib import Path
from osgeo import gdal, osr

ROOT = Path(__file__).resolve().parents[1]


def transformed(operation, point):
    try:
        result = operation.TransformPoint(*point)[:2]
        return list(result) if all(map(math.isfinite, result)) else None
    except RuntimeError:
        return None


def destination(lon, lat, bearing, distance):
    lon, lat, bearing = map(math.radians, (lon, lat, bearing))
    phi = math.asin(
        math.sin(lat) * math.cos(distance)
        + math.cos(lat) * math.sin(distance) * math.cos(bearing)
    )
    lam = lon + math.atan2(
        math.sin(bearing) * math.sin(distance) * math.cos(lat),
        math.cos(distance) - math.sin(lat) * math.sin(phi),
    )
    return [(math.degrees(lam) + 180) % 360 - 180, math.degrees(phi)]


def main():
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    version = ".".join(
        map(
            str,
            (
                osr.GetPROJVersionMajor(),
                osr.GetPROJVersionMinor(),
                osr.GetPROJVersionMicro(),
            ),
        )
    )
    assert version == "9.8.1"
    assert gdal.VersionInfo("RELEASE_NAME") == "3.12.2"
    gdal.PushErrorHandler("CPLQuietErrorHandler")
    cases = []
    for figure, radius in [("+R=6371000", 6371000.0), ("+datum=WGS84", 6378137.0)]:
        configurations = [
            (method, lat, 23, 3000000, "m", 0, 0, 20, 30)
            for method in ["nsper", "tpers"]
            for lat in [-90, 0, 35, 90]
        ]
        configurations += [
            ("tpers", -42, 147, 3000000, "km", 12345, -6789, 20, 30),
            ("tpers", 55, -125, 35785831, "us-ft", -8765, 3456, -15, -50),
        ]
        for method, lat, lon, height, units, x0, y0, tilt, azi in configurations:
            source = "+proj=longlat " + figure
            target = f"+proj={method} +lat_0={lat} +lon_0={lon} +h={height} {figure} +units={units} +x_0={x0} +y_0={y0}"
            if method == "tpers":
                target += f" +tilt={tilt} +azi={azi}"
            definitions = []
            for text in [source, target]:
                srs = osr.SpatialReference()
                srs.SetFromUserInput(text)
                srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                definitions.append(srs)
            forward = osr.CreateCoordinateTransformation(*definitions)
            inverse = osr.CreateCoordinateTransformation(*reversed(definitions))
            horizon = math.acos(radius / (radius + height))
            points = [
                [x, y]
                for x in range(-180, 180, 30)
                for y in [-90, -75, -45, -15, 0, 15, 45, 75, 90]
            ]
            points += [[lon, lat]]
            points += [
                destination(lon, lat, bearing, horizon + math.radians(delta))
                for bearing in range(0, 360, 45)
                for delta in [-0.001, 0.001]
            ]
            samples = []
            for point in points:
                projected = transformed(forward, point)
                back = transformed(inverse, projected) if projected else None
                samples.append([*point, projected, back])
            factor = definitions[1].GetLinearUnits()
            max_radius = radius * math.sqrt((height / radius) / (height / radius + 2))
            inverse_points = [[x0 / factor, y0 / factor], [1e8, 1e8], [-1e8, 1e8]]
            if method == "nsper":
                inverse_points += [
                    [(x0 + max_radius * scale) / factor, y0 / factor]
                    for scale in [1 - 1e-8, 1 + 1e-8]
                ]
            inverse_samples = [
                [point, transformed(inverse, point)] for point in inverse_points
            ]
            cases.append(
                dict(
                    name=f"{method} {figure} origin {lat}/{lon} {units}",
                    source=source,
                    target=target,
                    metres_per_unit=factor,
                    wkt2=definitions[1].ExportToWkt(["FORMAT=WKT2_2019"]),
                    samples=samples,
                    inverse_samples=inverse_samples,
                )
            )
    gdal.PopErrorHandler()
    output = dict(
        environment=dict(
            gdal="3.12.2",
            proj=version,
            network=False,
            forward_tolerance_metres=0.001,
            inverse_tolerance_degrees=1e-8,
        ),
        cases=cases,
    )
    path = ROOT / "crates/terrio-crs/tests/data/perspective_reference.json"
    path.write_text(json.dumps(output, separators=(",", ":")) + "\n")
    print(
        f"{len(cases)} configurations, {sum(len(c['samples']) for c in cases)} forward samples, {path.stat().st_size} bytes"
    )


if __name__ == "__main__":
    main()
