"""Untimed correctness gates for benchmark.py; differences suppress speed ratios.

Rasters: exact base/overview pixels, masks, types and nodata; affine tolerance 1e-8
absolute / 1e-12 relative; semantic CRS equality. Vector features: all attributes and
normalized geometry WKB, independent of order and generated FIDs, with exact coordinates.
Statistics: independent NumPy block reduction, relative tolerance 1e-6, absolute 1e-8.
"""

import hashlib
import json
import math
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()


def same_crs(a, b):
    """Compare CRS definitions semantically, including missing CRS."""
    return (a is None and b is None) or (a is not None and b is not None and bool(a.IsSame(b)))


def same_number(a, b):
    return a == b or (a is not None and b is not None and math.isnan(a) and math.isnan(b))


def compare_rasters(a, b):
    """Read every base and overview pixel in bounded strips, including validity masks."""
    if (a.RasterXSize, a.RasterYSize, a.RasterCount) != (
        b.RasterXSize,
        b.RasterYSize,
        b.RasterCount,
    ):
        return ["raster dimensions/band count differ"]
    if not np.allclose(a.GetGeoTransform(), b.GetGeoTransform(), rtol=1e-12, atol=1e-8):
        return ["geotransforms differ"]
    if not same_crs(a.GetSpatialRef(), b.GetSpatialRef()):
        return ["CRS differs"]
    for i in range(1, a.RasterCount + 1):
        x, y = a.GetRasterBand(i), b.GetRasterBand(i)
        if x.DataType != y.DataType or not same_number(x.GetNoDataValue(), y.GetNoDataValue()):
            return [f"band {i}: type/nodata differs"]
        if x.GetColorInterpretation() != y.GetColorInterpretation():
            return [f"band {i}: color interpretation differs"]
        if (x.GetScale(), x.GetOffset(), x.GetUnitType()) != (
            y.GetScale(),
            y.GetOffset(),
            y.GetUnitType(),
        ):
            return [f"band {i}: scale/offset/units differ"]
        xc, yc = x.GetColorTable(), y.GetColorTable()
        if bool(xc) != bool(yc) or (xc and [xc.GetColorEntry(k) for k in range(xc.GetCount())] != [yc.GetColorEntry(k) for k in range(yc.GetCount())]):
            return [f"band {i}: color tables differ"]
        if x.GetOverviewCount() != y.GetOverviewCount():
            return [f"band {i}: overview count differs"]
        for level in range(-1, x.GetOverviewCount()):
            xb, yb = (x, y) if level == -1 else (x.GetOverview(level), y.GetOverview(level))
            if (xb.XSize, xb.YSize) != (yb.XSize, yb.YSize):
                return [f"band {i}: overview dimensions differ"]
            for row in range(0, xb.YSize, 128):
                height = min(128, xb.YSize - row)
                xa, ya = (band.ReadAsArray(0, row, xb.XSize, height) for band in (xb, yb))
                if not np.array_equal(xa, ya, equal_nan=True):
                    different = np.count_nonzero(~((xa == ya) | (np.isnan(xa) & np.isnan(ya))))
                    return [f"band {i}, level {level}, rows {row}:{row + height}: {different} differing pixels (exact check)"]
                xm, ym = (band.GetMaskBand().ReadAsArray(0, row, xb.XSize, height) for band in (xb, yb))
                if not np.array_equal(xm, ym):
                    return [f"band {i}: validity masks differ"]
    return []


def numpy_statistics(band):
    """Stable merged moments from actual valid samples, independent of GDAL statistics."""
    count, mean, m2, minimum, maximum = 0, 0.0, 0.0, math.inf, -math.inf
    for row in range(0, band.YSize, 128):
        height = min(128, band.YSize - row)
        data = band.ReadAsArray(0, row, band.XSize, height).astype(np.float64)
        mask = band.GetMaskBand().ReadAsArray(0, row, band.XSize, height) != 0
        data = data[mask & np.isfinite(data)]
        n = data.size
        if not n:
            continue
        local_mean = float(data.mean())
        delta = local_mean - mean
        m2 += float(np.square(data - local_mean).sum()) + delta * delta * count * n / (count + n)
        mean += delta * n / (count + n)
        count += n
        minimum, maximum = (
            min(minimum, float(data.min())),
            max(maximum, float(data.max())),
        )
    if not count:
        raise ValueError("no valid samples")
    return [minimum, maximum, mean, math.sqrt(m2 / count)]


