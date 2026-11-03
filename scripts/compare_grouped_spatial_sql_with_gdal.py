"""Compare grouped and spatial SQL with GDAL 3.13.3 SQLite on Natural Earth.

Counts, nulls, text and CRS are exact. Logical schemas are exact except the two
explicitly checked static-type policies for empty/all-null expressions. Numeric values allow
1e-10 relative/absolute error. Constructed geometry coordinates allow 1e-7 CRS
units; buffer symmetric-difference area allows 1e-7 relative error. Existing
OGRSQL comparisons remain separate and unchanged.
"""

import argparse
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()

CASES = {
    "grouped aggregates": "SELECT CONTINENT, COUNT(*) AS n, COUNT(POP_EST) AS valid, SUM(POP_EST) AS total, AVG(POP_EST) AS mean, MIN(POP_EST) AS smallest, MAX(POP_EST) AS largest FROM countries GROUP BY CONTINENT ORDER BY CONTINENT",
    "multiple keys and distinct counts": "SELECT CONTINENT, NAME, COUNT(DISTINCT POP_EST) AS n FROM countries GROUP BY CONTINENT,NAME ORDER BY NAME LIMIT 12",
    "filtered groups and aggregate ordering": "SELECT CONTINENT AS continent, SUM(POP_EST) AS total FROM countries WHERE POP_EST>10000000 GROUP BY CONTINENT ORDER BY total DESC LIMIT 3",
    "unordered groups": "SELECT CONTINENT, COUNT(*) AS n FROM countries GROUP BY CONTINENT",
    "null predicate truthiness": "SELECT ADMIN,geometry FROM countries WHERE ST_Intersects(geometry,NULL) ORDER BY ADMIN LIMIT 3",
    "qualified grouped ordering": "SELECT CONTINENT AS region,COUNT(*) AS CONTINENT FROM countries c GROUP BY c.CONTINENT ORDER BY c.CONTINENT DESC LIMIT 1",
    "joined buffer distance": "SELECT c.ADMIN AS country,ST_Buffer(ST_Centroid(c.geometry),n.POP_EST/1000000000.0) AS geometry FROM countries c LEFT JOIN countries n ON c.ADMIN=n.ADMIN ORDER BY c.ADMIN LIMIT 4",
    "duplicate aliases rejected": "SELECT CONTINENT AS n,COUNT(*) AS n FROM countries GROUP BY CONTINENT",
    "empty groups": "SELECT CONTINENT, COUNT(*) AS n FROM countries WHERE POP_EST<0 GROUP BY CONTINENT",
    "area length and intersects": "SELECT ADMIN,geometry, ST_Area(geometry) AS area, ST_Length(geometry) AS length, ST_Intersects(geometry,geometry) AS intersects FROM countries ORDER BY ADMIN LIMIT 12",
    "centroid": "SELECT ADMIN,ST_Centroid(geometry) AS geometry FROM countries ORDER BY ADMIN LIMIT 12",
    "nested buffer": "SELECT ADMIN,ST_Buffer(ST_Centroid(geometry),0.1) AS geometry FROM countries ORDER BY ADMIN LIMIT 12",
    "transformed centroid": "SELECT ADMIN,ST_Transform(ST_Centroid(geometry),3857) AS geometry FROM countries ORDER BY ADMIN LIMIT 12",
    "spatial predicate": "SELECT ADMIN,geometry FROM countries WHERE ST_Area(geometry)>100 ORDER BY ADMIN LIMIT 12",
    "null spatial values": "SELECT geometry,ST_Area(NULL) AS area, ST_Length(NULL) AS length, ST_Intersects(geometry,NULL) AS intersects FROM countries LIMIT 3",
}


def run(args):
    result = subprocess.run([str(a) for a in args], text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr[-3000:])
    return result.stdout


