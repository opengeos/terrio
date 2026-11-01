#!/usr/bin/env python3
"""Compare raster creation, edits, overviews and updates with GDAL on real DEM data."""

import argparse
import json
import math
import shutil
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal

gdal.UseExceptions()


def run(command):
    result = subprocess.run(
        [str(value) for value in command], capture_output=True, text=True, timeout=300
    )
    if result.returncode:
        raise RuntimeError(
            f"{command[0]} exited {result.returncode}: {result.stderr.strip()[-1000:]}"
        )
    return result


def same_number(a, b):
    return a == b or (
        a is not None and b is not None and math.isnan(a) and math.isnan(b)
    )


def compare(reference, actual):
    a, b = gdal.Open(str(reference)), gdal.Open(str(actual))
    if (a.RasterXSize, a.RasterYSize, a.RasterCount) != (
        b.RasterXSize,
        b.RasterYSize,
        b.RasterCount,
    ):
        raise AssertionError("raster size/band count differs")
    if a.GetGeoTransform() != b.GetGeoTransform():
        raise AssertionError("geotransform differs")
    sa, sb = a.GetSpatialRef(), b.GetSpatialRef()
    if (sa is None) != (sb is None) or (sa is not None and not sa.IsSame(sb)):
        raise AssertionError("CRS differs")
    exact, maximum = True, 0.0
    for index in range(1, a.RasterCount + 1):
        ba, bb = a.GetRasterBand(index), b.GetRasterBand(index)
        if ba.DataType != bb.DataType or not same_number(
            ba.GetNoDataValue(), bb.GetNoDataValue()
        ):
            raise AssertionError("band type/nodata differs")
        if ba.GetMaskFlags() != bb.GetMaskFlags():
            raise AssertionError("mask flags differ")
        if ba.GetOverviewCount() != bb.GetOverviewCount():
            raise AssertionError("overview count differs")
        for aa, ab in [(ba, bb)] + [
            (ba.GetOverview(i), bb.GetOverview(i)) for i in range(ba.GetOverviewCount())
        ]:
            va, vb = aa.ReadAsArray(), ab.ReadAsArray()
            if va.shape != vb.shape or not np.array_equal(
                aa.GetMaskBand().ReadAsArray(), ab.GetMaskBand().ReadAsArray()
            ):
                raise AssertionError("level size/mask differs")
            if not np.array_equal(va, vb, equal_nan=True):
                exact = False
                difference = np.abs(va.astype(float) - vb.astype(float))
                finite = difference[np.isfinite(difference)]
                maximum = max(maximum, float(finite.max(initial=0)))
                if not np.allclose(va, vb, rtol=2e-7, atol=1e-6, equal_nan=True):
                    raise AssertionError(
                        f"pixels differ: max absolute difference {maximum}"
                    )
    return (
        "identical" if exact else "tolerance"
    ), f"values, masks, type, nodata, CRS, transform and levels; max abs {maximum:g}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []

    def case(name, operation):
        try:
            status, detail = operation()
        except (RuntimeError, AssertionError, OSError) as error:
            status, detail = "differs", str(error)
        results.append({"name": name, "status": status, "detail": detail})
        if not args.json:
            print(f"{status}: {name}: {detail}")

    with tempfile.TemporaryDirectory(prefix="terrio-management-") as directory:
        work = Path(directory)
        sample = work / "real-dem.tif"
        source = args.data / "dem.tif"
        gdal.Translate(
            str(sample),
            str(source),
            srcWin=[0, 0, 128, 128],
            outputType=gdal.GDT_Float32,
        )
        for data_type in ["Byte", "Int16", "UInt16", "Int32", "Float32", "Float64"]:

            def creation(data_type=data_type):
                a, b = work / "create-gdal.tif", work / "create-terrio.tif"
                for path in [a, b]:
                    path.unlink(missing_ok=True)
                run(
                    [
                        "gdal_create",
                        "-outsize",
                        32,
                        24,
                        "-bands",
                        2,
                        "-ot",
                        data_type,
                        "-burn",
                        7,
                        "-a_srs",
                        "EPSG:3857",
                        "-a_ullr",
                        0,
                        24,
                        32,
                        0,
                        "-a_nodata",
                        255,
                        a,
                    ]
                )
                run(
                    [
                        args.terrio,
                        "create-raster",
                        b,
                        "--size",
                        "32,24",
                        "--band-count",
                        2,
                        "--ot",
                        data_type,
                        "--burn",
                        7,
                        "--crs",
                        "EPSG:3857",
                        "--bbox",
                        "0,0,32,24",
                        "--nodata",
                        255,
                    ]
                )
                return compare(a, b)

            case(f"create {data_type} two bands", creation)
        for external in [False, True]:
            for method in ["nearest", "average", "mode"]:

                def overviews(external=external, method=method):
                    a, b = (
                        work / f"ov-gdal-{external}-{method}.tif",
                        work / f"ov-terrio-{external}-{method}.tif",
                    )
                    shutil.copyfile(sample, a)
                    shutil.copyfile(sample, b)
                    run(
                        [
                            "gdaladdo",
                            *(["-ro"] if external else []),
                            "-r",
                            method,
                            a,
                            2,
                            4,
                        ]
                    )
                    run(
                        [
                            args.terrio,
                            "overviews",
                            b,
                            *(["--external"] if external else []),
                            "-r",
                            method,
                            2,
                            4,
                        ]
                    )
                    status, detail = compare(a, b)
                    run(["gdaladdo", *(["-ro"] if external else []), "-clean", a])
                    run(
                        [
                            args.terrio,
                            "overviews",
                            b,
                            *(["--external"] if external else []),
                            "--clean",
                        ]
                    )
                    compare(a, b)
                    return status, detail + "; delete leaves matching base raster"

                case(
                    f"{'external' if external else 'internal'} overviews {method} and delete",
                    overviews,
                )

        # Real Byte imagery exercises the histogram path and parallel row groups; the
        # existing DEM cases exercise reusable non-Byte counters. Mode must be exact.
        for data_type in ([gdal.GDT_Byte, gdal.GDT_UInt16, gdal.GDT_Float32]
                          if (args.data / "landsat.tif").is_file() else []):
            def mode_image(data_type=data_type):
                a, b = work / "mode-gdal.tif", work / "mode-terrio.tif"
                ds = gdal.Translate(str(a), str(args.data / "landsat.tif"),
                                    srcWin=[0, 0, 1023, 1021], outputType=data_type,
                                    creationOptions=["TILED=YES", "COMPRESS=DEFLATE"])
                ds = None
                shutil.copyfile(a, b)
                run(["gdaladdo", "-r", "mode", a, 2, 3, 8])
                run([args.terrio, "overviews", b, "-r", "mode", 2, 3, 8])
                status, detail = compare(a, b)
                if status != "identical":
                    raise AssertionError("mode pixels must match exactly: " + detail)
                return status, detail
            case(f"Landsat mode {gdal.GetDataTypeName(data_type)} uneven levels", mode_image)

        # A per-dataset internal mask in 64-pixel tiles, the smallest that GDAL's overviews
        # keep: the first overview's last block row has 36 of 64 rows, and the others fit
        # in a tile.
        masked = work / "masked.tif"
        gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "YES")
        try:
            gdal.Translate(
                str(masked),
                str(sample),
                srcWin=[0, 0, 128, 100],
                width=256,
                height=200,
                creationOptions=[
                    "TILED=YES",
                    "BLOCKXSIZE=64",
                    "BLOCKYSIZE=64",
                    "COMPRESS=DEFLATE",
                ],
            )
            dataset = gdal.Open(str(masked), gdal.GA_Update)
            dataset.CreateMaskBand(gdal.GMF_PER_DATASET)
            rows, cols = np.indices((200, 256))
            valid = (cols >= 25) & ((7 * cols + 3 * rows) % 13 != 0)
            dataset.GetRasterBand(1).GetMaskBand().WriteArray(
                valid.astype(np.uint8) * 255
            )
            transform = dataset.GetGeoTransform()
            dataset = None
        finally:
            gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", None)
        # The right half of the last 60 rows: the first overview's last block row.
        x0, x1 = transform[0] + 128 * transform[1], transform[0] + 256 * transform[1]
        y0, y1 = transform[3] + 200 * transform[5], transform[3] + 140 * transform[5]
        bottom = ",".join(
            repr(v) for v in [min(x0, x1), min(y0, y1), max(x0, x1), max(y0, y1)]
        )

        # terrio adds the overviews gdaladdo adds, or updates gdaladdo's: GDAL reads every
        # mask overview, in GDAL's blocks, and the masks are gdaladdo's.
        for name, from_gdal, options in [
            ("added", False, ["-r", "nearest", 2, 4, 8]),
            ("refreshed", True, ["--refresh"]),
            (
                "refreshed over the last block row",
                True,
                ["--refresh", "--bbox", bottom],
            ),
            ("added again", True, ["-r", "nearest", 2, 4, 8]),
        ]:

            def mask_overviews(name=name, from_gdal=from_gdal, options=options):
                slug = name.replace(" ", "-")
                a, b = (
                    work / f"masked-gdal-{slug}.tif",
                    work / f"masked-terrio-{slug}.tif",
                )
                shutil.copyfile(masked, a)
                run(["gdaladdo", "-r", "nearest", a, 2, 4, 8])
                shutil.copyfile(a if from_gdal else masked, b)
                run([args.terrio, "overviews", b, *options])
                status, detail = compare(a, b)
                # The datasets stay open while their bands are used.
                da, db = gdal.Open(str(a)), gdal.Open(str(b))
                ba, bb = da.GetRasterBand(1), db.GetRasterBand(1)
                for i in range(ba.GetOverviewCount()):
                    expected = ba.GetOverview(i).GetMaskBand().GetBlockSize()
                    actual = bb.GetOverview(i).GetMaskBand().GetBlockSize()
                    if actual != expected:
                        raise AssertionError(
                            f"mask overview {i} blocks {actual}, not {expected}"
                        )
                return status, detail + "; mask overview blocks"

            case(f"internal mask overviews {name}", mask_overviews)

        def metadata_edit():
            a, b = work / "edit-gdal.tif", work / "edit-terrio.tif"
            shutil.copyfile(sample, a)
            shutil.copyfile(sample, b)
            run(["gdal_edit.py", "-a_nodata", -999, "-mo", "SOURCE=A & <B>", a])
            run(
                [
                    args.terrio,
                    "edit-raster",
                    b,
                    "--nodata",
                    -999,
                    "--metadata",
                    "SOURCE=A & <B>",
                ]
            )
            assert (
                gdal.Open(str(a)).GetMetadataItem("SOURCE")
                == gdal.Open(str(b)).GetMetadataItem("SOURCE")
                == "A & <B>"
            )
            return compare(a, b)

        case("metadata XML escaping and nodata edit", metadata_edit)
        for method in ["nearest", "bilinear", "average"]:

            def update(method=method):
                a, b = (
                    work / f"update-gdal-{method}.tif",
                    work / f"update-terrio-{method}.tif",
                )
                shutil.copyfile(sample, a)
                shutil.copyfile(sample, b)
                part = work / f"part-{method}.tif"
                gdal.Translate(str(part), str(sample), srcWin=[32, 32, 32, 32])
                ds = gdal.Open(str(part), gdal.GA_Update)
                ds.GetRasterBand(1).Fill(42)
                ds = None
                run(["gdal", "raster", "update", "-r", method, part, a])
                run([args.terrio, "update-raster", "-r", method, part, b])
                return compare(a, b)

            case(f"real DEM subset update {method}", update)
        for internal in [False, True]:

            def masked_update(internal=internal):
                a, b = (
                    work / f"mask-gdal-{internal}.tif",
                    work / f"mask-terrio-{internal}.tif",
                )
                part = work / f"masked-source-{internal}.tif"
                gdal.Translate(str(part), str(sample), srcWin=[32, 32, 32, 32])
                dataset = gdal.Open(str(part), gdal.GA_Update)
                dataset.GetRasterBand(1).Fill(42)
                dataset = None
                gdal.SetConfigOption(
                    "GDAL_TIFF_INTERNAL_MASK", "YES" if internal else "NO"
                )
                try:
                    for path in [a, b]:
                        shutil.copyfile(sample, path)
                        dataset = gdal.Open(str(path), gdal.GA_Update)
                        dataset.CreateMaskBand(gdal.GMF_PER_DATASET)
                        dataset.GetRasterBand(1).GetMaskBand().Fill(0)
                        dataset = None
                finally:
                    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", None)
                run(["gdal", "raster", "update", part, a])
                run([args.terrio, "update-raster", part, b])
                return compare(a, b)

            case(
                f"{'internal' if internal else 'external'} destination mask retained",
                masked_update,
            )

        for offset in [0.25, 0.75]:

            def fractional_update(offset=offset):
                a, b = (
                    work / f"fractional-gdal-{offset}.tif",
                    work / f"fractional-terrio-{offset}.tif",
                )
                part = work / f"fractional-source-{offset}.tif"
                for path, size, transform, fill in [
                    (part, 4, [4 + offset, 1, 0, 8, 0, -1], 42),
                    (a, 12, [0, 1, 0, 12, 0, -1], 0),
                    (b, 12, [0, 1, 0, 12, 0, -1], 0),
                ]:
                    dataset = gdal.GetDriverByName("GTiff").Create(
                        str(path), size, size, 1, gdal.GDT_Float32
                    )
                    dataset.SetGeoTransform(transform)
                    dataset.GetRasterBand(1).Fill(fill)
                    dataset = None
                run(["gdal", "raster", "update", "-r", "average", part, a])
                run([args.terrio, "update-raster", "-r", "average", part, b])
                return compare(a, b)

            case(
                f"average update retains genuine {offset} fractional overlaps",
                fractional_update,
            )

        def integer_bounds():
            a, b = work / "bounds-gdal.tif", work / "bounds-terrio.tif"
            part = work / "bounds-source.tif"
            dataset = gdal.GetDriverByName("GTiff").Create(
                str(part), 4, 1, 1, gdal.GDT_Float32
            )
            dataset.SetGeoTransform([0, 1, 0, 1, 0, -1])
            dataset.GetRasterBand(1).WriteArray(
                np.array([[400, -20, 255, 0.49]], dtype="float32")
            )
            dataset = None
            for path in [a, b]:
                dataset = gdal.GetDriverByName("GTiff").Create(
                    str(path), 4, 1, 1, gdal.GDT_Byte
                )
                dataset.SetGeoTransform([0, 1, 0, 1, 0, -1])
                dataset.GetRasterBand(1).SetNoDataValue(255)
                dataset.GetRasterBand(1).Fill(7)
                dataset = None
            run(["gdal", "raster", "update", part, a])
            run([args.terrio, "update-raster", part, b])
            return compare(a, b)

        case("integer update bounds and nodata collision", integer_bounds)
    if args.json:
        print(json.dumps(results, indent=2))
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
