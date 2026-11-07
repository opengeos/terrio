#!/usr/bin/env python3
"""Compare `terrio translate` resized from overviews with `gdal_translate -outsize/-ovr`.

Synthetic levels holding markers check which stored resolution each `-ovr` choice reads; real
USGS terrain (Float32 with a nodata value) and Landsat RGB (Byte with an internal mask) check the
resampled pixels, nodata values, types and geotransforms; a real COG served over HTTP checks that
an output resized with AUTO transfers less than half the raster bytes NONE does. Run with a
fetched test-data directory and a built terrio CLI. Writes the same JSON comparison records as
compare_ops_with_gdal.py for the conformance report.
"""

import argparse
import json
import re
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import numpy as np
from osgeo import gdal, osr

from compare_dem_calc_with_gdal import compare

gdal.UseExceptions()

# gdal_translate options and their terrio translate spellings.
FLAGS = {"-outsize": "--outsize", "-srcwin": "--srcwin", "-projwin": "--projwin", "-tr": "--tr", "-r": "-r",
         "-ovr": "--overview", "-b": "-b"}


def terrio_args(args):
    """A gdal_translate command line in terrio's spelling."""
    return [FLAGS.get(a, a) for a in args]


def cases(data, tmp):
    """Build the sources and yield `(name, source, gdal_translate arguments)`."""
    # Levels holding markers: the level read shows in the output.
    levels = str(tmp / "levels.tif")
    ds = gdal.GetDriverByName("GTiff").Create(
        levels, 192, 160, 1, gdal.GDT_Float32, ["TILED=YES", "BLOCKXSIZE=32", "BLOCKYSIZE=32", "COMPRESS=DEFLATE"])
    ds.SetGeoTransform([0, 1, 0, 160, 0, -1])
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(3857)
    ds.SetProjection(crs.ExportToWkt())
    ds.GetRasterBand(1).Fill(0)
    ds.BuildOverviews("NEAREST", [2, 4, 8])
    for i in range(3):
        ds.GetRasterBand(1).GetOverview(i).Fill((i + 1) * 100)
    ds = None
    for choice in ["AUTO", "AUTO-1", "AUTO-2", "NONE", "0", "1", "2", "8"]:
        for width, height in [(48, 40), (97, 81), (48, 160), (180, 150), (21, 17)]:
            for resampling in ["near", "bilinear", "average"]:
                yield (f"translate markers {choice} {width}x{height} {resampling}", levels,
                       ["-outsize", str(width), str(height), "-r", resampling, "-ovr", choice])
        yield (f"translate markers {choice} window", levels, ["-srcwin", "7", "5", "150", "120", "-outsize", "40", "30", "-ovr", choice])
        yield (f"translate markers {choice} level", levels, ["-ovr", choice])
    # The same levels in an external `.ovr`: GDAL's band of its image has the file's other images
    # as overviews, and AUTO chooses again among them. 60 x 50 by nearest neighbour and 50 x 42
    # otherwise read the second level (200) where one choice reads the first (100).
    external = str(tmp / "external_levels.tif")
    ds = gdal.GetDriverByName("GTiff").Create(
        external, 192, 160, 1, gdal.GDT_Float32, ["TILED=YES", "BLOCKXSIZE=32", "BLOCKYSIZE=32", "COMPRESS=DEFLATE"])
    ds.SetGeoTransform([0, 1, 0, 160, 0, -1])
    ds.SetProjection(crs.ExportToWkt())
    ds.GetRasterBand(1).Fill(0)
    ds = None
    ds = gdal.Open(external)
    ds.BuildOverviews("NEAREST", [2, 4, 8])
    ds = None
    ovr = gdal.Open(external + ".ovr", gdal.GA_Update)
    band = ovr.GetRasterBand(1)
    band.Fill(100)
    for i in range(band.GetOverviewCount()):
        band.GetOverview(i).Fill((i + 2) * 100)
    ovr = None
    for choice in ["AUTO", "AUTO-1", "NONE", "0", "1"]:
        for width, height in [(60, 50), (50, 42), (48, 40), (97, 81)]:
            for resampling in ["near", "bilinear", "average"]:
                yield (f"translate external markers {choice} {width}x{height} {resampling}", external,
                       ["-outsize", str(width), str(height), "-r", resampling, "-ovr", choice])
        yield (f"translate external markers {choice} window", external,
               ["-srcwin", "7", "5", "150", "120", "-outsize", "40", "30", "-ovr", choice])

    # Real USGS terrain: Float32 with a nodata value, average overviews.
    terrain = Path(data) / "usgs_dem.tif"
    if not terrain.exists():
        raise FileNotFoundError(f"Missing real-data fixture: {terrain}")
    dem = str(tmp / "terrain.tif")
    ds = gdal.Translate(dem, str(terrain), srcWin=[1000, 1000, 600, 500], outputType=gdal.GDT_Float32,
                        creationOptions=["TILED=YES", "COMPRESS=DEFLATE"])
    ds.BuildOverviews("AVERAGE", [2, 4, 8])
    ds = None
    # gdal_translate reads its output in swaths of 128 rows (into a GeoTIFF of default layout),
    # and in blocks of 128 x 128 pixels when its bands are not the source's first ones in order,
    # each a RasterIO request of its own: at a swath's first row or a block's first column, an
    # average, RMS or mode output may come from the next source pixel (the request's offset
    # rounded to the level), where terrio reads the output in one request, as rasterio does.
    # Outputs are at most 128 rows high, and 128 columns wide with bands so selected.
    for resampling in ["near", "bilinear", "cubic", "cubicspline", "lanczos", "average", "rms", "mode"]:
        for width, height in [(150, 125), (97, 81)]:
            yield f"translate USGS {resampling} {width}x{height}", dem, ["-outsize", str(width), str(height), "-r", resampling]
        yield f"translate USGS {resampling} window", dem, ["-srcwin", "37", "11", "401", "333", "-outsize", "128", "100", "-r", resampling]
        yield f"translate USGS {resampling} upsampled", dem, ["-srcwin", "100.3", "100.2", "40.4", "30.1", "-outsize", "120", "90", "-r", resampling]
    for choice in ["NONE", "0", "1", "AUTO-1"]:
        yield f"translate USGS -ovr {choice}", dem, ["-outsize", "97", "81", "-r", "bilinear", "-ovr", choice]
    gt = gdal.Open(dem).GetGeoTransform()
    x0, y0 = gt[0] + 55.3 * gt[1], gt[3] + 21.7 * gt[5]
    projwin = [str(v) for v in (x0, y0, x0 + 420.6 * gt[1], y0 + 300.2 * gt[5])]
    yield "translate USGS projwin bilinear", dem, ["-projwin", *projwin, "-outsize", "100", "0", "-r", "bilinear"]
    yield "translate USGS projwin near", dem, ["-projwin", *projwin, "-outsize", "50%", "50%"]
    tr = [str(gt[1] * 3), str(-gt[5] * 3)]
    yield "translate USGS tr average", dem, ["-srcwin", "0", "0", "600", "360", "-tr", *tr, "-r", "average"]
    # 500 rows into 167 at the resolution: the last row of the output is covered in part, its
    # window requested past the raster's edge.
    yield "translate USGS tr bilinear", dem, ["-tr", *tr, "-r", "bilinear"]

    # Real Landsat RGB: Byte with an internal mask, mask overviews built with the overviews.
    landsat = Path(data) / "landsat.tif"
    if not landsat.exists():
        raise FileNotFoundError(f"Missing real-data fixture: {landsat}")
    rgb = str(tmp / "rgb.tif")
    ds = gdal.Translate(rgb, str(landsat), bandList=[1, 2, 3], srcWin=[100, 100, 700, 500], outputType=gdal.GDT_Byte,
                        scaleParams=[[]], creationOptions=["TILED=YES", "COMPRESS=DEFLATE", "PHOTOMETRIC=RGB"])
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    mask = np.full((500, 700), 255, np.uint8)
    mask[:, :61] = 0
    mask[180:260, 300:420] = 0
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
    ds.BuildOverviews("AVERAGE", [2, 4, 8])
    ds = None
    for resampling in ["near", "bilinear", "cubic", "lanczos", "average", "mode"]:
        for width, height in [(178, 127), (125, 89)]:
            yield f"translate Landsat {resampling} {width}x{height}", rgb, ["-outsize", str(width), str(height), "-r", resampling]
        yield f"translate Landsat {resampling} bands", rgb, ["-b", "3", "-b", "1", "-outsize", "125", "89", "-r", resampling]


