#!/usr/bin/env python3
"""Compare conversion and warping of per-dataset masks with GDAL (#431).

Requires landsat.tif from scripts/fetch_test_data.sh. Checks synthetic Byte and real
Landsat pixels with internal/sidecar masks, regenerated COG overviews, window and
overview extraction, and full-resolution/overview/multi-source/cutline warps.
Masks, types, nodata, CRS and geotransforms must match exactly. Byte interpolation
from a forced overview permits one value of rounding difference, and must also
match GDAL exactly with Float32 working pixels. Cutlines use a GDAL alpha reference
because GDAL 3.13.2 drops per-dataset masks when adding a cutline.
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

gdal.UseExceptions()


def compare(reference, actual, tolerance=0):
    """Inspect pixels and masks independently at every stored resolution."""
    expected, got = gdal.Open(str(reference)), gdal.Open(str(actual))
    assert (got.RasterXSize, got.RasterYSize, got.RasterCount) == (
        expected.RasterXSize,
        expected.RasterYSize,
        expected.RasterCount,
    ), "raster dimensions"
    assert got.GetGeoTransform() == expected.GetGeoTransform(), "geotransform"
    assert bool(got.GetProjection()) == bool(expected.GetProjection()), "CRS presence"
    if expected.GetProjection():
        assert got.GetSpatialRef().IsSame(expected.GetSpatialRef()), "CRS"
    levels = 0
    for index in range(1, expected.RasterCount + 1):
        a, b = expected.GetRasterBand(index), got.GetRasterBand(index)
        assert a.GetOverviewCount() == b.GetOverviewCount(), "overview count"
        for level in range(-1, a.GetOverviewCount()):
            x, y = (
                (a, b) if level == -1 else (a.GetOverview(level), b.GetOverview(level))
            )
            assert x.DataType == y.DataType, "pixel type"
            xn, yn = x.GetNoDataValue(), y.GetNoDataValue()
            assert xn == yn or (
                xn is not None and yn is not None and np.isnan(xn) and np.isnan(yn)
            ), "nodata"
            assert x.GetColorInterpretation() == y.GetColorInterpretation(), (
                "color interpretation"
            )
            assert x.GetMaskFlags() == y.GetMaskFlags(), f"mask flags at level {level}"
            np.testing.assert_allclose(
                y.ReadAsArray(),
                x.ReadAsArray(),
                rtol=0,
                atol=tolerance,
                err_msg=f"band {index} level {level} pixels",
            )
            np.testing.assert_array_equal(
                y.GetMaskBand().ReadAsArray(),
                x.GetMaskBand().ReadAsArray(),
                err_msg=f"band {index} level {level} mask",
            )
            levels += 1
    pixels = f"pixels within {tolerance}" if tolerance else "exact pixels"
    return f"{pixels}; exact masks and metadata ({levels} band levels)"


def make_source(path, real, external, invert=False):
    """GDAL-authored internal and external masks, with an unaligned invalid edge. `invert`
    makes the valid pixels the complement of the default's."""
    with gdal.config_option("GDAL_TIFF_INTERNAL_MASK", "NO" if external else "YES"):
        if real:
            src = gdal.Open(str(real))
            w, h = min(769, src.RasterXSize), min(641, src.RasterYSize)
            w, h = w - w % 4, h - h % 4
            ds = gdal.Translate(
                str(path),
                src,
                srcWin=[0, 0, w, h],
                noData="none",
                bandList=[1],
                creationOptions=[
                    "TILED=YES",
                    "COMPRESS=DEFLATE",
                    "BLOCKXSIZE=32",
                    "BLOCKYSIZE=32",
                ],
            )
        else:
            w, h = 512, 512
            ds = gdal.GetDriverByName("GTiff").Create(
                str(path), w, h, 1, gdal.GDT_Byte, ["TILED=YES"]
            )
            ds.SetGeoTransform([0, 1, 0, h, 0, -1])
            crs = osr.SpatialReference()
            crs.ImportFromEPSG(3857)
            ds.SetProjection(crs.ExportToWkt())
            ds.GetRasterBand(1).Fill(100)
        y, x = np.mgrid[:h, :w]
        mask = np.where((x < w // 2 + 1) | ((x * 31 + y * 7) % 101 < 4), 0, 255).astype(
            "uint8"
        )
        if invert:
            mask = np.where(mask == 0, 255, 0).astype("uint8")
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        ds = None


def cases(work, real):
    """Yield GDAL commands and equivalent Terrio arguments after creating each source."""
    for label, scene in [("Byte", None), ("Landsat", real)]:
        for external in [False, True]:
            source = work / f"{label}-{'external' if external else 'internal'}.tif"
            make_source(source, scene, external)
            # Its valid pixels are absent from `source`, so dropping either input changes the mosaic.
            second = work / f"{label}-{'external' if external else 'internal'}-second.tif"
            make_source(second, scene, external, invert=True)
            prefix = f"{label} {'sidecar' if external else 'internal'}"
            for method in ["nearest", "average", "bilinear", "cubic", "mode"]:
                yield (
                    f"convert {prefix} COG {method}",
                    source,
                    (
                        "convert",
                        {
                            "format": "COG",
                            "creationOptions": [
                                "BLOCKSIZE=128",
                                f"OVERVIEW_RESAMPLING={method}",
                            ],
                        },
                        [
                            "convert",
                            str(source),
                            "-f",
                            "COG",
                            "--tile-size",
                            "128",
                            "--co",
                            f"OVERVIEW_RESAMPLING={method}",
                        ],
                    ),
                )
            with gdal.config_option(
                "GDAL_TIFF_INTERNAL_MASK", "NO" if external else "YES"
            ):
                ds = gdal.Open(str(source), gdal.GA_Update)
                ds.BuildOverviews("NEAREST", [2, 4])
                ds = None
            for name, opts, args in [
                ("GTiff", {}, []),
                (
                    "window",
                    {"srcWin": [91, 73, 313, 271]},
                    ["--srcwin", "91", "73", "313", "271"],
                ),
                ("overview", {"overviewLevel": 0}, ["--overview", "0"]),
                (
                    "overview window",
                    {"overviewLevel": 1, "srcWin": [40, 24, 200, 180]},
                    ["--overview", "1", "--srcwin", "10", "6", "50", "45"],
                ),
            ]:
                # GDAL's srcWin is always in full-resolution pixels; Terrio's is in the selected level.
                yield (
                    f"convert {prefix} {name}",
                    source,
                    ("convert", opts, ["convert", str(source), *args]),
                )
            ds = gdal.Open(str(source))
            w, h = ds.RasterXSize, ds.RasterYSize
            gt = ds.GetGeoTransform()
            cutline = work / "cutline.geojson"
            cutline.write_text(
                json.dumps(
                    {
                        "type": "FeatureCollection",
                        "crs": {
                            "type": "name",
                            "properties": {
                                "name": f"EPSG:{ds.GetSpatialRef().GetAuthorityCode(None)}"
                            },
                        },
                        "features": [
                            {
                                "type": "Feature",
                                "properties": {},
                                "geometry": {
                                    "type": "Polygon",
                                    "coordinates": [
                                        [
                                            [gt[0], gt[3]],
                                            [gt[0] + w * gt[1], gt[3]],
                                            [gt[0] + w * gt[1], gt[3] + h * gt[5] / 2],
                                            [gt[0], gt[3] + h * gt[5] / 2],
                                            [gt[0], gt[3]],
                                        ]
                                    ],
                                },
                            }
                        ],
                    }
                )
            )
            ds = None
            for choice in ["NONE", "AUTO", "0"]:
                for method in ["near", "bilinear", "average"]:
                    for mode in ["single", "multiple", "cutline"]:
                        inputs = [source, second] if mode == "multiple" else [source]
                        opts = {
                            "width": w if choice == "NONE" else w // 4,
                            "height": h if choice == "NONE" else h // 4,
                            "overviewLevel": choice,
                            "resampleAlg": method,
                        }
                        args = [
                            "warp",
                            *map(str, inputs),
                            "--ts",
                            str(opts["width"]),
                            str(opts["height"]),
                            "--overview",
                            choice,
                            "-r",
                            method,
                        ]
                        if mode == "cutline":
                            opts.update(cutlineDSName=str(cutline))
                            args += ["--cutline", str(cutline)]
                        yield (
                            f"warp {prefix} {mode} {choice} {method}",
                            inputs,
                            ("warp", opts, args),
                        )


def remote_cases(work, real, terrio):
    """Count payload bytes for full and small masked COG reads over HTTP."""
    source, cog = work / "remote-source.tif", work / "remote.tif"
    make_source(source, real, False)
    gdal.Translate(
        str(cog),
        str(source),
        format="COG",
        creationOptions=["BLOCKSIZE=128", "COMPRESS=DEFLATE"],
    )
    size = cog.stat().st_size
    counts = {"bytes": 0, "requests": 0}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_HEAD(self):
            if self.path != "/masked.tif":
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Length", str(size))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()

        def do_GET(self):
            if self.path != "/masked.tif":
                self.send_error(404)
                return
            match = re.fullmatch(r"bytes=(\d+)-(\d*)", self.headers.get("Range", ""))
            start, end = (
                (0, size - 1)
                if match is None
                else (
                    int(match[1]),
                    min(int(match[2]) if match[2] else size - 1, size - 1),
                )
            )
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
            counts["bytes"] += len(payload)
            counts["requests"] += 1

    records = []
    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}/masked.tif"
            for label, options, flags in [
                ("whole", {}, []),
                (
                    "window",
                    {"srcWin": [391, 193, 17, 19]},
                    ["--srcwin", "391", "193", "17", "19"],
                ),
                (
                    "overview window",
                    {"overviewLevel": 1, "srcWin": [392, 192, 64, 64]},
                    ["--overview", "1", "--srcwin", "98", "48", "16", "16"],
                ),
            ]:
                record = {
                    "name": f"convert remote Landsat {label}",
                    "status": "identical",
                    "detail": "",
                }
                try:
                    reference, actual = (
                        work / "remote-gdal.tif",
                        work / "remote-terrio.tif",
                    )
                    gdal.Translate(str(reference), str(cog), **options)
                    counts.update(bytes=0, requests=0)
                    subprocess.run(
                        [terrio, "convert", url, str(actual), *flags],
                        capture_output=True,
                        text=True,
                        check=True,
                    )
                    record["detail"] = compare(reference, actual)
                    record.update(
                        http_bytes=counts["bytes"],
                        http_requests=counts["requests"],
                        source_files=1,
                        file_bytes=size,
                    )
                    if label != "whole":
                        assert 0 < counts["bytes"] < records[0]["http_bytes"], (
                            "small window must transfer fewer bytes than the whole masked image"
                        )
                    record["detail"] += (
                        f"; {counts['bytes']} bytes / {counts['requests']} GETs from one {size}-byte COG"
                    )
                except (
                    AssertionError,
                    OSError,
                    RuntimeError,
                    subprocess.CalledProcessError,
                    KeyError,
                ) as exc:
                    record.update(status="differs", detail=str(exc))
                records.append(record)
        finally:
            server.shutdown()
            worker.join()
    return records


