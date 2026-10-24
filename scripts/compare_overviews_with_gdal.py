#!/usr/bin/env python3
"""Compare stored-overview warp selection and pixels with GDAL on fixtures and USGS terrain.

Run with a fetched test-data directory and a built terrio CLI. Writes the same JSON
comparison records as compare_ops_with_gdal.py for the conformance report.
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


def cases(data, tmp):
    """Build cases that distinguish selected source resolutions from full-resolution reads."""
    synthetic = str(tmp / "levels.tif")
    ds = gdal.GetDriverByName("GTiff").Create(
        synthetic, 192, 160, 1, gdal.GDT_Float32,
        ["TILED=YES", "BLOCKXSIZE=32", "BLOCKYSIZE=32", "COMPRESS=DEFLATE"],
    )
    ds.SetGeoTransform([0, 1, 0, 160, 0, -1])
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(3857)
    ds.SetProjection(crs.ExportToWkt())
    y, x = np.mgrid[:160, :192]
    ds.GetRasterBand(1).WriteArray((y * y + x).astype("float32"))
    ds.BuildOverviews("AVERAGE", [2, 4, 8])
    for i in range(3):
        ds.GetRasterBand(1).GetOverview(i).Fill((i + 1) * 100)
    ds = None
    sources = [("sentinel", synthetic)]
    terrain = Path(data) / "usgs_dem.tif"
    if not terrain.exists():
        raise FileNotFoundError(f"Missing real-data fixture: {terrain}")
    cropped = str(tmp / "terrain.tif")
    ds = gdal.Translate(cropped, str(terrain), srcWin=[0, 0, 512, 512], outputType=gdal.GDT_Float32,
                        creationOptions=["TILED=YES", "COMPRESS=DEFLATE"])
    ds.BuildOverviews("AVERAGE", [2, 4, 8])
    ds = None
    sources.append(("USGS", cropped))
    for label, path in sources:
        for choice in ["AUTO", "AUTO-1", "AUTO-2", "NONE", "0", "1", "2", "8"]:
            for width, height in [(48, 40), (97, 81), (48, 160)]:
                for resampling in ["near", "bilinear", "average"]:
                    yield (f"overview {label} {choice} {width}x{height} {resampling}", [path], {
                        "width": width, "height": height, "resampleAlg": resampling, "overviewLevel": choice,
                    }, ["--ts", str(width), str(height), "-r", resampling, "--overview", choice])
        for choice in ["AUTO", "AUTO-1", "NONE", "1"]:
            # Several sources go through the whole-selected-level compositing path.
            yield (f"overview {label} mosaic {choice}", [path, path], {
                "width": 48, "height": 40, "resampleAlg": "near", "overviewLevel": choice,
            }, ["--ts", "48", "40", "--overview", choice])


    # Real terrain pixels with non-affine georeferencing exercise source pixel
    # scaling independently of the overview's affine metadata.
    gcp_path = str(tmp / "terrain-gcps.tif")
    ds = gdal.Translate(gcp_path, cropped)
    ds.SetGCPs([gdal.GCP(1000 + x * 2, 2000 - y * 3, 0, x, y)
                for x, y in [(0, 0), (512, 0), (0, 512), (512, 512), (256, 256), (128, 384)]], crs.ExportToWkt())
    ds.BuildOverviews("AVERAGE", [2, 4, 8])
    ds = None
    for choice in ["AUTO", "AUTO-1", "NONE", "0", "1", "2"]:
        yield (f"overview GCP USGS {choice}", [gcp_path], {
            "width": 128, "height": 128, "resampleAlg": "near", "overviewLevel": choice,
            "polynomialOrder": 1, "outputBounds": [1000, 464, 2024, 2000],
        }, ["--ts", "128", "128", "--te", "1000", "464", "2024", "2000", "--order", "1", "--overview", choice])


    # Real USGS terrain with analytic RPC/geolocation mappings. Nonzero stored
    # levels must retain full-resolution pixel-center and array-step coordinates.
    geographic = osr.SpatialReference()
    geographic.ImportFromEPSG(4326)
    values = gdal.Open(cropped).ReadAsArray()
    def coefficients(position, value):
        return " ".join(str(value if i == position else 0) for i in range(20))
    rpc = {"LINE_OFF": "255.5", "SAMP_OFF": "255.5", "LAT_OFF": "44.744", "LONG_OFF": "10.256",
           "HEIGHT_OFF": "0", "LINE_SCALE": "256", "SAMP_SCALE": "256", "LAT_SCALE": "0.256",
           "LONG_SCALE": "0.256", "HEIGHT_SCALE": "1", "LINE_NUM_COEFF": coefficients(2, -1),
           "SAMP_NUM_COEFF": coefficients(1, 1), "LINE_DEN_COEFF": coefficients(0, 1), "SAMP_DEN_COEFF": coefficients(0, 1)}
    for kind in ["RPC", "geolocation"]:
        path = str(tmp / f"terrain-{kind}.tif")
        ds = gdal.GetDriverByName("GTiff").Create(path, 512, 512, 1, gdal.GDT_Float32, ["TILED=YES"])
        ds.GetRasterBand(1).WriteArray(values)
        if kind == "RPC":
            ds.SetMetadata(rpc, "RPC")
            georef = {"rpc": True}
            flags = ["--rpc"]
        else:
            coordinates = []
            for axis, array in [("x", [[10, 10.512], [10, 10.512]]), ("y", [[45, 45], [44.488, 44.488]])]:
                coordinate = str(tmp / f"coordinates-{axis}.tif")
                grid = gdal.GetDriverByName("GTiff").Create(coordinate, 2, 2, 1, gdal.GDT_Float64)
                grid.GetRasterBand(1).WriteArray(np.array(array))
                grid = None
                coordinates.append(coordinate)
            ds.SetMetadata({"SRS": geographic.ExportToWkt(), "X_DATASET": coordinates[0], "Y_DATASET": coordinates[1],
                            "X_BAND": "1", "Y_BAND": "1", "PIXEL_OFFSET": "0", "LINE_OFFSET": "0",
                            "PIXEL_STEP": "512", "LINE_STEP": "512"}, "GEOLOCATION")
            georef = {"geoloc": True}
            flags = ["--geoloc"]
        ds.BuildOverviews("AVERAGE", [2, 4, 8])
        ds = None
        for choice in ["NONE", "0", "1", "2"]:
            cli = ["--ts", "128", "128", "--te", "10", "44.488", "10.512", "45", "--t-srs", "EPSG:4326",
                   "--et", "0", "--overview", choice, *flags]
            if kind == "geolocation":
                # TIFF GEOLOCATION metadata is not exposed by Terrio's TIFF reader.
                # Exercise the supported explicit-array binding API on stored pixels.
                cli = {"t_srs": "EPSG:4326", "te": (10, 44.488, 10.512, 45), "ts": (128, 128),
                       "et": 0, "overview": choice, "geolocation": {"x": coordinates[0], "y": coordinates[1],
                       "pixel_step": 512, "line_step": 512}}
            yield (f"overview {kind} USGS {choice}", [path], {
                **georef, "dstSRS": "EPSG:4326", "width": 128, "height": 128,
                "outputBounds": [10, 44.488, 10.512, 45], "resampleAlg": "near", "overviewLevel": choice,
                "errorThreshold": 0,
            }, cli)


def remote_cases(data, tmp, terrio):
    """Measure actual HTTP bytes while downsampling a real USGS COG."""
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
                reference, actual = str(tmp / f"remote-gdal-{choice}.tif"), str(tmp / f"remote-terrio-{choice}.tif")
                gdal.Warp(reference, str(cog), width=64, height=64, resampleAlg="near", overviewLevel=choice)
                with lock:
                    counts.update(bytes=0, requests=0)
                run = subprocess.run([terrio, "warp", url, actual, "--ts", "64", "64", "--overview", choice], capture_output=True, text=True)
                if run.returncode:
                    raise RuntimeError(run.stderr.strip())
                record = compare(reference, actual, 0.0)
                record.update(name=f"overview remote USGS {choice}", http_bytes=counts["bytes"], http_requests=counts["requests"], file_bytes=size)
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
    with tempfile.TemporaryDirectory(prefix="terrio-overview-") as directory:
        tmp = Path(directory)
        for i, (name, inputs, options, cli) in enumerate(cases(args.data, tmp)):
            if args.only and args.only not in name:
                continue
            reference, actual = str(tmp / f"gdal-{i}.tif"), str(tmp / f"terrio-{i}.tif")
            try:
                gdal.Warp(reference, inputs, **options)
                if isinstance(cli, dict):
                    import terrio as bindings
                    bindings.warp(inputs[0], **cli).write(actual)
                else:
                    result = subprocess.run([args.terrio, "warp", *inputs, actual, *cli], capture_output=True, text=True)
                    if result.returncode:
                        raise RuntimeError(result.stderr.strip())
                record = compare(reference, actual, 0.0)
            except Exception as error:  # report every case, including CLI errors
                record = {"error": str(error)}
            record["name"] = name
            records.append(record)
        remote_names = ["overview remote USGS NONE", "overview remote USGS AUTO"]
        if args.only is None or any(args.only in name for name in remote_names):
            try:
                records.extend(r for r in remote_cases(args.data, tmp, args.terrio)
                               if args.only is None or args.only in r["name"])
            except Exception as error:
                records.append({"name": "overview remote USGS", "error": str(error)})
    if args.json:
        print(json.dumps(records, indent=2))
    else:
        for record in records:
            print(record["name"], record.get("exact", record.get("error")))
    return int(any("error" in r or r.get("exact") != 1.0 or not r.get("same_type") or not r.get("same_nodata")
                   or r.get("gt_diff_px") != 0 for r in records))


if __name__ == "__main__":
    raise SystemExit(main())
