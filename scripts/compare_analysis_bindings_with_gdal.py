#!/usr/bin/env python3
"""Compare analysis binding outputs with GDAL on real DEM, imagery and vector data."""

import argparse
from collections import defaultdict
import json
import math
from pathlib import Path
import subprocess
import tempfile

import numpy as np
from osgeo import gdal, ogr, osr
from shapely import from_geojson, from_wkb, hausdorff_distance, union_all
import terrio

from compare_topology_with_gdal import prepare

gdal.UseExceptions()


def shapes(layer):
    return [
        (from_wkb(bytes(f.GetGeometryRef().ExportToWkb())), f.items()) for f in layer
    ]


def geometry_union(records):
    return union_all([g for g, _ in records])


def compare(case, result):
    actual = [
        (from_geojson(json.dumps(f["geometry"])), f["properties"])
        for f in result["geojson"]["features"]
    ]
    expected = case["expected"]
    crs = osr.SpatialReference()
    crs.SetFromUserInput(result["crs"])
    if not crs.IsSame(case["crs"], ["IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES"]):
        raise AssertionError("output CRS differs")
    if case["operation"] == "contour":

        def grouped(records):
            out = defaultdict(list)
            for geometry, fields in records:
                out[fields["ELEV"]].append(geometry)
                if case["options"].get("with_z") and not geometry.has_z:
                    raise AssertionError("contour lost its Z coordinates")
                if geometry.has_z:
                    if not np.all(np.asarray(geometry.coords)[:, 2] == fields["ELEV"]):
                        raise AssertionError("contour Z does not equal its elevation")
            return out

        a, b = grouped(actual), grouped(expected)
        if a.keys() != b.keys():
            raise AssertionError("contour elevations differ")
        maximum = 0.0
        for key in a:
            if len(a[key]) != len(b[key]):
                raise AssertionError("contour line counts differ")
            distance = float(hausdorff_distance(union_all(a[key]), union_all(b[key])))
            if distance > case["tolerance"] or not math.isclose(
                sum(g.length for g in a[key]),
                sum(g.length for g in b[key]),
                rel_tol=1e-10,
                abs_tol=case["tolerance"] * sum(len(g.coords) for g in a[key]),
            ):
                raise AssertionError("contour position/length differs beyond tolerance")
            maximum = max(maximum, distance)
        return (
            "tolerance",
            f"{len(actual)} contours; matching levels/counts/Z; max Hausdorff distance {maximum}, limit {case['tolerance']} CRS units",
        )
    if case["operation"] == "check_geometry":
        if len(actual) != len(expected):
            raise AssertionError("diagnostic feature count differs")
        for (a, attrs), (b, fields) in zip(actual, expected, strict=True):
            if attrs["is_valid"] != bool(b.is_valid) or not attrs["valid_reason"]:
                raise AssertionError("validity verdict or reason missing")
            if (
                not a.equals_exact(b, 0)
                or {
                    k: v
                    for k, v in attrs.items()
                    if k not in {"is_valid", "valid_reason"}
                }
                != fields
            ):
                raise AssertionError("geometry diagnostics changed source data")
        return (
            "tolerance",
            "exact GDAL validity verdicts, retained source geometry/attributes and nonempty reasons; reason wording is not compared",
        )
    keys = case.get("keys", [])

    def grouped(records):
        groups = defaultdict(list)
        for geometry, fields in records:
            groups[tuple(fields[k] for k in keys)].append((geometry, fields))
        return groups

    a, b = grouped(actual), grouped(expected)
    if a.keys() != b.keys():
        raise AssertionError("attribute groups differ")
    exact, maximum = True, 0.0
    for key in a:
        x, y = geometry_union(a[key]), geometry_union(b[key])
        if (
            not x.is_valid
            or x.geom_type not in {"Polygon", "MultiPolygon"}
            or y.area <= 0
        ):
            raise AssertionError("non-polygonal, invalid or unexpectedly empty output")
        if case["operation"] != "dissolve":
            if len(a[key]) != len(b[key]) or [fields for _, fields in a[key]] != [
                fields for _, fields in b[key]
            ]:
                raise AssertionError("feature attributes/counts differ")
        elif [fields for _, fields in a[key]] != [fields for _, fields in b[key]]:
            raise AssertionError("dissolve fields/counts differ")
        error = x.symmetric_difference(y).area / y.area
        if not math.isfinite(error) or error > case["tolerance"]:
            raise AssertionError(
                f"relative symmetric-difference area {error} exceeds {case['tolerance']}"
            )
        exact &= bool(x.equals(y))
        maximum = max(maximum, error)
    return (
        "identical" if exact else "tolerance",
        f"{len(actual)} polygon features, matching groups/attributes/CRS, valid geometry; max relative symmetric-difference area {maximum}, limit {case['tolerance']}",
    )


