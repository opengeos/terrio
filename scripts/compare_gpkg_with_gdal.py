"""Compare terrio's GeoPackage updates and R-tree spatial filters with GDAL's.

Updates: runs ``terrio convert`` with ``--append`` and ``--overwrite`` (and adding layers to a
file that holds a raster) against ``ogr2ogr`` with the same options, on copies of the same
files, and compares the results:

* every layer, feature by feature: feature ids, attribute values and geometries (WKB);
* ``gpkg_contents`` extents, ``gpkg_ogr_contents`` feature counts and ``sqlite_sequence``;
* R-tree indexes: SQLite's ``rtreecheck()``, and ``ogr2ogr -spat`` read through them;
* ``PRAGMA integrity_check``, GDAL's ``validate_gpkg.py``, and raster tables (checksums);
* that the file was changed in place (the same inode), not rewritten.

Appends to a large file: ``terrio convert --append`` against ``ogr2ogr -append`` on the
benchmark file (timings, feature counts, the checks above), and crash safety: terrio killed
while it commits an append (as soon as its rollback journal appears, and a little later), after
which SQLite must find the file intact, holding either all the features or none of the new ones.

Spatial filters: ``terrio convert --spat`` against ``ogr2ogr -spat`` on GeoPackages (feature
ids and values), and the time both take on a large file with and without the R-tree.

Usage::

    python scripts/compare_gpkg_with_gdal.py ../terrio-data [--terrio target/release/terrio]
"""

from __future__ import annotations

import argparse
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()


def run(cmd: list) -> None:
    """Runs a command, raising with its error output when it fails.

    Args:
        cmd: The program and its arguments.
    """
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"{' '.join(map(str, cmd))}\n{r.stderr.strip()}")


def features(path: Path, spat: tuple | None = None) -> dict:
    """Reads every vector layer of a GeoPackage with GDAL.

    Args:
        path: The file.
        spat: A spatial filter rectangle, applied through GDAL (and the R-tree).

    Returns:
        Per layer name: a list of (fid, attribute values, ISO WKB) in fid order.
    """
    ds = ogr.Open(str(path))
    out = {}
    for i in range(ds.GetLayerCount()):
        lyr = ds.GetLayer(i)
        if spat:
            lyr.SetSpatialFilterRect(*spat)
        rows = []
        for f in lyr:
            g = f.GetGeometryRef()
            values = tuple(f.GetField(j) for j in range(f.GetFieldCount()))
            rows.append((f.GetFID(), values, bytes(g.ExportToIsoWkb()) if g else None))
        out[lyr.GetName()] = sorted(rows, key=lambda r: r[0])
    return out


def sqlite_state(path: Path) -> dict:
    """The bookkeeping GDAL keeps in a GeoPackage, and SQLite's checks.

    Args:
        path: The file.

    Returns:
        Extents, feature counts, sequences, R-tree sizes and checks, and the integrity check.
    """
    c = sqlite3.connect(path)
    q = lambda sql: c.execute(sql).fetchall()  # noqa: E731
    state = {
        "contents": {
            r[0].lower(): (r[1], *[round(v, 9) if v is not None else None for v in r[2:]])
            for r in q(
                "SELECT table_name, data_type, min_x, min_y, max_x, max_y, srs_id "
                "FROM gpkg_contents"
            )
        },
        "ogr_contents": {
            r[0].lower(): r[1] for r in q("SELECT table_name, feature_count FROM gpkg_ogr_contents")
        }
        if q("SELECT name FROM sqlite_master WHERE name='gpkg_ogr_contents'")
        else {},
        "sequence": {r[0].lower(): r[1] for r in q("SELECT name, seq FROM sqlite_sequence")}
        if q("SELECT name FROM sqlite_master WHERE name='sqlite_sequence'")
        else {},
        "geometry_columns": {
            r[0].lower(): r[1:]
            for r in q(
                "SELECT table_name, column_name, geometry_type_name, srs_id, z, m "
                "FROM gpkg_geometry_columns"
            )
        },
        "integrity": q("PRAGMA integrity_check")[0][0],
    }
    rtrees = {}
    for (name,) in q(
        "SELECT name FROM sqlite_master "
        "WHERE type='table' AND sql LIKE 'CREATE VIRTUAL TABLE%rtree%'"
    ):
        rtrees[name.lower()] = (
            q(f'SELECT count(*) FROM "{name}"')[0][0],
            q(f"SELECT rtreecheck('{name}')")[0][0],
        )
    state["rtrees"] = rtrees
    c.close()
    return state


