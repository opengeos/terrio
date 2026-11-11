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

from osgeo import ogr, osr

import terrio


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
            assert set(archive.namelist()) == members
        reference_path = f"/vsizip/{path}/places.{extension}"
    else:
        assert encoded.startswith(b"<?xml")
        assert not zipfile.is_zipfile(io.BytesIO(encoded))
        reference_path = str(path)
    reference = ogr.Open(reference_path)
    assert reference is not None
    output = reference.GetLayer(0)
    assert output.GetFeatureCount() == len(features)
    actual_crs = output.GetSpatialRef().Clone()
    actual_crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    assert actual_crs.IsSame(crs)
    restored_layer = terrio.open_bytes(encoded, filename)[0]
    assert restored_layer.crs == "EPSG:4326"
    native = restored_layer.to_geojson()["features"]
    largest = 0.0
    for expected, actual, restored in zip(features, output, native, strict=True):
        for field, value in expected["properties"].items():
            assert actual.GetField(field) == value
            assert restored["properties"][field] == value
        before = expected["geometry"]["coordinates"]
        after = actual.GetGeometryRef().GetPoint_2D()
        own = restored["geometry"]["coordinates"]
        error = max(
            abs(before[axis] - values[axis])
            for axis in (0, 1)
            for values in (after, own)
        )
        largest = max(largest, error)
    assert largest <= 1e-6, f"coordinate error {largest} exceeds 1e-6 degrees"
    return {
        "name": filename,
        "status": "within tolerance" if largest else "identical",
        "detail": f"16 Natural Earth points; attributes/CRS/members exact; maximum coordinate error {largest:.3g} degrees (limit 1e-6)",
    }


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
