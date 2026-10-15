"""Compare terrio's streamed vector conversions with ogr2ogr, and with terrio's whole-layer ones.

``terrio convert`` reads and writes vector layers a chunk of features at a time when both
formats allow it (GeoPackage, FlatGeobuf, GeoParquet, Shapefile, GeoJSON, GeoJSON text
sequences and CSV in; the same but GeoPackage out). For the real-world files of
``scripts/fetch_test_data.sh``, each streamed output is compared:

with terrio's whole-layer output of the same conversion (``--sql "SELECT * FROM layer"`` reads
the layer whole, and the vector round-trip suite compares those writers with GDAL): the files
must be identical, byte for byte (GeoParquet: by content, as GDAL reads it), and GDAL must read
as many features in them as in the input.

With ``--features N`` (0 to skip), a synthetic GeoJSON text sequence of N square polygons is
written, turned into a GeoPackage by ``ogr2ogr``, and both are converted by terrio and
``ogr2ogr`` into FlatGeobuf, GeoParquet and CSV, timing each conversion and measuring its peak
resident memory; outputs are compared by feature count and by a sample of features read by
GDAL.

Usage::

    python scripts/compare_streams_with_gdal.py ../terrio-data [--terrio target/release/terrio]
        [--features 15000000] [--work DIR]
"""

from __future__ import annotations

import argparse
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()

NUMBER = re.compile(r"-?\d+(?:\.\d*)?(?:[eE][-+]?\d+)?")

# Real-world inputs, read as streams, and the outputs written as streams.
INPUTS = [
    "countries.gpkg",
    "UScounties.gpkg",
    "countries.fgb",
    "ne_places.parquet",
    "ne_countries/ne_110m_admin_0_countries.shp",
    "cables.geojson",
    "hike_gps_trace.csv",
]
OUTPUTS = {
    "fgb": "FlatGeobuf",
    "parquet": "Parquet",
    "shp": "ESRI Shapefile",
    "geojsonl": "GeoJSONSeq",
    "csv": "CSV",
    "geojson": "GeoJSON",
}


def run(cmd: list) -> subprocess.CompletedProcess:
    """Runs a command, raising on failure.

    Args:
        cmd: The program and its arguments.

    Returns:
        The completed process.
    """
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"{' '.join(map(str, cmd))}\n{r.stderr.strip()}")
    return r


def measured(cmd: list) -> tuple[float, float]:
    """Runs a command in a child process and measures it.

    Args:
        cmd: The program and its arguments.

    Returns:
        The wall time in seconds and the peak resident memory in MB of the child.
    """
    code = (
        "import resource, subprocess, sys, time\n"
        "t = time.perf_counter()\n"
        "r = subprocess.run(sys.argv[1:], capture_output=True)\n"
        "dt = time.perf_counter() - t\n"
        "rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss / 1024\n"
        "print(dt, rss, r.returncode)\n"
    )
    r = run([sys.executable, "-c", code, *cmd])
    dt, rss, status = r.stdout.split()
    if status != "0":
        raise RuntimeError(f"{' '.join(map(str, cmd))} failed")
    return float(dt), float(rss)


def rows(path: Path) -> list[tuple]:
    """Reads a vector file with GDAL.

    Args:
        path: The file.

    Returns:
        Each feature as a tuple of its attribute values and geometry WKT (coordinates rounded
        to 1e-7), sorted.
    """
    ds = gdal.OpenEx(str(path), gdal.OF_VECTOR)
    lyr = ds.GetLayer(0)
    n = lyr.GetLayerDefn().GetFieldCount()
    out = []
    for f in lyr:
        g = f.GetGeometryRef()
        wkt = NUMBER.sub(lambda m: f"{float(m.group()):.7f}", g.ExportToIsoWkt()) if g is not None else ""
        values = tuple(f.GetField(i) if f.IsFieldSetAndNotNull(i) else None for i in range(n))
        out.append((repr(values), wkt))
    out.sort()
    return out


