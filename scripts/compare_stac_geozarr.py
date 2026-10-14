"""Compare terrio's STAC and GeoZarr support with GDAL's STACIT, GeoJSON and Zarr drivers, and
with rioxarray.

Usage:
    python scripts/compare_stac_geozarr.py stac
    python scripts/compare_stac_geozarr.py geozarr DATA_DIR

``stac`` queries Earth Search (Sentinel-2 L2A over Colorado: four MGRS tiles in one UTM zone,
and, paged one item at a time, tiles in two UTM zones) and checks that ``terrio info`` lists the
subdatasets GDAL's STACIT driver lists, that the mosaics have GDAL's size, geotransform and CRS,
that windows read from them (where tiles meet) hold GDAL's pixels, and that the item collection's
footprints have the fields ``ogrinfo`` reads from the same GeoJSON.

``geozarr`` writes rasters of DATA_DIR as Zarr v2 and v3 with each georeferencing convention
(``GDAL``, ``SPATIAL_PROJ``, ``CF``) and overviews, and checks that GDAL reads the arrays and
their georeferencing, that GDAL's ``gdaladdo -r nearest`` overviews equal terrio's, and, when
xarray and rioxarray are installed, that rioxarray reads the CF stores' CRS and transform.

Needs GDAL's Python bindings (``osgeo``) and NumPy, and a built ``terrio`` binary
(``target/release/terrio``, or the path in ``TERRIO_BIN``). Exits non-zero when anything differs.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.request

import numpy as np
from osgeo import gdal, ogr

gdal.UseExceptions()
gdal.PushErrorHandler("CPLQuietErrorHandler")

TERRIO = os.environ.get("TERRIO_BIN", os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
SEARCH = "https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a"
QUERIES = [
    SEARCH + "&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05T00:00:00Z/2023-07-05T23:59:59Z&limit=10",
    SEARCH + "&bbox=-108.02,40.0,-107.98,40.05&datetime=2023-07-03T00:00:00Z/2023-07-08T23:59:59Z&limit=1",
]
FAILURES = []


def check(ok, what):
    """Record and print a comparison.

    Args:
        ok: Whether terrio agrees with the reference.
        what: A description of the comparison.
    """
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        FAILURES.append(what)


def terrio(*args):
    """Run terrio.

    Args:
        *args: The command-line arguments after ``terrio``.

    Returns:
        str: The standard output.
    """
    return subprocess.run([TERRIO, *args], check=True, capture_output=True, text=True).stdout


def subdatasets(summary):
    """The subdataset names of a ``terrio info --json`` summary.

    Args:
        summary: The summary.

    Returns:
        list: The names.
    """
    return [s["name"] for s in summary.get("subdatasets", [])]


def compare_stac():
    """Compare STAC footprints, subdatasets and STACIT mosaics with GDAL on Earth Search."""
    tmp = tempfile.mkdtemp()
    try:
        for q in QUERIES:
            name = f'STACIT:"{q}"'
            ref = gdal.Open(name)
            ref_subs = [v for k, v in ref.GetMetadata("SUBDATASETS").items() if k.endswith("_NAME")]
            ref_subs.sort(key=lambda s: int(next(k for k, v in ref.GetMetadata("SUBDATASETS").items() if v == s).split("_")[1]))
            subs = subdatasets(json.loads(terrio("info", "--json", name)))
            check(subs == ref_subs, f"{q[-60:]}: {len(subs)} subdatasets as GDAL's {len(ref_subs)}")
            for sub in [s for s in ref_subs if s.split(":")[-1].split(",")[0] in ("asset=red", "asset=scl")]:
                g = gdal.Open(sub)
                t = json.loads(terrio("info", "--json", sub))
                same = (
                    t["size"] == [g.RasterXSize, g.RasterYSize]
                    and np.allclose(t["geo_transform"], g.GetGeoTransform())
                    and str(t["crs"]["epsg"]) == g.GetSpatialRef().GetAuthorityCode(None)
                )
                check(same, f"{sub[-40:]}: size {t['size']}, geotransform and CRS as GDAL's")
                # A window where tiles meet (or the middle of the mosaic).
                w = 48
                x = min(g.RasterXSize // 2, g.RasterXSize - w)
                y = min(g.RasterYSize // 2, g.RasterYSize - w)
                out = os.path.join(tmp, "w.tif")
                terrio("convert", "--srcwin", str(x), str(y), str(w), str(w), sub, out)
                a = gdal.Open(out).ReadAsArray()
                b = g.ReadAsArray(x, y, w, w)
                check(np.array_equal(a, b), f"{sub[-40:]}: window {w}x{w} at ({x}, {y}) identical")
        # Footprints and fields, against ogrinfo on the same GeoJSON.
        path = os.path.join(tmp, "items.json")
        with urllib.request.urlopen(QUERIES[0]) as r, open(path, "wb") as f:
            f.write(r.read())
        src = ogr.Open(path)
        lyr = src.GetLayer(0)
        ref_fields = [lyr.GetLayerDefn().GetFieldDefn(i).GetName() for i in range(lyr.GetLayerDefn().GetFieldCount())]
        v = json.loads(terrio("info", "--json", path))["layers"][0]
        check([f["name"] for f in v["fields"]] == ref_fields, f"footprints: {len(v['fields'])} fields as ogrinfo's")
        check(v["feature_count"] == lyr.GetFeatureCount(), f"footprints: {v['feature_count']} features")
    finally:
        shutil.rmtree(tmp)


def compare_geozarr(data_dir):
    """Compare terrio's GeoZarr stores with GDAL's reading and overviews, and rioxarray's.

    Args:
        data_dir: The directory of test rasters.
    """
    tmp = tempfile.mkdtemp()
    try:
        for name in ("srtm_sub.tif", "dem.tif", "landsat.tif"):
            src = os.path.join(data_dir, name)
            ref = gdal.Open(src)
            tif = os.path.join(tmp, "ref_" + name)
            gdal.Translate(tif, ref)
            upd = gdal.Open(tif, gdal.GA_Update)
            upd.BuildOverviews("NEAREST", [2])
            upd = None
            ref_ovr = gdal.Open(tif)
            ovr = ref_ovr.GetRasterBand(1).GetOverview(0).ReadAsArray()
            for conv in ("GDAL", "SPATIAL_PROJ", "CF"):
                for fmt in ("ZARR_V2", "ZARR_V3"):
                    store = os.path.join(tmp, f"{name[:-4]}_{conv}_{fmt}.zarr")
                    arr = os.path.basename(store)[:-5]
                    terrio(
                        "convert", src, store, "--co", f"FORMAT={fmt}", "--co", f"GEOREFERENCING_CONVENTION={conv}",
                        "--co", "OVERVIEW_LEVELS=2",
                    )
                    g = gdal.Open(f'ZARR:"{store}":/{arr}')
                    same = (
                        np.array_equal(g.ReadAsArray(), ref.ReadAsArray(), equal_nan=True)
                        and np.allclose(g.GetGeoTransform(), ref.GetGeoTransform(), rtol=0, atol=1e-9)
                        and g.GetSpatialRef().IsSame(ref.GetSpatialRef())
                    )
                    check(same, f"{name} {conv} {fmt}: GDAL reads the pixels, geotransform and CRS")
                    o_ds = gdal.Open(f'ZARR:"{store}":/ovr_2x/{arr}')
                    o = o_ds.GetRasterBand(1).ReadAsArray()
                    check(np.array_equal(o, ovr, equal_nan=True), f"{name} {conv} {fmt}: overview equals gdaladdo -r nearest")
                    info = json.loads(terrio("info", "--json", store))
                    check(info["overviews"] == [[o.shape[1], o.shape[0]]], f"{name} {conv} {fmt}: terrio lists the overview")
                    if conv == "CF":
                        try:
                            import rioxarray  # noqa: F401
                            import xarray as xr
                        except ImportError:
                            continue
                        da = xr.open_zarr(store, consolidated=False)[arr]
                        ok = da.rio.crs is not None and np.allclose(tuple(da.rio.transform())[:6], [
                            ref.GetGeoTransform()[i] for i in (1, 2, 0, 4, 5, 3)
                        ])
                        check(ok, f"{name} CF {fmt}: rioxarray reads the CRS and transform")
    finally:
        shutil.rmtree(tmp)


def main():
    """Run the comparison named on the command line."""
    if len(sys.argv) < 2 or sys.argv[1] not in ("stac", "geozarr"):
        sys.exit(__doc__)
    if sys.argv[1] == "stac":
        compare_stac()
    else:
        compare_geozarr(sys.argv[2])
    print(f"{len(FAILURES)} difference(s)")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
