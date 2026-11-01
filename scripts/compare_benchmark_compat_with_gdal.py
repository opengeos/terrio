#!/usr/bin/env python3
"""Compare #253 compatibility profiles and codec contracts using independent GDAL reads."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

import numpy as np
from osgeo import gdal, ogr

from benchmark_validation import compare_rasters, compare_vectors

gdal.UseExceptions()
ogr.UseExceptions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument(
        "--terrio", default=os.environ.get("TERRIO_BIN", "target/release/terrio")
    )
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    records = []
    version = "3.13" if int(gdal.VersionInfo("VERSION_NUM")) >= 3130000 else "3.12"
    with tempfile.TemporaryDirectory(prefix="terrio-benchmark-compat-") as folder:
        tmp = Path(folder)

        def run(name, work):
            record = {
                "name": name,
                "same_size": True,
                "same_type": True,
                "same_crs": True,
                "same_nodata": True,
                "same_metadata": True,
                "differing": 0,
                "exact": 1.0,
            }
            try:
                work()
            except (
                RuntimeError,
                ValueError,
                AssertionError,
                subprocess.CalledProcessError,
                OSError,
            ) as error:
                record["error"] = str(error)
                if isinstance(error, subprocess.CalledProcessError):
                    record["error"] += ": " + error.stderr
            records.append(record)

        def command(args):
            subprocess.run(
                list(map(str, args)), check=True, capture_output=True, text=True
            )

        def exact(a, b):
            differences = compare_rasters(gdal.Open(str(a)), gdal.Open(str(b)))
            if differences:
                raise AssertionError("; ".join(differences))

        def cog(source, name, method="average"):
            actual, reference = tmp / f"{name}-t.tif", tmp / f"{name}-g.tif"
            command(
                [
                    args.terrio,
                    "convert",
                    source,
                    actual,
                    "-f",
                    "COG",
                    "--compress",
                    "DEFLATE",
                    "--co",
                    f"RESAMPLING={method}",
                    "--tile-size",
                    128,
                    "--co",
                    "COG_OVERVIEW_POLICY=GDAL_3_13",
                ]
            )
            command(
                [
                    "gdal_translate",
                    "-q",
                    "-of",
                    "COG",
                    "-co",
                    "COMPRESS=DEFLATE",
                    "-co",
                    "BLOCKSIZE=128",
                    "-co",
                    "OVERVIEWS=IGNORE_EXISTING",
                    "-co",
                    f"RESAMPLING={method.upper()}",
                    source,
                    reference,
                ]
            )
            exact(actual, reference)

        source = gdal.Open(str(args.data / "landsat.tif"))
        optical = tmp / "optical.tif"
        width, height = min(source.RasterXSize, 1001), min(source.RasterYSize, 777)
        gdal.Translate(
            str(optical),
            source,
            srcWin=[0, 0, width, height],
            bandList=[1, 2, 3],
            outputType=gdal.GDT_Byte,
        )
        source = None
        if version == "3.13":
            run(
                "COG USGS recursive overviews every pixel",
                lambda: cog(args.data / "usgs_dem.tif", "dem"),
            )
            run(
                "COG optical complete odd-sized pyramid every pixel",
                lambda: cog(optical, "optical"),
            )

        if version == "3.13":
            run("COG optical nearest complete pyramid every pixel", lambda: cog(optical, "optical-near", "nearest"))

        def metadata_entities():
            source, actual = tmp / "metadata-source.tif", tmp / "metadata-output.tif"
            dataset = gdal.Translate(str(source), str(optical), srcWin=[0, 0, 16, 16])
            dataset.SetMetadataItem("title", "a<b &lt;literal")
            dataset = None
            command([args.terrio, "convert", source, actual])
            output = gdal.Open(str(actual))
            if output.GetMetadataItem("title") != "a<b &lt;literal":
                raise AssertionError("GDAL metadata entity text changed")
            exact(actual, source)

        run("GDAL-authored dataset metadata preserves entity text", metadata_entities)

        dem = tmp / "dem-clip.tif"
        gdal.Translate(
            str(dem), str(args.data / "usgs_dem.tif"), srcWin=[0, 0, 512, 512]
        )
        for mode in ["slope", "hillshade"]:
            for algorithm in ["Horn", "ZevenbergenThorne"]:

                def terrain(mode=mode, algorithm=algorithm):
                    actual, reference = tmp / "terrain-t.tif", tmp / "terrain-g.tif"
                    command(
                        [
                            args.terrio,
                            "dem",
                            mode,
                            dem,
                            actual,
                            "--kernel-version",
                            version,
                            "--alg",
                            algorithm,
                        ]
                    )
                    command(["gdaldem", mode, "-q", "-alg", algorithm, dem, reference])
                    exact(actual, reference)

                run(f"Terrain {mode} {algorithm} GDAL {version} every pixel", terrain)

        for bands in [3, 4]:
            webp_source = tmp / f"webp-{bands}.tif"
            original = gdal.Open(str(optical))
            subset = gdal.Translate("", original, format="MEM", srcWin=[0, 0, 128, 128])
            if bands == 4:
                original = gdal.GetDriverByName("GTiff").Create(
                    str(webp_source), 128, 128, 4, gdal.GDT_Byte
                )
                original.SetGeoTransform(subset.GetGeoTransform())
                original.SetProjection(subset.GetProjection())
                for i in range(3):
                    original.GetRasterBand(i + 1).WriteArray(
                        subset.GetRasterBand(i + 1).ReadAsArray()
                    )
                original.GetRasterBand(4).WriteArray(
                    (np.indices((128, 128))[1] % 3 * 127).astype(np.uint8)
                )
                original.GetRasterBand(4).SetColorInterpretation(gdal.GCI_AlphaBand)
                original = None
            else:
                gdal.Translate(str(webp_source), subset)

            def lossless(source=webp_source):
                actual = tmp / "lossless.tif"
                command(
                    [
                        args.terrio,
                        "convert",
                        source,
                        actual,
                        "--compress",
                        "WEBP",
                        "--co",
                        "WEBP_LOSSLESS=YES",
                    ]
                )
                exact(actual, source)

            run(f"WebP lossless {bands} bands source pixels and masks", lossless)

        def quality():
            outputs = []
            for value in [10, 95]:
                actual = tmp / f"quality-{value}.tif"
                command(
                    [
                        args.terrio,
                        "convert",
                        tmp / "webp-3.tif",
                        actual,
                        "--compress",
                        "WEBP",
                        "--co",
                        f"WEBP_LEVEL={value}",
                    ]
                )
                dataset = gdal.Open(str(actual))
                if (dataset.RasterCount, dataset.RasterXSize, dataset.RasterYSize) != (
                    3,
                    128,
                    128,
                ):
                    raise AssertionError("WebP sample layout changed")
                outputs.append((actual.read_bytes(), dataset.ReadAsArray()))
            if outputs[0][0] == outputs[1][0] or np.array_equal(
                outputs[0][1], outputs[1][1]
            ):
                raise AssertionError("requested WebP quality settings were ignored")

        run("WebP quality settings change valid independently decoded output", quality)

        def lossy_alpha():
            source, actual = tmp / "webp-4.tif", tmp / "lossy-alpha.tif"
            command(
                [
                    args.terrio,
                    "convert",
                    source,
                    actual,
                    "--compress",
                    "WEBP",
                    "--co",
                    "WEBP_LEVEL=35",
                ]
            )
            expected, output = [gdal.Open(str(path)) for path in (source, actual)]
            if not np.array_equal(
                expected.GetRasterBand(4).ReadAsArray(),
                output.GetRasterBand(4).ReadAsArray(),
            ):
                raise AssertionError("lossy RGB compression changed alpha")
            if not np.array_equal(
                expected.GetRasterBand(1).GetMaskBand().ReadAsArray(),
                output.GetRasterBand(1).GetMaskBand().ReadAsArray(),
            ):
                raise AssertionError("lossy WebP changed alpha masks")

        run("WebP lossy RGB preserves exact independent alpha and masks", lossy_alpha)

        for dtype, label in [(gdal.GDT_Byte, "Byte"), (gdal.GDT_UInt16, "UInt16")]:
            for bands in [1, 3]:
                for tiled in [False, True]:

                    def jxl(dtype=dtype, bands=bands, tiled=tiled):
                        source = tmp / "jxl-source.tif"
                        ds = gdal.Open(str(optical))
                        gdal.Translate(
                            str(source),
                            ds,
                            srcWin=[0, 0, 79, 53],
                            bandList=list(range(1, bands + 1)),
                            outputType=dtype,
                            scaleParams=[[0, 255, 0, 65535]]
                            if dtype == gdal.GDT_UInt16
                            else None,
                        )
                        actual = tmp / "jxl.tif"
                        options = ["--tile-size", 32] if tiled else []
                        command(
                            [
                                args.terrio,
                                "convert",
                                source,
                                actual,
                                "--compress",
                                "JXL",
                                *options,
                            ]
                        )
                        dataset = gdal.Open(str(actual))
                        block = dataset.GetRasterBand(1).GetBlockSize()
                        if (tiled and block != [32, 32]) or (not tiled and block[0] != dataset.RasterXSize):
                            raise AssertionError(f"JPEG XL layout differs: tiled={tiled}, block={block}")
                        exact(actual, source)

                    run(
                        f"JPEG-XL {label} {bands} bands {'tiles' if tiled else 'strips'} independent roundtrip",
                        jxl,
                    )

        warp_source = args.data / "usgs_dem.tif"
        counties = args.data / "UScounties.gpkg"

        def warp_exact():
            actual, reference = tmp / "warp-terrio.tif", tmp / "warp-gdal.tif"
            command(
                [
                    args.terrio,
                    "warp",
                    "--kernel-version",
                    version,
                    warp_source,
                    actual,
                    "--t-srs",
                    "EPSG:26913",
                    "-r",
                    "bilinear",
                ]
            )
            command(["gdalwarp", "-q", "-t_srs", "EPSG:26913", "-r", "bilinear", warp_source, reference])
            exact(actual, reference)

        def counties_exact():
            actual, reference = tmp / "counties-terrio.gpkg", tmp / "counties-gdal.gpkg"
            where = "STATE_FIPS = '47'"
            command(
                [
                    args.terrio,
                    "convert",
                    counties,
                    actual,
                    "--where",
                    where,
                    "--t-srs",
                    "EPSG:5070",
                ]
            )
            command(["ogr2ogr", "-where", where, "-t_srs", "EPSG:5070", reference, counties])
            differences = compare_vectors(ogr.Open(str(actual)), ogr.Open(str(reference)))
            if differences:
                raise AssertionError("; ".join(differences))

        if version == "3.13":
            run("Bilinear NAD83 to UTM warp bit-identical", warp_exact)
            run("Counties to Conus Albers coordinates bit-identical", counties_exact)

    if args.json:
        print(json.dumps(records, indent=2))
    else:
        for record in records:
            print(
                "FAIL" if "error" in record else "OK",
                record["name"],
                record.get("error", ""),
            )
    return int(any("error" in record for record in records))


if __name__ == "__main__":
    raise SystemExit(main())