def remote_cases(data, tmp, terrio):
    """Measure actual HTTP bytes while resizing a real USGS COG."""
    source = str(tmp / "remote-source.tif")
    ds = gdal.Translate(source, str(Path(data) / "usgs_dem.tif"), width=2048, height=2048,
                        outputType=gdal.GDT_Float32, creationOptions=["TILED=YES", "COMPRESS=DEFLATE"])
    ds.BuildOverviews("AVERAGE", [2, 4, 8, 16])
    ds = None
    cog = tmp / "remote-cog.tif"
    gdal.Translate(str(cog), source, format="COG", creationOptions=["BLOCKSIZE=128", "COMPRESS=DEFLATE"])
    size = cog.stat().st_size
    counts = {"bytes": 0, "requests": 0}
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_HEAD(self):
            if self.path != "/dem.tif":
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Length", str(size))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()

        def do_GET(self):
            if self.path != "/dem.tif":
                self.send_error(404)
                return
            match = re.fullmatch(r"bytes=(\d+)-(\d*)", self.headers.get("Range", ""))
            start, end = (0, size - 1) if match is None else (int(match[1]), min(int(match[2]) if match[2] else size - 1, size - 1))
            if start >= size:
                self.send_error(416)
                return
            self.send_response(206 if match else 200)
            self.send_header("Content-Length", str(end - start + 1))
            self.send_header("Accept-Ranges", "bytes")
            if match:
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.end_headers()
            with cog.open("rb") as stream:
                stream.seek(start)
                payload = stream.read(end - start + 1)
            self.wfile.write(payload)
            with lock:
                counts["bytes"] += len(payload)
                counts["requests"] += 1

    records = []
    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}/dem.tif"
            for choice in ["NONE", "AUTO"]:
                args = ["-outsize", "64", "64", "-r", "average", "-ovr", choice]
                reference, actual = str(tmp / f"remote-gdal-{choice}.tif"), str(tmp / f"remote-terrio-{choice}.tif")
                gdal.Translate(reference, str(cog), options=args)
                with lock:
                    counts.update(bytes=0, requests=0)
                run = subprocess.run([terrio, "translate", url, actual, *terrio_args(args)], capture_output=True, text=True)
                if run.returncode:
                    raise RuntimeError(run.stderr.strip())
                record = compare(reference, actual, 0.0)
                record.update(name=f"translate remote USGS {choice}", http_bytes=counts["bytes"], http_requests=counts["requests"],
                              file_bytes=size)
                records.append(record)
        finally:
            server.shutdown()
            worker.join()
    full, auto = records
    if not (0 < auto["http_bytes"] < full["http_bytes"] / 2):
        auto["error"] = f"AUTO did not reduce network transfer: {auto['http_bytes']} vs {full['http_bytes']} bytes"
    return records


