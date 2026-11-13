#!/usr/bin/env python3
"""Compare WKT keyword identity and real USGS raster round trips with live GDAL."""

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal, osr

import terrio
from terrio.rio.crs import CRS


def require(condition, message):
    """Fail a comparison even when Python optimization is enabled.

    Args:
        condition: Whether the comparison passed.
        message: Diagnostic describing the failed comparison.

    Raises:
        AssertionError: If the comparison failed.
    """
    if not condition:
        raise AssertionError(message)


def main():
    """Compare CRS metadata and raster values, returning a failure exit code."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    osr.UseExceptions()
    fixture = Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/wkt_keywords_reference.json"
    cases = json.loads(fixture.read_text())["cases"]
    # Exercise identifiers absent from Terrio's registry with full engineering definitions.
    cases += [
        {**case, "name": case["name"] + "-root-id", "epsg": 5800,
         "wkt": case["wkt"][:-1] + (',AUTHORITY["EPSG","5800"]]' if case["name"] == "LOCAL_CS" else ',ID["EPSG",5800]]')}
        for case in cases if case["local"]
    ]
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
                require(actual.to_epsg() == case["epsg"], "wrong root authority code")
                restored = osr.SpatialReference()
                restored.ImportFromWkt(actual.to_wkt())
                require(restored.IsSame(reference), "CRS identity differs")
                require(bool(restored.IsLocal()) == case["local"], "local CRS kind differs")
                require(bool(restored.IsVertical()) == case["vertical"], "vertical CRS kind differs")
            result = subprocess.run(
                [args.terrio, "crs", case["wkt"], "--wkt"],
                capture_output=True, text=True, timeout=20, check=True,
            )
            require(case["wkt"] in result.stdout, "CLI changed the definition")
            raster = terrio.Raster.from_array(values, transform, crs=case["wkt"], nodata=nodata)
            np.testing.assert_array_equal(raster.read(1), values)
            require(tuple(raster.transform) == transform, "affine changed")
            require(raster.nodata == nodata, "nodata changed")
            require(raster.read(1).dtype == values.dtype, "pixel type changed")
            actual_crs = CRS.from_user_input(raster.crs_wkt or raster.crs)
            require(actual_crs.to_epsg() == case["epsg"], "raster root authority differs")
            raster_srs = osr.SpatialReference()
            raster_srs.ImportFromWkt(actual_crs.to_wkt())
            require(raster_srs.IsSame(reference), "raster CRS identity differs")
            with tempfile.TemporaryDirectory(prefix="terrio-wkt-geotiff-") as directory:
                for driver in ("GTiff", "COG"):
                    path = str(Path(directory) / f"{driver}.tif")
                    raster.write(path, driver=driver)
                    restored_raster = terrio.open_raster(path)
                    require(restored_raster.crs_wkt == case["wkt"], "GeoTIFF lost the retained WKT")
                    np.testing.assert_array_equal(restored_raster.read(1), values)
                    require(tuple(restored_raster.transform) == transform, "GeoTIFF affine changed")
                    require(restored_raster.nodata == nodata, "GeoTIFF nodata changed")
                    reopened = gdal.Open(path)
                    written_srs = osr.SpatialReference()
                    written_srs.ImportFromWkt(reopened.GetProjectionRef())
                    require(written_srs.IsSame(reference), "GDAL reads a different GeoTIFF CRS")
                    require(bool(written_srs.IsLocal()) == case["local"], "GDAL reads a different CRS kind")
                    require(bool(written_srs.IsVertical()) == case["vertical"], "GDAL reads a different vertical CRS kind")
                    np.testing.assert_array_equal(reopened.ReadAsArray(), values)
                    require(reopened.GetGeoTransform() == transform, "GDAL reads a different affine")
                    require(reopened.GetRasterBand(1).GetNoDataValue() == nodata, "GDAL reads different nodata")
                    require(reopened.GetRasterBand(1).DataType == crop.GetRasterBand(1).DataType, "GDAL reads a different sample type")
                    reopened = None
                    if case["local"]:
                        try:
                            terrio.warp(path, t_srs="EPSG:4326")
                        except terrio.CRSError:
                            pass
                        else:
                            require(False, "engineering coordinates were warped as an Earth CRS")
            detail = "Exact CRS kind/root authority, CLI/GeoTIFF WKT retention, GTiff/COG USGS pixels, type, nodata and affine"
            if case["local"]:
                detail += "; local-to-Earth warp refused"
            results.append(dict(name=case["name"], status="identical", detail=detail))
        except AssertionError as error:
            results.append(dict(name=case["name"], status="differs", detail=str(error)))
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']}: {r['detail']}" for r in results))
    return int(any(r["status"] != "identical" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