def write_reference(reference, source, operation, options):
    """Keep the direct reference and its documented alpha/working-type probes consistent."""
    if operation == "convert":
        gdal.Translate(str(reference), str(source), **options)
    elif "cutlineDSName" in options:
        # GDAL 3.13.2 drops per-dataset validity when adding a cutline.
        # Supply the same binary validity as alpha, which GDAL combines correctly.
        alpha = gdal.Translate(
            "", str(source[0]), format="VRT", bandList=[1, "mask"], maskBand="none"
        )
        alpha.GetRasterBand(2).SetColorInterpretation(gdal.GCI_AlphaBand)
        warped = gdal.Warp("", alpha, format="MEM", srcAlpha=True, **options)
        gdal.Translate(str(reference), warped, bandList=[1], maskBand="none")
    else:
        gdal.Warp(str(reference), list(map(str, source)), **options)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/debug/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    real = args.data / "landsat.tif"
    if not real.is_file():
        parser.error(f"missing real-data fixture: {real}")
    records = []
    with tempfile.TemporaryDirectory(prefix="terrio-raster-masks-") as directory:
        work = Path(directory)
        for name, source, (operation, options, command) in cases(work, real):
            record = {"name": name, "status": "identical", "detail": ""}
            try:
                reference, actual = work / "gdal.tif", work / "terrio.tif"
                for path in [reference, actual]:
                    path.unlink(missing_ok=True)
                write_reference(reference, source, operation, options)
                if operation == "convert":
                    command.insert(2, str(actual))
                else:
                    command.insert(1 + len(source), str(actual))
                subprocess.run(
                    [args.terrio, *command], check=True, capture_output=True, text=True
                )
                try:
                    record["detail"] = compare(reference, actual)
                except AssertionError:
                    if not (
                        name.startswith("warp Landsat")
                        and options.get("overviewLevel") == "0"
                        and options.get("resampleAlg") in ("bilinear", "average")
                    ):
                        raise
                    record["detail"] = compare(reference, actual, tolerance=1)
                    aligned = work / "gdal-float32.tif"
                    aligned.unlink(missing_ok=True)
                    write_reference(
                        aligned,
                        source,
                        operation,
                        {**options, "workingType": gdal.GDT_Float32},
                    )
                    compare(aligned, actual)
                    record.update(
                        status="within tolerance",
                        detail=record["detail"]
                        + "; exact pixels with GDAL -wt Float32 (rounding also differs on an unmasked source)",
                    )
                if "cutlineDSName" in options:
                    record["detail"] += (
                        "; GDAL reference receives the binary mask as alpha (direct cutline path drops it)"
                    )
            except (
                AssertionError,
                OSError,
                RuntimeError,
                subprocess.CalledProcessError,
            ) as exc:
                record.update(
                    status="differs",
                    detail=(
                        exc.stderr
                        if isinstance(exc, subprocess.CalledProcessError)
                        else str(exc)
                    ),
                )
            records.append(record)
        records.extend(remote_cases(work, real, args.terrio))
    print(
        json.dumps(records, indent=2)
        if args.json
        else "\n".join(f"{r['status']}: {r['name']}: {r['detail']}" for r in records)
    )
    return int(any(r["status"] == "differs" for r in records))


if __name__ == "__main__":
    raise SystemExit(main())
