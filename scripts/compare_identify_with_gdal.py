#!/usr/bin/env python3
"""Compare bounded dataset identification with GDAL 3.13 on real data."""

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal

gdal.UseExceptions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []

    def compare(path, deep=False, label=None):
        name = label or path.name
        try:
            reference = gdal.IdentifyDriverEx(str(path))
            expected = reference.ShortName if reference else None
            command = [
                args.terrio,
                "identify",
                "--json",
                "--report-failures",
                str(path),
            ]
            if deep:
                command.append("--deep")
            process = subprocess.run(
                command, capture_output=True, text=True, check=True
            )
            records = json.loads(process.stdout)
            if len(records) != 1:
                raise AssertionError(f"expected one record, got {len(records)}")
            actual = records[0].get("driver")
            families = {
                "KML": "KML",
                "LIBKML": "KML",
                "HDF5Image": "HDF5",
                "HDF5": "HDF5",
                "JP2": "JP2",
                "JP2OpenJPEG": "JP2",
                "JPEG2000": "JP2",
            }
            if (
                expected is None
                and actual == "Parquet"
                and gdal.GetDriverByName("Parquet") is None
            ):
                results.append(
                    {
                        "name": name,
                        "status": "not compared",
                        "detail": "GDAL build lacks the Parquet driver; Terrio recognizes the real GeoParquet file",
                    }
                )
                return
            status = (
                "identical"
                if actual == expected
                else "tolerance"
                if actual
                and expected
                and families.get(actual, actual) == families.get(expected, expected)
                else "differs"
            )
            results.append(
                {
                    "name": name + (" deep" if deep else ""),
                    "status": status,
                    "detail": ""
                    if status == "identical"
                    else f"GDAL={expected}, Terrio={actual}",
                    "reference_driver": expected,
                    "driver": actual,
                }
            )
        except Exception as error:
            results.append({"name": name, "status": "differs", "detail": str(error)})

    with tempfile.TemporaryDirectory(prefix="terrio-identify-") as temporary:
        root = Path(temporary)
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
        for pattern in ["GATMO*.h5", "S5P*.nc", "*.jp2"]:
            path = next(args.data.glob(pattern), None)
            if path:
                compare(path)
        rgb = root / "rgb.tif"
        gdal.Translate(
            str(rgb),
            str(args.data / "landsat.tif"),
            srcWin=[600, 500, 63, 47],
            bandList=[1, 2, 3],
            outputType=gdal.GDT_Byte,
        )
        gray = root / "dem.tif"
        dataset = gdal.Translate(
            str(gray),
            str(args.data / "USGS_1_n40w106.tif"),
            srcWin=[1000, 1000, 32, 24],
            outputType=gdal.GDT_Float32,
        )
        transform = list(dataset.GetGeoTransform())
        transform[5] = -transform[
            1
        ]  # SAGA requires exactly square cells. Values are unchanged.
        dataset.SetGeoTransform(transform)
        dataset = None
        for driver, extension, source, deep in [
            ("GTiff", "tif", rgb, False),
            ("PNG", "png", rgb, False),
            ("JPEG", "jpg", rgb, False),
            ("JP2OpenJPEG", "jp2", rgb, False),
            ("HFA", "img", rgb, False),
            ("ENVI", "dat", gray, True),
            ("EHdr", "bil", gray, True),
            ("ERS", "ers", gray, False),
            ("GSBG", "grd", gray, False),
            ("SAGA", "sdat", gray, True),
            ("RST", "rst", gray, True),
            ("AAIGrid", "asc", gray, False),
        ]:
            if gdal.GetDriverByName(driver) is None:
                results.append(
                    {
                        "name": driver,
                        "status": "not compared",
                        "detail": "GDAL driver unavailable",
                    }
                )
                continue
            path = root / (driver + "." + extension)
            gdal.Translate(str(path), str(source), format=driver)
            compare(path, deep, label="derived " + driver)
        renamed = root / "renamed.bin"
        renamed.write_bytes(rgb.read_bytes())
        compare(renamed, label="GeoTIFF without extension")
        unknown = root / "invalid.tif"
        unknown.write_text("not a raster or vector dataset")
        compare(unknown, label="unrecognized input with misleading extension")
    print(
        json.dumps(results, indent=2)
        if args.json
        else "\n".join(str(result) for result in results)
    )
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
