#!/usr/bin/env python3
"""Compare native retile layouts and samples against GDAL on real imagery/DEM."""

import argparse
from contextlib import nullcontext
import csv
import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys
import tempfile

import numpy as np

from osgeo import gdal, ogr

from compare_raster_tiles_with_gdal import PixelDifference, compare_folders, run

gdal.UseExceptions()


def compare_samples(reference, actual, offset=(0.0, 0.0), allow_roundoff=False):
    try:
        return compare_folders(
            reference, actual, "tif", reference_origin_offset=offset
        ), False
    except PixelDifference:
        if not allow_roundoff:
            raise
        return compare_float32_roundoff(reference, actual), True


def compare_float32_roundoff(reference, actual):
    """Allow one Float32 ULP only after structural/mask checks have passed."""
    count, maximum = 0, 0.0
    for path in reference.rglob("*.tif"):
        other = actual / path.relative_to(reference)
        a, b = gdal.Open(str(path)), gdal.Open(str(other))
        x, y = a.ReadAsArray(), b.ReadAsArray()
        if x.dtype != np.float32 or y.dtype != np.float32 or x.shape != y.shape:
            raise AssertionError("roundoff allowance requires matching Float32 grids")
        equal = (x == y) | (np.isnan(x) & np.isnan(y))
        if equal.all():
            continue
        u, v = x[~equal], y[~equal]
        if not (np.isfinite(u).all() and np.isfinite(v).all()):
            raise AssertionError("nonfinite values differ")
        delta = np.abs(u.astype(np.float64) - v)
        if np.any(np.nextafter(u, v) != v):
            raise AssertionError("Float32 samples differ by more than one ULP")
        count += len(u)
        maximum = max(maximum, float(delta.max()))
    return f"{count} samples differ by at most one Float32 ULP (max absolute delta {maximum}); structure, masks and metadata match"


def verify_cog_reference_byte_bug(reference, correct):
    """Require the observed COG defect to equal GDAL's own Byte conversion."""
    names = {p.relative_to(reference) for p in reference.rglob("*.tif")}
    if names != {p.relative_to(correct) for p in correct.rglob("*.tif")}:
        raise AssertionError("COG reference defect has a different tile layout")
    changed = False
    for name in names:
        bad, good = gdal.Open(str(reference / name)), gdal.Open(str(correct / name))
        clipped = gdal.Translate("", good, format="MEM", outputType=gdal.GDT_Byte)
        if bad.ReadAsArray().shape != good.ReadAsArray().shape or not np.array_equal(
            bad.ReadAsArray(), clipped.ReadAsArray(), equal_nan=True
        ):
            raise AssertionError(
                "COG reference does not match the known Byte conversion defect"
            )
        changed |= not np.array_equal(
            bad.ReadAsArray(), good.ReadAsArray(), equal_nan=True
        )
        for band in range(1, bad.RasterCount + 1):
            if bad.GetRasterBand(band).DataType != good.GetRasterBand(band).DataType:
                raise AssertionError("COG reference defect changed the declared type")
    if not changed:
        raise AssertionError("COG reference bug workaround was unnecessary")


def compare_csv(reference, actual, offset=(0.0, 0.0)):
    refs = {p.relative_to(reference) for p in reference.rglob("*.csv")}
    outs = {p.relative_to(actual) for p in actual.rglob("*.csv")}
    if refs != outs:
        raise AssertionError("CSV layouts differ")
    for name in refs:
        rows = []
        for root in (reference, actual):
            with (root / name).open(newline="") as stream:
                rows.append(sorted(csv.reader(stream, delimiter=";")))
        if len(rows[0]) != len(rows[1]):
            raise AssertionError(f"{name}: CSV row counts differ")
        for a, b in zip(*rows, strict=True):
            if len(a) != 5 or len(b) != 5 or a[0] != b[0]:
                raise AssertionError(f"{name}: CSV filenames/columns differ")
            expected = np.asarray(a[1:], dtype=float) - [
                offset[0],
                offset[0],
                offset[1],
                offset[1],
            ]
            if not np.allclose(
                expected, np.asarray(b[1:], dtype=float), atol=1.00001e-6, rtol=0
            ):
                raise AssertionError(
                    f"{name}: CSV extents differ beyond six-decimal serialization"
                )