def validate_summary(tcmd, gcmd, a, b):
    """Check the actual captured summaries, using source pixels as the statistics oracle."""
    # This helper installs a global GDAL error handler; load it only for summary checks.
    from compare_with_gdal import compare_raster, compare_vector, epsg_of
    from osgeo import osr

    t, g = (json.loads((Path(d) / "stdout.json").read_text()) for d in (a, b))
    # The fixed info-case catalog places all options before the source path.
    source = tcmd[-1]
    if t["type"] == "vector":
        differences = compare_vector(source, t)
        # Verify GDAL's timed summary as well, rather than assuming successful exit is enough.
        ds = ogr.Open(source)
        if len(g["layers"]) != ds.GetLayerCount():
            differences.append("GDAL summary layer count differs")
        for layer, summary in zip(ds, g["layers"]):
            if summary["featureCount"] != layer.GetFeatureCount():
                differences.append("GDAL summary feature count differs")
            definition = layer.GetLayerDefn()
            if [f["name"] for f in summary["fields"]] != [definition.GetFieldDefn(i).GetName() for i in range(definition.GetFieldCount())]:
                differences.append("GDAL summary fields differ")
            fields = summary.get("geometryFields", [])
            if fields:
                geometry = fields[0]
                wkt = geometry.get("coordinateSystem", {}).get("wkt")
                srs = osr.SpatialReference(wkt) if wkt else None
                if epsg_of(srs) != epsg_of(layer.GetSpatialRef()):
                    differences.append("GDAL summary CRS differs")
                if "extent" in geometry and layer.GetFeatureCount():
                    extent = layer.GetExtent()
                    expected = [extent[0], extent[2], extent[1], extent[3]]
                    if not np.allclose(geometry["extent"], expected, rtol=1e-9, atol=1e-8):
                        differences.append("GDAL summary extent differs")
            for i, field in enumerate(summary["fields"]):
                expected_type = ogr.GetFieldTypeName(definition.GetFieldDefn(i).GetType())
                if field["type"].split("(")[0] != expected_type:
                    differences.append("GDAL summary field types differ")
        return differences
    # compare_raster normally asks GDAL for statistics; here neither implementation is the oracle.
    no_stats = dict(
        t,
        bands=[{k: v for k, v in band.items() if k != "statistics"} for band in t["bands"]],
    )
    source = "/vsicurl/" + source if source.startswith(("http://", "https://")) else source
    differences = compare_raster(source, no_stats)
    ds = gdal.Open(source)
    wkt = g.get("coordinateSystem", {}).get("wkt")
    srs = osr.SpatialReference(wkt) if wkt else None
    normalized_gdal = {
        "size": g["size"],
        "band_count": len(g["bands"]),
        "geo_transform": g["geoTransform"],
        "crs": {"epsg": epsg_of(srs)},
        "bands": [
            {
                "band": band["band"],
                "data_type": band["type"],
                "nodata": band.get("noDataValue"),
            }
            for band in g["bands"]
        ],
    }
    differences += ["GDAL summary: " + difference for difference in compare_raster(source, normalized_gdal)]
    for i, tb in enumerate(t["bands"]):
        if not tb.get("statistics"):
            if "--stats" in tcmd:
                differences.append(f"terrio band {i + 1}: requested statistics missing")
            continue
        expected = numpy_statistics(ds.GetRasterBand(i + 1))
        ts = tb["statistics"]
        tv = [ts["min"], ts["max"], ts["mean"], ts["std_dev"]]
        gb = g["bands"][i]
        # gdalinfo -stats emits rounded display statistics as well as full precision metadata.
        metadata = gb.get("metadata", {}).get("", {})
        checks = [("terrio", tv)]
        try:
            gv = [float(metadata[f"STATISTICS_{key}"]) for key in ("MINIMUM", "MAXIMUM", "MEAN", "STDDEV")]
        except (KeyError, TypeError, ValueError):
            differences.append(f"GDAL band {i + 1}: statistics missing or invalid")
        else:
            checks.append(("GDAL", gv))
        for name, values in checks:
            if not np.allclose(values, expected, rtol=1e-6, atol=1e-8):
                differences.append(f"{name} statistics differ from NumPy pixel reduction: {values} vs {expected}")
    return differences


