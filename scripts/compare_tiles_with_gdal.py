#!/usr/bin/env python3
"""Compare terrio's PMTiles, MBTiles and MVT support with GDAL's drivers on real files.

Rasters: writes MBTiles with ``gdal_translate -of MBTiles`` (then ``gdaladdo -r bilinear``) and
with ``terrio convert``, reads both with GDAL and reports how many pixels agree at full
resolution and in each overview; then checks that terrio's PMTiles hold the same pixels (GDAL 3.12
reads only vector PMTiles, so they are compared with GDAL's MBTiles).

Vectors: reads archives written by GDAL (and a Tippecanoe archive) with both tools, and writes
vector tiles with ``ogr2ogr`` and ``terrio convert``, reading both with GDAL. Reports feature
counts, total areas and lengths, and the keys whose features differ. Polygons that GDAL repairs
with GEOS's ``MakeValid`` (or fails to clip) are compared tile by tile, reading the tiles
unclipped (``CLIP=NO``): Natural Earth's countries (Sudan is invalid), US counties, and synthetic
invalid polygons (crossing rings, overlapping parts, holes outside their exterior).

Usage:
    python scripts/compare_tiles_with_gdal.py ../terrio-data [--terrio target/release/terrio]

Needs GDAL's command-line tools and Python bindings (``osgeo``) with NumPy.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from collections import defaultdict

import numpy as np
from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()


def run(cmd):
    """Runs a command and raises with its output when it fails.

    Args:
        cmd: The command and its arguments.
    """
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)}\n{p.stdout}{p.stderr}")


def levels(path):
    """Reads a raster's full resolution and overviews with GDAL.

    Args:
        path: The raster's path.

    Returns:
        A list of ``(geotransform, array)`` for the full resolution then each overview; the
        overviews' geotransforms are ``None``.
    """
    ds = gdal.Open(path)
    out = [(ds.GetGeoTransform(), ds.ReadAsArray().astype(np.int64))]
    for i in range(ds.GetRasterBand(1).GetOverviewCount()):
        bands = [ds.GetRasterBand(b + 1).GetOverview(i).ReadAsArray() for b in range(ds.RasterCount)]
        out.append((None, np.stack(bands).astype(np.int64)))
    return out


def agreement(a, b):
    """Describes how two arrays of the same shape agree.

    Args:
        a: The first array.
        b: The second array.

    Returns:
        A short text: the share of identical values, the largest and mean differences.
    """
    if a.shape != b.shape:
        return f"shapes differ: {a.shape} vs {b.shape}"
    d = np.abs(a - b)
    return f"{100 * (d == 0).mean():.3f}% identical, max diff {d.max()}, mean {d.mean():.4f}"


def aligned(gt_a, a, gt_b, b):
    """Compares two rasters on the same pixel grid over their common area.

    Args:
        gt_a: The first geotransform.
        a: The first array (bands, rows, cols).
        gt_b: The second geotransform.
        b: The second array.

    Returns:
        The agreement over the overlap (see `agreement`).
    """
    # Gray and alpha against RGBA: gray against red, alpha against alpha.
    if b.shape[0] == 2 and a.shape[0] == 4:
        a = a[[0, 3]]
    dx = round((gt_b[0] - gt_a[0]) / gt_a[1])
    dy = round((gt_b[3] - gt_a[3]) / gt_a[5])
    n = min(a.shape[0], b.shape[0])
    x0, y0 = max(0, dx), max(0, dy)
    x1, y1 = min(a.shape[2], dx + b.shape[2]), min(a.shape[1], dy + b.shape[1])
    return agreement(a[:n, y0:y1, x0:x1], b[:n, y0 - dy : y1 - dy, x0 - dx : x1 - dx])


def compare_rasters(data, terrio, tmp):
    """Writes rasters as MBTiles and PMTiles with both tools and compares them.

    Args:
        data: The test data directory.
        terrio: The terrio executable.
        tmp: A temporary directory.
    """
    srtm = os.path.join(tmp, "srtm_byte.tif")
    run(["gdal_translate", "-q", "-ot", "Byte", "-scale", "0", "4000", "1", "255", os.path.join(data, "srtm90.tif"), srtm])
    l7 = os.path.join(tmp, "landsat7_4326.tif")
    run(["gdalwarp", "-q", "-t_srs", "EPSG:4326", os.path.join(data, "landsat7.tif"), l7])
    cases = [("landsat.tif (EPSG:3857, 4 bands)", os.path.join(data, "landsat.tif")), ("srtm90 as Byte (EPSG:4326, gray)", srtm), ("landsat7 in EPSG:4326 (RGB)", l7)]
    for i, (label, src) in enumerate(cases):
        g = os.path.join(tmp, f"g{i}.mbtiles")
        t = os.path.join(tmp, f"t{i}.mbtiles")
        p = os.path.join(tmp, f"t{i}.pmtiles")
        run(["gdal_translate", "-q", "-of", "MBTiles", src, g])
        run(["gdaladdo", "-q", "-r", "bilinear", g])
        run([terrio, "convert", src, t])
        run([terrio, "convert", src, p])
        gl, tl = levels(g), levels(t)
        print(f"{label}:")
        same_gt = gl[0][0] == tl[0][0]
        print(f"  MBTiles size {gl[0][1].shape[2]}x{gl[0][1].shape[1]} vs {tl[0][1].shape[2]}x{tl[0][1].shape[1]}, geotransform {'same' if same_gt else 'differs'}")
        print(f"  full resolution: {agreement(gl[0][1], tl[0][1])}")
        for k in range(1, max(len(gl), len(tl))):
            if k < len(gl) and k < len(tl):
                print(f"  overview {k - 1} {gl[k][1].shape[2]}x{gl[k][1].shape[1]}: {agreement(gl[k][1], tl[k][1])}")
            else:
                print(f"  overview {k - 1}: only in {'GDAL' if k < len(gl) else 'terrio'}")
        # PMTiles, read by terrio, against GDAL's MBTiles read by GDAL.
        pt = os.path.join(tmp, f"t{i}_pm.tif")
        run([terrio, "convert", p, pt])
        pl = levels(pt)
        print(f"  PMTiles (terrio) vs GDAL's MBTiles: {aligned(gl[0][0], gl[0][1], pl[0][0], pl[0][1])}")


def vector_stats(path, key, zoom=None, clip=True):
    """Reads a vector dataset with GDAL and sums its features' areas and lengths by key.

    Args:
        path: The dataset.
        key: The field the features are grouped by.
        zoom: The zoom level to read (``ZOOM_LEVEL`` open option), or the highest.
        clip: Whether tile geometries are clipped to their tiles (``CLIP`` open option).

    Returns:
        ``(count, {key: [count, area, length]})`` of the first layer.
    """
    oo = [f"ZOOM_LEVEL={zoom}"] if zoom is not None else []
    if not clip:
        oo.append("CLIP=NO")
    ds = gdal.OpenEx(path, gdal.OF_VECTOR, open_options=oo)
    layer = ds.GetLayer(0)
    by_key = defaultdict(lambda: [0, 0.0, 0.0])
    n = 0
    for f in layer:
        n += 1
        e = by_key[f.GetField(key)]
        e[0] += 1
        g = f.GetGeometryRef()
        if g is not None:
            e[1] += g.GetArea()
            e[2] += g.Length()
    return n, by_key


def report(label, a, b):
    """Prints how two `vector_stats` results agree.

    Args:
        label: What is compared.
        a: GDAL's result.
        b: terrio's result.
    """
    (na, da), (nb, db) = a, b
    keys = set(da) | set(db)

    def rel(x, y):
        return abs(x - y) / max(abs(x), 1e-9)

    area = rel(sum(v[1] for v in da.values()), sum(v[1] for v in db.values()))
    length = rel(sum(v[2] for v in da.values()), sum(v[2] for v in db.values()))
    other = [k for k in keys if da[k][0] != db[k][0] or rel(da[k][1], db[k][1]) > 1e-4]
    print(f"  {label}: {na} vs {nb} features, area rel. diff {area:.1e}, length rel. diff {length:.1e}, {len(other)} of {len(keys)} keys differ {sorted(map(str, other))[:5]}")


def terrio_geojson(terrio, src, out, *options):
    """Converts a dataset to GeoJSON with terrio.

    Args:
        terrio: The terrio executable.
        src: The dataset.
        out: The GeoJSON path.
        *options: More `terrio convert` arguments.

    Returns:
        The output path.
    """
    run([terrio, "convert", *options, src, out])
    return out


def compare_vectors(data, terrio, tmp):
    """Reads and writes vector tiles with both tools and compares them.

    Args:
        data: The test data directory.
        terrio: The terrio executable.
        tmp: A temporary directory.
    """
    print("Vector tiles read by GDAL and by terrio:")
    for name, key in [("countries_mvt.mbtiles", "ADM0_A3"), ("countries_mvt.pmtiles", "ADM0_A3"), ("cb_2018_us_zcta510_500k.pmtiles", "GEOID10")]:
        src = os.path.join(data, name)
        if not os.path.exists(src):
            print(f"  {name}: missing")
            continue
        out = terrio_geojson(terrio, src, os.path.join(tmp, f"{name}.geojson"))
        report(name, vector_stats(src, key), vector_stats(out, key))
    print("Vector tiles written by ogr2ogr and by terrio (MAXZOOM=4), read by GDAL:")
    for name, key in [("countries.geojson", "ADM0_A3"), ("cables.geojson", "id"), ("ne_places.parquet", "name")]:
        src = os.path.join(data, name)
        for ext in ("pmtiles", "mbtiles"):
            g = os.path.join(tmp, f"g_{name}.{ext}")
            t = os.path.join(tmp, f"t_{name}.{ext}")
            run(["ogr2ogr", "-dsco", "MAXZOOM=4", g, src])
            run([terrio, "convert", "--maxzoom", "4", src, t])
            for z in (0, 2, 4):
                report(f"{name} {ext} zoom {z}", vector_stats(g, key, z), vector_stats(t, key, z))
    print("A directory of MVT tiles (ogr2ogr -f MVT), read by GDAL and terrio, and written by both:")
    src = os.path.join(data, "countries.geojson")
    gdir, tdir = os.path.join(tmp, "gdir"), os.path.join(tmp, "tdir")
    run(["ogr2ogr", "-f", "MVT", "-dsco", "MAXZOOM=2", gdir, src])
    run([terrio, "convert", "-f", "MVT", "--maxzoom", "2", src, tdir])
    out = terrio_geojson(terrio, os.path.join(gdir, "2"), os.path.join(tmp, "gdir2.geojson"))
    report("GDAL's directory, zoom 2", vector_stats(os.path.join(gdir, "2"), "ADM0_A3"), vector_stats(out, "ADM0_A3"))
    report("both directories read by GDAL, zoom 2", vector_stats(os.path.join(gdir, "2"), "ADM0_A3"), vector_stats(os.path.join(tdir, "2"), "ADM0_A3"))


def synthetic_invalid_polygons(path):
    """Writes a GeoJSON file of invalid polygons, of the kinds GDAL repairs with GEOS.

    Args:
        path: The GeoJSON path.

    Returns:
        The path.
    """

    def square(x0, y0, x1, y1):
        return [[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]

    shapes = {
        "bowtie": {"type": "Polygon", "coordinates": [[[0, 0], [10, 10], [10, 0], [0, 10], [0, 0]]]},
        "lobe": {"type": "Polygon", "coordinates": [[[20, 1], [40, 1], [40, 20], [20, 20], [20.5, 0.6], [20, 1]]]},
        "overlap": {"type": "MultiPolygon", "coordinates": [[square(-30, 1, -20, 11)], [square(-25, 1, -15, 11)]]},
        "hole_out": {"type": "Polygon", "coordinates": [square(-60, 21, -50, 31), square(-45, 21, -40, 26)]},
        "valid": {"type": "Polygon", "coordinates": [square(-100, -40, -80, -20), square(-95, -35, -90, -30)]},
        "touch": {"type": "Polygon", "coordinates": [[[60, 1], [80, 1], [70, 11], [80, 21], [60, 21], [70, 11], [60, 1]]]},
    }
    features = [{"type": "Feature", "properties": {"name": k}, "geometry": g} for k, g in shapes.items()]
    with open(path, "w") as f:
        json.dump({"type": "FeatureCollection", "features": features}, f)
    return path


def compare_repaired_polygons(data, terrio, tmp):
    """Writes polygons GDAL repairs (or fails to clip) with both tools, and compares the tiles.

    Args:
        data: The test data directory.
        terrio: The terrio executable.
        tmp: A temporary directory.
    """
    print("Invalid polygons in vector tiles (GEOS's MakeValid), tiles read unclipped by GDAL:")
    cases = [
        (os.path.join(data, "countries.geojson"), "ADM0_A3", 6),
        (os.path.join(data, "UScounties.gpkg"), "FIPS", 8),
        (synthetic_invalid_polygons(os.path.join(tmp, "invalid.geojson")), "name", 3),
    ]
    for src, key, maxzoom in cases:
        if not os.path.exists(src):
            print(f"  {os.path.basename(src)}: missing")
            continue
        name = os.path.basename(src)
        g = os.path.join(tmp, f"g_{name}.pmtiles")
        t = os.path.join(tmp, f"t_{name}.pmtiles")
        run(["ogr2ogr", "-dsco", f"MAXZOOM={maxzoom}", g, src])
        run([terrio, "convert", "--maxzoom", str(maxzoom), src, t])
        for z in range(maxzoom + 1):
            report(f"{name} zoom {z}", vector_stats(g, key, z, clip=False), vector_stats(t, key, z, clip=False))


def compare_updates(data, terrio, tmp):
    """Updates archives with both tools and compares them.

    Rasters: ``gdalwarp -r bilinear`` into an MBTiles file then ``gdaladdo -r bilinear``, against
    ``terrio convert --append`` (a window of landsat.tif, inverted, in EPSG:3857 and EPSG:4326).
    Vectors (GDAL cannot update vector tiles): layers appended by terrio against the same
    layers written at once by ``ogr2ogr``, both read by GDAL.

    Args:
        data: The test data directory.
        terrio: The terrio executable.
        tmp: A temporary directory.
    """
    print("Updates of archives:")
    src = os.path.join(data, "landsat.tif")
    if not os.path.exists(src):
        print("  landsat.tif: missing")
        return
    part = gdal.Translate(os.path.join(tmp, "part.tif"), src, srcWin=[600, 400, 700, 500], bandList=[1, 2, 3])
    inverted = 255 - part.ReadAsArray()
    for i in range(3):
        part.GetRasterBand(i + 1).WriteArray(inverted[i])
    part = None
    gdal.Warp(os.path.join(tmp, "part4326.tif"), os.path.join(tmp, "part.tif"), dstSRS="EPSG:4326")
    for added in ("part.tif", "part4326.tif"):
        g, t = os.path.join(tmp, "upd_g.mbtiles"), os.path.join(tmp, "upd_t.mbtiles")
        for f in (g, t):
            if os.path.exists(f):
                os.remove(f)
        run(["gdal_translate", "-q", "-of", "MBTiles", src, g])
        run(["gdaladdo", "-q", "-r", "bilinear", g])
        run([terrio, "convert", src, t])
        run(["gdalwarp", "-q", "-r", "bilinear", os.path.join(tmp, added), g])
        run(["gdaladdo", "-q", "-r", "bilinear", g])
        run([terrio, "convert", "--append", os.path.join(tmp, added), t])
        for i, ((_, a), (_, b)) in enumerate(zip(levels(g), levels(t))):
            label = "full resolution" if i == 0 else f"overview {i - 1}"
            print(f"  raster {added} appended, {label}: {agreement(a, b) if a.shape == b.shape else f'size {a.shape} vs {b.shape}'}")
    countries = os.path.join(data, "countries.geojson")
    g, t = os.path.join(tmp, "upd_g.pmtiles"), os.path.join(tmp, "upd_t.pmtiles")
    for f in (g, t):
        if os.path.exists(f):
            os.remove(f)
    run(["ogr2ogr", "-dsco", "MAXZOOM=4", g, countries])
    run([terrio, "convert", "--maxzoom", "4", "--where", "CONTINENT = 'Africa'", countries, t])
    run([terrio, "convert", "--append", "--where", "CONTINENT <> 'Africa'", countries, t])
    for z in (0, 2, 4):
        report(f"countries appended to vector tiles, zoom {z}", vector_stats(g, "ADM0_A3", z, clip=False), vector_stats(t, "ADM0_A3", z, clip=False))


def compare_avif(data, terrio, tmp):
    """Reads AVIF tiles with terrio and with GDAL's AVIF driver (when GDAL has it).

    The tiles of landsat.mbtiles are encoded as AVIF by GDAL's AVIF driver; GDAL's MBTiles driver
    does not read AVIF tiles, so the AVIF tiles decoded by GDAL's AVIF driver are stored as PNG
    and that archive read by GDAL is compared with the AVIF archive read by terrio.

    Args:
        data: The test data directory.
        terrio: The terrio executable.
        tmp: A temporary directory.
    """
    print("AVIF tiles:")
    if gdal.GetDriverByName("AVIF") is None:
        print("  skipped: this GDAL has no AVIF driver")
        return
    import shutil
    import sqlite3

    src = os.path.join(data, "landsat.mbtiles")
    avif, png = os.path.join(tmp, "avif.mbtiles"), os.path.join(tmp, "avif_as_png.mbtiles")
    for out, driver, options in ((avif, "AVIF", ["QUALITY=60", "YUV_SUBSAMPLING=420"]), (png, "PNG", [])):
        shutil.copy(src if out == avif else avif, out)
        db = sqlite3.connect(out)
        for z, x, y, blob in db.execute("SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles").fetchall():
            gdal.FileFromMemBuffer("/vsimem/tile_in", bytes(blob))
            gdal.GetDriverByName(driver).CreateCopy("/vsimem/tile_out", gdal.Open("/vsimem/tile_in"), options=options)
            f = gdal.VSIFOpenL("/vsimem/tile_out", "rb")
            gdal.VSIFSeekL(f, 0, 2)
            n = gdal.VSIFTellL(f)
            gdal.VSIFSeekL(f, 0, 0)
            encoded = gdal.VSIFReadL(1, n, f)
            gdal.VSIFCloseL(f)
            db.execute("UPDATE tiles SET tile_data=? WHERE zoom_level=? AND tile_column=? AND tile_row=?", (encoded, z, x, y))
        db.execute("UPDATE metadata SET value=? WHERE name='format'", ("avif" if out == avif else "png",))
        db.commit()
        db.close()
    ref = levels(png)
    for i, (_, a) in enumerate(ref):
        out = os.path.join(tmp, f"avif_{i}.tif")
        run([terrio, "convert", avif, out] + (["--overview", str(i - 1)] if i else []))
        b = gdal.Open(out).ReadAsArray().astype(np.int64)
        label = "full resolution" if i == 0 else f"overview {i - 1}"
        print(f"  landsat.mbtiles as AVIF, {label}: {agreement(a, b)}")


def main():
    """Runs the comparisons."""
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("data", help="the directory filled by scripts/fetch_test_data.sh")
    ap.add_argument("--terrio", default="target/release/terrio", help="the terrio executable")
    args = ap.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        compare_rasters(args.data, args.terrio, tmp)
        compare_vectors(args.data, args.terrio, tmp)
        compare_repaired_polygons(args.data, args.terrio, tmp)
        compare_updates(args.data, args.terrio, tmp)
        compare_avif(args.data, args.terrio, tmp)
    return 0


if __name__ == "__main__":
    sys.exit(main())
