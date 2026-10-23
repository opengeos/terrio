"""Compare contours, multi-raster zonal statistics and SQL with GDAL/OGR.

Requires GDAL, NumPy and Shapely. Inputs are read-only; generated files are temporary::

    python scripts/compare_raster_analysis_with_gdal.py ../terrio-data --terrio target/debug/terrio

Contours are compared by level, line count, length and Hausdorff distance. Zonal statistics
use GDAL's rasterization on the full source grid and NumPy as independent references; this
also checks terrio's cropped masks, reprojection, nodata handling and overlapping zones.
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import tempfile
from collections import defaultdict
from pathlib import Path

import numpy as np
from osgeo import gdal, ogr, osr
from shapely import from_wkb, hausdorff_distance, union_all

gdal.UseExceptions()
ogr.UseExceptions()
osr.UseExceptions()


def run(terrio, command, source, output, *args):
    result = subprocess.run(
        [str(terrio), command, str(source), str(output), *map(str, args)],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"{command}: {result.stderr}")


def rows(path):
    ds = ogr.Open(str(path))
    return [f.Clone() for f in ds.GetLayer(0)]


def traditional(srs):
    srs = srs.Clone()
    srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    return srs


def write_raster(path, values, gt, nodata=None, epsg=4326, data_type=gdal.GDT_Float64):
    values = np.asarray(values, dtype=np.float64)
    if values.ndim == 2:
        values = values[np.newaxis]
    ds = gdal.GetDriverByName("GTiff").Create(
        str(path), values.shape[2], values.shape[1], values.shape[0], data_type
    )
    ds.SetGeoTransform(gt)
    srs = osr.SpatialReference()
    srs.ImportFromEPSG(epsg)
    ds.SetSpatialRef(srs)
    for i, band_values in enumerate(values, 1):
        band = ds.GetRasterBand(i)
        band.WriteArray(band_values)
        if nodata is not None:
            band.SetNoDataValue(nodata)
    ds.Close()


def compare_contour(
    terrio,
    source,
    work,
    label,
    levels=(),
    interval=None,
    offset=0,
    ignore=False,
    with_z=False,
):
    raster = gdal.Open(str(source))
    expected_ds = ogr.GetDriverByName("MEM").CreateDataSource("")
    expected = expected_ds.CreateLayer(
        "contour",
        raster.GetSpatialRef(),
        ogr.wkbLineString25D if with_z else ogr.wkbLineString,
    )
    expected.CreateField(ogr.FieldDefn("ID", ogr.OFTInteger64))
    expected.CreateField(ogr.FieldDefn("ELEV", ogr.OFTReal))
    options = ["ID_FIELD=0", "ELEV_FIELD=1"]
    args = []
    if levels:
        joined = ",".join(map(str, levels))
        options.append("FIXED_LEVELS=" + joined)
        args += ["--levels", joined]
    if interval is not None:
        options += [f"LEVEL_INTERVAL={interval}", f"LEVEL_BASE={offset}"]
        args += ["--interval", interval, "--offset", offset]
    nd = raster.GetRasterBand(1).GetNoDataValue()
    if ignore:
        args += ["--ignore-nodata"]
    elif nd is not None:
        options += [f"NODATA={nd}"]
    if with_z:
        args += ["--3d"]
    gdal.ContourGenerateEx(raster.GetRasterBand(1), expected, options=options)
    output = work / f"contour-{label}.gpkg"
    run(terrio, "contour", source, output, *args)
    reference, actual = defaultdict(list), defaultdict(list)
    for f in expected:
        reference[f["ELEV"]].append(from_wkb(bytes(f.GetGeometryRef().ExportToWkb())))
    for f in rows(output):
        g = f.GetGeometryRef()
        actual[f["ELEV"]].append(from_wkb(bytes(g.ExportToWkb())))
        if with_z:
            assert all(g.GetZ(i) == f["ELEV"] for i in range(g.GetPointCount()))
    assert reference.keys() == actual.keys(), (label, list(reference), list(actual))
    gt = raster.GetGeoTransform()
    tolerance = max(
        1e-9,
        max(abs(gt[1]), abs(gt[2]), abs(gt[4]), abs(gt[5])) * 1e-8
        + max(abs(gt[0]), abs(gt[3])) * 2e-15,
    )
    max_distance = 0.0
    for level in reference:
        a, b = reference[level], actual[level]
        assert len(a) == len(b), (label, level, len(a), len(b))
        distance = float(hausdorff_distance(union_all(a), union_all(b)))
        assert distance <= tolerance, (label, level, distance, tolerance)
        assert math.isclose(
            sum(g.length for g in a),
            sum(g.length for g in b),
            rel_tol=1e-10,
            abs_tol=tolerance * sum(len(g.coords) for g in a),
        )
        max_distance = max(max_distance, distance)
    print(
        f"PASS contour {label}: {sum(map(len, actual.values()))} lines; max Hausdorff distance {max_distance:.3g}"
    )


def expected_statistics(source, polygons, band_number, all_touched):
    raster = gdal.Open(str(source))
    band = raster.GetRasterBand(band_number)
    values = band.ReadAsArray().astype(np.float64)
    valid = (band.GetMaskBand().ReadAsArray() != 0) & np.isfinite(values)
    source_ds = ogr.Open(str(polygons))
    source_layer = source_ds.GetLayer(0)
    transform = osr.CoordinateTransformation(
        traditional(source_layer.GetSpatialRef()), traditional(raster.GetSpatialRef())
    )
    result = []
    for feature in source_layer:
        mask = gdal.GetDriverByName("MEM").Create(
            "", raster.RasterXSize, raster.RasterYSize, 1, gdal.GDT_Byte
        )
        mask.SetGeoTransform(raster.GetGeoTransform())
        mask.SetSpatialRef(raster.GetSpatialRef())
        geometry = feature.GetGeometryRef()
        if geometry is not None:
            geometry = geometry.Clone()
            geometry.Transform(transform)
            ds = ogr.GetDriverByName("MEM").CreateDataSource("")
            layer = ds.CreateLayer("zone", raster.GetSpatialRef(), ogr.wkbUnknown)
            f = ogr.Feature(layer.GetLayerDefn())
            f.SetGeometry(geometry)
            layer.CreateFeature(f)
            gdal.RasterizeLayer(
                mask,
                [1],
                layer,
                burn_values=[1],
                options=["ALL_TOUCHED=TRUE"] if all_touched else [],
            )
        selected = values[(mask.ReadAsArray() != 0) & valid]
        if selected.size:
            result.append(
                {
                    "count": int(selected.size),
                    "min": float(selected.min()),
                    "max": float(selected.max()),
                    "mean": float(selected.mean()),
                    "sum": float(selected.sum()),
                    "stddev": float(selected.std()),
                    "median": float(np.median(selected)),
                }
            )
        else:
            result.append(
                {
                    "count": 0,
                    "min": None,
                    "max": None,
                    "mean": None,
                    "sum": None,
                    "stddev": None,
                    "median": None,
                }
            )
    return result


def compare_zonal(
    terrio, polygons, rasters, work, label, bands=(1,), all_touched=False
):
    output = work / f"zonal-{label}.gpkg"
    args = [
        "--band",
        ",".join(map(str, bands)),
        "--stats",
        "count,min,max,mean,sum,stddev,median",
    ]
    for i, raster in enumerate(rasters):
        args += ["--raster", raster, "--prefix", f"r{i}"]
    if all_touched:
        args += ["--all-touched"]
    run(terrio, "zonal-stats", polygons, output, *args)
    actual, original = rows(output), rows(polygons)
    assert len(actual) == len(original)
    for a, b in zip(actual, original):
        assert all(a[k] == v for k, v in b.items().items())
        if b.GetGeometryRef() is not None:
            assert a.GetGeometryRef().Equals(b.GetGeometryRef())
    for i, raster in enumerate(rasters):
        for band in bands:
            reference = expected_statistics(raster, polygons, band, all_touched)
            for j, (feature, expected) in enumerate(zip(actual, reference)):
                for stat, value in expected.items():
                    got = feature[f"r{i}_b{band}_{stat}"]
                    if value is None:
                        assert got is None, (label, j, stat, got, value)
                    elif stat == "count":
                        assert got == value, (label, j, stat, got, value)
                    else:
                        assert math.isclose(got, value, rel_tol=1e-10, abs_tol=1e-9), (
                            label,
                            j,
                            stat,
                            got,
                            value,
                        )
    print(
        f"PASS zonal {label}: {len(actual)} zones, {len(rasters)} rasters, bands {bands}; all counts/statistics match GDAL + NumPy"
    )


def write_zones(path, gt):
    def ring(points):
        return [
            [gt[0] + x * gt[1] + y * gt[2], gt[3] + x * gt[4] + y * gt[5]]
            for x, y in points
        ]

    geometries = [
        {
            "type": "Polygon",
            "coordinates": [
                ring([(0, 0), (3, 0), (3, 3), (0, 3), (0, 0)]),
                ring([(1, 1), (2, 1), (2, 2), (1, 2), (1, 1)]),
            ],
        },
        {
            "type": "Polygon",
            "coordinates": [
                ring([(0.9, 0.9), (2.1, 0.9), (2.1, 2.1), (0.9, 2.1), (0.9, 0.9)])
            ],
        },
        {
            "type": "Polygon",
            "coordinates": [ring([(10, 10), (11, 10), (11, 11), (10, 11), (10, 10)])],
        },
        None,
    ]
    path.write_text(
        json.dumps(
            {
                "type": "FeatureCollection",
                "features": [
                    {"type": "Feature", "properties": {"zone": i}, "geometry": g}
                    for i, g in enumerate(geometries)
                ],
            }
        )
    )


def compare_sql(terrio, data, work):
    source = work / "countries.gpkg"
    ds = gdal.VectorTranslate(
        str(source),
        str(data / "countries.geojson"),
        format="GPKG",
        layerName="countries",
    )
    ds.Close()
    statement = "SELECT ADMIN, POP_EST AS population FROM countries WHERE CONTINENT = 'Asia' ORDER BY POP_EST DESC LIMIT 10"
    reference = work / "sql-reference.gpkg"
    ds = gdal.VectorTranslate(
        str(reference),
        str(source),
        format="GPKG",
        SQLStatement=statement,
        SQLDialect="OGRSQL",
    )
    ds.Close()
    expected = rows(reference)
    output = work / "sql.gpkg"
    run(terrio, "sql", source, output, "--query", statement)
    actual = rows(output)
    assert len(actual) == len(expected)
    for a, b in zip(actual, expected):
        assert a.items() == b.items()
        assert a.GetFID() == b.GetFID()
        assert a.GetGeometryRef().Equals(b.GetGeometryRef())
    print(
        f"PASS SQL: {len(actual)} features; attributes, file IDs and geometry match ogr2ogr SQL output"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", type=Path, default=Path("target/debug/terrio"))
    args = parser.parse_args()
    terrio, data = args.terrio.resolve(), args.data.resolve()
    print(gdal.VersionInfo("--version"))
    with tempfile.TemporaryDirectory(prefix="terrio-raster-analysis-") as tmp:
        work = Path(tmp)
        for label, values, levels, nd in [
            ("ramp", [[0, 10], [0, 10]], [5], None),
            ("saddle-nw", [[10, 0], [0, 10]], [5], None),
            ("saddle-ne", [[0, 10], [10, 0]], [5], None),
            ("ties", [[0, 5, 10], [5, 10, 5], [10, 5, 0]], [0, 5, 10], None),
            ("nodata", [[0, 4, 8], [-9999, 5, 9], [0, 3, -9999]], [2, 4, 6, 8], -9999),
        ]:
            source = work / f"{label}.tif"
            write_raster(source, values, [0, 1, 0, len(values), 0, -1], nd)
            compare_contour(terrio, source, work, label, levels=levels, with_z=True)
        source = work / "float32-nodata.tif"
        write_raster(
            source,
            [[42, -9999.1], [42, -9999.1]],
            [0, 1, 0, 2, 0, -1],
            -9999.1,
            data_type=gdal.GDT_Float32,
        )
        compare_contour(terrio, source, work, "float32-nodata", levels=[0])
        compare_contour(
            terrio, source, work, "float32-nodata-included", levels=[0], ignore=True
        )
        source = work / "dem-crop.tif"
        ds = gdal.Translate(
            str(source), str(data / "dem.tif"), srcWin=[100, 100, 160, 160]
        )
        ds.Close()
        compare_contour(terrio, source, work, "real-dem", interval=5)
        compare_contour(
            terrio,
            source,
            work,
            "combined-levels",
            interval=5,
            offset=1,
            levels=[64.5],
        )
        for label, gt in [
            ("north", [0, 1, 0, 3, 0, -1]),
            ("rotated", [10, 1, 0.2, 20, 0.1, -1]),
            ("south", [0, 1, 0, 0, 0, 1]),
        ]:
            source, polygons = work / f"{label}.tif", work / f"{label}.geojson"
            values = np.arange(1, 10, dtype=float).reshape(3, 3)
            values[1, 1] = -9999
            write_raster(source, np.stack([values, values * 2]), gt, -9999)
            write_zones(polygons, gt)
            compare_zonal(terrio, polygons, [source], work, label, bands=(1, 2))
            compare_zonal(
                terrio,
                polygons,
                [source],
                work,
                label + "-touched",
                bands=(1, 2),
                all_touched=True,
            )
        compare_zonal(
            terrio,
            data / "TN_Counties.geojson",
            [data / "nlcd_knoxville.tif", data / "dem.tif"],
            work,
            "real-multi",
        )
        compare_zonal(
            terrio,
            data / "TN_Counties.geojson",
            [data / "nlcd_knoxville.tif"],
            work,
            "real-touched",
            all_touched=True,
        )
        compare_sql(terrio, data, work)


if __name__ == "__main__":
    main()
