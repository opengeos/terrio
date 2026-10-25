#!/usr/bin/env python3
"""Regress #253's color interpretation and declared integer schema on real fixtures."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from osgeo import gdal, ogr, osr

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
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-benchmark-metadata-") as directory:
        tmp = Path(directory)
        tiles = [
            args.data / f"USGS_1_{name}.tif"
            for name in ("n40w106", "n40w107", "n41w106", "n41w107")
        ]
        gcp_source = tmp / "gcp-source.tif"
        source = gdal.Open(str(args.data / "srtm90.tif"))
        gt = source.GetGeoTransform()
        points = [
            gdal.GCP(
                gt[0] + (x + 0.5) * gt[1],
                gt[3] + (y + 0.5) * gt[5],
                0,
                x + 0.5,
                y + 0.5,
            )
            for y in range(39)
            for x in range(41)
        ]
        gdal.Translate(
            str(gcp_source),
            source,
            srcWin=[0, 0, 64, 64],
            GCPs=points,
            outputSRS=source.GetProjection(),
        )
        source = None
        cases = [
            (
                "COG GCP-only georeferencing and every pixel",
                "gcps",
                ["convert", gcp_source, tmp / "t-gcp.tif", "-f", "COG"],
                ["gdal_translate", "-q", "-of", "COG", gcp_source, tmp / "g-gcp.tif"],
            ),
            (
                "Zarr SRTM color and every pixel",
                "raster",
                ["convert", args.data / "srtm90.tif", tmp / "t.zarr"],
                [
                    "gdal_translate",
                    "-q",
                    "-of",
                    "Zarr",
                    args.data / "srtm90.tif",
                    tmp / "g.zarr",
                ],
            ),
            # gdalbuildvrt matches Terrio's supported base-resolution mosaic. GDAL 3.13's
            # GDAL 3.13 synthesizes an OverviewList unless explicitly disabled.
            (
                "VRT USGS color and every pixel",
                "raster",
                ["mosaic", *tiles, tmp / "t.vrt"],
                [
                    "gdalbuildvrt",
                    "--config",
                    "VRT_VIRTUAL_OVERVIEWS",
                    "NO",
                    "-q",
                    "-resolution",
                    "average",
                    tmp / "g.vrt",
                    *tiles,
                ],
            ),
            (
                "GeoPackage places declared schema and every feature",
                "vector",
                [
                    "convert",
                    args.data / "ne_places/ne_10m_populated_places_simple.shp",
                    tmp / "t.gpkg",
                ],
                [
                    "ogr2ogr",
                    tmp / "g.gpkg",
                    args.data / "ne_places/ne_10m_populated_places_simple.shp",
                ],
            ),
        ]
        for name, kind, terrio, reference in cases:
            record = {
                "name": name,
                "same_size": True,
                "same_type": True,
                "same_crs": True,
                "same_metadata": True,
                "differing": 0,
                "exact": 1.0,
            }
            try:
                for command in ([args.terrio, *terrio], reference):
                    subprocess.run(
                        list(map(str, command)),
                        check=True,
                        capture_output=True,
                        text=True,
                    )
                if kind == "gcps":
                    tpath, gpath = tmp / "t-gcp.tif", tmp / "g-gcp.tif"
                    datasets = [gdal.Open(str(path)) for path in (tpath, gpath)]
                    differences = compare_rasters(*datasets)
                    for dataset in datasets:
                        if (
                            dataset.GetGCPCount() != 1599
                            or not dataset.GetGCPProjection()
                            or dataset.GetGeoTransform(can_return_null=True) is not None
                        ):
                            differences.append(
                                "GCPs lost or replaced by an affine transform"
                            )
                    if not osr.SpatialReference(datasets[0].GetGCPProjection()).IsSame(
                        osr.SpatialReference(datasets[1].GetGCPProjection())
                    ):
                        differences.append("GCP CRS differs")

                    def gcps(ds):
                        return [
                            (g.GCPPixel, g.GCPLine, g.GCPX, g.GCPY, g.GCPZ)
                            for g in ds.GetGCPs()
                        ]

                    if gcps(datasets[0]) != gcps(datasets[1]):
                        differences.append("GCP coordinates differ")
                    if differences:
                        record["error"] = "; ".join(differences)
                    results.append(record)
                    continue
                tpath, gpath = terrio[-1], reference[-1]
                if kind == "vector":
                    gpath = tmp / "g.gpkg"
                    differences = compare_vectors(
                        ogr.Open(str(tpath)), ogr.Open(str(gpath))
                    )
                else:
                    gpath = tmp / ("g.vrt" if str(tpath).endswith(".vrt") else "g.zarr")
                    differences = compare_rasters(
                        gdal.Open(str(tpath)), gdal.Open(str(gpath))
                    )
                if differences:
                    record["error"] = "; ".join(differences)
            except (RuntimeError, subprocess.CalledProcessError) as error:
                record["error"] = str(error)
                if isinstance(error, subprocess.CalledProcessError):
                    record["error"] += ": " + error.stderr
            results.append(record)
    if args.json:
        print(json.dumps(results, indent=2))
    else:
        for record in results:
            print(
                ("FAIL" if "error" in record else "OK"),
                record["name"],
                record.get("error", ""),
            )
    return int(any("error" in record for record in results))


if __name__ == "__main__":
    raise SystemExit(main())
