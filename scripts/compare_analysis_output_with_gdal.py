"""Check zonal/point-sampling output CRS and rounding on real rasters against GDAL.

Baseline values are independently checked with GDAL rasterization, GDAL pixel
reads and NumPy by the existing comparison helpers. Output reprojection must
match GDAL within 1e-5 metres; rounded values, nulls, counts and source attributes
must match exactly. Inputs are read-only; all outputs are temporary.
"""

from __future__ import annotations

import argparse
import tempfile
from decimal import ROUND_HALF_UP, Decimal
from pathlib import Path

from compare_processing_with_gdal import compare_sample
from compare_raster_analysis_with_gdal import compare_zonal, run, traditional
from osgeo import ogr, osr
from shapely import from_wkb, hausdorff_distance


def check_output(terrio, command, source, baseline, rasters, work):
    reference = ogr.Open(str(baseline))
    layer = reference.GetLayer(0)
    original = ogr.Open(str(source))
    source_fields = {field.GetName() for field in original.GetLayer(0).schema}
    for target in (None, "EPSG:3857"):
        output = work / f"{command}-{target is not None}.gpkg"
        args = ["--decimal-places", "2"]
        for i, raster in enumerate(rasters):
            args.extend(["--raster", raster, "--prefix", f"r{i}"])
        if command == "zonal-stats":
            args.extend(["--stats", "count,min,max,mean,sum,stddev,median"])
        if target:
            args.extend(["--t-srs", target])
        run(terrio, command, source, output, *args)
        result = ogr.Open(str(output))
        actual = result.GetLayer(0)
        dst = traditional(layer.GetSpatialRef())
        if target:
            dst = osr.SpatialReference()
            dst.SetFromUserInput(target)
            dst = traditional(dst)
        assert actual.GetSpatialRef().IsSame(dst)
        transform = osr.CoordinateTransformation(
            traditional(layer.GetSpatialRef()), dst
        )
        layer.ResetReading()
        assert actual.GetFeatureCount() == layer.GetFeatureCount()
        for got, expected in zip(actual, layer):
            assert got.GetFID() == expected.GetFID()
            for name, value in expected.items().items():
                if name not in source_fields and isinstance(value, float):
                    # Match f64 scaling, but use Decimal's independent rounding engine.
                    scaled = Decimal.from_float(value * 100)
                    value = (
                        float(scaled.to_integral_value(rounding=ROUND_HALF_UP)) / 100
                    )
                assert got[name] == value, (command, target, name, got[name], value)
            geometry = expected.GetGeometryRef()
            if geometry is None:
                assert got.GetGeometryRef() is None
            else:
                geometry = geometry.Clone()
                geometry.Transform(transform)
                distance = hausdorff_distance(
                    from_wkb(bytes(got.GetGeometryRef().ExportToWkb())),
                    from_wkb(bytes(geometry.ExportToWkb())),
                )
                assert distance <= (1e-5 if target else 1e-12), distance
        print(
            f"PASS {command} target={target or 'source'}: values/nulls/counts/attributes exact; "
            "geometry within 1e-5 m for EPSG:3857, 1e-12 source units otherwise"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", type=Path, default=Path("target/debug/terrio"))
    args = parser.parse_args()
    data, terrio = args.data.resolve(), args.terrio.resolve()
    with tempfile.TemporaryDirectory(prefix="terrio-analysis-output-") as tmp:
        work = Path(tmp)
        rasters = [data / "dem.tif", data / "nlcd_knoxville.tif"]
        source = data / "TN_Counties.geojson"
        compare_zonal(terrio, source, rasters, work, "output-baseline")
        check_output(
            terrio,
            "zonal-stats",
            source,
            work / "zonal-output-baseline.gpkg",
            rasters,
            work,
        )
        rasters = [data / "dem.tif", data / "landsat.tif", data / "nlcd_knoxville.tif"]
        compare_sample(terrio, rasters, work)
        check_output(
            terrio,
            "point-sample",
            work / "sites.geojson",
            work / "samples.gpkg",
            rasters,
            work,
        )


if __name__ == "__main__":
    main()
