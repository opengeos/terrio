#!/usr/bin/env python3
"""Cross-read DWG/DXF geometry with GDAL, including GDAL's public CAD fixtures.

Run scripts/fetch_test_data.sh first. CAD files retain drawing coordinates and
have no CRS. Curves use different sample grids: compare Hausdorff distance with
0.001 times the geometry's diagonal (4-degree chord error), and length/area with
relative tolerance 0.002. Straight geometry uses coordinate tolerance 1e-8.
DWG is cross-read through Terrio's DXF export: GDAL builds need no DWG backend.
"""

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal, ogr
from shapely import from_wkb, from_wkt, get_coordinates

FIXTURES = [
    "3dface.dxf",
    "LWPOLYLINE-OCS.dxf",
    "circle.dxf",
    "closed_polyline_with_bulge.dxf",
    "solid-vertex-ordering.dxf",
    "hatch_as_multipolygon.dxf",
    "bin_dxf_r2000.dxf",
    "block-basepoint.dxf",
]


# Independent AutoCAD R2000 drawings from LibreDWG's public fixture corpus.
# Expected coordinates checked with GDAL 3.12.2's libopencad CAD driver.
NATIVE_DWG = {
    "Line.dwg": "LINESTRING Z (21.607766723021 2.811923503435 0,11.0307372928544 16.6375447379915 0)",
    "Point.dwg": "POINT Z (27.9280106553128 8.65307688043195 0)",
}


def features(path):
    ds = ogr.Open(str(path))
    if ds is None:
        raise RuntimeError(f"GDAL cannot open {path}")
    layer = ds.GetLayer(0)
    return [
        (from_wkb(bytes(f.GetGeometryRef().ExportToIsoWkb())), f.GetField("Layer"))
        for f in layer
        if f.GetGeometryRef() is not None
    ]


