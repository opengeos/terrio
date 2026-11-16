#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Read GML documents and explicit MapInfo ZIP output with GDAL/OGR.

Uses 16 real Natural Earth populated places with explicit scalar attributes.
Locally validated with GDAL 3.13.3. Coordinates allow 1e-6 degrees for MapInfo's grid;
attributes, feature count, archive members and the geographic CRS must agree.
"""

import argparse
import io
import json
import tempfile
import zipfile
from pathlib import Path

from osgeo import gdal, ogr, osr

import terrio


def require(condition, message):
    """Keep conformance failures active under optimized Python execution.

    Args:
        condition: Whether the comparison succeeded.
        message: Failure diagnostic.

    Raises:
        AssertionError: The condition is false.
    """
    if not condition:
        raise AssertionError(message)


def check(data, directory, suffix):
    """Compare one byte output with OGR and Terrio using real public point coordinates.

    Args:
        data: Test-data directory containing Natural Earth populated places.
        directory: Temporary output directory.
        suffix: Document extension or explicit ZIP suffix.

    Returns:
        dict: Conformance status and measured coordinate error.
    """
    source = ogr.Open(str(data / "ne_places/ne_10m_populated_places_simple.shp"))
    if source is None:
        raise RuntimeError("Natural Earth populated places are unavailable")
    features = []
    for i, feature in enumerate(source.GetLayer(0)):
        if i == 16:
            break
        features.append(
            {
                "type": "Feature",
                "properties": {"name": f"place_{i}", "n": i},
                "geometry": json.loads(feature.GetGeometryRef().ExportToJson()),
            }
        )
    if len(features) != 16:
        raise RuntimeError("expected 16 Natural Earth point features")
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(4326)
    crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    layer = terrio.Layer.from_geojson(
        {"type": "FeatureCollection", "features": features},
        name="places",
        crs="EPSG:4326",
    )
    filename = f"places.{suffix}"
    encoded = layer.to_bytes(filename)
    path = directory / filename
    path.write_bytes(encoded)
    if suffix.endswith(".zip"):
        extension = suffix.removesuffix(".zip")
        members = {
            f"places.{part}"
            for part in (
                ["mif", "mid"] if extension == "mif" else ["tab", "dat", "map", "id"]
            )
        }
        with zipfile.ZipFile(io.BytesIO(encoded)) as archive:
            require(
                set(archive.namelist()) == members,
                f"unexpected archive members: {archive.namelist()}",
            )
        reference_path = f"/vsizip/{path}/places.{extension}"
    else:
        require(encoded.startswith(b"<?xml"), "GML output must be an XML document")
        require(
            not zipfile.is_zipfile(io.BytesIO(encoded)),
            "GML document must not be a ZIP",
        )
        reference_path = str(path)
    reference = ogr.Open(reference_path)
    require(reference is not None, f"GDAL could not open {reference_path}")
    output = reference.GetLayer(0)
    require(output.GetFeatureCount() == len(features), "GDAL feature count differs")
    actual_crs = output.GetSpatialRef().Clone()
    actual_crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    require(actual_crs.IsSame(crs), "GDAL CRS differs from EPSG:4326")
    restored_layer = terrio.open_bytes(encoded, filename)[0]
    require(restored_layer.crs == "EPSG:4326", "Terrio CRS differs from EPSG:4326")
    native = restored_layer.to_geojson()["features"]
    largest = 0.0
    for expected, actual, restored in zip(features, output, native, strict=True):
        for field, value in expected["properties"].items():
            require(actual.GetField(field) == value, f"GDAL attribute {field} differs")
            require(
                restored["properties"][field] == value,
                f"Terrio attribute {field} differs",
            )
        before = expected["geometry"]["coordinates"]
        after = actual.GetGeometryRef().GetPoint_2D()
        own = restored["geometry"]["coordinates"]
        error = max(
            abs(before[axis] - values[axis])
            for axis in (0, 1)
            for values in (after, own)
        )
        largest = max(largest, error)
    require(largest <= 1e-6, f"coordinate error {largest} exceeds 1e-6 degrees")
    return {
        "name": filename,
        "status": "within tolerance" if largest else "identical",
        "detail": f"16 Natural Earth points; attributes/CRS/members exact; maximum coordinate error {largest:.3g} degrees (limit 1e-6)",
    }


def check_mif_headers(directory):
    """Compare explicit MIF identification and valid empty schemas with GDAL.

    Args:
        directory: Temporary directory for standalone MIF files.

    Returns:
        dict: Exact rejection and empty-layer comparison status.
    """
    if ogr.GetDriverByName("MapInfo File") is None:
        return {"name": "MIF header recognition", "status": "not compared",
                "detail": "GDAL MapInfo driver is unavailable"}
    cases = [
        (b"", None),
        (b"hello\n", None),
        (b'{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},'
         b'"geometry":{"type":"Point","coordinates":[10,46]}}]}', None),
        (b"Version 300\nColumns 0\nData\n", {}),
        (b"Version 300\nColumns 1\nN Integer\nData\n", {"N": "Integer"}),
    ]
    path = directory / "header.mif"
    for payload, fields in cases:
        path.write_bytes(payload)
        # Explicit driver selection prevents GDAL from opening GeoJSON by content.
        gdal.PushErrorHandler("CPLQuietErrorHandler")
        try:
            try:
                reference = gdal.OpenEx(str(path), gdal.OF_VECTOR, allowed_drivers=["MapInfo File"])
            except RuntimeError:
                reference = None
        finally:
            gdal.PopErrorHandler()
        if fields is None:
            require(reference is None, "GDAL MapInfo driver accepted non-MIF content")
        else:
            require(reference is not None, "GDAL refused a valid empty MIF")
            layer = reference.GetLayer(0)
            require(layer.GetFeatureCount() == 0, "GDAL empty MIF has features")
            schema = layer.GetLayerDefn()
            actual = {schema.GetFieldDefn(i).GetName(): schema.GetFieldDefn(i).GetTypeName()
                      for i in range(schema.GetFieldCount())}
            require(actual == fields, "GDAL empty MIF schema differs")
        for read in (lambda: terrio.open_bytes(payload, "header.mif"),
                     lambda: terrio.open_vector(str(path))):
            if fields is None:
                try:
                    read()
                except terrio.InvalidDataError as error:
                    require("MIF:" in str(error), "error does not identify MIF")
                else:
                    require(False, "Terrio accepted non-MIF content")
            else:
                result = read()
                require(len(result) == 1 and len(result[0]) == 0, "empty MIF layer differs")
                require(result[0].fields == fields, "Terrio empty MIF schema differs")
        reference = None
    return {"name": "MIF header recognition", "status": "identical",
            "detail": "3 wrong-format payloads refused; 2 valid empty schemas exact; explicit GDAL MapInfo driver"}


def main():
    """Run document and archive comparisons.

    Returns:
        int: Nonzero if a comparison fails.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    ogr.UseExceptions()
    source = ogr.Open(str(args.data / "ne_places/ne_10m_populated_places_simple.shp"))
    if source is None or source.GetLayer(0).GetFeatureCount() < 16:
        raise RuntimeError(
            "Natural Earth source setup requires at least 16 point features"
        )
    source = None
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-vector-bytes-") as temporary:
        for suffix in ("gml", "mif.zip", "tab.zip"):
            driver = "GML" if suffix == "gml" else "MapInfo File"
            if ogr.GetDriverByName(driver) is None:
                results.append(
                    {
                        "name": f"places.{suffix}",
                        "status": "not compared",
                        "detail": f"GDAL driver {driver} is unavailable",
                    }
                )
                continue
            try:
                results.append(check(args.data, Path(temporary), suffix))
            except Exception as error:
                results.append(
                    {
                        "name": f"places.{suffix}",
                        "status": "differs",
                        "detail": repr(error),
                    }
                )
        try:
            results.append(check_mif_headers(Path(temporary)))
        except Exception as error:
            results.append({"name": "MIF header recognition", "status": "differs", "detail": repr(error)})
    print(
        json.dumps(results, indent=2)
        if args.json
        else "\n".join(
            f"{result['name']}: {result['status']} {result['detail']}"
            for result in results
        )
    )
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
