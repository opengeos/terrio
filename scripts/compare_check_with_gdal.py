#!/usr/bin/env python3
"""Compare dataset read checking with GDAL 3.13 on real and damaged data."""

import argparse
import functools
import http.server
import json
import shutil
import sqlite3
import subprocess
import tempfile
import threading
from pathlib import Path

import numpy as np
from osgeo import gdal

gdal.UseExceptions()


def run(command):
    return subprocess.run(command, capture_output=True, text=True, timeout=180)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []

    def compare(path, label=None, expected=0):
        name = label or path.name
        try:
            actual = run([args.terrio, "check", str(path), "--json"])
            reference = run(["gdal", "dataset", "check", str(path), "--quiet"])
            unavailable = (
                path.suffix == ".parquet" and gdal.GetDriverByName("Parquet") is None
            )
            if unavailable:
                status = "not compared" if actual.returncode == 0 else "differs"
                detail = (
                    "GDAL build lacks Parquet; Terrio must still read the real file"
                )
            else:
                ok = (
                    reference.returncode in [0, 1]
                    and actual.returncode == reference.returncode
                )
                if expected is not None:
                    ok &= actual.returncode == expected
                ok &= "panicked at" not in actual.stderr
                status = "identical" if ok else "differs"
                detail = f"read status GDAL={reference.returncode}, Terrio={actual.returncode}"
            record = dict(name=name, status=status, detail=detail)
            if actual.returncode == 0:
                record["summary"] = json.loads(actual.stdout)
            elif status == "differs":
                record["detail"] += (
                    ": " + actual.stderr[-800:] + " / GDAL: " + reference.stderr[-800:]
                )
            results.append(record)
        except Exception as error:
            results.append(dict(name=name, status="differs", detail=str(error)))

    def damage_tile(path, overview=None):
        dataset = gdal.Open(str(path))
        band = dataset.GetRasterBand(1)
        if overview is not None:
            band = band.GetOverview(overview)
        width, height = band.GetBlockSize()
        x = (band.XSize - 1) // width
        y = (band.YSize - 1) // height
        offset = int(band.GetMetadataItem(f"BLOCK_OFFSET_{x}_{y}", "TIFF"))
        count = int(band.GetMetadataItem(f"BLOCK_SIZE_{x}_{y}", "TIFF"))
        dataset = None
        with path.open("r+b") as stream:
            stream.seek(offset)
            stream.write(bytes(count))

    def compare_auxiliary(path, label, expected=0):
        try:
            actual = run(
                [args.terrio, "check", str(path), "--overviews", "--masks", "--json"]
            )
            wanted_samples = wanted_masks = 0
            try:
                dataset = gdal.Open(str(path))
                for number in range(1, dataset.RasterCount + 1):
                    original = dataset.GetRasterBand(number)
                    bands = [original] + [
                        original.GetOverview(i)
                        for i in range(original.GetOverviewCount())
                    ]
                    for band in bands:
                        band.ReadRaster()
                        wanted_samples += band.XSize * band.YSize
                        if number == 1 and band.GetMaskFlags() & gdal.GMF_PER_DATASET:
                            band.GetMaskBand().ReadRaster()
                            wanted_masks += band.XSize * band.YSize
                reference = 0
            except RuntimeError:
                reference = 1
            ok = actual.returncode == reference == expected
            if actual.returncode == 0:
                summary = json.loads(actual.stdout)
                ok &= (
                    summary["raster_samples"] == wanted_samples
                    and summary["mask_samples"] == wanted_masks
                )
            results.append(
                dict(
                    name=label,
                    status="identical" if ok else "differs",
                    detail=f"GDAL Python all-level/mask read={reference}, Terrio={actual.returncode}; sample/mask counts checked",
                )
            )
        except Exception as error:
            results.append(dict(name=label, status="differs", detail=str(error)))

    for name in [
        "landsat.tif",
        "USGS_1_n40w106.tif",
        "OPERA_L3_DSWx_WTR.tif",
        "N37W120.hgt",
        "wind_global.nc",
        "countries.fgb",
        "countries.parquet",
        "countries.gpkg",
        "countries.geojson",
        "countries.kml",
        "countries.mif",
        "countries.tab",
        "hike.gpx",
        "hike_gps_trace.csv",
        "liechtenstein.osm.pbf",
        "ne_countries/ne_110m_admin_0_countries.shp",
    ]:
        path = args.data / name
        if not path.exists():
            raise FileNotFoundError(path)
        compare(path)
    with tempfile.TemporaryDirectory(prefix="terrio-check-") as directory:
        root = Path(directory)
        raster = root / "real-rgb.tif"
        ds = gdal.Translate(
            str(raster),
            str(args.data / "landsat.tif"),
            srcWin=[500, 500, 513, 517],
            creationOptions=[
                "TILED=YES",
                "BLOCKXSIZE=128",
                "BLOCKYSIZE=128",
                "COMPRESS=DEFLATE",
            ],
        )
        del ds
        compare(raster, "real RGB tiled raster")
        auxiliary = root / "with-overviews-and-mask.tif"
        shutil.copyfile(raster, auxiliary)
        with gdal.config_option("GDAL_TIFF_INTERNAL_MASK", "YES"):
            dataset = gdal.Open(str(auxiliary), gdal.GA_Update)
            dataset.CreateMaskBand(gdal.GMF_PER_DATASET)
            mask = np.full(
                (dataset.RasterYSize, dataset.RasterXSize), 255, dtype="uint8"
            )
            mask[::4, ::4] = 0
            dataset.GetRasterBand(1).GetMaskBand().WriteArray(mask)
            dataset.BuildOverviews("NEAREST", [2, 4])
            dataset = None
        compare_auxiliary(auxiliary, "real raster all overviews and masks")
        bad_overview = root / "damaged-overview.tif"
        shutil.copyfile(auxiliary, bad_overview)
        damage_tile(bad_overview, overview=0)
        compare(bad_overview, "default check excludes overview payloads")
        compare_auxiliary(
            bad_overview, "explicit check detects overview damage", expected=1
        )
        scientific = root / "wind-groups.nc"

        def scientific_subset():
            source = gdal.OpenEx(
                str(args.data / "wind_global.nc"), gdal.OF_MULTIDIM_RASTER
            )
            values = source.GetRootGroup().OpenMDArray("u_wind").ReadAsArray()[:13, :17]
            out = gdal.GetDriverByName("netCDF").CreateMultiDimensional(
                str(scientific), options=["FORMAT=NC4"]
            )
            group = out.GetRootGroup()
            axes = []
            for name, length in [("row", 13), ("col", 17)]:
                axis = group.CreateDimension(name, None, None, length)
                coordinate = group.CreateMDArray(
                    name, [axis], gdal.ExtendedDataType.Create(gdal.GDT_Float64)
                )
                coordinate.Write(np.arange(length, dtype="<f8").tobytes())
                axes.append(axis)
            group.CreateMDArray(
                "crs", [], gdal.ExtendedDataType.Create(gdal.GDT_Int32)
            ).Write(np.asarray(4326, dtype="<i4").tobytes())
            nested = group.CreateGroup("science")
            array = nested.CreateMDArray(
                "wind", axes, gdal.ExtendedDataType.Create(gdal.GDT_Float32)
            )
            array.Write(values.astype("<f4").tobytes())
            out.FlushCache()

        scientific_subset()
        compare(scientific, "real wind derived scientific groups")
        summary = results[-1].get("summary", {})
        if summary.get("arrays") != 4 or summary.get("array_elements") != 252:
            results[-1]["status"] = "differs"
            results[-1]["detail"] += "; expected all four arrays and 252 elements"
        damaged = root / "damaged-last-tile.tif"
        shutil.copyfile(raster, damaged)
        damage_tile(damaged)
        compare(damaged, "damaged compressed last tile", expected=1)
        truncated = root / "truncated.tif"
        shutil.copyfile(raster, truncated)
        with truncated.open("r+b") as stream:
            stream.truncate(truncated.stat().st_size - 100)
        compare(truncated, "truncated compressed last tile", expected=1)
        vector = root / "real-countries.gpkg"
        gdal.VectorTranslate(
            str(vector), str(args.data / "countries.fgb"), format="GPKG", limit=3
        )
        compare(vector, "real vector subset")
        with sqlite3.connect(vector) as connection:
            table, geometry = connection.execute(
                "SELECT table_name, column_name FROM gpkg_geometry_columns"
            ).fetchone()

            def quote(value):
                return '"' + value.replace('"', '""') + '"'

            triggers = connection.execute(
                "SELECT name FROM sqlite_master WHERE type='trigger' AND tbl_name=?",
                (table,),
            ).fetchall()
            for (name,) in triggers:
                connection.execute("DROP TRIGGER " + quote(name))
            connection.execute(
                f"UPDATE {quote(table)} SET {quote(geometry)}=? WHERE rowid=(SELECT max(rowid) FROM {quote(table)})",
                (sqlite3.Binary(b"GP\0\1\0\0\0\0\1\0\0\0\0"),),
            )
        # A reader can accept mutations it does not validate; compare its result
        # explicitly rather than treating success as a checksum guarantee.
        compare(vector, "malformed geometry blob reader behavior", expected=None)
        geojson = root / "countries.geojson"
        gdal.VectorTranslate(
            str(geojson), str(args.data / "countries.fgb"), format="GeoJSON", limit=3
        )
        compare(geojson, "real GeoJSON subset")
        geojson.write_bytes(geojson.read_bytes()[:-25])
        compare(geojson, "truncated GeoJSON features", expected=1)
        # Storage chunks larger than the old 256-element slab should be fetched
        # once when each decoded chunk fits the checker budget.
        store = root / "chunked-wind.zarr"
        store.mkdir()
        wind = gdal.OpenEx(str(args.data / "wind_global.nc"), gdal.OF_MULTIDIM_RASTER)
        values = wind.GetRootGroup().OpenMDArray("u_wind").ReadAsArray()
        values = np.resize(values, (2048, 2048)).astype("<f4")
        (store / ".zarray").write_text(
            json.dumps(
                dict(
                    zarr_format=2,
                    shape=[2048, 2048],
                    chunks=[1024, 1024],
                    dtype="<f4",
                    compressor=None,
                    fill_value=0,
                    order="C",
                    filters=None,
                )
            )
        )
        (store / ".zattrs").write_text(json.dumps({"_ARRAY_DIMENSIONS": ["Y", "X"]}))
        for y in range(2):
            for x in range(2):
                (store / f"{y}.{x}").write_bytes(
                    values[
                        y * 1024 : (y + 1) * 1024, x * 1024 : (x + 1) * 1024
                    ].tobytes()
                )
        compare(store, "real wind with 1024-square Zarr storage chunks")
        requests = []

        class Handler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                requests.append(self.path)
                super().do_GET()

        server = http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0), functools.partial(Handler, directory=str(root))
        )
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            actual = run(
                [
                    args.terrio,
                    "check",
                    f"http://127.0.0.1:{server.server_port}/{store.name}",
                    "--json",
                ]
            )
            counts = {
                f"{y}.{x}": requests.count(f"/{store.name}/{y}.{x}")
                for y in range(2)
                for x in range(2)
            }
            ok = actual.returncode == 0 and all(n == 1 for n in counts.values())
            if actual.returncode == 0:
                summary = json.loads(actual.stdout)
                ok &= (
                    summary["arrays"] == 1 and summary["array_elements"] == values.size
                )
            results.append(
                dict(
                    name="HTTP Zarr storage chunk reads",
                    status="identical" if ok else "differs",
                    detail=f"real wind values; chunk GET counts={counts}; encoded chunk bytes={values.nbytes}; {actual.stderr[-400:]}",
                )
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        unknown = root / "unknown.bin"
        unknown.write_bytes(b"not a dataset")
        compare(unknown, "unrecognized dataset", expected=1)
    print(
        json.dumps(results, indent=2)
        if args.json
        else "\n".join(str(result) for result in results)
    )
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