def snapshot(layer):
    definition = layer.GetLayerDefn()
    fields = []
    for i in range(definition.GetFieldCount()):
        field = definition.GetFieldDefn(i)
        ty = (
            "Integer"
            if field.GetType() in (ogr.OFTInteger, ogr.OFTInteger64)
            else field.GetTypeName()
        )
        fields.append((field.GetName(), ty))
    crs = layer.GetSpatialRef()
    rows = [
        (list(f.items().values()), f.GetGeometryRef().Clone() if f.GetGeometryRef() else None)
        for f in layer
    ]
    return fields, crs.Clone() if crs else None, rows


def compare(expected, actual, buffer=False, schema_policy=None, unordered=False):
    ef, ec, er = expected
    af, ac, ar = actual
    policy_applied = False
    if ef != af:
        # OGR SQLite infers String for an expression with no non-null row to
        # inspect. Terrio deliberately retains the expression's static type.
        allowed = {
            "empty groups": (
                [("CONTINENT", "String"), ("n", "String")],
                [("CONTINENT", "String"), ("n", "Integer")],
            ),
            "null spatial values": (
                [("area", "String"), ("length", "String"), ("intersects", "Integer")],
                [("area", "Real"), ("length", "Real"), ("intersects", "Integer")],
            ),
        }.get(schema_policy)
        if allowed != (ef, af):
            raise AssertionError(f"schema differs: {af} vs {ef}")
        if schema_policy == "empty groups" and (er or ar):
            raise AssertionError("empty-schema policy requires zero rows")
        if schema_policy == "null spatial values" and any(
            values[:2] != [None, None] for values, _ in er + ar
        ):
            raise AssertionError("null-schema policy requires all-null area/length")
        policy_applied = True
    if bool(ec) != bool(ac) or (ec and not ec.IsSame(ac)):
        raise AssertionError("CRS differs")
    if len(er) != len(ar):
        raise AssertionError(f"row counts differ: {len(ar)} vs {len(er)}")
    if unordered:
        # SQL does not promise GROUP BY row order without ORDER BY. All values,
        # counts and duplicate rows still participate in the comparison.
        er = sorted(er, key=lambda row: json.dumps(row[0], ensure_ascii=False))
        ar = sorted(ar, key=lambda row: json.dumps(row[0], ensure_ascii=False))
    approximate = policy_applied
    for (ev, eg), (av, ag) in zip(er, ar):
        for e, a in zip(ev, av):
            if isinstance(e, float) and a is not None:
                if not math.isclose(e, a, rel_tol=1e-10, abs_tol=1e-10):
                    raise AssertionError(f"numeric value differs: {a} vs {e}")
                approximate |= e != a
            elif e != a:
                raise AssertionError(f"attribute differs: {a!r} vs {e!r}")
        if bool(eg) != bool(ag):
            raise AssertionError("geometry nullness differs")
        if ag and not all(math.isfinite(v) for v in ag.GetEnvelope()):
            raise AssertionError("nonfinite geometry coordinates")
        if not eg or eg.ExportToWkb() == ag.ExportToWkb():
            continue
        approximate = True
        if buffer:
            if not ag.IsValid() or eg.SymmetricDifference(ag).GetArea() > 1e-7 * eg.GetArea():
                raise AssertionError("buffer symmetric difference exceeds relative 1e-7")
            if any(abs(a - b) > 1e-7 for a, b in zip(eg.GetEnvelope(), ag.GetEnvelope())):
                raise AssertionError("buffer envelope exceeds 1e-7 source units")
        else:
            # Constructed non-buffer outputs in this suite are points.
            if (
                eg.GetGeometryType() != ogr.wkbPoint
                or ag.GetGeometryType() != ogr.wkbPoint
                or eg.Distance(ag) > 1e-7
            ):
                raise AssertionError("constructed geometry exceeds 1e-7 CRS units")
    return "within tolerance" if approximate else "identical"


