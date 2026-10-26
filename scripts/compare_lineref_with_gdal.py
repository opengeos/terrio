#!/usr/bin/env python3
"""Compare `terrio lineref` with GDAL's `ogrlineref` on a real route and its milestones.

The data is GDAL's ogrlineref test route (a road in England, geographic) with its milestones,
as is, reprojected to UTM zone 30N (projected parts), and as two routes told apart by an
identifier field (one drawn against the milestones' direction, which both tools reverse). The
parts (`-create`: their positions, scales and vertices) are compared feature by feature, the
positions (`-get_pos`) and coordinates (`-get_coord`) as ogrlineref prints them (six decimals),
and the sublines (`-get_subline`) vertex by vertex. The lookups run on GDAL's parts.
"""

import argparse
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

from osgeo import ogr, osr

ogr.UseExceptions()


def run(cmd):
    """Runs a command and returns its standard output.

    Args:
        cmd: The command and its arguments.

    Returns:
        The standard output.
    """
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)}\n{p.stdout}{p.stderr}")
    return p.stdout


def features(path):
    """Reads a layer's features.

    Args:
        path: The dataset.

    Returns:
        A list of ``(fields, points)`` tuples.
    """
    ds = ogr.Open(path)
    layer = ds.GetLayer(0)
    out = []
    for f in layer:
        fields = {layer.GetLayerDefn().GetFieldDefn(i).GetName(): f.GetField(i) for i in range(f.GetFieldCount())}
        g = f.GetGeometryRef()
        out.append((fields, [tuple(p) for p in g.GetPoints()] if g else []))
    return out


def compare_layers(gdal_path, terrio_path, tol):
    """Compares two layers feature by feature.

    Args:
        gdal_path: ogrlineref's output.
        terrio_path: terrio's output.
        tol: The largest difference of a value or coordinate within tolerance.

    Returns:
        A ``(status, detail)`` tuple.
    """
    g, t = features(gdal_path), features(terrio_path)
    if len(g) != len(t):
        return "differs", f"{len(t)} features, GDAL {len(g)}"
    worst = 0.0
    for (gf, gp), (tf, tp) in zip(g, t):
        if set(gf) != set(tf):
            return "differs", f"fields {sorted(tf)}, GDAL {sorted(gf)}"
        if len(gp) != len(tp):
            return "differs", f"{len(tp)} vertices, GDAL {len(gp)}"
        for k, v in gf.items():
            if isinstance(v, float):
                worst = max(worst, abs(v - tf[k]))
            elif v != tf[k]:
                return "differs", f"{k} {tf[k]!r}, GDAL {v!r}"
        for a, b in zip(gp, tp):
            worst = max(worst, *(abs(x - y) for x, y in zip(a, b)))
    if worst == 0:
        return "identical", f"{len(g)} features"
    status = "tolerance" if worst <= tol else "differs"
    return status, f"{len(g)} features, max difference {worst:.3g}"


def datasets(data, work):
    """Writes the projected and two-route copies of the route and milestones.

    Args:
        data: The test data directory.
        work: A directory for the copies.

    Returns:
        A list of ``(label, lines, points, step, extra GDAL options, extra terrio options)``.
    """
    src = Path(data) / "gdal_utilities"
    path, stones = str(src / "path.shp"), str(src / "mstones.shp")
    utm = osr.SpatialReference()
    utm.ImportFromEPSG(32630)
    out = [("geographic", path, stones, 1000, [], [])]
    ppath, pstones = str(work / "path_utm.gpkg"), str(work / "mstones_utm.gpkg")
    for a, b in [(path, ppath), (stones, pstones)]:
        ds = ogr.GetDriverByName("GPKG").CreateDataSource(b)
        s = ogr.Open(a)
        layer = s.GetLayer(0)
        ct = osr.CoordinateTransformation(layer.GetSpatialRef(), utm)
        o = ds.CreateLayer(Path(b).stem, utm, layer.GetGeomType())
        for i in range(layer.GetLayerDefn().GetFieldCount()):
            o.CreateField(layer.GetLayerDefn().GetFieldDefn(i))
        for f in layer:
            g = f.GetGeometryRef().Clone()
            g.Transform(ct)
            n = ogr.Feature(o.GetLayerDefn())
            n.SetFrom(f)
            n.SetGeometry(g)
            o.CreateFeature(n)
        ds = None
    out.append(("projected", ppath, pstones, 500, [], []))
    # Two routes: the road, and a copy 0.05 degrees east drawn the other way.
    rpath, rstones = str(work / "routes.gpkg"), str(work / "route_stones.gpkg")
    path_ds = ogr.Open(path)
    srs = path_ds.GetLayer(0).GetSpatialRef().Clone()
    path_ds = None
    for target, source, kind in [(rpath, path, ogr.wkbLineString), (rstones, stones, ogr.wkbPoint)]:
        ds = ogr.GetDriverByName("GPKG").CreateDataSource(target)
        o = ds.CreateLayer(Path(target).stem, srs, kind)
        s = ogr.Open(source)
        layer = s.GetLayer(0)
        for i in range(layer.GetLayerDefn().GetFieldCount()):
            o.CreateField(layer.GetLayerDefn().GetFieldDefn(i))
        o.CreateField(ogr.FieldDefn("route", ogr.OFTString))
        for f in layer:
            for route, dx in [("a", 0.0), ("b", 0.05)]:
                g = f.GetGeometryRef().Clone()
                pts = [(x + dx, y) for x, y, *_ in g.GetPoints()]
                if kind == ogr.wkbLineString and route == "b":
                    pts.reverse()
                n = ogr.Feature(o.GetLayerDefn())
                n.SetFrom(f)
                n.SetField("route", route)
                if kind == ogr.wkbLineString:
                    line = ogr.Geometry(ogr.wkbLineString)
                    for x, y in pts:
                        line.AddPoint_2D(x, y)
                    n.SetGeometry(line)
                else:
                    pt = ogr.Geometry(ogr.wkbPoint)
                    pt.AddPoint_2D(*pts[0])
                    n.SetGeometry(pt)
                o.CreateFeature(n)
        ds = None
    out.append(("two routes", rpath, rstones, 1000, ["-lf", "route", "-pf", "route"], ["--line-field", "route", "--point-field", "route"]))
    return out