def compare_indexes(reference, actual, offset=(0.0, 0.0)):
    refs = {p.relative_to(reference) for p in reference.rglob("*.shp")}
    outs = {p.relative_to(actual) for p in actual.rglob("*.shp")}
    if refs != outs:
        raise AssertionError("index layouts differ")
    for name in refs:
        datasets = [ogr.Open(str(root / name)) for root in (reference, actual)]
        a, b = [ds.GetLayer(0) for ds in datasets]
        if not a.GetSpatialRef().IsSame(b.GetSpatialRef()):
            raise AssertionError(f"{name}: index CRS differs")
        features = []
        for layer in (a, b):
            features.append(
                {f.GetField("location"): f.GetGeometryRef().Clone() for f in layer}
            )
        if features[0].keys() != features[1].keys():
            raise AssertionError(f"{name}: indexed filenames differ")
        for key, geom in features[0].items():
            other = features[1][key]
            if (
                geom.GetGeometryName() != "POLYGON"
                or other.GetGeometryName() != "POLYGON"
            ):
                raise AssertionError(f"{name}: index is not polygonal")
            if geom.GetGeometryCount() != 1 or other.GetGeometryCount() != 1:
                raise AssertionError(f"{name}: unexpected index rings")
            points = np.asarray(geom.GetGeometryRef(0).GetPoints())[:, :2] - offset
            expected = np.asarray(other.GetGeometryRef(0).GetPoints())[:, :2]
            # Ring start and orientation are serialization choices.
            if points.shape != expected.shape or not np.allclose(
                sorted(map(tuple, points)),
                sorted(map(tuple, expected)),
                atol=5.0001e-7,
                rtol=0,
            ):
                raise AssertionError(f"{name}: footprint differs for {key}")


def compare_band_properties(reference, actual):
    def properties(value):
        return (
            value.GetScale(),
            value.GetOffset(),
            value.GetUnitType(),
            value.GetDescription(),
            value.GetColorInterpretation(),
            value.GetMetadata(),
        )

    for path in reference.rglob("*.tif"):
        relative = path.relative_to(reference)
        a, b = gdal.Open(str(path)), gdal.Open(str(actual / relative))
        for band in range(1, a.RasterCount + 1):
            x, y = a.GetRasterBand(band), b.GetRasterBand(band)

            if properties(x) != properties(y):
                raise AssertionError(
                    f"{relative}: band properties/default items differ"
                )