def count(path: Path) -> int:
    """The number of features GDAL reads in a file's first layer.

    Args:
        path: The file.

    Returns:
        The feature count.
    """
    ds = gdal.OpenEx(str(path), gdal.OF_VECTOR)
    return ds.GetLayer(0).GetFeatureCount()


def same_files(a: Path, b: Path, ext: str) -> list[str]:
    """Compares two outputs of the same conversion, byte for byte.

    Args:
        a: The first output.
        b: The second output.
        ext: Their extension; a shapefile's sidecar files are compared too.

    Returns:
        What differs.
    """
    if ext == "parquet":
        return [] if rows(a) == rows(b) else ["content differs"]
    exts = ["shp", "shx", "dbf", "prj", "cpg"] if ext == "shp" else [ext]
    diffs = []
    for e in exts:
        pa, pb = a.with_suffix("." + e), b.with_suffix("." + e)
        if pa.exists() != pb.exists() or (pa.exists() and pa.read_bytes() != pb.read_bytes()):
            diffs.append(f".{e} differs")
    return diffs


def real_cases(data: Path, terrio: str, work: Path) -> int:
    """Streamed conversions of the real-world files, against whole conversions.

    Args:
        data: The test data directory.
        terrio: The terrio command-line tool.
        work: A scratch directory.

    Returns:
        The number of cases that differ.
    """
    bad = 0
    for name in INPUTS:
        src = data / name
        if not src.exists():
            continue
        ds = gdal.OpenEx(str(src), gdal.OF_VECTOR)
        layer = ds.GetLayer(0).GetName()
        expected = ds.GetLayer(0).GetFeatureCount()
        ds = None
        for ext in OUTPUTS:
            stem = Path(name).stem
            ours = work / f"s_{stem}.{ext}"
            whole = work / f"w_{stem}.{ext}"
            for p in (ours, whole):
                for e in ["shp", "shx", "dbf", "prj", "cpg", ext]:
                    p.with_suffix("." + e).unlink(missing_ok=True)
            try:
                run([terrio, "convert", src, ours])
                run([terrio, "convert", src, whole, "--sql", f'SELECT * FROM "{layer}"'])
                diffs = same_files(ours, whole, ext)
                n = count(ours)
            except RuntimeError as e:
                diffs, n = [str(e).splitlines()[-1]], -1
            if n != expected:
                diffs.append(f"GDAL reads {n} features, the input {expected}")
            bad += bool(diffs)
            detail = "; ".join(diffs) or f"identical files, {n} features read by GDAL"
            print(f"{'BAD' if diffs else 'ok '} {name}->{ext}  {detail}")
    return bad