def validate(path: Path) -> list[str]:
    """Runs GDAL's GeoPackage validator.

    Args:
        path: The file.

    Returns:
        The errors it reports (empty when the file is valid).
    """
    r = subprocess.run(
        [sys.executable, "-m", "osgeo_utils.samples.validate_gpkg", str(path)],
        capture_output=True,
        text=True,
    )
    return (
        [line for line in (r.stdout + r.stderr).splitlines() if line.strip()]
        if r.returncode
        else []
    )


def rasters(path: Path) -> dict:
    """The checksums of a GeoPackage's raster tables.

    Args:
        path: The file.

    Returns:
        Per table: the checksum of each band.
    """
    c = sqlite3.connect(path)
    tables = [
        r[0]
        for r in c.execute(
            "SELECT table_name FROM gpkg_contents "
            "WHERE data_type IN ('tiles', '2d-gridded-coverage')"
        )
    ]
    c.close()
    out = {}
    for t in tables:
        ds = gdal.Open(f"GPKG:{path}:{t}")
        out[t] = [ds.GetRasterBand(b + 1).Checksum() for b in range(ds.RasterCount)]
    return out


def compare(a: Path, b: Path) -> list[str]:
    """Compares terrio's output with GDAL's.

    Args:
        a: terrio's file.
        b: GDAL's file.

    Returns:
        The differences found (empty when they agree).
    """
    diffs = []
    fa, fb = features(a), features(b)
    if sorted(n.lower() for n in fa) != sorted(n.lower() for n in fb):
        diffs.append(f"layers {sorted(fa)} vs {sorted(fb)}")
    for name, rows in fb.items():
        mine = next((v for k, v in fa.items() if k.lower() == name.lower()), None)
        if mine is None:
            continue
        if len(mine) != len(rows):
            diffs.append(f"{name}: {len(mine)} features vs {len(rows)}")
            continue
        fids = sum(x[0] == y[0] for x, y in zip(mine, rows, strict=False))
        values = sum(x[1] == y[1] for x, y in zip(mine, rows, strict=False))
        geoms = sum(x[2] == y[2] for x, y in zip(mine, rows, strict=False))
        if fids != len(rows) or values != len(rows) or geoms != len(rows):
            diffs.append(
                f"{name}: fids {fids}/{len(rows)}, values {values}/{len(rows)}, "
                f"geometries {geoms}/{len(rows)}"
            )
    sa, sb = sqlite_state(a), sqlite_state(b)
    for key in ("contents", "ogr_contents", "sequence", "geometry_columns"):
        for t, v in sb[key].items():
            if sa[key].get(t) != v:
                diffs.append(f"{key}[{t}]: {sa[key].get(t)} vs {v}")
    for t, (n, check) in sb["rtrees"].items():
        got = sa["rtrees"].get(t)
        if got is None or got[0] != n or got[1] != "ok":
            diffs.append(f"R-tree {t}: {got} vs {(n, check)}")
    if sa["integrity"] != "ok":
        diffs.append(f"integrity_check: {sa['integrity']}")
    errors = validate(a)
    if errors:
        diffs.append("validate_gpkg: " + "; ".join(errors[:3]))
    if rasters(a) != rasters(b):
        diffs.append(f"rasters {rasters(a)} vs {rasters(b)}")
    # Spatial filters read through the R-trees.
    for name, rows in fb.items():
        ds = ogr.Open(str(b))
        lyr = ds.GetLayerByName(name)
        if lyr.GetGeomType() == ogr.wkbNone or not rows:
            continue
        x0, x1, y0, y1 = lyr.GetExtent()
        rect = (
            x0 + (x1 - x0) * 0.3,
            y0 + (y1 - y0) * 0.3,
            x0 + (x1 - x0) * 0.6,
            y0 + (y1 - y0) * 0.6,
        )
        ra = next(v for k, v in features(a, rect).items() if k.lower() == name.lower())
        rb = features(b, rect)[name]
        if [r[0] for r in ra] != [r[0] for r in rb]:
            diffs.append(f"{name}: -spat through the R-tree finds {len(ra)} features vs {len(rb)}")
    return diffs