def remote_resume_probe(data, root, terrio):
    """Count original-source HTTP bytes while rebuilding from a stored parent."""
    cog = root / "remote-dem.tif"
    ds = gdal.Translate(
        str(cog),
        str(data / "usgs_dem.tif"),
        format="COG",
        width=2048,
        height=2048,
        creationOptions=["BLOCKSIZE=128", "COMPRESS=DEFLATE", "OVERVIEWS=NONE"],
    )
    size = cog.stat().st_size
    output = root / "remote-resume"
    output.mkdir()
    parent = output / "probe_1_1.tif"
    saved = gdal.GetDriverByName("GTiff").Create(
        str(parent),
        2048,
        2048,
        1,
        gdal.GDT_Float32,
        options=["TILED=YES", "COMPRESS=LZW"],
    )
    saved.SetGeoTransform(ds.GetGeoTransform())
    saved.SetProjection(ds.GetProjection())
    saved.GetRasterBand(1).Fill(7)
    nodata = ds.GetRasterBand(1).GetNoDataValue()
    if nodata is not None:
        saved.GetRasterBand(1).SetNoDataValue(nodata)
    saved.Close()
    ds.Close()
    counts = {"bytes": 0, "requests": 0}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_HEAD(self):
            self.send_response(200)
            self.send_header("Content-Length", str(size))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()

        def do_GET(self):
            match = re.fullmatch(r"bytes=(\d+)-(\d*)", self.headers.get("Range", ""))
            start, end = (
                (0, size - 1)
                if match is None
                else (
                    int(match[1]),
                    min(int(match[2]) if match[2] else size - 1, size - 1),
                )
            )
            with cog.open("rb") as stream:
                stream.seek(start)
                payload = stream.read(end - start + 1)
            self.send_response(206 if match else 200)
            self.send_header("Content-Length", str(len(payload)))
            if match:
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.end_headers()
            counts["bytes"] += len(payload)
            counts["requests"] += 1
            self.wfile.write(payload)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            report = json.loads(
                run(
                    [
                        terrio,
                        "retile",
                        f"http://127.0.0.1:{server.server_port}/dem.tif",
                        "-o",
                        output,
                        "--tile-size",
                        "2048",
                        "2048",
                        "--levels",
                        "1",
                        "--prefix",
                        "probe",
                        "--resume",
                    ]
                )
            )
        finally:
            server.shutdown()
            worker.join()
    if (report["written"], report["resumed"]) != (1, 1):
        raise AssertionError("resume did not reuse the single existing parent")
    child = gdal.Open(str(output / "1/probe_1_1.tif"))
    if child.ReadAsArray().shape != (1024, 1024) or not np.all(
        child.ReadAsArray() == 7
    ):
        raise AssertionError("resumed child did not use stored parent samples")
    if not 0 < counts["bytes"] <= 65536 or counts["bytes"] >= size / 4:
        raise AssertionError(
            f"resume source I/O is not metadata-only: {counts} of {size} bytes"
        )
    return f"one local parent supplies the missing pyramid child; original {size}-byte COG transfers only {counts['bytes']} bytes in {counts['requests']} GETs"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--keep", type=Path, help="retain outputs in a new directory")
    args = parser.parse_args()
    results = []
    if args.keep is not None:
        args.keep.mkdir(parents=True, exist_ok=False)
    context = (
        nullcontext(str(args.keep))
        if args.keep is not None
        else tempfile.TemporaryDirectory(prefix="terrio-retile-")
    )
    with context as directory:
        root = Path(directory)
        sources = {}
        for kind, filename, bands in [
            ("landsat", "landsat.tif", [1, 2, 3]),
            ("dem", "usgs_dem.tif", [1]),
        ]:
            path = root / f"{kind}.tif"
            ds = gdal.Translate(
                str(path),
                str(args.data / filename),
                srcWin=[600, 500, 513, 299],
                bandList=bands,
            )
            band = ds.GetRasterBand(1)
            band.SetScale(2)
            band.SetOffset(10)
            band.SetUnitType("m")
            band.SetDescription("measurement")
            band.SetMetadataItem("SOURCE", "sensor")
            band.ComputeStatistics(False)
            band = None
            ds.Close()
            sources[kind] = path
        for kind, source in sources.items():
            for layout in [
                "flat",
                "overlap",
                "rows",
                "pyramid-only",
                "index",
                "bilinear",
                "cubic",
                "cubicspline",
                "lanczos",
                "deep",
                "deep-overlap",
                "pyramid-only-deep",
                "cog",
                "byte",
            ]:
                name = f"{kind}-{layout}"
                reference, actual = root / f"{name}-gdal", root / f"{name}-native"
                reference.mkdir()
                levels = (
                    "3"
                    if layout
                    in {
                        "deep",
                        "deep-overlap",
                        "pyramid-only-deep",
                        "bilinear",
                        "cubic",
                        "cubicspline",
                        "lanczos",
                    }
                    else "1"
                )
                gd = ["-ps", "128", "96", "-levels", levels, "-csv", "tiles.csv"]
                native = [
                    "--tile-size",
                    "128",
                    "96",
                    "--levels",
                    levels,
                    "--csv",
                    "tiles.csv",
                ]
                if layout in {"overlap", "deep-overlap"}:
                    gd += ["-overlap", "16"]
                    native += ["--overlap", "16"]
                elif layout == "rows":
                    gd += ["-useDirForEachRow"]
                    native += ["--row-directories"]
                elif layout in {"pyramid-only", "pyramid-only-deep"}:
                    gd += ["-pyramidOnly"]
                    native += ["--pyramid-only"]
                elif layout == "index":
                    gd += ["-tileIndex", "tiles.shp"]
                    native += ["--tile-index", "tiles.shp"]
                elif layout == "cog":
                    gd += ["-of", "COG"]
                    native += ["--format", "COG"]
                elif layout == "byte":
                    gd += ["-ot", "Byte"]
                    native += ["--output-type", "Byte"]
                elif layout in {"bilinear", "cubic", "cubicspline", "lanczos"}:
                    gd += ["-r", layout]
                    native += ["--resampling", layout]
                try:
                    run(
                        [
                            sys.executable,
                            "-m",
                            "osgeo_utils.gdal_retile",
                            "-targetDir",
                            reference,
                            *gd,
                            source,
                        ]
                    )
                    run([args.terrio, "retile", source, "-o", actual, *native])
                    policy = ""
                    if kind == "dem" and layout == "cog":
                        # gdal_retile's COG path in GDAL 3.13.3 saturates
                        # Float32 samples through Byte while declaring Float32.
                        # Verify that precise failure before substituting its
                        # lossless GTiff retile as the sample oracle.
                        correct = root / f"{name}-gdal-gtiff"
                        correct.mkdir()
                        run(
                            [
                                sys.executable,
                                "-m",
                                "osgeo_utils.gdal_retile",
                                "-targetDir",
                                correct,
                                "-ps",
                                "128",
                                "96",
                                "-levels",
                                levels,
                                "-csv",
                                "tiles.csv",
                                source,
                            ]
                        )
                        verify_cog_reference_byte_bug(reference, correct)
                        reference = correct
                        policy = "; GDAL COG Byte-conversion defect independently verified; samples compared against GDAL GTiff retile"
                    if layout == "cog":
                        for tile in actual.rglob("*.tif"):
                            run(
                                [
                                    args.terrio,
                                    "validate-cog",
                                    tile,
                                    "--profile",
                                    "gdal",
                                    "--full-check",
                                    "yes",
                                ]
                            )
                    source_gt = gdal.Open(str(source)).GetGeoTransform()
                    # GDAL 3.13.3 builds its input index using six-decimal WKT,
                    # translating the whole output grid by this known amount.
                    # Native retile keeps the original source grid. Normalize only
                    # that independently derived offset, never pixels or scales.
                    offset = tuple(
                        float(f"{source_gt[i]:.6f}") - source_gt[i] for i in (0, 3)
                    )
                    detail, roundoff = compare_samples(
                        reference,
                        actual,
                        offset,
                        allow_roundoff=kind == "dem" and layout == "bilinear",
                    )
                    compare_band_properties(reference, actual)
                    compare_csv(reference, actual, offset)
                    compare_indexes(reference, actual, offset)
                    translated = any(abs(value) > 1e-8 for value in offset)
                    if translated:
                        detail += f"; GDAL six-decimal index shifts its origin by {offset} CRS units; native origin retains source precision"

                    results.append(
                        {
                            "name": name,
                            "status": "tolerance"
                            if translated or roundoff or policy
                            else "identical",
                            "detail": detail
                            + "; band properties, CSV and index footprints match"
                            + policy,
                        }
                    )
                except (AssertionError, RuntimeError) as error:
                    results.append(
                        {"name": name, "status": "differs", "detail": str(error)}
                    )
        try:
            results.append(
                {
                    "name": "remote-resume-parent-I/O",
                    "status": "identical",
                    "detail": remote_resume_probe(args.data, root, args.terrio),
                }
            )
        except (AssertionError, RuntimeError) as error:
            results.append(
                {
                    "name": "remote-resume-parent-I/O",
                    "status": "differs",
                    "detail": str(error),
                }
            )
    print(json.dumps(results, indent=None if args.json else 2))
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