def main():
    """Runs and reports the comparisons."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("data", help="the test data directory (see scripts/fetch_test_data.sh)")
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    work = Path(tempfile.mkdtemp(prefix="terrio_lineref_"))
    results = []

    def report(operation, name, status, detail=""):
        results.append({"operation": operation, "name": name, "status": status, "detail": detail})
        if not args.json:
            print(f"{status:9} {operation} {name}: {detail}", flush=True)

    try:
        for label, lines, points, step, gopts, topts in datasets(args.data, work):
            gparts, tparts = str(work / f"gdal_{label}.gpkg"), str(work / f"terrio_{label}.gpkg")
            name = f"{label} step {step}"
            try:
                run(["ogrlineref", "-quiet", "-create", "-f", "GPKG", "-l", lines, "-p", points, "-pm", "pos", "-o", gparts, "-on", "parts", "-s", str(step), *gopts])
                run([args.terrio, "lineref", "create", "--lines", lines, "--points", points, "--position-field", "pos", "--step", str(step), tparts, "--layer-name", "parts", *topts])
                # The first and last parts of a geographic route are measured in UTM, by PROJ in
                # GDAL and by terrio's own projection.
                report("create", name, *compare_layers(gparts, tparts, 1e-9))
            except RuntimeError as e:
                report("create", name, "differs", str(e).splitlines()[-1])
                continue
            parts = features(gparts)
            if label == "two routes":
                continue
            # Positions of the milestones, of points beside the route and of GDAL's test point.
            src = ogr.Open(points)
            probes = [tuple(f.GetGeometryRef().GetPoint_2D()) for f in src.GetLayer(0)]
            probes += [(x + 0.0004, y - 0.0003) if label == "geographic" else (x + 30.0, y - 20.0) for x, y in probes[:3]]
            if label == "geographic":
                probes.append((-1.4345, 51.9497))
            for x, y in probes:
                g = run(["ogrlineref", "-quiet", "-get_pos", "-r", gparts, "-x", repr(x), "-y", repr(y)]).strip()
                t = run([args.terrio, "lineref", "get-pos", "--parts", gparts, "-x", repr(x), "-y", repr(y)]).strip()
                report("get_pos", f"{label} ({x:.6f}, {y:.6f})", "identical" if g == t else "differs", t if g == t else f"{t}, GDAL {g}")
            begs = [f["beg"] for f, _ in parts]
            ends = [f["end"] for f, _ in parts]
            positions = [(b + e) / 2 for b, e in zip(begs, ends)] + [begs[0] + 0.25, ends[-1] - 0.5, 15977.724709]
            for m in positions:
                if not begs[0] < m < ends[-1]:
                    continue
                g = run(["ogrlineref", "-quiet", "-get_coord", "-r", gparts, "-m", repr(m)]).strip()
                t = run([args.terrio, "lineref", "get-coord", "--parts", gparts, "-m", repr(m)]).strip()
                report("get_coord", f"{label} {m:.6f}", "identical" if g == t else "differs", t if g == t else f"{t}, GDAL {g}")
            for mb, me in [(13300, 17400), (begs[0] + 10, begs[0] + 20), (begs[1] + 1, ends[-2] - 1), (begs[2], ends[2])]:
                gsub, tsub = str(work / f"gsub_{label}_{mb}.gpkg"), str(work / f"tsub_{label}_{mb}.gpkg")
                try:
                    run(["ogrlineref", "-quiet", "-get_subline", "-r", gparts, "-mb", repr(mb), "-me", repr(me), "-f", "GPKG", "-o", gsub, "-on", "sub"])
                    run([args.terrio, "lineref", "get-subline", "--parts", gparts, "--begin", repr(mb), "--end", repr(me), tsub, "--layer-name", "sub"])
                    report("get_subline", f"{label} {mb:.6f} to {me:.6f}", *compare_layers(gsub, tsub, 1e-9))
                except RuntimeError as e:
                    report("get_subline", f"{label} {mb:.6f} to {me:.6f}", "differs", str(e).splitlines()[-1])
    finally:
        shutil.rmtree(work, ignore_errors=True)
    if args.json:
        print(json.dumps(results, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
