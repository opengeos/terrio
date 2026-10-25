#!/usr/bin/env python3
"""Shapefile ZIP member and geometry/attribute/CRS parity on real public geometries.

Uses 16 Natural Earth places, Las Vegas roads and Tennessee counties per case,
with explicit scalar test attributes. Both GDAL and Terrio read the generated ZIP.
Geometry comparisons normalize ring orientation/start and part order, then require
identical coordinates (no numerical tolerance). DBF reals use exactly representable
halves; general real fields retain the documented width-24/15-decimal policy.
"""

import argparse
import json
import io
import zipfile
import tempfile
from pathlib import Path

from osgeo import ogr, osr
from shapely import MultiPolygon, from_geojson, from_wkb

import terrio


FIXTURES = [
    ("points", "ne_places/ne_10m_populated_places_simple.shp"),
    ("lines", "las_vegas_roads.geojson"),
    ("polygons", "TN_Counties.geojson"),
]


def check(data, directory, kind, filename):
    source = ogr.Open(str(data / filename))
    if source is None:
        raise RuntimeError(f"cannot open {filename}")
    layer = source.GetLayer(0)
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(4326)
    crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    if not layer.GetSpatialRef().IsSame(crs):
        raise RuntimeError(f"{filename} must be EPSG:4326")
    features = []
    for i, feature in enumerate(layer):
        if i == 16:
            break
        features.append({
            "type": "Feature",
            "geometry": json.loads(feature.GetGeometryRef().ExportToJson()),
            "properties": {"id": feature.GetFID(), "label": "Zürich", "value": i + 0.5, "empty": None},
        })
    assert len(features) == 16
    document = {"type": "FeatureCollection", "features": features}
    native = terrio.Layer.from_geojson(document, name=kind)
    archive = directory / f"{kind}.shp.zip"
    archive.write_bytes(native.to_bytes(archive.name))
    reference = ogr.Open(f"/vsizip/{archive}")
    assert reference is not None
    output = reference.GetLayer(0)
    assert output.GetFeatureCount() == len(features)
    assert output.GetSpatialRef().IsSame(crs)
    assert [output.GetLayerDefn().GetFieldDefn(i).GetName() for i in range(4)] == ["id", "label", "value", "empty"]
    types = [output.GetLayerDefn().GetFieldDefn(i).GetType() for i in range(4)]
    assert types[0] in (ogr.OFTInteger, ogr.OFTInteger64)
    assert types[1:] == [ogr.OFTString, ogr.OFTReal, ogr.OFTString]
    for expected, actual in zip(features, output, strict=True):
        assert actual.items() == expected["properties"]
        geometry = from_wkb(bytes(actual.GetGeometryRef().ExportToIsoWkb()))
        expected_geometry = from_geojson(json.dumps(expected["geometry"]))
        # Shapefile stores Polygon and single-part MultiPolygon identically. Promote
        # only this structural wrapper, retaining every ring, vertex and coordinate.
        if geometry.geom_type == "Polygon":
            geometry = MultiPolygon([geometry])
        if expected_geometry.geom_type == "Polygon":
            expected_geometry = MultiPolygon([expected_geometry])
        assert geometry.normalize().wkb == expected_geometry.normalize().wkb, "normalized geometry coordinates differ"
    back = terrio.open_bytes(archive.read_bytes(), archive.name)[0]
    assert len(back) == len(features)
    assert back.crs == "EPSG:4326"
    assert back.columns() == {key: [feature["properties"][key] for feature in features] for key in features[0]["properties"]}
    return {"name": f"Shapefile ZIP {kind}", "status": "identical", "detail": "16 real geometries; normalized coordinates, scalar attributes/nulls and EPSG:4326"}


def check_z(directory):
    cases = [
        ("point", "POINT Z (1 2 3)"),
        ("multipoint", "MULTIPOINT Z ((1 2 3),(4 5 6))"),
        ("line", "LINESTRING Z (0 0 1,2 3 2,5 1 3)"),
        ("polygon", "POLYGON Z ((0 0 1,0 5 2,5 5 3,5 0 4,0 0 1))"),
    ]
    for kind, wkt in cases:
        geometry = ogr.CreateGeometryFromWkt(wkt)
        source = {"type": "Feature", "geometry": json.loads(geometry.ExportToJson()), "properties": {"name": "*"}}
        archive = directory / f"xyz_{kind}.shp.zip"
        archive.write_bytes(terrio.Layer.from_geojson(source).to_bytes(archive.name))
        dataset = ogr.Open(f"/vsizip/{archive}")
        feature = dataset.GetLayer(0).GetNextFeature()
        actual = feature.GetGeometryRef()
        assert not actual.IsMeasured()
        assert from_wkb(bytes(actual.ExportToIsoWkb())).normalize().wkb == from_wkb(bytes(geometry.ExportToIsoWkb())).normalize().wkb
        assert feature.GetField("name") == "*"
        # Compare the raw optional-M layout with a fresh independent GDAL write.
        reference_path = directory / f"reference_{kind}.shp"
        reference = ogr.GetDriverByName("ESRI Shapefile").CreateDataSource(str(reference_path))
        layer = reference.CreateLayer(kind, geom_type=geometry.GetGeometryType())
        item = ogr.Feature(layer.GetLayerDefn())
        item.SetGeometry(geometry)
        layer.CreateFeature(item)
        item = None
        layer = None
        reference = None
        with zipfile.ZipFile(io.BytesIO(archive.read_bytes())) as members:
            raw = members.read(f"xyz_{kind}.shp")
        expected = reference_path.read_bytes()
        assert raw[84:100] == expected[84:100] == bytes(16), "file M bounds"
        assert raw[104:108] == expected[104:108], "optional-M record layout differs from GDAL"
    return {"name": "Shapefile ZIP Z and asterisk text", "status": "identical", "detail": "four XYZ layouts match independent GDAL writes; no invented M; asterisk text retained"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    ogr.UseExceptions()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-shapefile-") as tmp:
        for kind, filename in FIXTURES:
            try:
                results.append(check(args.data, Path(tmp), kind, filename))
            except Exception as error:
                results.append({"name": f"Shapefile ZIP {kind}", "status": "differs", "detail": repr(error)})
        try:
            results.append(check_z(Path(tmp)))
        except Exception as error:
            results.append({"name": "Shapefile ZIP Z and asterisk text", "status": "differs", "detail": repr(error)})
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']} {r['detail']}" for r in results))
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
