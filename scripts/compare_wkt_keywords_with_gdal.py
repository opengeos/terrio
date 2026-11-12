#!/usr/bin/env python3
"""Compare WKT keyword identity and real USGS pixels with GDAL 3.13.3."""

import argparse
import json
import subprocess
from pathlib import Path

import numpy as np
from osgeo import gdal, osr

import terrio
from terrio.rio.crs import CRS


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    osr.UseExceptions()
    fixture = Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/wkt_keywords_reference.json"
    cases = json.loads(fixture.read_text())["cases"]
    source = gdal.Open(str(args.data / "usgs_dem.tif"))
    # Real elevation samples; assigning a CRS changes metadata, not coordinates or pixels.
    crop = gdal.Translate("", source, format="MEM", srcWin=[1000, 1000, 16, 16])
    values = crop.ReadAsArray()
    transform = crop.GetGeoTransform()
    nodata = crop.GetRasterBand(1).GetNoDataValue()
    results = []
    for case in cases:
        try:
            reference = osr.SpatialReference()
            reference.ImportFromWkt(case["wkt"])
            for entry in [CRS.from_wkt, CRS.from_user_input, CRS.from_string]:
                actual = entry(case["wkt"])
                assert actual.to_epsg() == case["epsg"], "wrong root authority code"
                restored = osr.SpatialReference()
                restored.ImportFromWkt(actual.to_wkt())
                assert restored.IsSame(reference), "CRS identity differs"
                assert bool(restored.IsLocal()) == case["local"], "local CRS kind differs"
                assert bool(restored.IsVertical()) == case["vertical"], "vertical CRS kind differs"
            result = subprocess.run(
                [args.terrio, "crs", case["wkt"], "--wkt"],
                capture_output=True, text=True, timeout=20, check=True,
            )
            assert case["wkt"] in result.stdout, "CLI changed the definition"
            raster = terrio.Raster.from_array(values, transform, crs=case["wkt"], nodata=nodata)
            np.testing.assert_array_equal(raster.read(1), values)
            assert tuple(raster.transform) == transform, "affine changed"
            assert raster.nodata == nodata, "nodata changed"
            assert raster.read(1).dtype == values.dtype, "pixel type changed"
            actual_crs = CRS.from_user_input(raster.crs)
            assert actual_crs.to_epsg() == case["epsg"]
            raster_srs = osr.SpatialReference()
            raster_srs.ImportFromWkt(actual_crs.to_wkt())
            assert raster_srs.IsSame(reference), "raster CRS identity differs"
            results.append(dict(name=case["name"], status="identical", detail="Exact CRS kind/root authority, CLI WKT, USGS pixels, nodata and affine"))
        except Exception as error:
            results.append(dict(name=case["name"], status="differs", detail=str(error)))
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']}: {r['detail']}" for r in results))
    return int(any(r["status"] != "identical" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
