"""Zonal binding comparisons with GDAL masks and NumPy on real raster data."""

import argparse
from decimal import Decimal, ROUND_HALF_UP
import json
import math
from pathlib import Path
import subprocess
import tempfile

from osgeo import gdal, ogr, osr
from shapely import from_geojson, from_wkb, hausdorff_distance

from compare_raster_analysis_with_gdal import (
    expected_statistics,
    traditional,
    write_zones,
)

gdal.UseExceptions()


def compare(result, expected, source, options):
    """Reject lost features/attributes/geometry/CRS and statistics outside explicit bounds."""
    actual = result["geojson"]["features"]
    if len(actual) != len(expected) or len(actual) != len(source):
        raise AssertionError("zone count differs")
    crs = osr.SpatialReference()
    if not result.get("crs"):
        raise AssertionError("output CRS missing")
    crs.SetFromUserInput(result["crs"])
    target = osr.SpatialReference()
    target.SetFromUserInput(options.get("t_srs") or source[0]["crs"])
    if not crs.IsSame(target, ["IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES"]):
        raise AssertionError("output CRS differs")
    source_crs = traditional(osr.SpatialReference(wkt=source[0]["crs"]))
    transform = osr.CoordinateTransformation(source_crs, traditional(target))
    reprojected = not source_crs.IsSame(target, ["IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES"])
    for feature, values, original in zip(actual, expected, source, strict=True):
        fields = feature["properties"]
        wanted = original["properties"] | values
        if set(fields) != set(wanted):
            raise AssertionError("attribute schema differs")
        for key, value in wanted.items():
            got = fields[key]
            if key in original["properties"] or value is None or key.endswith("_count"):
                if got != value or isinstance(got, bool) != isinstance(value, bool):
                    raise AssertionError(f"attribute/count/null differs: {key}")
            else:
                places = options.get("decimal_places")
                if places is not None:
                    scale = 10**places
                    value = (
                        float(
                            Decimal.from_float(value * scale).to_integral_value(
                                rounding=ROUND_HALF_UP
                            )
                        )
                        / scale
                    )
                if (
                    isinstance(got, bool)
                    or not isinstance(got, (int, float))
                    or not math.isclose(got, value, rel_tol=1e-10, abs_tol=1e-9)
                ):
                    raise AssertionError(
                        f"statistic differs: {key}: {got} versus {value}"
                    )
        if original["geometry"] is None:
            if feature["geometry"] is not None:
                raise AssertionError("null geometry changed")
        else:
            if feature.get("geometry") is None:
                raise AssertionError("output geometry missing")
            geometry = ogr.CreateGeometryFromJson(json.dumps(original["geometry"]))
            geometry.Transform(transform)
            actual_geometry = from_geojson(json.dumps(feature["geometry"]))
            expected_geometry = from_wkb(bytes(geometry.ExportToWkb()))
            if actual_geometry.geom_type != expected_geometry.geom_type:
                raise AssertionError("output geometry type differs")
            distances = (
                float(hausdorff_distance(actual_geometry, expected_geometry)),
                float(
                    hausdorff_distance(
                        actual_geometry.boundary, expected_geometry.boundary
                    )
                ),
            )
            if any(
                not math.isfinite(distance)
                or distance > (1e-5 if reprojected else 1e-12)
                for distance in distances
            ):
                raise AssertionError("output geometry differs")
    return "GDAL masks + NumPy: counts/nulls/source attributes exact; seven statistics relative 1e-10 / absolute 1e-9; geometry 1e-12 source degrees or 1e-5 projected metres"


def prepare(data, work):
    requests, references = [], {}
    landsat = data / "landsat.tif"
    zones = work / "landsat-zones.geojson"
    dataset = gdal.Open(str(landsat))
    # Construct zones on actual Landsat pixels, then transform them to EPSG:4326.
    native = work / "native-zones.geojson"
    gt = list(dataset.GetGeoTransform())
    gt[0] += 500 * gt[1] + 500 * gt[2]
    gt[3] += 500 * gt[4] + 500 * gt[5]
    write_zones(native, gt)
    gdal.VectorTranslate(
        str(zones),
        str(native),
        srcSRS=dataset.GetProjection(),
        dstSRS="EPSG:4326",
        format="GeoJSON",
    ).Close()
    for name, zone_file, rasters, bands, touched, target, places in [
        (
            "counties-center",
            data / "TN_Counties.geojson",
            [data / "dem.tif", data / "nlcd_knoxville.tif"],
            [1],
            False,
            None,
            None,
        ),
        (
            "counties-touched",
            data / "TN_Counties.geojson",
            [data / "dem.tif", data / "nlcd_knoxville.tif"],
            [1],
            True,
            None,
            None,
        ),
        ("landsat-bands", zones, [landsat], [1, 2, 3], False, None, None),
        (
            "landsat-touched-rounded-projected",
            zones,
            [landsat],
            [2, 1],
            True,
            "EPSG:3857",
            2,
        ),
    ]:
        options = dict(
            bands=bands,
            prefixes=[f"r{i}" for i in range(len(rasters))],
            stats=["count", "min", "max", "mean", "sum", "stddev", "median"],
            all_touched=touched,
        )
        if target:
            options.update(t_srs=target, decimal_places=places)
        ds = ogr.Open(str(zone_file))
        layer = ds.GetLayer(0)
        source = [
            dict(
                properties=f.items(),
                geometry=json.loads(f.GetGeometryRef().ExportToJson())
                if f.GetGeometryRef() is not None
                else None,
                crs=layer.GetSpatialRef().ExportToWkt(),
            )
            for f in layer
        ]
        expected = [{} for _ in source]
        for i, raster in enumerate(rasters):
            for band in bands:
                for row, values in zip(
                    expected,
                    expected_statistics(raster, zone_file, band, touched),
                    strict=True,
                ):
                    row.update(
                        {
                            f"r{i}_b{band}_{stat}": value
                            for stat, value in values.items()
                        }
                    )
        if not any(
            value > 0
            for row in expected
            for key, value in row.items()
            if key.endswith("_count")
        ):
            raise AssertionError(f"{name} has no contributing real pixels")
        if not any(
            value == 0
            for row in expected
            for key, value in row.items()
            if key.endswith("_count")
        ):
            raise AssertionError(f"{name} lacks a zero-count reference zone")
        requests.append(
            dict(
                name=name,
                zones=str(zone_file),
                rasters=list(map(str, rasters)),
                options=options,
            )
        )
        references[name] = (expected, source, options)
    return requests, references


