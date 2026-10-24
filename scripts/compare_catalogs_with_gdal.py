#!/usr/bin/env python3
"""Compare indexed real DEM mosaics with GDAL GTI, including small-window HTTP I/O.

GeoPackage/FlatGeobuf indexes are written by both GDAL and Terrio. Tiles are real
Las Vegas DEM windows; tests cover z-order, nodata, masks, alpha and mixed CRSs.
"""

import argparse
import functools
import http.server
import json
import subprocess
import tempfile
import threading
from pathlib import Path

import numpy as np
from osgeo import gdal, ogr
from compare_ops_with_gdal import compare

gdal.UseExceptions()
ogr.UseExceptions()


def nodata_equal(left, right):
    return left == right or (
        left is not None and right is not None and np.isnan(left) and np.isnan(right)
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-catalog-") as directory:
        directory = Path(directory)
        source = str(args.data / "las_vegas_dem.tif")
        for name, x in [("left", 100), ("right", 164)]:
            gdal.Translate(
                str(directory / f"{name}.tif"),
                source,
                srcWin=[x, 100, 128, 128],
                bandList=[1],
                outputType=gdal.GDT_Float32,
                creationOptions=["TILED=YES", "BLOCKXSIZE=32", "BLOCKYSIZE=32"],
            )
        left = gdal.Open(str(directory / "left.tif"))
        srs = left.GetSpatialRef()
        gt = left.GetGeoTransform()
        bounds = (gt[0], gt[3] - 128, gt[0] + 192, gt[3])
        right = gdal.Open(str(directory / "right.tif"), gdal.GA_Update)
        # Keep real terrain values, but mirror the second tile so overlapping donors differ:
        # ignoring a mask or z-order must change pixels, even where source extents overlap.
        right.GetRasterBand(1).WriteArray(np.fliplr(right.ReadAsArray()))
        right.FlushCache()
        right_bounds = (gt[0] + 64, gt[3] - 128, gt[0] + 192, gt[3])
        gdal.Warp(
            str(directory / "projected.tif"),
            right,
            dstSRS="EPSG:4326",
            dstNodata=-9999,
            resampleAlg="near",
            creationOptions=["TILED=YES"],
        )
        nodata = gdal.Translate(str(directory / "nodata.tif"), right)
        values = nodata.ReadAsArray()
        values[20:80, 0:32] = -9999
        nodata.GetRasterBand(1).SetNoDataValue(-9999)
        nodata.GetRasterBand(1).WriteArray(values)
        nodata = None
        masked = gdal.Translate(str(directory / "masked.tif"), right)
        masked.CreateMaskBand(gdal.GMF_PER_DATASET)
        mask = np.full((128, 128), 255, dtype="uint8")
        mask[20:80, :32] = 0
        mask[20:80, 32:64] = 128
        masked.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        masked = None
        alpha = gdal.GetDriverByName("GTiff").Create(
            str(directory / "alpha.tif"), 128, 128, 2, gdal.GDT_Float32
        )
        alpha.SetGeoTransform(right.GetGeoTransform())
        alpha.SetSpatialRef(srs)
        alpha.GetRasterBand(1).WriteArray(right.ReadAsArray())
        alpha.GetRasterBand(2).WriteArray(mask)
        alpha.GetRasterBand(2).SetColorInterpretation(gdal.GCI_AlphaBand)
        alpha = None
        untagged = gdal.Translate(
            str(directory / "untagged.tif"), str(directory / "alpha.tif")
        )
        untagged.GetRasterBand(2).SetColorInterpretation(gdal.GCI_Undefined)
        untagged = None
        alpha_left = gdal.GetDriverByName("GTiff").Create(
            str(directory / "left-alpha.tif"), 128, 128, 2, gdal.GDT_Float32
        )
        alpha_left.SetGeoTransform(left.GetGeoTransform())
        alpha_left.SetSpatialRef(srs)
        alpha_left.GetRasterBand(1).WriteArray(left.ReadAsArray())
        alpha_left.GetRasterBand(2).WriteArray(
            np.full((128, 128), 255, dtype="float32")
        )
        alpha_left.GetRasterBand(2).SetColorInterpretation(gdal.GCI_AlphaBand)
        alpha_left = None
        for name, raster, second in [
            ("left-spectral", left, 1),
            ("spectral", right, 0),
        ]:
            spectral = gdal.GetDriverByName("GTiff").Create(
                str(directory / f"{name}.tif"), 128, 128, 2, gdal.GDT_Float32
            )
            spectral.SetGeoTransform(raster.GetGeoTransform())
            spectral.SetSpatialRef(srs)
            spectral.GetRasterBand(1).WriteArray(raster.ReadAsArray())
            spectral.GetRasterBand(2).WriteArray(
                np.full((128, 128), second, dtype="float32")
            )
            spectral.GetRasterBand(2).SetColorInterpretation(gdal.GCI_Undefined)
            spectral = None
        left = right = None

        def write_index(
            path,
            driver,
            tile,
            alpha=False,
            remote=None,
            many=False,
            spectral=False,
            declare_alpha=True,
            grid_size=False,
            declare_nodata=True,
            infer_type=False,
            infer_resolution=False,
        ):
            ds = ogr.GetDriverByName(driver).CreateDataSource(str(path))
            layer = ds.CreateLayer("tiles", srs, ogr.wkbPolygon)
            layer.CreateField(ogr.FieldDefn("location", ogr.OFTString))
            layer.CreateField(ogr.FieldDefn("z", ogr.OFTInteger))
            metadata = {
                "RESX": "1",
                "RESY": "1",
                "BAND_COUNT": "2" if alpha or spectral else "1",
                "DATA_TYPE": "Float32",
                "NODATA": "-9999",
                "SORT_FIELD": "z",
                "MINX": str(bounds[0]),
                "MINY": str(bounds[1]),
                "MAXX": str(bounds[2]),
                "MAXY": str(bounds[3]),
            }
            if not declare_nodata:
                metadata.pop("NODATA")
            if infer_type:
                metadata.pop("DATA_TYPE")
            if infer_resolution:
                metadata.pop("RESX")
                metadata.pop("RESY")
            if grid_size:
                metadata.pop("RESX")
                metadata.pop("RESY")
                metadata.update(
                    XSIZE="96",
                    YSIZE="64",
                    GEOTRANSFORM=f"{bounds[0]},2,0,{bounds[3]},0,-2",
                )
            if alpha and declare_alpha:
                metadata["COLOR_INTERPRETATION"] = "gray,alpha"
            layer.SetMetadata(metadata)
            entries = [
                (
                    "left-spectral.tif"
                    if spectral
                    else "left-alpha.tif"
                    if alpha
                    else "left.tif",
                    bounds[0],
                    10,
                ),
                (tile, right_bounds[0], 20),
            ]
            if many:
                entries += [
                    (f"missing-{i}.tif", bounds[0] + 1000 + i * 128, 0)
                    for i in range(1000)
                ]
            for location, x, z in entries:
                feature = ogr.Feature(layer.GetLayerDefn())
                feature.SetField(
                    "location", f"/vsicurl/{remote}/{location}" if remote else location
                )
                feature.SetField("z", z)
                ring = ogr.Geometry(ogr.wkbLinearRing)
                for a, b in [
                    (x, bounds[1]),
                    (x + 128, bounds[1]),
                    (x + 128, bounds[3]),
                    (x, bounds[3]),
                    (x, bounds[1]),
                ]:
                    ring.AddPoint_2D(a, b)
                polygon = ogr.Geometry(ogr.wkbPolygon)
                polygon.AddGeometry(ring)
                feature.SetGeometry(polygon)
                layer.CreateFeature(feature)
            ds = None

        def check(name, index, alpha=False, bands=None):
            reference = str(directory / "reference.tif")
            actual = str(directory / "actual.tif")
            gdal.Translate(
                reference,
                gdal.Open(str(index)),
                **({"bandList": bands} if bands else {}),
            )
            command = [args.terrio, "catalog", str(index), actual]
            if bands:
                command = [args.terrio, "translate", str(index), actual]
                for band in bands:
                    command += ["-b", str(band)]
            if alpha:
                command += ["--co", "ALPHA=YES"]
            proc = subprocess.run(command, capture_output=True, text=True)
            if proc.returncode:
                results.append({"name": name, "error": proc.stderr})
                return
            result = compare(reference, actual, 1e-4)
            result["name"] = name
            ref_ds, actual_ds = gdal.Open(reference), gdal.Open(actual)
            same_roles = all(
                ref_ds.GetRasterBand(b).GetColorInterpretation()
                == actual_ds.GetRasterBand(b).GetColorInterpretation()
                for b in range(1, ref_ds.RasterCount + 1)
            )
            same_masks = all(
                np.array_equal(
                    ref_ds.GetRasterBand(b).GetMaskBand().ReadAsArray(),
                    actual_ds.GetRasterBand(b).GetMaskBand().ReadAsArray(),
                )
                for b in range(1, ref_ds.RasterCount + 1)
            )
            same_nodata = all(
                ref_ds.GetRasterBand(b).GetNoDataValue()
                == actual_ds.GetRasterBand(b).GetNoDataValue()
                for b in range(1, ref_ds.RasterCount + 1)
            )
            result.update(
                same_color_interpretation=same_roles,
                same_masks=same_masks,
                same_nodata=same_nodata,
            )
            if not same_roles or not same_masks or not same_nodata:
                result["error"] = "color interpretation or validity masks differ"
            results.append(result)

        for driver, extension in [("GPKG", "gpkg"), ("FlatGeobuf", "fgb")]:
            for variant in [
                "right",
                "nodata",
                "masked",
                "untagged",
                "alpha",
                "spectral",
                "projected",
            ]:
                index = directory / f"{variant}-{extension}.gti.{extension}"
                write_index(
                    index,
                    driver,
                    f"{variant}.tif",
                    alpha=variant == "alpha",
                    spectral=variant == "spectral",
                )
                check(f"GTI {extension} {variant}", index, alpha=variant == "alpha")
                if variant in ["alpha", "spectral"]:
                    band = 1 if variant == "alpha" else 2
                    check(
                        f"GTI {extension} {variant} selected band {band}",
                        index,
                        bands=[band],
                    )
            for infer_type, infer_resolution in [
                (False, False),
                (True, False),
                (False, True),
            ]:
                optional = (
                    directory
                    / f"no-nodata-{infer_type}-{infer_resolution}-{extension}.gti.{extension}"
                )
                write_index(
                    optional,
                    driver,
                    "nodata.tif",
                    declare_nodata=False,
                    infer_type=infer_type,
                    infer_resolution=infer_resolution,
                )
                check(
                    f"GTI {extension} optional metadata (infer type={infer_type}, resolution={infer_resolution})",
                    optional,
                )
            size_index = directory / f"size-{extension}.gti.{extension}"
            write_index(size_index, driver, "right.tif", grid_size=True)
            check(f"GTI {extension} XSIZE/YSIZE metadata", size_index)
            # Terrio-written indexes cross-read by GDAL. Explicit GTI XML supplies mosaic
            # characteristics, since a generic vector conversion does not retain layer metadata.
            geojson = directory / "tiles.geojson"
            original = directory / f"right-{extension}.gti.{extension}"
            gdal.VectorTranslate(str(geojson), str(original), format="GeoJSON")
            written = directory / f"terrio-{extension}.{extension}"
            subprocess.run(
                [args.terrio, "convert", str(geojson), str(written)],
                check=True,
                capture_output=True,
            )
            descriptor = directory / f"terrio-{extension}.gti"
            descriptor.write_text(
                f"<GDALTileIndexDataset><IndexDataset>{written}</IndexDataset>"
                "<SortField>z</SortField><ResX>1</ResX><ResY>1</ResY><BandCount>1</BandCount>"
                '<Band band="1" dataType="Float32"><NoDataValue>-9999</NoDataValue></Band>'
                "</GDALTileIndexDataset>"
            )
            check(f"GTI {extension} Terrio index", descriptor)
            size_descriptor = directory / f"terrio-size-{extension}.gti"
            size_descriptor.write_text(
                descriptor.read_text().replace(
                    "<ResX>1</ResX><ResY>1</ResY>",
                    f"<XSize>96</XSize><YSize>64</YSize><GeoTransform>{bounds[0]},2,0,{bounds[3]},0,-2</GeoTransform><MinX>{bounds[0]}</MinX>"
                    f"<MinY>{bounds[1]}</MinY><MaxX>{bounds[2]}</MaxX><MaxY>{bounds[3]}</MaxY>",
                )
            )
            check(f"GTI {extension} XML XSize/YSize", size_descriptor)

        class RangeHandler(http.server.SimpleHTTPRequestHandler):
            bytes_sent = 0
            paths = set()
            lock = threading.Lock()

            def log_message(self, *_):
                pass

            def send_head(self):
                path = Path(self.translate_path(self.path))
                if not path.is_file():
                    self.send_error(404)
                    return None
                file = path.open("rb")
                size = path.stat().st_size
                start, end = 0, size - 1
                if self.headers.get("Range"):
                    span = self.headers["Range"].removeprefix("bytes=").split("-", 1)
                    start = int(span[0])
                    end = min(int(span[1]) if span[1] else end, end)
                    self.send_response(206)
                    self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
                else:
                    self.send_response(200)
                self.send_header("Content-Length", str(end - start + 1))
                self.send_header("Accept-Ranges", "bytes")
                self.end_headers()
                file.seek(start)
                self.length = end - start + 1
                return file

            def copyfile(self, source, output):
                payload = source.read(self.length)
                with type(self).lock:
                    type(self).bytes_sent += len(payload)
                    type(self).paths.add(self.path)
                output.write(payload)

        server = http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0), functools.partial(RangeHandler, directory=str(directory))
        )
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            remote_alpha = directory / "remote-alpha.gti.gpkg"
            write_index(
                remote_alpha,
                "GPKG",
                "alpha.tif",
                alpha=True,
                remote=f"http://127.0.0.1:{server.server_port}",
                declare_alpha=False,
            )
            check("GTI remote tagged alpha", remote_alpha)
            RangeHandler.bytes_sent = 0
            RangeHandler.paths = set()
            index = directory / "remote.gti.gpkg"
            write_index(
                index,
                "GPKG",
                "right.tif",
                remote=f"http://127.0.0.1:{server.server_port}",
                many=True,
            )
            startup = subprocess.run(
                [args.terrio, "info", str(index), "--json"],
                capture_output=True,
                text=True,
            )
            startup_bytes, startup_sources = (
                RangeHandler.bytes_sent,
                len(RangeHandler.paths),
            )
            actual = str(directory / "small.tif")
            proc = subprocess.run(
                [
                    args.terrio,
                    "catalog",
                    str(index),
                    actual,
                    "--window",
                    "8",
                    "8",
                    "8",
                    "8",
                    "--cache-size",
                    "1",
                    "--stats",
                ],
                capture_output=True,
                text=True,
            )
            try:
                stats = json.loads(proc.stdout) if proc.returncode == 0 else {}
                if not isinstance(stats, dict):
                    stats = {}
            except json.JSONDecodeError:
                stats = {}
            stats.update(
                http_bytes=RangeHandler.bytes_sent,
                http_sources=len(RangeHandler.paths),
                startup_http_bytes=startup_bytes,
                startup_http_sources=startup_sources,
            )
            RangeHandler.bytes_sent = 0
            RangeHandler.paths = set()
            gdal.SetConfigOption("GDAL_DISABLE_READDIR_ON_OPEN", "EMPTY_DIR")
            gdal.SetConfigOption("CPL_VSIL_CURL_ALLOWED_EXTENSIONS", ".tif")
            gdal.VSICurlClearCache()
            reference = str(directory / "small-reference.tif")
            gdal_catalog = gdal.Open(str(index))
            gdal_startup_bytes = RangeHandler.bytes_sent
            gdal.Translate(reference, gdal_catalog, srcWin=[8, 8, 8, 8])
            gdal_catalog = None
            result = compare(reference, actual, 0) if proc.returncode == 0 else {}
            result.update(
                name="GTI small-window HTTP I/O",
                **stats,
                gdal_http_bytes=RangeHandler.bytes_sent,
                gdal_http_sources=len(RangeHandler.paths),
                gdal_startup_http_bytes=gdal_startup_bytes,
            )
            if (
                startup.returncode
                or startup_bytes
                or startup_sources
                or proc.returncode
                or stats.get("index_opens") != 1
                or stats.get("index_queries") != 1
                or stats.get("source_opens") != 1
                or stats.get("selected_features") != 1
                or stats.get("http_sources") != 1
                or not stats.get("http_bytes")
                or stats.get("peak_cached_sources") != 1
            ):
                result["error"] = (
                    proc.stderr if proc.returncode else ""
                ) or f"unexpected I/O counters: {stats}"
            results.append(result)
            # Supplied Python readers must remain lazy too: constructing a catalog must not
            # transfer the large tile's pixels, and an 8x8 query must stay a range read.
            import terrio as bindings

            large_path = directory / "large.tif"
            large = gdal.Translate(
                str(large_path),
                source,
                srcWin=[100, 100, 870, 398],
                outputType=gdal.GDT_Float32,
                creationOptions=[
                    "TILED=YES",
                    "BLOCKXSIZE=64",
                    "BLOCKYSIZE=64",
                    "COMPRESS=NONE",
                ],
            )
            lgt = large.GetGeoTransform()
            large_bounds = (lgt[0], lgt[3] - 398, lgt[0] + 870, lgt[3])
            x0, y0, x1, y1 = large_bounds
            supplied_index = directory / "supplied.gpkg"
            index_ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(supplied_index))
            index_layer = index_ds.CreateLayer("tiles", srs, ogr.wkbPolygon)
            index_layer.CreateField(ogr.FieldDefn("location", ogr.OFTString))
            feature = ogr.Feature(index_layer.GetLayerDefn())
            feature.SetField("location", "large")
            feature.SetGeometry(
                ogr.CreateGeometryFromWkt(
                    f"POLYGON (({x0} {y0},{x1} {y0},{x1} {y1},{x0} {y1},{x0} {y0}))"
                )
            )
            index_layer.CreateFeature(feature)
            feature = index_layer = index_ds = None
            reference = str(directory / "supplied-reference.tif")
            gdal.Translate(reference, large, srcWin=[400, 200, 8, 8])
            large = None
            url = f"http://127.0.0.1:{server.server_port}/large.tif"
            lazy = bindings.open_raster(url, lazy=True)
            for mode, supplied in [("lazy reader", lazy), ("deferred path", url)]:
                RangeHandler.bytes_sent = 0
                RangeHandler.paths = set()
                try:
                    catalog = bindings.open_catalog(
                        str(supplied_index),
                        {"large": supplied, "unused": "nonexistent.tif"},
                        tr=(1, 1),
                        te=large_bounds,
                        bands=1,
                        ot="Float32",
                        nodata=-9999,
                        cache_size=1,
                    )
                    constructor_bytes = RangeHandler.bytes_sent
                    values = catalog.read(1, window=(400, 200, 8, 8))
                    read_bytes = RangeHandler.bytes_sent
                    actual = str(directory / f"supplied-{mode.replace(' ', '-')}.tif")
                    bindings.Raster.from_array(
                        values,
                        (lgt[0] + 400, 1, 0, lgt[3] - 200, 0, -1),
                        crs=srs.ExportToWkt(),
                        nodata=-9999,
                    ).write(actual)
                    result = compare(reference, actual, 0)
                    result.update(
                        constructor_http_bytes=constructor_bytes,
                        window_http_bytes=read_bytes,
                        full_tile_bytes=large_path.stat().st_size,
                    )
                    if (
                        result.get("valid_mismatch")
                        or constructor_bytes
                        or read_bytes >= large_path.stat().st_size // 2
                        or len(RangeHandler.paths) > 1
                    ):
                        result["error"] = f"supplied source was not lazy: {result}"
                except Exception as error:
                    result = {"error": str(error)}
                result["name"] = f"GTI supplied {mode} HTTP window I/O"
                results.append(result)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
    print(
        json.dumps(results)
        if args.json
        else "\n".join(f"{r['name']}: {r}" for r in results)
    )
    return int(any("error" in result for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
