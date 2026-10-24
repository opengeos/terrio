"""Compare topology commands with GDAL/GEOS on Tennessee counties and Las Vegas roads.

Usage: python scripts/compare_topology_with_gdal.py ../terrio-data --terrio target/release/terrio

Uses osgeo (with GEOS). Saves projected inputs, outputs and a JSON report under
DATA/topology-validation. The source files come from scripts/fetch_test_data.sh. Geometry
coverage is compared by source attributes, allowing different polygon component order. The
relative symmetric-difference area tolerance is 1e-7 for topology, and 5e-4 for buffer curves.
Collapsed linework from polygon repair is intentionally excluded from area comparisons.
"""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
import shutil
import subprocess

from osgeo import gdal, ogr, osr

gdal.UseExceptions()
ogr.UseExceptions()


def spatial_ref(epsg):
    s = osr.SpatialReference()
    s.ImportFromEPSG(epsg)
    s.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    return s


def write(path, name, rows, srs, fields=None):
    driver = ogr.GetDriverByName("GPKG")
    if path.exists():
        driver.DeleteDataSource(str(path))
    ds = driver.CreateDataSource(str(path))
    layer = ds.CreateLayer(name, srs=srs, geom_type=ogr.wkbUnknown)
    fields = fields or [("source_id", ogr.OFTString), ("group_key", ogr.OFTString)]
    for key, kind in fields:
        layer.CreateField(ogr.FieldDefn(key, kind))
    for geometry, attrs in rows:
        feature = ogr.Feature(layer.GetLayerDefn())
        if geometry is not None:
            feature.SetGeometry(geometry)
        for key, value in attrs.items():
            if value is not None:
                feature.SetField(key, value)
        layer.CreateFeature(feature)
    ds = None


def read(path):
    ds = ogr.Open(str(path))
    layer = ds.GetLayerByName("result") or ds.GetLayer(0)
    rows = [
        (
            f.GetGeometryRef().Clone() if f.GetGeometryRef() else None,
            f.items(),
            f.GetFID(),
        )
        for f in layer
    ]
    return rows


def prepare(data, work):
    paths = {}
    rows = {}
    schemas = {}
    for name, source, epsg in [
        ("counties", "TN_Counties.geojson", 5070),
        ("roads", "las_vegas_roads.geojson", 32611),
    ]:
        ds = ogr.Open(str(data / source))
        layer = ds.GetLayer(0)
        src = layer.GetSpatialRef().Clone()
        src.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
        dst = spatial_ref(epsg)
        transform = osr.CoordinateTransformation(src, dst)
        records = []
        fields = [(f.GetName(), f.GetType()) for f in layer.schema] + [
            ("source_id", ogr.OFTString),
            ("group_key", ogr.OFTString),
        ]
        for i, f in enumerate(layer):
            g = f.GetGeometryRef().Clone()
            g.Transform(transform)
            if not g.IsValid():
                g = g.MakeValid()
            group = (
                str(f.GetField("COUNTY_FIP"))[0]
                if name == "counties"
                else str(f.GetField("class"))
            )
            records.append(
                (g, {**f.items(), "source_id": str(i + 1), "group_key": group})
            )
        path = work / f"{name}.gpkg"
        write(path, name, records, dst, fields)
        paths[name], rows[name] = path, records
        schemas[name] = fields
    srs = spatial_ref(5070)
    rows["points"] = [(g.Centroid(), a) for g, a in rows["counties"]]
    paths["points"] = work / "points.gpkg"
    write(paths["points"], "points", rows["points"], srs, schemas["counties"])
    write(work / "left.gpkg", "left", rows["counties"][:12], srs, schemas["counties"])
    right = [
        (g.Buffer(5000, 30), {**a, "source_id": "R" + a["source_id"]})
        for g, a in rows["counties"][:3]
    ]
    write(work / "right.gpkg", "right", right, srs, schemas["counties"])
    # A real county outline, simplified and deliberately corrupted to exercise repair.
    original = rows["counties"][0][0].SimplifyPreserveTopology(500)
    obj = json.loads(original.ExportToJson())
    ring = (
        obj["coordinates"][0][0]
        if obj["type"] == "MultiPolygon"
        else obj["coordinates"][0]
    )
    i, j = len(ring) // 3, 2 * len(ring) // 3
    ring[i], ring[j] = ring[j], ring[i]
    invalid = ogr.CreateGeometryFromJson(json.dumps(obj))
    assert not invalid.IsValid(), "corrupted county must exercise invalid topology"
    rows["invalid"] = [
        (
            invalid,
            {
                **rows["counties"][0][1],
                "source_id": "corrupted-county",
                "group_key": "invalid",
            },
        )
    ]
    paths["invalid"] = work / "invalid.gpkg"
    write(paths["invalid"], "invalid", rows["invalid"], srs, schemas["counties"])
    paths["left"], paths["right"] = work / "left.gpkg", work / "right.gpkg"
    return paths, rows