def compare_vectors(a, b):
    """Compare every feature via disk-sorted hashes, bounding memory on large datasets.

    Normalize ring starts/orientations and multipart ordering through GEOS. Generated
    feature IDs and layer names are storage details; field definitions and CRS are not.
    """
    sort = shutil.which("gsort") or shutil.which("sort")
    if sort is None:
        return ["vector validation requires external sort with -S/-T support; install coreutils or a compatible BSD sort"]
    if a.GetLayerCount() != b.GetLayerCount():
        return ["vector layer count differs"]
    with tempfile.TemporaryDirectory(prefix="terrio-bench-validation-") as scratch:
        for i in range(a.GetLayerCount()):
            layers = [ds.GetLayer(i) for ds in (a, b)]
            schemas = []
            for layer in layers:
                d = layer.GetLayerDefn()
                schemas.append(
                    [
                        (
                            d.GetFieldDefn(k).GetName(),
                            d.GetFieldDefn(k).GetType(),
                            d.GetFieldDefn(k).GetSubType(),
                        )
                        for k in range(d.GetFieldCount())
                    ]
                )
            if schemas[0] != schemas[1]:
                return [f"layer {i}: field names/types differ"]
            if not same_crs(layers[0].GetSpatialRef(), layers[1].GetSpatialRef()):
                return [f"layer {i}: CRS differs"]
            for tool, layer in enumerate(layers):
                raw = Path(scratch) / f"{tool}.raw"
                with raw.open("wb") as stream:
                    for feature in layer:
                        geom = feature.GetGeometryRef()
                        geometry = bytes(geom.Normalize().ExportToIsoWkb()) if geom else b""
                        attributes = json.dumps(
                            [feature.GetField(k) for k in range(len(schemas[tool]))],
                            ensure_ascii=False,
                            default=str,
                        ).encode()
                        stream.write(hashlib.sha256(attributes + b"\0" + geometry).hexdigest().encode() + b"\n")
                with (Path(scratch) / f"{tool}.sorted").open("wb") as stream:
                    try:
                        subprocess.run(
                            [sort, "-S", "64M", "-T", scratch, str(raw)],
                            stdout=stream,
                            stderr=subprocess.PIPE,
                            text=True,
                            env=dict(os.environ, LC_ALL="C"),
                            check=True,
                        )
                    except subprocess.CalledProcessError as exc:
                        return [f"vector validation sort failed (requires -S/-T support): {exc.stderr.strip()}"]
                    except OSError as exc:
                        return [f"vector validation cannot execute sort: {exc}"]
            with (
                (Path(scratch) / "0.sorted").open("rb") as x,
                (Path(scratch) / "1.sorted").open("rb") as y,
            ):
                while True:
                    xb, yb = x.read(1024 * 1024), y.read(1024 * 1024)
                    if xb != yb:
                        return [f"layer {i}: feature attributes/normalized geometries differ (exact check)"]
                    if not xb:
                        break
    return []


def validate_case(tcmd, gcmd, a, b):
    """Find each command's output and dispatch to an exhaustive correctness gate."""
    if tcmd[1] == "info":
        return validate_summary(tcmd, gcmd, a, b)
    outputs = []
    for cmd, directory in ((tcmd, a), (gcmd, b)):
        candidates = [v for v in cmd if v.startswith(directory + os.sep) and os.path.exists(v)]
        if len(candidates) != 1:
            return [f"cannot identify unique output: {candidates}"]
        outputs.append(candidates[0])
    # Raster outputs are distinguished by their extensions; GeoPackage is vector here.
    if outputs[0].endswith((".tif", ".vrt", ".zarr")):
        return compare_rasters(*(gdal.Open(p) for p in outputs))
    return compare_vectors(*(ogr.Open(p) for p in outputs))
