# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Compare real GeoPackage pages through Python paths/bytes and CLI with GDAL."""

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import pandas as pd
from osgeo import gdal, ogr, osr

import terrio
import terrio.pyogrio as tpg


LABELS = ("full", "head", "middle", "tail", "empty", "bbox")
NAMES = [f"{source} {label}" for source in ("path", "bytes") for label in LABELS] + ["CLI limit"]


def require(condition, message):
    """Keep conformance checks active under Python optimization."""
    if not condition:
        raise AssertionError(message)


def reference_rows(layer, skip, limit, bbox):
    """Read GDAL's ordered matches, then apply the feature range."""
    layer.SetSpatialFilter(None)
    if bbox is not None:
        layer.SetSpatialFilterRect(*bbox)
    layer.ResetReading()
    rows = []
    for feature in layer:
        geometry = feature.GetGeometryRef()
        rows.append((feature.GetFID(), [feature.GetField(i) for i in range(feature.GetFieldCount())],
                     None if geometry is None else bytes(geometry.ExportToIsoWkb(ogr.wkbNDR))))
    return rows[skip:] if limit is None else rows[skip:skip + limit]


def check_crs(actual_wkt, expected):
    """Compare coordinate-system identity without changing coordinate order."""
    require((actual_wkt is None) == (expected is None), "CRS presence differs")
    if expected is not None:
        actual = osr.SpatialReference()
        actual.ImportFromWkt(actual_wkt)
        reference = expected.Clone()
        for crs in (actual, reference):
            crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
        require(bool(actual.IsSame(reference)), "CRS identity differs")


def main():
    """Report every declared comparison, failing on differences or infrastructure errors."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    ogr.UseExceptions()
    osr.UseExceptions()
    path = args.data / "countries.gpkg"
    if not path.is_file():
        results = [dict(name=name, status="not compared", detail="countries.gpkg is unavailable") for name in NAMES]
    else:
        dataset = ogr.Open(str(path))
        layer = dataset.GetLayerByIndex(0)
        count = layer.GetFeatureCount()
        require(count > 10, "real GeoPackage must contain enough features to page")
        definition = layer.GetLayerDefn()
        fields = [definition.GetFieldDefn(i).GetName() for i in range(definition.GetFieldCount())]
        crs = layer.GetSpatialRef()
        schema = terrio._terrio.pyogrio_read(str(path), layer=layer.GetName())["fields"]
        specs = [(0, None, None), (1, 3, None), (count // 2, 5, None),
                 (max(0, count - 2), 5, None), (count + 7, 3, None),
                 (1, 3, (-10, 35, 30, 65))]
        results = []
        for source_name, source in (("path", path), ("bytes", path.read_bytes())):
            for label, (skip, limit, bbox) in zip(LABELS, specs, strict=True):
                name = f"{source_name} {label}"
                try:
                    expected = reference_rows(layer, skip, limit, bbox)
                    frame = tpg.read_dataframe(source, layer=layer.GetName(), skip_features=skip,
                                               max_features=limit, bbox=bbox, fid_as_index=True)
                    require(frame.index.tolist() == [row[0] for row in expected], "FIDs/order differ")
                    require([c for c in frame.columns if c != frame.geometry.name] == fields, "fields differ")
                    for (_, row), (_, values, wkb) in zip(frame.iterrows(), expected, strict=True):
                        actual = [None if pd.isna(row[field]) else row[field] for field in fields]
                        require(actual == values, "attribute values differ")
                        geometry = row[frame.geometry.name]
                        require((None if geometry is None else geometry.wkb) == wkb, "geometry WKB differs")
                    check_crs(None if frame.crs is None else frame.crs.to_wkt(), crs)
                    if source_name == "path" and bbox is None:
                        raw = terrio._terrio.pyogrio_read(str(path), skip, limit, layer=layer.GetName())
                        require(raw["fields"] == schema, "declared field metadata differs")
                    results.append(dict(name=name, status="identical", detail="Exact FIDs/order, attributes, geometry WKB, fields and CRS against GDAL"))
                except AssertionError as error:
                    results.append(dict(name=name, status="differs", detail=str(error)))
                except Exception as error:
                    results.append(dict(name=name, status="error", detail=f"{type(error).__name__}: {error}"))
        try:
            expected = reference_rows(layer, 0, 3, None)
            with tempfile.TemporaryDirectory(prefix="terrio-gpkg-page-") as directory:
                output = Path(directory) / "limited.geojson"
                subprocess.run([args.terrio, "convert", str(path), str(output), "--limit", "3"],
                               check=True, capture_output=True, text=True, timeout=60)
                converted = ogr.Open(str(output))
                actual = reference_rows(converted.GetLayerByIndex(0), 0, None, None)
                require([row[1:] for row in actual] == [row[1:] for row in expected], "CLI limited values/geometries differ")
                check_crs(converted.GetLayerByIndex(0).GetSpatialRef().ExportToWkt(), crs)
            results.append(dict(name="CLI limit", status="identical", detail="Exact first-three attributes, WKB and CRS; output-writer FID policy is not compared"))
        except AssertionError as error:
            results.append(dict(name="CLI limit", status="differs", detail=str(error)))
        except Exception as error:
            results.append(dict(name="CLI limit", status="error", detail=f"{type(error).__name__}: {error}"))
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']}: {r['detail']}" for r in results))
    return int(any(row["status"] in {"differs", "error"} for row in results))


if __name__ == "__main__":
    raise SystemExit(main())
