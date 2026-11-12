#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Compound OGC URNs against GDAL 3.13.3: component IDs, axes and GML XYZ."""

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal, ogr, osr

import terrio
from terrio.rio.crs import CRS


def require(condition, detail):
    """Require a conformance condition to hold.

    Args:
        condition: Whether the comparison succeeded.
        detail: Diagnostic to report on failure.

    Raises:
        AssertionError: If the condition is false.
    """
    if not condition:
        raise AssertionError(detail)


def components(value):
    """Extract the authority, code and axis count of each CRS component.

    Args:
        value: Compound CRS represented as PROJJSON.

    Returns:
        Component descriptors in their declared order.
    """
    require(value["type"] == "CompoundCRS", "expected a compound CRS")
    return [
        (
            part["id"]["authority"],
            part["id"]["code"],
            len(part["coordinate_system"]["axis"]),
        )
        for part in value["components"]
    ]


def compare(executable):
    """Compare CLI and Python compound CRS behavior with GDAL.

    Args:
        executable: Path to the Terrio CLI to validate.

    Returns:
        Conformance results for component metadata and GML coordinates.
    """
    results = []
    osr.UseExceptions()
    ogr.UseExceptions()
    # GDAL otherwise intentionally reports only the horizontal component of GML.
    with (
        gdal.config_options({"GML_REPORT_COMPD_CS": "YES"}),
        tempfile.TemporaryDirectory() as directory,
    ):
        for horizontal, vertical, position in [
            (2154, 5720, (652000, 6862000)),
            (4269, 5713, (52, 4)),
            (4326, 5773, (52, 4)),
        ]:
            name = f"urn:ogc:def:crs,crs:EPSG::{horizontal},crs:EPSG::{vertical}"
            reference = osr.SpatialReference()
            reference.SetFromUserInput(name)
            expected = components(json.loads(reference.ExportToPROJJSON()))
            require(
                reference.IsCompound() and reference.GetAxesCount() == 3,
                "GDAL compound axis count",
            )
            actual = components(CRS.from_user_input(name).to_dict(projjson=True))
            cli = subprocess.run(
                [executable, "crs", name, "--projjson"],
                check=True,
                capture_output=True,
                text=True,
            )
            require(
                actual == components(json.loads(cli.stdout)) == expected,
                f"{name}: component IDs or axes differ",
            )
            results.append(
                dict(
                    name=f"compound {horizontal}+{vertical} CRS",
                    status="identical",
                    detail="Python and CLI component authorities/codes and 2+1 axes match GDAL exactly",
                )
            )
            text = f'''<g:FeatureCollection xmlns:g="http://www.opengis.net/gml/3.2" xmlns:ex="urn:example">
            <g:featureMember><ex:p g:id="p.1"><ex:geom><g:Point srsName="{name}" srsDimension="3" g:id="g.1">
            <g:pos>{position[0]} {position[1]} 35</g:pos></g:Point></ex:geom></ex:p></g:featureMember></g:FeatureCollection>'''
            path = Path(directory) / f"{horizontal}.gml"
            path.write_text(text)
            source = ogr.Open(str(path))
            layer = source.GetLayer(0)
            # Keep the owning feature alive while reading its borrowed geometry.
            feature = layer.GetNextFeature()
            require(feature is not None, f"{name}: GDAL returned an empty GML layer")
            expected_coordinates = list(feature.GetGeometryRef().GetPoint())
            expected_crs = components(
                json.loads(layer.GetSpatialRef().ExportToPROJJSON())
            )
            for dataset in [
                terrio.open_vector(str(path)),
                terrio.open_bytes(text.encode(), filename="point.gml"),
            ]:
                native = dataset[0]
                require(
                    components(CRS.from_user_input(native.crs).to_dict(projjson=True))
                    == expected_crs,
                    f"{name}: GML component metadata differs",
                )
                require(
                    native.to_geojson()["features"][0]["geometry"]["coordinates"]
                    == expected_coordinates,
                    f"{name}: GML XYZ differs",
                )
            require(expected_coordinates[2] == 35, "height changed")
            results.append(
                dict(
                    name=f"compound {horizontal}+{vertical} GML",
                    status="identical",
                    detail="GDAL GML_REPORT_COMPD_CS=YES; path/bytes: both component IDs, axis counts and normalized XYZ match GDAL exactly; vertical ordinate remains 35",
                )
            )
    return results


def main():
    """Run the comparisons and print their text or JSON results."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "data", nargs="?", help="unused; accepted by the conformance harness"
    )
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = compare(args.terrio)
    print(
        json.dumps(results)
        if args.json
        else "\n".join(f"{r['status']}: {r['name']}" for r in results)
    )


if __name__ == "__main__":
    main()