def compare(reference, actual, curved=False):
    if len(reference) != len(actual):
        return "differs", f"feature counts {len(reference)} != {len(actual)}"
    remaining = list(actual)
    approximate = False
    max_distance = 0.0
    zero_z_promotion = False
    for expected, layer in reference:
        candidates = [
            (i, g)
            for i, (g, name) in enumerate(remaining)
            if name == layer and g.geom_type == expected.geom_type
        ]
        if not candidates:
            return "differs", f"no matching {expected.geom_type} on layer {layer}"
        i, found = min(
            candidates, key=lambda pair: expected.hausdorff_distance(pair[1])
        )
        remaining.pop(i)
        distance = expected.hausdorff_distance(found)
        diagonal = (
            (expected.bounds[2] - expected.bounds[0]) ** 2
            + (expected.bounds[3] - expected.bounds[1]) ** 2
        ) ** 0.5
        tolerance = max(1e-8, 0.001 * diagonal) if curved else 1e-8
        if distance > tolerance:
            return "differs", f"Hausdorff distance {distance:.6g} > {tolerance:.6g}"
        # GEOS distance is only 2D. For straight geometry, also compare every
        # coordinate (including interior Zs) after normalizing ring/part order.
        if not curved:
            ac = np.nan_to_num(
                get_coordinates(expected.normalize(), include_z=True), nan=0.0
            )
            bc = np.nan_to_num(
                get_coordinates(found.normalize(), include_z=True), nan=0.0
            )
            if ac.shape != bc.shape or not np.allclose(ac, bc, atol=1e-8, rtol=0):
                return "differs", "linear coordinate arrays differ"
        # Curves can have different vertex counts; check their Z bounds.
        az = get_coordinates(expected, include_z=True)[:, 2]
        bz = get_coordinates(found, include_z=True)[:, 2]
        az = az[np.isfinite(az)]
        bz = bz[np.isfinite(bz)]
        if (
            len(az)
            and len(bz)
            and max(abs(az.min() - bz.min()), abs(az.max() - bz.max())) > tolerance
        ):
            return "differs", "Z bounds differ"
        for a, b in [(expected.length, found.length), (expected.area, found.area)]:
            if abs(a - b) > max(1e-8, abs(a) * (0.002 if curved else 1e-10)):
                return "differs", "length or area differs"
        zero_z_promotion |= expected.has_z != found.has_z
        approximate |= expected.has_z != found.has_z or not expected.equals_exact(
            found, 0, normalize=True
        )
        max_distance = max(max_distance, distance)
    detail = f"max Hausdorff distance {max_distance:.6g}"
    if zero_z_promotion:
        detail += "; Terrio exposes zero-Z coordinates as XYZ"
    return ("within tolerance", detail) if approximate else ("identical", "")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []
    gdal.UseExceptions()
    ogr.UseExceptions()
    # Match Terrio's per-entity block expansion rather than GDAL's default merge.
    gdal.SetConfigOption("DXF_MERGE_BLOCK_GEOMETRIES", "FALSE")
    with tempfile.TemporaryDirectory(prefix="terrio-cad-") as directory:
        directory = Path(directory)

        def convert(src, dst):
            subprocess.run(
                [args.terrio, "convert", str(src), str(dst)],
                check=True,
                capture_output=True,
                text=True,
            )

        for name in FIXTURES:
            source = args.data / "cad" / name
            try:
                target = directory / f"{source.stem}.geojson"
                convert(source, target)
                curved = name in [
                    "LWPOLYLINE-OCS.dxf",
                    "circle.dxf",
                    "closed_polyline_with_bulge.dxf",
                    "bin_dxf_r2000.dxf",
                ]
                # This GDAL fixture contains UTF-8 Russian names despite its legacy
                # code-page header. acadrust detects UTF-8; tell GDAL explicitly.
                with gdal.config_option(
                    "DXF_ENCODING",
                    "UTF-8" if name == "hatch_as_multipolygon.dxf" else None,
                ):
                    status, detail = compare(
                        features(source), features(target), curved=curved
                    )
                info = json.loads(
                    subprocess.check_output(
                        [args.terrio, "info", str(source), "--json"], text=True
                    )
                )
                if any((info["layers"][0].get("crs") or {}).values()):
                    status, detail = (
                        "differs",
                        "drawing unexpectedly has a geographic CRS",
                    )
            except (RuntimeError, OSError, subprocess.CalledProcessError) as e:
                status, detail = "differs", str(e) + " " + getattr(e, "stderr", "")
            results.append(
                {"name": f"DXF read {name}", "status": status, "detail": detail}
            )

        for name, wkt in NATIVE_DWG.items():
            source = args.data / "cad" / name
            try:
                target = directory / f"native-{source.stem}.geojson"
                convert(source, target)
                status, detail = compare([(from_wkt(wkt), "0")], features(target))
            except (RuntimeError, OSError, subprocess.CalledProcessError) as e:
                status, detail = "differs", str(e) + " " + getattr(e, "stderr", "")
            results.append(
                {
                    "name": f"DWG independent R2000 {name}",
                    "status": status,
                    "detail": detail,
                }
            )

        # Real OSM roads: GIS -> CAD -> GIS, DWG -> DXF, and GDAL's own DXF
        # writer -> Terrio. Non-CAD GIS attributes deliberately have no mapping.
        source_data = ogr.Open(str(args.data / "las_vegas_roads.geojson"))
        source = directory / "roads.geojson"
        from itertools import islice

        source.write_text(
            json.dumps(
                {
                    "type": "FeatureCollection",
                    "features": [
                        json.loads(f.ExportToJson())
                        for f in islice(source_data.GetLayer(0), 64)
                    ],
                }
            )
        )
        for ext in ["dxf", "dwg"]:
            try:
                cad = directory / f"roads.{ext}"
                convert(source, cad)
                cross = directory / f"roads-{ext}.dxf"
                convert(cad, cross)
                actual = directory / f"roads-{ext}.geojson"
                convert(cad, actual)
                reference = directory / "gdal-roads.dxf"
                if not reference.exists():
                    gdal.VectorTranslate(
                        str(reference), str(source), format="DXF", selectFields=[]
                    )
                # Match all features on a common layer: layer naming conventions
                # differ (GDAL defaults to 0; Terrio uses the input layer name).
                expected = [(g, "roads") for g, _ in features(reference)]
                status, detail = compare(
                    expected, [(g, "roads") for g, _ in features(cross)]
                )
                if status != "differs":
                    status, detail = compare(
                        expected, [(g, "roads") for g, _ in features(actual)]
                    )
            except (RuntimeError, OSError, subprocess.CalledProcessError) as e:
                status, detail = "differs", str(e) + " " + getattr(e, "stderr", "")
            results.append(
                {
                    "name": f"{ext.upper()} real OSM roads roundtrip",
                    "status": status,
                    "detail": detail,
                }
            )
    if args.json:
        print(json.dumps(results))
    else:
        for result in results:
            print(f"{result['name']}: {result['status']} {result['detail']}")
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