def node_surface_status(package, required=False):
    """Report an unavailable optional surface, or reject an explicit request."""
    reason = None
    if not package.is_file():
        reason = f"Node/WASM package missing: {package}"
    elif not shutil.which("node"):
        reason = "Node runtime unavailable"
    if reason:
        if required:
            raise ValueError(reason)
        return dict(name="node: optional WASM validation", status="not compared", detail=reason)
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", type=Path, default=Path("target/release/terrio"))
    parser.add_argument("--node-package", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    import terrio

    rows = []
    with tempfile.TemporaryDirectory(prefix="terrio-grouped-spatial-sql-") as directory:
        work = Path(directory)
        source = work / "countries.geojson"
        gdal.VectorTranslate(
            str(source), str(args.data / "countries.geojson"), layerName="countries"
        )
        surfaces = ["sql", "convert", "python"]
        package = args.node_package or Path(
            os.environ.get("TERRIO_JS_PKG", "js/pkg-node/terrio.js")
        )
        try:
            node_status = node_surface_status(package, required=args.node_package is not None)
        except ValueError as error:
            parser.error(str(error))
        if node_status is None:
            surfaces.append("node")
        else:
            rows.append(node_status)
        for name, query in CASES.items():
            ds = ogr.Open(str(source))
            reference = ds.ExecuteSQL(query, dialect="SQLite")
            if reference is None:
                raise RuntimeError(f"GDAL SQLite did not execute: {query}")
            expected = snapshot(reference)
            ds.ReleaseResultSet(reference)
            for surface in surfaces:
                try:
                    output = work / f"{surface}.gpkg"
                    output.unlink(missing_ok=True)
                    if surface in ("sql", "convert"):
                        run(
                            [
                                args.terrio.resolve(),
                                surface,
                                source,
                                output,
                                "--query" if surface == "sql" else "--sql",
                                query,
                            ]
                        )
                    elif surface == "python":
                        terrio.sql(str(source), query).write(str(output))
                    else:
                        # The Node API writes bytes; disk I/O stays in this harness.
                        run(
                            [
                                "node",
                                "-e",
                                "const fs=require('node:fs'); const t=require(process.argv[1]); const r=t.sql(new Uint8Array(fs.readFileSync(process.argv[2])),process.argv[3]); fs.writeFileSync(process.argv[4],r.toBytes('out.gpkg')); r.free();",
                                package.resolve(),
                                source,
                                query,
                                output,
                            ]
                        )
                    if name == "duplicate aliases rejected":
                        raise AssertionError("ambiguous SQL was accepted")
                    actual_ds = ogr.Open(str(output))
                    status = compare(
                        expected,
                        snapshot(actual_ds.GetLayer(0)),
                        buffer=name in ("nested buffer", "joined buffer distance"),
                        schema_policy=name,
                        unordered=name == "unordered groups",
                    )
                    actual_ds = None
                    rows.append(
                        dict(
                            name=f"{surface}: {name}",
                            status=status,
                            detail="GDAL "
                            + gdal.VersionInfo("RELEASE_NAME")
                            + (
                                "; buffers use a stable MultiPolygon declaration; shape area/envelope compared with SpatiaLite"
                                if name in ("nested buffer", "joined buffer distance")
                                else ""
                            )
                            + (
                                "; intentional schema policy: statically typed empty/all-null expressions vs SQLite String inference"
                                if name in ("empty groups", "null spatial values")
                                else ""
                            )
                            + (
                                "; unordered GROUP BY row order excluded; complete rows compared"
                                if name == "unordered groups"
                                else ""
                            ),
                        )
                    )
                except Exception as error:
                    if name == "duplicate aliases rejected" and "duplicate output field" in str(
                        error
                    ):
                        rows.append(
                            dict(
                                name=f"{surface}: {name}",
                                status="expected rejection",
                                detail="GDAL "
                                + gdal.VersionInfo("RELEASE_NAME")
                                + " accepts duplicate aliases; Terrio deliberately rejects them before schema insertion",
                            )
                        )
                    else:
                        rows.append(
                            dict(name=f"{surface}: {name}", status="differs", detail=str(error))
                        )
    print(
        json.dumps(rows, indent=2)
        if args.json
        else "\n".join(f"{r['status']}: {r['name']}: {r['detail']}" for r in rows)
    )
    return int(any(row["status"] == "differs" for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