def union(geometries):
    collection = ogr.Geometry(ogr.wkbGeometryCollection)
    for g in geometries:
        if g is not None and not g.IsEmpty():
            collection.AddGeometry(g)
    return collection.UnaryUnion() if collection.GetGeometryCount() else None


def compare(name, actual, expected, keys, tolerance):
    def grouped(records):
        groups = {}
        for geometry, attrs in records:
            key = tuple(attrs.get(k) for k in keys)
            groups.setdefault(key, []).append(geometry)
        return {k: union(gs) for k, gs in groups.items()}

    a, b = grouped(actual), grouped(expected)
    assert a.keys() == b.keys(), (
        f"{name}: source attributes differ: {a.keys()} != {b.keys()}"
    )
    maximum = 0.0
    identical = True
    for key in a:
        ga, gb = a[key], b[key]
        assert (ga is None) == (gb is None), f"{name}/{key}: null geometry differs"
        if ga is None:
            continue
        assert ga.IsValid(), f"{name}/{key}: invalid output"
        identical = identical and bool(ga.Equals(gb))
        difference = ga.SymmetricDifference(gb).GetArea()
        relative = difference / max(1.0, gb.GetArea())
        assert math.isfinite(relative) and relative <= tolerance, (
            f"{name}/{key}: coverage error {relative} > {tolerance}"
        )
        maximum = max(maximum, relative)
    status = "identical" if identical else "within tolerance"
    print(
        f"OK {name}: {len(a)} source groups, max relative coverage error {maximum:.3g}",
        flush=True,
    )
    return {
        "name": name,
        "status": status,
        "groups": len(a),
        "max_relative_symmetric_difference": maximum,
        "tolerance": tolerance,
    }