def update_cases(data: Path, terrio: str, work: Path) -> list[tuple[str, list[str]]]:
    """Runs the update cases.

    Args:
        data: The test data directory.
        terrio: The terrio executable.
        work: A scratch directory.

    Returns:
        Per case: its name and the differences found.
    """
    countries = data / "countries.geojson"
    places = data / "ne_places.zip"
    gdal_places = "/vsizip/" + str(places)
    cases = []

    def case(
        name: str, base: Path | None, terrio_steps: list[list], gdal_steps: list[list]
    ) -> None:
        a, b = work / f"{name}_terrio.gpkg", work / f"{name}_gdal.gpkg"
        for p in (a, b):
            p.unlink(missing_ok=True)
            if base is not None:
                shutil.copy(base, p)
        inode = a.stat().st_ino if base is not None else None
        for s in terrio_steps:
            run([terrio, "convert", *[a if x == "OUT" else x for x in s]])
        for s in gdal_steps:
            run(["ogr2ogr", *[b if x == "OUT" else x for x in s]])
        diffs = compare(a, b)
        if inode is not None and a.stat().st_ino != inode:
            diffs.append("the file was rewritten, not changed in place")
        cases.append((name, diffs))

    # A GDAL-written file: features appended to its layer.
    base = work / "base_gdal.gpkg"
    base.unlink(missing_ok=True)
    run(["ogr2ogr", base, countries, "-nln", "countries"])
    case(
        "append to a GDAL file",
        base,
        [[countries, "OUT", "--append", "--nln", "countries"]],
        [["-append", "OUT", countries, "-nln", "countries"]],
    )
    # The same twice, with a filter: ids keep counting.
    case(
        "append twice, filtered",
        base,
        [[countries, "OUT", "--append", "--nln", "countries", "--where", "CONTINENT = 'Europe'"]]
        * 2,
        [["-append", "OUT", countries, "-nln", "countries", "-where", "CONTINENT = 'Europe'"]] * 2,
    )
    # A terrio-written file.
    base_t = work / "base_terrio.gpkg"
    base_t.unlink(missing_ok=True)
    run([terrio, "convert", countries, base_t, "--nln", "countries"])
    case(
        "append to a terrio file",
        base_t,
        [[countries, "OUT", "--append", "--nln", "countries"]],
        [["-append", "OUT", countries, "-nln", "countries"]],
    )
    # A new layer added to a file, then another layer replaced.
    case(
        "add a layer, overwrite another",
        base,
        [
            [places, "OUT", "--append", "--nln", "places"],
            [places, "OUT", "--overwrite", "--nln", "countries", "--where", "SOV0NAME = 'France'"],
        ],
        [
            ["-update", "OUT", gdal_places, "-nln", "places"],
            [
                "-update",
                "-overwrite",
                "OUT",
                gdal_places,
                "-nln",
                "countries",
                "-where",
                "SOV0NAME = 'France'",
            ],
        ],
    )
    # Vector layers added to a raster GeoPackage: the raster is kept.
    dem = data / "dem.gpkg"
    if dem.exists():
        case(
            "add a layer to a raster file",
            dem,
            [[countries, "OUT", "--append", "--nln", "countries"]],
            [["-update", "OUT", countries, "-nln", "countries"]],
        )
    # A multi-layer file: features appended to one of its layers.
    multi = data / "multi.gpkg"
    if multi.exists():
        case(
            "append to one layer of several",
            multi,
            [[multi, "OUT", "--append", "--layer", "roads", "--nln", "roads"]],
            [["-append", "OUT", multi, "roads", "-nln", "roads"]],
        )
    return cases


