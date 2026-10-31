#!/usr/bin/env python3
"""Compare composed pipelines and saved definitions with GDAL 3.13.3."""

import argparse
import json
import os
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal, ogr, osr
from compare_layer_algebra_with_gdal import points


def run(command, config=None):
    result = subprocess.run(
        list(map(str, command)),
        capture_output=True,
        text=True,
        env=None if config is None else {**os.environ, **config},
    )
    assert result.returncode == 0, result.stderr or result.stdout
    return result.stdout


def compare_raster(actual, reference, metadata=False):
    a, b = gdal.Open(str(actual)), gdal.Open(str(reference))
    assert (a.RasterXSize, a.RasterYSize, a.RasterCount) == (
        b.RasterXSize,
        b.RasterYSize,
        b.RasterCount,
    ), "raster dimensions"
    ag, bg = (
        a.GetGeoTransform(can_return_null=True),
        b.GetGeoTransform(can_return_null=True),
    )
    assert (ag is None) == (bg is None), "affine transform presence"
    if ag is not None:
        np.testing.assert_allclose(ag, bg, rtol=1e-12, atol=1e-12)
    ac, bc = a.GetSpatialRef(), b.GetSpatialRef()
    assert (ac is None) == (bc is None), "CRS presence"
    if ac is not None:
        assert ac.IsSame(bc), "CRS"
    for band in range(1, a.RasterCount + 1):
        x, y = a.GetRasterBand(band), b.GetRasterBand(band)
        assert x.DataType == y.DataType, "band type"
        np.testing.assert_array_equal(x.ReadAsArray(), y.ReadAsArray())
        np.testing.assert_array_equal(
            x.GetMaskBand().ReadAsArray(), y.GetMaskBand().ReadAsArray()
        )
        assert x.GetNoDataValue() == y.GetNoDataValue(), "nodata"
        assert x.GetColorInterpretation() == y.GetColorInterpretation(), (
            "color interpretation"
        )
        assert x.GetScale() == y.GetScale(), "scale"
        assert x.GetOffset() == y.GetOffset(), "offset"
        if metadata:
            assert x.GetDescription() == y.GetDescription(), "band description"
            assert x.GetMetadata() == y.GetMetadata(), "band metadata/statistics"
    return (
        True,
        "exact pixels, masks and band properties; CRS and affine transform checked",
    )


