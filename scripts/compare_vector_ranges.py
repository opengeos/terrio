#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Compare STAC vector asset ranges with GDAL on real Natural Earth points.

Validated with GDAL 3.13.3. Layer names, feature order, scalar values and CRS must
agree; point coordinates allow 1e-9 degrees. All checks remain active under python -O.
"""

import argparse
import json
import math
import tempfile
from pathlib import Path

from osgeo import ogr

import terrio
import terrio.pyogrio as tpg


def compare(path, item):
    """Compare selected STAC asset layers and ranges with the underlying OGR dataset.

    Args:
        path: Real-point GeoJSON or GeoPackage asset.
        item: STAC item containing the asset.

    Returns:
        tuple: Comparison detail and maximum coordinate error in degrees.

    Raises:
        AssertionError: A layer, value, coordinate or CRS differs.
    """
    reference = ogr.Open(str(path))
    cases = 0
    largest = 0.0
    name = f"STAC:{item}:data"
    for layer in reference:
        layer_name = layer.GetName()
        definition = layer.GetLayerDefn()
        fields = [definition.GetFieldDefn(index).GetName() for index in range(definition.GetFieldCount())]
        reference_epsg = int(layer.GetSpatialRef().GetAuthorityCode(None))
        full = [
            (
                tuple(feature.GetField(field) for field in fields),
                feature.GetGeometryRef().GetX(),
                feature.GetGeometryRef().GetY(),
            )
            for feature in layer
        ]
        selected = terrio.open_vector(name, layers=[layer_name])
        if [part.name for part in selected] != [layer_name]:
            raise AssertionError(f"layer selection differs: {layer_name}")
        for skip, limit in [(0, 1), (1, 2), (8, None), (99, 1)]:
            expected = full[skip:] if limit is None else full[skip : skip + limit]
            frame = tpg.read_dataframe(
                name, layer=layer_name, skip_features=skip, max_features=limit
            )
            actual_values = list(frame[fields].itertuples(index=False, name=None))
            if actual_values != [row[0] for row in expected]:
                raise AssertionError(
                    f"feature range differs: {layer_name}, {skip}, {limit}"
                )
            if frame.crs.to_epsg() != reference_epsg:
                raise AssertionError("CRS differs")
            for geometry, (_, x, y) in zip(frame.geometry, expected, strict=True):
                differences = (abs(geometry.x - x), abs(geometry.y - y))
                if not all(math.isfinite(value) for value in differences):
                    raise AssertionError("non-finite coordinate difference")
                error = max(differences)
                largest = max(largest, error)
                if error > 1e-9:
                    raise AssertionError(
                        f"coordinate error {error} exceeds 1e-9 degrees"
                    )
            cases += 1
    return (
        f"{cases} GDAL layer/range comparisons; values/order/CRS exact; maximum coordinate error {largest:g} degrees",
        largest,
    )


def main():
    """Build assets from 16 public populated places and report each format's comparison.

    Returns:
        int: Zero on agreement, one if a comparison fails.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    ogr.UseExceptions()
    source = ogr.Open(str(args.data / "ne_places/ne_10m_populated_places_simple.shp"))
    features = []
    for index, feature in enumerate(source.GetLayer(0)):
        if index == 16:
            break
        features.append(
            {
                "type": "Feature",
                "properties": {"sequence": index, "label": f"place {index}"},
                "geometry": json.loads(feature.GetGeometryRef().ExportToJson()),
            }
        )
    if len(features) != 16:
        raise RuntimeError("expected 16 Natural Earth points")
    document = {"type": "FeatureCollection", "features": features}
    layers = [
        terrio.Layer.from_geojson(document, name=name, crs="EPSG:4326")
        for name in ["places", "copy"]
    ]
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-vector-ranges-") as directory:
        for extension in ["geojson", "gpkg"]:
            asset = Path(directory) / f"places.{extension}"
            terrio.write_vector(
                layers[:1] if extension == "geojson" else layers, str(asset)
            )
            item = Path(directory) / f"item-{extension}.json"
            item.write_text(
                json.dumps(
                    {
                        "type": "Feature",
                        "stac_version": "1.0.0",
                        "id": extension,
                        "geometry": features[0]["geometry"],
                        "properties": {"datetime": "2024-01-01T00:00:00Z"},
                        "links": [],
                        "assets": {"data": {"href": asset.name, "roles": ["data"]}},
                    }
                )
            )
            try:
                detail, maximum_error = compare(asset, item)
                status = "identical" if maximum_error == 0.0 else "within tolerance"
            except AssertionError as error:
                status, detail = "differs", str(error)
            results.append({"name": extension, "status": status, "detail": detail})
    print(
        json.dumps(results)
        if args.json
        else "\n".join(str(result) for result in results)
    )
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