def spatial_cases(data: Path, terrio: str, work: Path) -> list[tuple[str, list[str]]]:
    """Compares ``--spat`` on GeoPackages with ``ogr2ogr -spat``.

    Args:
        data: The test data directory.
        terrio: The terrio executable.
        work: A scratch directory.

    Returns:
        Per case: its name and the differences found.
    """
    cases = []
    for name, rect in [
        ("UScounties.gpkg", (-90, 35, -85, 40)),
        ("UScounties.gpkg", (-180, -90, 180, 90)),
        ("UScounties.gpkg", (0, 0, 1, 1)),
        ("countries.gpkg", (-10, 35, 30, 60)),
        ("countries.gpkg", (179, -20, 180, -15)),
        ("multi.gpkg", None),
    ]:
        src = data / name
        if not src.exists():
            continue
        if rect is None:
            ds = ogr.Open(str(src))
            x0, x1, y0, y1 = ds.GetLayer(0).GetExtent()
            ds = None
            rect = (x0, y0, (x0 + x1) / 2, (y0 + y1) / 2)
        a, b = work / "spat_terrio.gpkg", work / "spat_gdal.gpkg"
        a.unlink(missing_ok=True)
        b.unlink(missing_ok=True)
        run([terrio, "convert", src, a, "--spat", *rect])
        run(["ogr2ogr", b, src, "-spat", *rect])
        fa, fb = features(a), features(b)
        diffs = []
        for layer, rows in fb.items():
            mine = fa.get(layer, [])
            if [r[1:] for r in mine] != [r[1:] for r in rows]:
                diffs.append(f"{layer}: {len(mine)} features vs {len(rows)}")
        cases.append((f"--spat {name} {rect}", diffs))
    return cases


def big_file(work: Path, n: int) -> Path:
    """A GeoPackage of random points written by ``ogr2ogr``, created once.

    Args:
        work: The scratch directory holding it.
        n: The number of points.

    Returns:
        The file.
    """
    import random

    big = work / "big.gpkg"
    if not big.exists():
        csv = work / "big.csv"
        rnd = random.Random(1)
        with open(csv, "w") as f:
            f.write("id,name,x,y\n")
            for i in range(n):
                f.write(f"{i},p{i},{rnd.uniform(-180, 180):.6f},{rnd.uniform(-90, 90):.6f}\n")
        run(
            [
                "ogr2ogr",
                big,
                csv,
                "-oo",
                "X_POSSIBLE_NAMES=x",
                "-oo",
                "Y_POSSIBLE_NAMES=y",
                "-a_srs",
                "EPSG:4326",
                "-nln",
                "points",
            ]
        )
    return big


def points_csv(path: Path, n: int, seed: int) -> Path:
    """A CSV file of random points, to append.

    Args:
        path: The file to write.
        n: The number of points.
        seed: The random seed.

    Returns:
        The file.
    """
    import random

    rnd = random.Random(seed)
    with open(path, "w") as f:
        f.write("id,name,x,y\n")
        for i in range(n):
            f.write(f"{i},q{i},{rnd.uniform(-180, 180):.6f},{rnd.uniform(-90, 90):.6f}\n")
    return path