def compare_vector(actual, reference, polygon_tolerance=1e-7):
    exact = True
    identifiers_differ = False
    a, b = ogr.Open(str(actual)), ogr.Open(str(reference))
    assert a.GetLayerCount() == b.GetLayerCount(), "layer count"
    for index in range(a.GetLayerCount()):
        x, y = a.GetLayer(index), b.GetLayer(index)
        assert x.GetFeatureCount() == y.GetFeatureCount(), "feature count"
        xd, yd = x.GetLayerDefn(), y.GetLayerDefn()
        assert [
            (xd.GetFieldDefn(i).GetName(), xd.GetFieldDefn(i).GetType())
            for i in range(xd.GetFieldCount())
        ] == [
            (yd.GetFieldDefn(i).GetName(), yd.GetFieldDefn(i).GetType())
            for i in range(yd.GetFieldCount())
        ], "schema"
        xc, yc = x.GetSpatialRef(), y.GetSpatialRef()
        assert (xc is None) == (yc is None), "CRS presence"
        if xc is not None:
            assert xc.IsSame(yc), "CRS"
        actual_ids, expected_ids = [f.GetFID() for f in x], [f.GetFID() for f in y]
        x.ResetReading()
        y.ResetReading()
        if actual_ids != expected_ids:
            assert actual_ids == list(range(1, len(actual_ids) + 1)), (
                "native generated feature IDs"
            )
            exact = False
            identifiers_differ = True
        for xf, yf in zip(x, y, strict=True):
            assert xf.items() == yf.items(), "attributes"
            xg, yg = xf.GetGeometryRef(), yf.GetGeometryRef()
            assert (xg is None) == (yg is None), "geometry presence"
            if xg is not None:
                assert xg.GetDimension() == yg.GetDimension(), "geometry dimension"
                assert xg.IsValid(), "invalid output geometry"
                if xg.Equals(yg):
                    continue
                exact = False
                if yg.GetDimension() == 2:
                    error = xg.SymDifference(yg).GetArea() / max(1.0, yg.GetArea())
                    assert error <= polygon_tolerance, (
                        f"relative polygon symmetric-difference area {error}"
                    )
                elif yg.GetDimension() == 1:
                    error = abs(xg.Length() - yg.Length()) / max(1.0, yg.Length())
                    assert error <= 1e-8, f"relative line length error {error}"
                    for source, target in [(xg, yg), (yg, xg)]:
                        for coordinate in points(source):
                            point = ogr.Geometry(ogr.wkbPoint)
                            point.AddPoint_2D(*coordinate[:2])
                            assert point.Distance(target) <= 1e-7, (
                                "line displacement exceeds 1e-7"
                            )
                else:
                    actual_points, expected_points = points(xg), points(yg)
                    assert len(actual_points) == len(expected_points), "point count"
                    for actual_point, expected_point in zip(
                        sorted(actual_points), sorted(expected_points), strict=True
                    ):
                        assert (
                            sum(
                                (x - y) ** 2
                                for x, y in zip(
                                    actual_point[:2], expected_point[:2], strict=True
                                )
                            )
                            <= 1e-16
                        ), "point displacement exceeds 1e-8"
    return (
        exact,
        "schema/attributes, validity, CRS and geometry checked; polygon relative area tolerance "
        + str(polygon_tolerance)
        + ", line relative length 1e-8 and displacement 1e-7, point displacement 1e-8; "
        + (
            "writer policy: native sequential FIDs independently checked; GDAL retains source IDs"
            if identifiers_differ
            else "feature IDs match"
        ),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", required=True, type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    ogr.UseExceptions()
    osr.UseExceptions()
    binary, data = args.terrio.resolve(), args.data.resolve()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-pipeline-") as temporary:
        root = Path(temporary)
        source = root / "input with spaces.tif"
        ds = gdal.GetDriverByName("GTiff").Create(
            str(source), 10, 10, 2, gdal.GDT_UInt16
        )
        ds.SetGeoTransform([0, 1, 0, 10, 0, -1])
        crs = osr.SpatialReference()
        crs.ImportFromEPSG(4326)
        ds.SetSpatialRef(crs)
        for band in [1, 2]:
            ds.GetRasterBand(band).WriteArray(
                np.arange(100, dtype=np.uint16).reshape(10, 10) + band
            )
        ds.SetMetadataItem("source", "field campaign")
        band = ds.GetRasterBand(1)
        band.SetDescription("elevation")
        band.SetScale(2)
        band.SetOffset(5)
        band.SetMetadataItem("quality", "surveyed")
        band.GetStatistics(False, True)
        band = None
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        mask = np.full((10, 10), 255, dtype=np.uint8)
        mask[:2, :3] = 0
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        ds = None
        unreferenced = root / "unreferenced.tif"
        ds = gdal.GetDriverByName("GTiff").Create(
            str(unreferenced), 2, 2, 1, gdal.GDT_Byte
        )
        ds.GetRasterBand(1).WriteArray(np.arange(4, dtype=np.uint8).reshape(2, 2))
        ds = None
        shapes = root / "shapes.gpkg"
        ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(shapes))
        projected = osr.SpatialReference()
        projected.ImportFromEPSG(3857)
        layer = ds.CreateLayer("shapes", projected, geom_type=ogr.wkbLineString)
        layer.CreateField(ogr.FieldDefn("group", ogr.OFTInteger))
        for index, wkt in enumerate(
            ["LINESTRING (0 0,10 1,20 0)", "LINESTRING (20 0,30 1,40 0)"]
        ):
            feature = ogr.Feature(layer.GetLayerDefn())
            feature.SetField("group", 1)
            feature.SetGeometry(ogr.CreateGeometryFromWkt(wkt))
            layer.CreateFeature(feature)
        layer = feature = ds = None
        multipart = root / "multipart.gpkg"
        ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(multipart))
        layer = ds.CreateLayer("multipart", projected, geom_type=ogr.wkbUnknown)
        layer.CreateField(ogr.FieldDefn("id", ogr.OFTInteger))
        for index, wkt in enumerate(
            [
                "MULTILINESTRING ((0 0,4 0),(2 0,6 0))",
                "MULTILINESTRING ((0 0,4 4),(0 4,4 0))",
                "MULTIPOINT ((1 1),(1 1),(2 2))",
                "MULTIPOLYGON (((0 0,2 0,2 2,0 2,0 0)),((1 0,3 0,3 2,1 2,1 0)))",
                "GEOMETRYCOLLECTION (POINT (1 1),LINESTRING (0 0,2 2))",
                "GEOMETRYCOLLECTION EMPTY",
                "MULTIPOINT EMPTY",
                None,
            ]
        ):
            feature = ogr.Feature(layer.GetLayerDefn())
            feature.SetField("id", index)
            if wkt is not None:
                feature.SetGeometry(ogr.CreateGeometryFromWkt(wkt))
            layer.CreateFeature(feature)
        feature = layer = ds = None
        identified = root / "identified.gpkg"
        ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(identified))
        layer = ds.CreateLayer("identified", projected, geom_type=ogr.wkbPoint)
        layer.CreateField(ogr.FieldDefn("value", ogr.OFTInteger))
        for index, fid in enumerate([42, 87]):
            feature = ogr.Feature(layer.GetLayerDefn())
            feature.SetFID(fid)
            feature.SetField("value", index)
            feature.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({index} 1)"))
            layer.CreateFeature(feature)
        feature = layer = ds = None
        cases = [
            ("masked copy", source, []),
            ("vector source identifiers", identified, []),
            ("band selection", source, ["select", "2"]),
            ("masked crop", source, ["clip", "--bbox", "1,1,8,9"]),
            ("masked resize", source, ["resize", "--size", "3,4"]),
            ("type conversion", source, ["set-type", "--output-data-type", "Float32"]),
            ("materialization", source, ["materialize"]),
            ("unreferenced copy", unreferenced, []),
            ("unreferenced resize", unreferenced, ["resize", "--size", "3,4"]),
            ("unreferenced materialization", unreferenced, ["materialize"]),
            ("real raster select", data / "usgs_dem.tif", ["select", "1"]),
            (
                "real vector selection",
                data / "TN_Counties.geojson",
                ["filter", "--where", "1=1", "!", "select", "NAME"],
            ),
            (
                "real vector materialization",
                data / "TN_Counties.geojson",
                ["materialize"],
            ),
        ]
        cases.extend(
            [
                (
                    "raster reproject",
                    source,
                    ["reproject", "--output-crs", "EPSG:3857", "--size", "8,8"],
                ),
                (
                    "raster clip and resize",
                    source,
                    ["clip", "--bbox", "2,2,8,8", "!", "resize", "--size", "2,2"],
                ),
                (
                    "vector geometry selection",
                    shapes,
                    ["select", "group,_ogr_geometry_"],
                ),
                ("vector buffer", shapes, ["buffer", "--distance", "2"]),
                ("vector simplify", shapes, ["simplify", "--tolerance", "2"]),
                ("vector segmentize", shapes, ["segmentize", "--max-length", "3"]),
                ("vector swap XY", shapes, ["swap-xy"]),
                ("vector dissolve", shapes, ["dissolve"]),
                ("vector multipart dissolve", multipart, ["dissolve"]),
                (
                    "real vector clipping",
                    data / "TN_Counties.geojson",
                    ["clip", "--bbox", "-87,35,-85,36"],
                ),
                (
                    "real vector reproject",
                    data / "TN_Counties.geojson",
                    ["reproject", "--output-crs", "EPSG:5070"],
                ),
                (
                    "mixed raster to vector",
                    source,
                    ["as-features", "--geometry-type", "point", "--include-xy"],
                ),
                (
                    "mixed raster round trip",
                    source,
                    [
                        "as-features",
                        "--geometry-type",
                        "point",
                        "--include-xy",
                        "!",
                        "filter",
                        "--where",
                        "BAND_1 > 50",
                        "!",
                        "rasterize",
                        "--attribute-name",
                        "BAND_1",
                        "--size",
                        "5,5",
                        "--extent",
                        "0,0,10,10",
                    ],
                ),
            ]
        )
        for index, (name, path, middle) in enumerate(cases):
            row = {"name": name}
            vector = "vector" in name
            extension = "gpkg" if vector else "tif"
            actual, reference = (
                root / f"actual-{index}.{extension}",
                root / f"ref-{index}.{extension}",
            )
            steps = ["read", str(path)] + (["!", *middle] if middle else [])
            try:
                run([binary, "pipeline", *steps, "!", "write", actual])
                run(["gdal", "pipeline", *steps, "!", "write", reference])
                if vector:
                    run([binary, "gpkg-validate", actual, "--full-check", "--json"])
                exact, detail = (
                    compare_vector(
                        actual, reference, 5e-4 if "buffer" in name else 1e-7
                    )
                    if vector
                    else compare_raster(actual, reference, metadata="real" not in name)
                )
                row.update(
                    status="identical" if exact else "within tolerance", detail=detail
                )
            except Exception as error:
                row.update(status="differs", detail=str(error))
            results.append(row)
        row = {"name": "masked COG export"}
        try:
            actual, reference = (
                root / "masked-cog-native.tif",
                root / "masked-cog-reference.tif",
            )
            for program, output in [(binary, actual), ("gdal", reference)]:
                run(
                    [
                        program,
                        "pipeline",
                        "read",
                        source,
                        "!",
                        "write",
                        output,
                        "--format",
                        "COG",
                    ]
                )
            run(
                [
                    binary,
                    "validate-cog",
                    actual,
                    "--profile",
                    "gdal",
                    "--full-check",
                    "yes",
                    "--json",
                ]
            )
            exact, detail = compare_raster(actual, reference, metadata=True)
            row.update(
                status="identical" if exact else "within tolerance", detail=detail
            )
        except Exception as error:
            row.update(status="differs", detail=str(error))
        results.append(row)
        # Saved definitions from each implementation, including materialization.
        for kind, path, middle in [
            ("raster", source, ["select", "2"]),
            ("vector", data / "TN_Counties.geojson", ["select", "NAME"]),
        ]:
            compare = (
                compare_vector
                if kind == "vector"
                else lambda a, b: compare_raster(a, b, metadata=True)
            )
            extension = "gpkg" if kind == "vector" else "tif"
            for producer in ["terrio", "gdal"]:
                for checkpoint_mode in ["none", "memory", "named"]:
                    label = f"{kind}-{producer}-{checkpoint_mode}"
                    row = {
                        "name": f"{kind} saved definition from {producer}, materialize={checkpoint_mode}"
                    }
                    definition = root / f"{label}.gdalg.json"
                    actual, reference = (
                        root / f"actual-{label}.{extension}",
                        root / f"reference-{label}.{extension}",
                    )
                    checkpoint = root / f"checkpoint-{label}.{extension}"
                    steps = ["read", str(path), "!", *middle]
                    if checkpoint_mode != "none":
                        steps.extend(["!", "materialize"])
                    if checkpoint_mode == "named":
                        steps.extend(["--output", str(checkpoint)])
                    try:
                        run(
                            [
                                binary if producer == "terrio" else "gdal",
                                "pipeline",
                                *steps,
                                "!",
                                "write",
                                definition,
                                "--format",
                                "GDALG",
                            ]
                        )
                        assert not checkpoint.exists(), (
                            "saving evaluated the checkpoint"
                        )
                        run(
                            [
                                binary,
                                "pipeline",
                                "--load",
                                definition,
                                "--output",
                                actual,
                            ]
                        )
                        if checkpoint_mode == "named":
                            assert checkpoint.exists(), "native checkpoint missing"
                            compare(checkpoint, actual)
                            checkpoint.unlink()
                            nested = root / f"nested-{label}.{extension}"
                            run(
                                [
                                    binary,
                                    "pipeline",
                                    "read",
                                    definition,
                                    "!",
                                    "write",
                                    nested,
                                ]
                            )
                            assert not checkpoint.exists(), (
                                "nested input wrote a checkpoint"
                            )
                            compare(nested, actual)
                        config = (
                            {"GDAL_ALGORITHM_ALLOW_WRITES_IN_STREAM": "YES"}
                            if checkpoint_mode == "named"
                            else None
                        )
                        run(
                            [
                                "gdal",
                                "pipeline",
                                "read",
                                definition,
                                "!",
                                "write",
                                reference,
                            ],
                            config=config,
                        )
                        if checkpoint_mode == "named":
                            assert checkpoint.exists(), "reference checkpoint missing"
                            compare(checkpoint, reference)
                        if kind == "vector":
                            run(
                                [
                                    binary,
                                    "gpkg-validate",
                                    actual,
                                    "--full-check",
                                    "--json",
                                ]
                            )
                        exact, detail = compare(actual, reference)
                        row.update(
                            status="identical" if exact else "within tolerance",
                            detail=detail,
                        )
                    except Exception as error:
                        row.update(status="differs", detail=str(error))
                    results.append(row)
    print(
        json.dumps(results, indent=2)
        if args.json
        else "\n".join(f"{r['status']}: {r['name']}: {r['detail']}" for r in results)
    )
    raise SystemExit(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    main()