def main():
    """Run each case and return a nonzero status when comparisons fail."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data")
    parser.add_argument("--terrio", default="target/debug/terrio")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--only")
    args = parser.parse_args()
    records = []
    with tempfile.TemporaryDirectory(prefix="terrio-translate-overview-") as directory:
        tmp = Path(directory)
        for i, (name, source, options) in enumerate(cases(args.data, tmp)):
            if args.only and args.only not in name:
                continue
            reference, actual = str(tmp / f"gdal-{i}.tif"), str(tmp / f"terrio-{i}.tif")
            try:
                with gdal.quiet_errors():
                    gdal.Translate(reference, source, options=options)
                result = subprocess.run([args.terrio, "translate", source, actual, *terrio_args(options)], capture_output=True, text=True)
                if result.returncode:
                    raise RuntimeError(result.stderr.strip())
                record = compare(reference, actual, 1.0)
            except Exception as error:  # report every case, including CLI errors
                record = {"error": str(error)}
            record["name"] = name
            records.append(record)
        if args.only is None or "remote" in args.only:
            try:
                records.extend(remote_cases(args.data, tmp, args.terrio))
            except Exception as error:
                records.append({"name": "translate remote USGS", "error": str(error)})
    if args.json:
        print(json.dumps(records, indent=2))
    else:
        for record in records:
            print(record["name"], record.get("exact", record.get("error")), record.get("http_bytes", ""))
    return int(any("error" in r or r.get("exact") != 1.0 or not r.get("same_type") or not r.get("same_nodata")
                   or r.get("gt_diff_px") != 0 for r in records))


if __name__ == "__main__":
    raise SystemExit(main())