def snapped(g, spacing):
    obj = json.loads(g.ExportToJson())

    def coords(value):
        if value and isinstance(value[0], (float, int)):
            return [
                math.copysign(math.floor(abs(v) / spacing + 0.5) * spacing, v)
                for v in value[:2]
            ]
        return [coords(v) for v in value]

    obj["coordinates"] = coords(obj["coordinates"])
    g = ogr.CreateGeometryFromJson(json.dumps(obj))
    return g if g.IsValid() else g.MakeValid()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("data", type=Path)
    local_terrio = Path(__file__).resolve().parents[1] / "target/release/terrio"
    p.add_argument("--terrio", default=shutil.which("terrio") or str(local_terrio))
    p.add_argument("--work-dir", type=Path)
    p.add_argument("--json", type=Path)
    args = p.parse_args()
    if not shutil.which(args.terrio):
        p.error("pass --terrio with the compiled CLI path")
    work = (args.work_dir or args.data / "topology-validation").resolve()
    work.mkdir(parents=True, exist_ok=True)
    paths, rows = prepare(args.data.resolve(), work)
    cases = []

    def run(command, source, name, *options):
        out = work / f"{name}.gpkg"
        subprocess.run(
            [
                args.terrio,
                command,
                str(paths[source]),
                str(out),
                "--nln",
                "result",
                "--overwrite",
                *options,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        source_ds, output_ds = ogr.Open(str(paths[source])), ogr.Open(str(out))
        assert (
            source_ds.GetLayer(0)
            .GetSpatialRef()
            .IsSame(output_ds.GetLayerByName("result").GetSpatialRef())
        ), name + ": CRS changed"
        actual = read(out)
        if command in ["buffer", "is-valid", "make-valid", "clean"]:
            assert [f.GetFID() for f in source_ds.GetLayer(0)] == [
                fid for _, _, fid in actual
            ], name + ": feature IDs changed"
        return actual

    for source, distance in [
        ("counties", 100),
        ("counties", -100),
        ("roads", 50),
        ("points", 100),
    ]:
        name = f"buffer-{source}-{distance}"
        actual = run("buffer", source, name, "--distance", str(distance))
        expected = [(g.Buffer(distance, 30), a) for g, a in rows[source]]
        cases.append(
            compare(name, [(g, a) for g, a, _ in actual], expected, ["source_id"], 5e-4)
        )
        assert [a for _, a, _ in actual] == [a for _, a in rows[source]], (
            name + ": attributes changed"
        )
    for group_fields in [[], ["group_key"]]:
        name = "dissolve-groups" if group_fields else "dissolve-all"
        actual = run(
            "dissolve",
            "counties",
            name,
            *(["--field", "group_key"] if group_fields else []),
        )
        groups = {}
        for g, a in rows["counties"]:
            groups.setdefault(a["group_key"] if group_fields else "all", []).append(g)
        expected = [
            (
                union(gs),
                {
                    **({"group_key": key} if group_fields else {}),
                    "feature_count": len(gs),
                },
            )
            for key, gs in groups.items()
        ]
        cases.append(
            compare(
                name,
                [(g, a) for g, a, _ in actual],
                expected,
                group_fields + ["feature_count"],
                1e-7,
            )
        )
    left, right = ogr.Open(str(paths["left"])), ogr.Open(str(paths["right"]))
    for operation, method in [
        ("intersection", "Intersection"),
        ("union", "Union"),
        ("difference", "Erase"),
        ("symmetric-difference", "SymDifference"),
    ]:
        name = "overlay-" + operation
        reference = work / (name + "-reference.gpkg")
        if reference.exists():
            ogr.GetDriverByName("GPKG").DeleteDataSource(str(reference))
        ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(reference))
        output = ds.CreateLayer(
            "result", srs=spatial_ref(5070), geom_type=ogr.wkbUnknown
        )
        getattr(left.GetLayer(0), method)(
            right.GetLayer(0),
            output,
            options=[
                "INPUT_PREFIX=a_",
                "METHOD_PREFIX=b_",
                "KEEP_LOWER_DIMENSION_GEOMETRIES=NO",
            ],
        )
        ds = None
        out = work / (name + ".gpkg")
        subprocess.run(
            [
                args.terrio,
                "overlay",
                str(paths["left"]),
                str(paths["right"]),
                str(out),
                "--operation",
                operation,
                "--nln",
                "result",
                "--overwrite",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        keys = (
            ["a_source_id"]
            if operation == "difference"
            else ["a_source_id", "b_source_id"]
        )
        actual = read(out)
        cases.append(
            compare(
                name,
                [(g, a) for g, a, _ in actual],
                [(g, a) for g, a, _ in read(reference)],
                keys,
                1e-7,
            )
        )
        for prefix, originals in [
            ("a_", rows["counties"][:12]),
            (
                "b_",
                [
                    (g, {**a, "source_id": "R" + a["source_id"]})
                    for g, a in rows["counties"][:3]
                ],
            ),
        ]:
            lookup = {a["source_id"]: a for _, a in originals}
            for _, attrs, _ in actual:
                source_id = attrs[prefix + "source_id"]
                expected = (
                    lookup[source_id]
                    if source_id is not None
                    else {k: None for k in originals[0][1]}
                )
                for field, value in expected.items():
                    assert attrs[prefix + field] == value, (
                        name + ": attribute " + prefix + field
                    )
    for source in ["counties", "invalid"]:
        report = run("is-valid", source, "validity-" + source)
        for (_, a, _), (g, original) in zip(report, rows[source], strict=True):
            assert bool(a["is_valid"]) == bool(g.IsValid())
            assert a["source_id"] == original["source_id"] and a["valid_reason"]
            assert all(a[k] == v for k, v in original.items()), "validity attributes"
        cases.append(
            {
                "name": "validity-" + source,
                "status": "identical",
                "features": len(report),
            }
        )
        fixed = run("make-valid", source, "make-valid-" + source)
        assert [a for _, a, _ in fixed] == [a for _, a in rows[source]], (
            "make-valid attributes"
        )
        expected = [(g if g.IsValid() else g.MakeValid(), a) for g, a in rows[source]]
        cases.append(
            compare(
                "make-valid-" + source,
                [(g, a) for g, a, _ in fixed],
                expected,
                ["source_id"],
                1e-7,
            )
        )
    cleaned = run("clean", "counties", "clean-snapped-counties", "--snap-grid", "0.1")
    assert [a for _, a, _ in cleaned] == [a for _, a in rows["counties"]], (
        "clean attributes"
    )
    cases.append(
        compare(
            "clean-snapped-counties",
            [(g, a) for g, a, _ in cleaned],
            [(snapped(g, 0.1), a) for g, a in rows["counties"]],
            ["source_id"],
            1e-7,
        )
    )
    report = {
        "reference": "GDAL " + gdal.VersionInfo("RELEASE_NAME") + "/GEOS",
        "sources": [
            "https://github.com/opengeos/datasets/releases/download/vector/TN_Counties.geojson",
            "https://github.com/opengeos/datasets/releases/download/places/las_vegas_roads.geojson",
        ],
        "cases": cases,
    }
    (args.json or work / "report.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