def fixtures(data, root):
    paths, rows = prepare(data, root)
    cases = []

    def add(name, operation, source, options, expected, crs, tolerance, keys=None):
        cases.append(
            dict(
                name=name,
                operation=operation,
                source=str(source),
                options=options,
                expected=expected,
                crs=crs,
                tolerance=tolerance,
                keys=keys or [],
            )
        )

    for label, distance in [("counties", 100), ("roads", 50)]:
        ds = ogr.Open(str(paths[label]))
        crs = ds.GetLayer(0).GetSpatialRef().Clone()
        expected = [
            (from_wkb(bytes(g.Buffer(distance, 30).ExportToWkb())), attrs)
            for g, attrs in rows[label]
        ]
        add(
            "buffer-" + label,
            "buffer",
            paths[label],
            dict(distance=distance),
            expected,
            crs,
            5e-4,
            ["source_id"],
        )
    ds = ogr.Open(str(paths["counties"]))
    crs = ds.GetLayer(0).GetSpatialRef().Clone()
    groups = defaultdict(list)
    for geometry, attrs in rows["counties"]:
        groups[attrs["group_key"]].append(from_wkb(bytes(geometry.ExportToWkb())))
    expected = [
        (union_all(gs), dict(group_key=key, feature_count=len(gs)))
        for key, gs in groups.items()
    ]
    add(
        "dissolve-groups",
        "dissolve",
        paths["counties"],
        dict(fields=["group_key"]),
        expected,
        crs,
        1e-7,
        ["group_key"],
    )
    invalid = [
        (from_wkb(bytes(g.ExportToWkb())), attrs) for g, attrs in rows["invalid"]
    ]
    add("check-real-invalid", "check_geometry", paths["invalid"], {}, invalid, crs, 0)
    fixed = [
        (from_wkb(bytes(g.MakeValid().ExportToWkb())), attrs)
        for g, attrs in rows["invalid"]
    ]
    add(
        "repair-real-invalid",
        "make_valid",
        paths["invalid"],
        {},
        fixed,
        crs,
        1e-7,
        ["source_id"],
    )
    source = root / "dem.tif"
    ds = gdal.Translate(
        str(source), str(data / "usgs_dem.tif"), srcWin=[600, 500, 96, 96]
    )
    expected_ds = ogr.GetDriverByName("MEM").CreateDataSource("")
    layer = expected_ds.CreateLayer("contour", ds.GetSpatialRef(), ogr.wkbLineString25D)
    layer.CreateField(ogr.FieldDefn("ELEV", ogr.OFTReal))
    gdal.ContourGenerateEx(
        ds.GetRasterBand(1),
        layer,
        options=[
            "LEVEL_INTERVAL=25",
            "ELEV_FIELD=0",
            "NODATA=" + str(ds.GetRasterBand(1).GetNoDataValue()),
        ],
    )
    gt = ds.GetGeoTransform()
    tolerance = max(1e-9, abs(gt[1]) * 1e-8 + max(abs(gt[0]), abs(gt[3])) * 2e-15)
    add(
        "contour-real-dem",
        "contour",
        source,
        dict(interval=25, with_z=True),
        shapes(layer),
        ds.GetSpatialRef().Clone(),
        tolerance,
    )
    ds.Close()
    image = root / "landsat.tif"
    ds = gdal.Translate(
        str(image),
        str(data / "landsat.tif"),
        srcWin=[600, 500, 96, 96],
        bandList=[1, 2, 3],
    )
    for index in range(1, 4):
        band = ds.GetRasterBand(index)
        band.SetNoDataValue(0)
        values = band.ReadAsArray()
        values[:, 48:50] = 0
        values[8:12, 8 * index : 8 * index + 4] = 0
        band.WriteArray(values)
    band = None
    ds.Close()
    for intersection in [False, True]:
        ds = gdal.Footprint(
            "",
            str(image),
            format="MEM",
            simplify=0,
            maxPoints="unlimited",
            combineBands="intersection" if intersection else "union",
        )
        expected = [(g, {"source": "real-imagery"}) for g, _ in shapes(ds.GetLayer(0))]
        add(
            "footprint-" + str(intersection),
            "footprint",
            image,
            dict(intersection=intersection, attributes={"source": "real-imagery"}),
            expected,
            ds.GetLayer(0).GetSpatialRef().Clone(),
            1e-12,
        )
    return cases


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("data", type=Path)
    p.add_argument("--js", type=Path)
    p.add_argument("--json", action="store_true")
    args = p.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-analysis-bindings-") as directory:
        root = Path(directory)
        cases = fixtures(args.data.resolve(), root)
        outputs = {}
        for case in cases:
            output = getattr(terrio, case["operation"])(
                case["source"], **case["options"]
            )
            outputs["python/" + case["name"]] = dict(
                geojson=output.to_geojson(), crs=output.crs or output.crs_wkt
            )
        if args.js:
            requests = root / "requests.json"
            requests.write_text(
                json.dumps(
                    [
                        {k: case[k] for k in ["name", "operation", "source", "options"]}
                        for case in cases
                    ]
                )
            )
            process = subprocess.run(
                [
                    "node",
                    str(Path(__file__).with_name("run_analysis_bindings.mjs")),
                    str(args.js.resolve()),
                    str(requests),
                ],
                capture_output=True,
                text=True,
                check=True,
            )
            outputs.update(
                {"js/" + item["name"]: item for item in json.loads(process.stdout)}
            )
        for backend in ["python"] + (["js"] if args.js else []):
            for case in cases:
                name = backend + "/" + case["name"]
                try:
                    status, detail = compare(case, outputs[name])
                    results.append(dict(name=name, status=status, detail=detail))
                except (AssertionError, RuntimeError) as error:
                    results.append(dict(name=name, status="differs", detail=str(error)))
    print(json.dumps(results, indent=None if args.json else 2))
    return int(any(x["status"] == "differs" for x in results))


if __name__ == "__main__":
    raise SystemExit(main())