def compare_singlepart(result, path):
    from compare_processing_with_gdal import parts

    dataset = ogr.Open(str(path))
    layer = dataset.GetLayer(0)
    expected = [
        (g.Clone() if g is not None else None, f.items())
        for f in layer
        for g in parts(f.GetGeometryRef())
    ]
    actual = result["geojson"]["features"]
    if len(actual) != len(expected):
        raise AssertionError("singlepart feature count differs")
    crs = osr.SpatialReference()
    if not result.get("crs"):
        raise AssertionError("singlepart CRS missing")
    crs.SetFromUserInput(result["crs"])
    if not crs.IsSame(
        layer.GetSpatialRef(), ["IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES"]
    ):
        raise AssertionError("singlepart CRS differs")
    for feature, (geometry, attrs) in zip(actual, expected, strict=True):
        if feature["properties"] != attrs:
            raise AssertionError("singlepart attributes differ")
        if geometry is None:
            if feature["geometry"] is not None:
                raise AssertionError("singlepart null geometry changed")
        elif feature.get("geometry") is None:
            raise AssertionError("singlepart output geometry missing")
        elif (
            ogr.CreateGeometryFromJson(json.dumps(feature["geometry"])).ExportToWkb()
            != geometry.ExportToWkb()
        ):
            raise AssertionError("singlepart geometry coordinates/type differ")
    encoded = ogr.Open(result["output"])
    ids = [feature.GetFID() for feature in encoded.GetLayer(0)]
    if ids != list(range(1, len(actual) + 1)):
        raise AssertionError("singlepart sequential IDs differ")
    return f"{len(actual)} recursively traversed OGR parts: exact geometry WKB, attributes, sequential IDs and CRS"


def main():
    import terrio

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--js", type=Path)
    parser.add_argument(
        "--json", action="store_true", help="emit compact JSON for the harness"
    )
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-zonal-bindings-") as temp:
        requests, references = prepare(args.data.resolve(), Path(temp))
        for filename in ["countries.geojson", "countries.fgb"]:
            requests.append(
                dict(
                    name="singlepart-" + filename,
                    zones=str(args.data.resolve() / filename),
                    options={},
                    operation="singlepart",
                    output=str(Path(temp) / (filename + ".gpkg")),
                )
            )
        output = []
        for request in requests:
            try:
                if request.get("operation") == "singlepart":
                    layer = terrio.singlepart(request["zones"])
                    encoded = str(Path(request["output"]).with_suffix(".python.gpkg"))
                    layer.write(encoded)
                    output.append(
                        (
                            "python",
                            dict(
                                name=request["name"],
                                geojson=layer.to_geojson(),
                                crs=layer.crs or layer.crs_wkt,
                                output=encoded,
                            ),
                        )
                    )
                    continue
                layer = terrio.zonal_stats(
                    request["zones"], request["rasters"], **request["options"]
                )
                output.append(
                    (
                        "python",
                        dict(
                            name=request["name"],
                            geojson=layer.to_geojson(),
                            crs=layer.crs or layer.crs_wkt,
                        ),
                    )
                )
            except Exception as error:
                output.append(("python", dict(name=request["name"], error=f"{type(error).__name__}: {error}")))
        if args.js:
            path = Path(temp) / "requests.json"
            path.write_text(json.dumps(requests))
            try:
                response = subprocess.run(
                    [
                        "node",
                        str(Path(__file__).with_name("run_zonal_bindings.mjs")),
                        str(args.js.resolve()),
                        str(path),
                    ],
                    text=True,
                    capture_output=True,
                    timeout=600,
                )
                if response.returncode:
                    raise RuntimeError(f"Node exited {response.returncode}: {response.stderr[-4000:]}")
                rows = json.loads(response.stdout)
                if (not isinstance(rows, list) or len(rows) != len(requests)
                        or {row["name"] for row in rows} != {request["name"] for request in requests}):
                    raise ValueError("Node result inventory differs from requested cases")
            except Exception as error:
                rows = [dict(name=request["name"], error=f"{type(error).__name__}: {error}") for request in requests]
            output.extend(("js", row) for row in rows)
        for backend, row in output:
            try:
                if "error" in row:
                    raise RuntimeError(row["error"])
                if row["name"].startswith("singlepart-"):
                    detail = compare_singlepart(
                        row,
                        args.data.resolve() / row["name"].removeprefix("singlepart-"),
                    )
                    status = "identical"
                else:
                    detail = compare(row, *references[row["name"]])
                    status = "tolerance"
            except Exception as error:
                detail = str(error)
                status = "differs"
            results.append(
                dict(name=f"{backend}/{row['name']}", status=status, detail=detail)
            )
    print(json.dumps(results, indent=None if args.json else 2))
    return int(any(row["status"] == "differs" for row in results))


if __name__ == "__main__":
    raise SystemExit(main())
