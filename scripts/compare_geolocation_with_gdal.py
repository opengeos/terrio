#!/usr/bin/env python3
"""Compare real Sentinel-5P swaths and HDF-EOS dimension maps with GDAL -geoloc.

The HDF-EOS cases transplant an actual Sentinel-5P coordinate/data window into the
bundled HDF-EOS dimension-map fixture. No coordinates or pixel values are invented.
All comparisons use the same explicit output grid and exact coordinate transforms.
"""

import argparse
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

import h5py
import numpy as np
from osgeo import gdal

from compare_ops_with_gdal import compare

gdal.UseExceptions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    data = args.data.resolve()
    satellite = next(data.glob("S5P*.nc"))
    source = f'NETCDF:"{satellite}":/PRODUCT/qa_value'
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-geolocation-") as tmp:
        tmp = Path(tmp)

        def check(name, src, bounds=None):
            reference = str(tmp / "gdal.tif")
            actual = str(tmp / "terrio.tif")
            opts = dict(geoloc=True, dstSRS="EPSG:4326", width=160, height=100,
                        dstNodata=-9999, outputType=gdal.GDT_Float32, errorThreshold=0)
            if bounds is not None:
                opts["outputBounds"] = bounds
            ref = gdal.Warp(reference, src, **opts)
            gt = ref.GetGeoTransform()
            bounds = (gt[0], gt[3] + ref.RasterYSize * gt[5],
                      gt[0] + ref.RasterXSize * gt[1], gt[3])
            ref = None
            proc = subprocess.run([args.terrio, "warp", src, actual, "--geoloc", "--t-srs", "EPSG:4326",
                                   "--te", *map(str, bounds), "--ts", "160", "100", "--et", "0",
                                   "--dstnodata", "-9999", "--ot", "Float32"], capture_output=True, text=True)
            if proc.returncode:
                results.append({"name": name, "error": proc.stderr})
                return
            result = compare(reference, actual, 0)
            result["name"] = name
            results.append(result)

        # Both sides of the dateline use canonical geographic longitudes, as GDAL does.
        for name, bounds in [("S5P Antarctic", (-140, -85, -120, -75)),
                             ("S5P equatorial", (-180, -10, -165, 10)),
                             ("S5P antimeridian east", (175, 20, 180, 40)),
                             ("S5P antimeridian west", (-180, 20, -175, 40))]:
            check(name, source, bounds)
        arrays = {var: gdal.Open(f'NETCDF:"{satellite}":/PRODUCT/{var}').ReadAsArray()[3340:3380, 30:50]
                  for var in ["longitude", "latitude", "qa_value"]}
        fixture = tmp / "real-swath.h5"
        shutil.copyfile(data / "gdal_hdf5/hdfeos_sample_swath.h5", fixture)
        with h5py.File(fixture, "r+") as f:
            swath = f["HDFEOS/SWATHS/Swath1"]
            swath["Geolocation Fields/Longitude"][:] = arrays["longitude"][::2, ::2]
            swath["Geolocation Fields/Latitude"][:] = arrays["latitude"][::2, ::2]
            swath["Data Fields/Spectra"][:] = np.broadcast_to(arrays["qa_value"], (15, 40, 20))
            swath["Data Fields/Temperature"][:] = arrays["qa_value"][::2, ::2]
        for invalid in [False, True]:
            if invalid:
                with h5py.File(fixture, "r+") as f:
                    for variable in ["Longitude", "Latitude"]:
                        f[f"HDFEOS/SWATHS/Swath1/Geolocation Fields/{variable}"][7:10, 4:6] = np.nan
            for variable in ["Temperature", "Spectra"]:
                src = f'HDF5:"{fixture}"://HDFEOS/SWATHS/Swath1/Data_Fields/{variable}'
                check(f"HDF-EOS {'subsampled' if variable == 'Spectra' else 'full'} {'invalid' if invalid else 'valid'}", src)
    if args.json:
        print(json.dumps(results))
    else:
        for result in results:
            print(f"{result['name']}: {json.dumps(result)}")
    return int(any("error" in r for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