def generate(path: Path, n: int) -> None:
    """Writes a GeoJSON text sequence of `n` square polygons with five attributes.

    Args:
        path: The output file.
        n: The number of features.
    """
    with open(path, "w") as f:
        for i in range(n):
            x = -179.0 + (i % 3580) * 0.1
            y = -85.0 + (i // 3580) % 1700 * 0.1
            d = 0.05
            f.write(
                '{"type":"Feature","properties":{"id":%d,"name":"parcel %d","value":%.3f,"class":"c%d",'
                '"day":"2024-%02d-%02d"},"geometry":{"type":"Polygon","coordinates":[[[%.6f,%.6f],[%.6f,%.6f],'
                "[%.6f,%.6f],[%.6f,%.6f],[%.6f,%.6f]]]}}\n"
                % (i, i, i * 0.001, i % 17, i % 12 + 1, i % 28 + 1, x, y, x + d, y, x + d, y + d, x, y + d, x, y)
            )


def sample(path: Path, ids: list[int]) -> list[tuple]:
    """Reads the features named `parcel <id>` with GDAL.

    Args:
        path: The file.
        ids: The numbers in the names (``ogr2ogr`` makes the ``id`` property the feature id of
            the GeoPackage, so the names identify the features).

    Returns:
        Each feature's name, value and geometry envelope, sorted by name.
    """
    # ogr2ogr names the WKT column of a CSV file after the geometry column (`geom`).
    oo = ["GEOM_POSSIBLE_NAMES=WKT,geom", "KEEP_GEOM_COLUMNS=NO"] if path.suffix == ".csv" else []
    ds = gdal.OpenEx(str(path), gdal.OF_VECTOR, open_options=oo)
    lyr = ds.GetLayer(0)
    lyr.SetAttributeFilter("name IN (" + ",".join(f"'parcel {i}'" for i in ids) + ")")
    out = []
    for f in lyr:
        g = f.GetGeometryRef()
        env = tuple(round(v, 6) for v in g.GetEnvelope()) if g is not None else None
        out.append((f.GetField("name"), round(float(f.GetField("value")), 6), env))
    return sorted(out)


def large_cases(terrio: str, work: Path, n: int) -> int:
    """Converts a large synthetic layer with terrio and ogr2ogr, timing them.

    Args:
        terrio: The terrio command-line tool.
        work: A scratch directory.
        n: The number of features.

    Returns:
        The number of cases that differ.
    """
    seq = work / "big.geojsonl"
    gpkg = work / "big.gpkg"
    generate(seq, n)
    run(["ogr2ogr", "-f", "GPKG", gpkg, seq, "-nln", "big"])
    print(f"\nLarge layer: {n} polygons; {seq.stat().st_size / 1e9:.2f} GB GeoJSONSeq, {gpkg.stat().st_size / 1e9:.2f} GB GeoPackage")
    print(f"  {'conversion':<28} {'terrio':>22} {'ogr2ogr':>22}  output")
    ids = sorted(random.Random(1).sample(range(n), min(n, 500)))
    bad = 0
    for src in (gpkg, seq):
        for ext in ("fgb", "parquet", "csv"):
            ours, theirs = work / f"s_big.{ext}", work / f"g_big.{ext}"
            for p in (ours, theirs):
                p.unlink(missing_ok=True)
            t1, m1 = measured([terrio, "convert", src, ours])
            lco = ["-lco", "GEOMETRY=AS_WKT"] if ext == "csv" else []
            t2, m2 = measured(["ogr2ogr", "-f", OUTPUTS[ext], theirs, src, *lco])
            ds = gdal.OpenEx(str(ours), gdal.OF_VECTOR)
            count = ds.GetLayer(0).GetFeatureCount()
            ds = None
            a, b = sample(ours, ids), sample(theirs, ids)
            ok = count == n and a == b and len(a) == len(ids)
            bad += not ok
            label = f"{src.suffix[1:]} -> {ext}"
            print(f"  {label:<28} {t1:7.1f} s {m1:8.0f} MB {t2:7.1f} s {m2:8.0f} MB  {'same' if ok else 'DIFFERS'} ({count} features)")
            ours.unlink(missing_ok=True)
            theirs.unlink(missing_ok=True)
    return bad


def main() -> int:
    """Runs the comparisons.

    Returns:
        The exit status: 1 when something differs.
    """
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("data", type=Path)
    p.add_argument("--terrio", default="target/release/terrio")
    p.add_argument("--features", type=int, default=0, help="features in the large synthetic layer (0 to skip)")
    p.add_argument("--work", type=Path, help="scratch directory (kept); a temporary one by default")
    args = p.parse_args()
    work = args.work or Path(tempfile.mkdtemp(prefix="terrio_streams_"))
    work.mkdir(parents=True, exist_ok=True)
    bad = real_cases(args.data, args.terrio, work)
    if args.features:
        bad += large_cases(args.terrio, work, args.features)
    if not args.work:
        shutil.rmtree(work, ignore_errors=True)
    print(f"\n{bad} case(s) differ")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