def count_and_check(path: Path) -> tuple[int, str, str]:
    """What SQLite finds in the benchmark file: features, integrity and R-tree checks.

    Opening the file plays back a hot rollback journal, as any SQLite connection does.

    Args:
        path: The file.

    Returns:
        The number of points, ``PRAGMA integrity_check`` and ``rtreecheck()`` results.
    """
    c = sqlite3.connect(path)
    n = c.execute("SELECT count(*) FROM points").fetchone()[0]
    integrity = c.execute("PRAGMA integrity_check").fetchone()[0]
    rtree = c.execute("SELECT rtreecheck('rtree_points_geom')").fetchone()[0]
    c.close()
    return n, integrity, rtree


def append_benchmark(terrio: str, work: Path, n: int) -> tuple[list[str], int]:
    """Times appends to the large file, compares them with GDAL's, and kills terrio mid-commit.

    Args:
        terrio: The terrio executable.
        work: A scratch directory.
        n: The number of points of the large file.

    Returns:
        Lines describing the results, and the number of failed checks.
    """
    import os
    import signal

    big = big_file(work, n)
    lines = []
    failed = 0
    oo = ["-oo", "X_POSSIBLE_NAMES=x", "-oo", "Y_POSSIBLE_NAMES=y", "-a_srs", "EPSG:4326"]
    too = ["--oo", "X_POSSIBLE_NAMES=x", "--oo", "Y_POSSIBLE_NAMES=y", "--a-srs", "EPSG:4326"]
    for m in (1_000, 100_000, n):
        src = points_csv(work / f"append_{m}.csv", m, m)
        a, b = work / "append_terrio.gpkg", work / "append_gdal.gpkg"
        times = {}
        for label, out, cmd in [
            (
                "terrio",
                a,
                [terrio, "convert", src, a, "--append", "--nln", "points", *too],
            ),
            ("ogr2ogr", b, ["ogr2ogr", "-append", b, src, "-nln", "points", *oo]),
        ]:
            shutil.copy(big, out)
            t = time.perf_counter()
            run(cmd)
            times[label] = time.perf_counter() - t
        ca, cb = count_and_check(a), count_and_check(b)
        problems = validate(a)
        ok = ca[0] == cb[0] == n + m and ca[1:] == ("ok", "ok") and not problems
        failed += not ok
        lines.append(
            f"append {m:>9} points to {n}: terrio {times['terrio']:7.3f} s, "
            f"ogr2ogr {times['ogr2ogr']:7.3f} s; {ca[0]} vs {cb[0]} features, "
            f"integrity {ca[1]}, rtreecheck {ca[2]}, validate_gpkg "
            f"{'ok' if not problems else problems[:2]}{'' if ok else '  BAD'}"
        )
    # Crash safety: terrio killed during the commit.
    src = points_csv(work / "append_crash.csv", 200_000, 7)
    victim = work / "crash.gpkg"
    journal = Path(str(victim) + "-journal")
    for delay in (0.0, 0.002, 0.01, 0.05):
        shutil.copy(big, victim)
        journal.unlink(missing_ok=True)
        proc = subprocess.Popen(
            [terrio, "convert", src, victim, "--append", "--nln", "points", *too],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        seen = False
        while proc.poll() is None:
            if journal.exists():
                seen = True
                time.sleep(delay)
                os.kill(proc.pid, signal.SIGKILL)
                break
        proc.wait()
        hot = journal.exists() and journal.stat().st_size > 0
        got = count_and_check(victim)
        # SQLite plays a hot journal back and deletes it; an empty one (terrio killed as it
        # created it) is not hot and may stay.
        left = journal.exists() and journal.stat().st_size > 0
        ok = got[0] in (n, n + 200_000) and got[1:] == ("ok", "ok") and not left
        failed += not ok
        lines.append(
            f"killed {delay * 1000:4.0f} ms into the commit ({'hot journal' if hot else 'empty journal'}"
            f"{'' if seen else ', finished first'}): SQLite finds {got[0]} features, "
            f"integrity {got[1]}, rtreecheck {got[2]}{'' if ok else '  BAD'}"
        )
    return lines, failed


def benchmark(terrio: str, work: Path, n: int) -> list[str]:
    """Times a spatial filter on a large GeoPackage.

    terrio with the R-tree, terrio scanning the whole file (the same file without its R-tree), and
    ``ogr2ogr``.

    Args:
        terrio: The terrio executable.
        work: A scratch directory.
        n: The number of points.

    Returns:
        Lines describing the timings.
    """
    big = big_file(work, n)
    noindex = work / "big_noindex.gpkg"
    shutil.copy(big, noindex)
    c = sqlite3.connect(noindex)
    for t in ("rtree_points_geom",):
        c.execute(f'DROP TABLE IF EXISTS "{t}"')
    c.execute("DELETE FROM gpkg_extensions WHERE extension_name = 'gpkg_rtree_index'")
    for (trig,) in c.execute(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'rtree_%'"
    ).fetchall():
        c.execute(f'DROP TRIGGER "{trig}"')
    c.commit()
    c.close()
    rect = ["10", "40", "12", "42"]
    lines = []
    for label, cmd in [
        ("terrio --spat (R-tree)", [terrio, "convert", big, work / "o1.gpkg", "--spat", *rect]),
        (
            "terrio --spat (no R-tree: full scan)",
            [terrio, "convert", noindex, work / "o2.gpkg", "--spat", *rect],
        ),
        ("ogr2ogr -spat (R-tree)", ["ogr2ogr", work / "o3.gpkg", big, "-spat", *rect]),
        ("terrio convert, no filter", [terrio, "convert", big, work / "o4.gpkg"]),
    ]:
        for p in (work / "o1.gpkg", work / "o2.gpkg", work / "o3.gpkg", work / "o4.gpkg"):
            p.unlink(missing_ok=True)
        best = min(timed(cmd) for _ in range(3))
        out = next(
            p
            for p in (work / "o1.gpkg", work / "o2.gpkg", work / "o3.gpkg", work / "o4.gpkg")
            if p.exists()
        )
        ds = ogr.Open(str(out))
        count = ds.GetLayer(0).GetFeatureCount()
        ds = None
        lines.append(f"{label:<40} {best:8.3f} s  {count} features")
    return lines


def timed(cmd: list) -> float:
    """Runs a command and returns how long it took.

    Args:
        cmd: The program and its arguments.

    Returns:
        Seconds.
    """
    for p in cmd:
        if isinstance(p, Path) and p.suffix == ".gpkg" and p.name.startswith("o"):
            p.unlink(missing_ok=True)
    t = time.perf_counter()
    run(cmd)
    return time.perf_counter() - t


def main() -> int:
    """Runs the comparisons.

    Returns:
        The exit status: 1 when something differs.
    """
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("data", type=Path)
    p.add_argument("--terrio", default="target/release/terrio")
    p.add_argument(
        "--points", type=int, default=2_000_000, help="points in the benchmark file (0 to skip)"
    )
    p.add_argument("--work", type=Path, help="scratch directory (kept); a temporary one by default")
    args = p.parse_args()
    work = args.work or Path(tempfile.mkdtemp(prefix="terrio_gpkg_"))
    work.mkdir(parents=True, exist_ok=True)
    results = update_cases(args.data, args.terrio, work) + spatial_cases(
        args.data, args.terrio, work
    )
    bad = 0
    for name, diffs in results:
        bad += bool(diffs)
        print(f"{'BAD' if diffs else 'ok '} {name}")
        for d in diffs:
            print(f"      {d}")
    print(f"\n{len(results) - bad} of {len(results)} cases identical to GDAL")
    if args.points:
        print(f"\nSpatial filter on {args.points} points (best of 3):")
        for line in benchmark(args.terrio, work, args.points):
            print("  " + line)
        print(f"\nAppends to {args.points} points:")
        lines, failed = append_benchmark(args.terrio, work, args.points)
        bad += failed
        for line in lines:
            print("  " + line)
    if not args.work:
        shutil.rmtree(work, ignore_errors=True)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
