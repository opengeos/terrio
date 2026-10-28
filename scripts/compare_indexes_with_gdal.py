"""Compare raster/vector indexes and GTI catalogs against GDAL 3.13 on real data."""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
import traceback
from contextlib import nullcontext
from pathlib import Path

import numpy as np
from osgeo import gdal, ogr, osr


def run(args):
    result = subprocess.run(list(map(str, args)), capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(result.stderr[-3000:])


def compare(a, b, *, metadata=False):
    left, right = ogr.Open(str(a)), ogr.Open(str(b))
    assert left is not None and right is not None, "cannot open indexes"
    x, y = left.GetLayer(0), right.GetLayer(0)
    assert x.GetFeatureCount() == y.GetFeatureCount(), "feature count"
    assert bool(x.GetSpatialRef()) == bool(y.GetSpatialRef()), "CRS presence"
    if x.GetSpatialRef():
        assert x.GetSpatialRef().IsSame(y.GetSpatialRef()), "CRS"
    schema = lambda layer: [(f.GetName(), f.GetType()) for f in layer.schema]
    assert schema(x) == schema(y), (schema(x), schema(y))
    # Preserve duplicate locations (dataset-name-only and repeated inputs).
    features = lambda layer: sorted(list(layer), key=lambda f: (f.GetField(0), f.GetGeometryRef().GetEnvelope()))
    for f, h in zip(features(x), features(y), strict=True):
        key = f.GetField(0)
        assert key == h.GetField(0), "source locations"
        for i in range(f.GetFieldCount()):
            u, v = f.GetField(i), h.GetField(i)
            if isinstance(u, float):
                assert np.isclose(u, v, rtol=1e-12, atol=1e-12), (u, v)
            else:
                assert u == v, (key, i, u, v)
        u = json.loads(f.GetGeometryRef().ExportToJson())["coordinates"]
        v = json.loads(h.GetGeometryRef().ExportToJson())["coordinates"]
        np.testing.assert_allclose(u, v, rtol=0, atol=1e-8)
    if metadata:
        # GDAL stores LOCATION_FIELD implicitly for the default field name.
        for key, value in x.GetMetadata().items():
            actual = y.GetMetadataItem(key)
            if key in {"MINX", "MINY", "MAXX", "MAXY", "RESX", "RESY"}:
                assert actual is not None and float(actual) == float(value), (key,value,actual)
            else:
                assert actual == value, (key, value, y.GetMetadata())
    return "schema/CRS/text/integer fields exact; real fields within 1e-12; vertices within 1e-8 index CRS units"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--work-dir", type=Path)
    args = parser.parse_args()
    gdal.UseExceptions()
    ogr.UseExceptions()
    if int(gdal.VersionInfo("VERSION_NUM")) < 3130000:
        raise RuntimeError("GDAL 3.13 or newer is required")
    data = args.data.resolve()
    results = []
    grid_cache = tempfile.TemporaryDirectory(prefix="terrio-index-grids-")
    common_paths = osr.GetPROJSearchPaths()
    os.environ.update(PROJ_NETWORK="OFF", TERRIO_NETWORK="OFF",
                      XDG_DATA_HOME=grid_cache.name, TERRIO_GRID_CACHE=grid_cache.name,
                      TERRIO_GRID_DIR=os.pathsep.join(common_paths),
                      PROJ_DATA=os.pathsep.join(common_paths), PROJ_LIB=os.pathsep.join(common_paths))
    osr.SetPROJEnableNetwork(False)

    def case(name, fn):
        try:
            detail = fn()
            results.append(dict(name=name, status="identical" if not detail else "tolerance", detail=detail or ""))
        except Exception as error:
            traceback.print_exc()
            results.append(dict(name=name, status="differs", detail=str(error)))
        if not args.json:
            print(f"{results[-1]['status']:12} {name}: {results[-1]['detail']}")

    if args.work_dir:
        args.work_dir.mkdir(parents=True, exist_ok=True)
    with nullcontext(args.work_dir) if args.work_dir else tempfile.TemporaryDirectory(prefix="terrio-index-") as directory:
        work = Path(directory)
        cog, dem = data / "cog.tif", data / "usgs_dem.tif"
        if not dem.exists():
            dem = next(data.glob("USGS*.tif"))
        assert cog.exists() and dem.exists(), "need real COG and USGS DEM fixtures"
        # Real DEM values in small affine/CRS fixtures expose rotated-grid filtering
        # and mixed CRS behavior without manufacturing pixel values.
        rotated = work / "rotated.tif"
        ds = gdal.Translate(str(rotated), str(dem), srcWin=[0, 0, 4, 6])
        ds.SetGeoTransform([10, 2, .5, 20, .25, -3])
        srs = osr.SpatialReference()
        srs.ImportFromEPSG(4326)
        ds.SetSpatialRef(srs)
        ds.SetMetadataItem("QUALITY", "23")
        ds = None
        mercator = work / "mercator.tif"
        ds = gdal.Translate(str(mercator), str(rotated))
        srs.ImportFromEPSG(3857)
        ds.SetSpatialRef(srs)
        ds = None

        counter = 0

        def indexes(name, sources, options=(), *, vector=False, gti=False, suffix="gpkg"):
            nonlocal counter
            counter += 1
            a, b = work / f"gdal-{counter}.{suffix}", work / f"terrio-{counter}.{suffix}"
            command = ["gdal", "vector" if vector else "raster", "index"]
            if gti:
                command = ["gdal", "driver", "gti", "create"]
            common = [*sources, "--output-layer", "tileindex", *options]
            run([*command, *common, a])
            run([args.terrio, "vector-index" if vector else "gti-create" if gti else "raster-index", *common, b])
            detail = compare(a, b, metadata=suffix in ("gpkg", "fgb"))
            if gti:
                left, right = gdal.Open("GTI:" + str(a)), gdal.Open("GTI:" + str(b))
                assert left is not None and right is not None
                assert left.RasterCount == right.RasterCount
                shape_equal = (left.RasterXSize, left.RasterYSize) == (right.RasterXSize, right.RasterYSize)
                # GPKG's GDAL-authored cached bounds are rounded to 15 digits.
                # GTI ceil() can consequently add one border row/column. Do not
                # compare different geographic extents via whole-image resizing.
                assert abs(left.RasterXSize-right.RasterXSize) <= 1
                assert abs(left.RasterYSize-right.RasterYSize) <= 1
                np.testing.assert_allclose(left.GetGeoTransform(), right.GetGeoTransform(), rtol=0, atol=1e-12)
                if "--bbox" in options:
                    assert shape_equal
                if not shape_equal:
                    assert suffix == "gpkg" and "--bbox" not in options
                    detail += "; GDAL cached-bound rounding adds at most one border pixel"
                assert left.GetSpatialRef().IsSame(right.GetSpatialRef())
                for i in range(1, left.RasterCount + 1):
                    x, y = left.GetRasterBand(i), right.GetRasterBand(i)
                    assert x.DataType == y.DataType
                    np.testing.assert_equal(x.GetNoDataValue(), y.GetNoDataValue())
                    assert x.GetColorInterpretation() == y.GetColorInterpretation()
                # Window and resampled whole mosaic queries compare actual DEM values.
                np.testing.assert_array_equal(left.ReadAsArray(0, 0, min(8, left.RasterXSize), min(8, left.RasterYSize)), right.ReadAsArray(0, 0, min(8, right.RasterXSize), min(8, right.RasterYSize)))
                width, height = min(left.RasterXSize,right.RasterXSize), min(left.RasterYSize,right.RasterYSize)
                np.testing.assert_array_equal(left.ReadAsArray(0,0,width,height,buf_xsize=64,buf_ysize=64), right.ReadAsArray(0,0,width,height,buf_xsize=64,buf_ysize=64))
                detail += "; GTI band metadata and shared-window queried pixels exact"
            return detail

        for suffix in ("gpkg", "fgb", "shp"):
            case(f"raster extents {suffix}", lambda suffix=suffix: indexes("raster", [cog, dem], ["--absolute-path", "--source-crs-field-name", "srs", "--output-crs", "EPSG:4326"], suffix=suffix))
        for options in ([], ["--min-pixel-size", "2.7"], ["--max-pixel-size", "3"], ["--min-pixel-size", "4"], ["--output-crs", "EPSG:3857"]):
            case(f"rotated raster {options}", lambda options=options: indexes("rotated", [rotated], options))
        for target in ([], ["--output-crs", "EPSG:4326"]):
            case(f"mixed CRS {target}", lambda target=target: indexes("mixed", [rotated, mercator], [*target, "--source-crs-field-name", "srs"]))
        case("duplicate raster", lambda: indexes("duplicates", [cog, cog]))
        case("mixed CRS default skip", lambda: indexes("skip", [rotated, mercator]))
        case("custom source field", lambda: indexes("location", [dem], ["--location-name", "tile", "--source-crs-field-name", "srs", "--source-crs-format", "EPSG"]))
        for suffix in ("gpkg", "fgb"):
            case(f"GTI real DEM {suffix}", lambda suffix=suffix: indexes("gti", [dem], ["--absolute-path", "--band-count", "1", "--ot", "Float32", "--color-interpretation", "gray"], gti=True, suffix=suffix))
        metadata_source = work / "metadata.tif"
        ds = gdal.Translate(str(metadata_source), str(dem), srcWin=[0, 0, 64, 64])
        ds.SetMetadataItem("QUALITY", "23")
        ds = None
        case("GTI typed metadata and pixel size", lambda: indexes("metadata", [metadata_source], ["--fetch-metadata", "QUALITY,quality,Integer", "--fetch-metadata", "{PIXEL_SIZE},resolution,Real", "--metadata", "TITLE=DEM & index"], gti=True))
        src = gdal.Open(str(dem))
        gt = src.GetGeoTransform()
        bbox = [gt[0], gt[3]+src.RasterYSize*gt[5], gt[0]+src.RasterXSize*gt[1], gt[3]]
        case("GTI explicit grid", lambda: indexes("explicit", [dem], ["--resolution", f"{gt[1]},{-gt[5]}", "--bbox", ",".join(map(str,bbox))], gti=True))
        discovery = work / "discovery"
        discovery.mkdir()
        (discovery / "nested").mkdir()
        shutil.copyfile(cog, discovery / "a.tif")
        shutil.copyfile(dem, discovery / "nested" / "b.tif")
        (discovery / "ignore.txt").write_text("not a raster")
        case("recursive discovery filter", lambda: indexes("discovery", [discovery], ["--recursive", "--filename-filter", "*.tif", "--output-crs", "EPSG:4326"]))
        counties = data / "TN_Counties.geojson"
        multi = data / "multi.gpkg"
        for source in (counties, multi):
            for suffix in ("gpkg", "fgb", "shp"):
                case(f"vector {source.name} {suffix}", lambda source=source, suffix=suffix: indexes("vector", [source], ["--accept-different-schemas", "--source-crs-field-name", "srs"], vector=True, suffix=suffix))
        case("duplicate vector inputs", lambda: indexes("duplicates", [counties,counties], vector=True))
        for options in (["--source-layer-index", "1"], ["--dataset-name-only"], ["--output-crs", "EPSG:3857", "--accept-different-schemas"]):
            case(f"vector options {options}", lambda options=options: indexes("vector", [multi], options, vector=True))
    grid_cache.cleanup()
    if args.json:
        print(json.dumps(results, indent=2))
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
