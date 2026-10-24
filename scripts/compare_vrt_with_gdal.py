#!/usr/bin/env python3
"""Cross-read lazy, warped, derived and kernel-filtered VRTs on real USGS terrain."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import xml.etree.ElementTree as ET

import numpy as np
from osgeo import gdal

from compare_dem_calc_with_gdal import compare, same_value

gdal.UseExceptions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data")
    parser.add_argument("--terrio", default="target/debug/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    records = []
    with tempfile.TemporaryDirectory(prefix="terrio-vrt-") as directory:
        tmp = Path(directory)
        # Match GDAL's installed grid search paths. Terrio's private download cache
        # may contain extra datum grids from unrelated earlier runs.
        env = {**os.environ, "TERRIO_GRID_CACHE": str(tmp / "datum-grids")}
        source = tmp / "source.tif"
        ds = gdal.Translate(str(source), str(Path(args.data) / "usgs_dem.tif"), srcWin=[0, 0, 96, 80], outputType=gdal.GDT_Float32)
        transform, projection = ds.GetGeoTransform(), ds.GetProjection()
        values = ds.ReadAsArray()
        ds = None
        second = tmp / "second.tif"
        ds = gdal.GetDriverByName("GTiff").Create(str(second), 96, 80, 1, gdal.GDT_Float32)
        ds.SetGeoTransform(transform)
        ds.SetProjection(projection)
        ds.GetRasterBand(1).WriteArray(values * np.float32(0.3))
        ds = None

        def run(name, path, authored=False, reference_path=None, tolerance=1e-5, windows=None):
            for window in windows or [None, [17, 13, 31, 27], [0, 0, 11, 9]]:
                label = name + (" whole" if window is None else f" window {window[0]},{window[1]}")
                reference, actual = tmp / "reference.vrt", tmp / "actual.tif"
                try:
                    gdal.Translate(str(reference), str(reference_path or path), srcWin=window, format="VRT")
                    cmd = [args.terrio, "convert", str(path), str(actual)]
                    if window is not None:
                        cmd += ["--srcwin", *map(str, window)]
                    process = subprocess.run(cmd, capture_output=True, text=True, env=env)
                    if process.returncode:
                        raise RuntimeError(process.stderr)
                    record = compare(str(reference), str(actual), tolerance)
                except Exception as error:
                    record = {"error": str(error)}
                record["name"] = label
                if reference_path is not None:
                    record["reference_note"] = "GDAL affine sum path: redundant identity ReprojectTransformer removed; generic GDAL 3.12 polygon clipping loses boundary overlaps"
                    record["absolute_tolerance"] = tolerance
                    if "error" not in record:
                        expected = gdal.Open(str(reference)).ReadAsArray().astype(np.float32)
                        observed = gdal.Open(str(actual)).ReadAsArray().astype(np.float32)
                        ulp = np.maximum(np.abs(np.spacing(expected)), np.abs(np.spacing(observed)))
                        record["max_float32_ulp"] = float(np.max(np.abs(expected - observed) / ulp))
                records.append(record)

        pyramid = tmp / "pyramid.tif"
        ds = gdal.Translate(str(pyramid), str(source))
        ds.BuildOverviews("AVERAGE", [2, 4, 8])
        ds = None
        for overview in ["AUTO", "AUTO-1", "NONE", "0", "1", "2"]:
            reference = tmp / f"overview-gdal-{overview}.vrt"
            gdal.Warp(str(reference), str(pyramid), format="VRT", width=24, height=20,
                      overviewLevel=overview, errorThreshold=0)
            windows = [None, [3, 2, 5, 4], [0, 0, 3, 2]]
            run(f"VRT GDAL source overview {overview}", reference, windows=windows)
            authored = tmp / f"overview-terrio-{overview}.vrt"
            subprocess.run([args.terrio, "warp", str(pyramid), str(authored),
                            "--ts", "24", "20", "--overview", overview],
                           check=True, capture_output=True, text=True, env=env)
            run(f"VRT Terrio source overview {overview}", authored, authored=True, windows=windows)

        def source_xml(tag="SimpleSource", filename="source.tif", extra=""):
            return f'<{tag}><SourceFilename relativeToVRT="1">{filename}</SourceFilename><SourceBand>1</SourceBand>{extra}</{tag}>'

        def write(name, band):
            path = tmp / f"{name}.vrt"
            path.write_text(f'<VRTDataset rasterXSize="96" rasterYSize="80"><SRS>{projection}</SRS><GeoTransform>{",".join(map(str, transform))}</GeoTransform>{band}</VRTDataset>')
            return path

        for label, nodata in [("long-decimal", 1.23456789), ("decimal-tenth", 0.1)]:
            path = write(f"nodata-{label}", f'<VRTRasterBand dataType="Float32"><NoDataValue>{nodata}</NoDataValue>{source_xml()}</VRTRasterBand>')
            run(f"VRT raw decimal nodata {label}", path)

        for function, arguments in [
            ("sum", 'k="3"'), ("diff", ""), ("mul", 'k="0.5"'), ("div", ""),
            ("mean", ""), ("min", ""), ("max", ""), ("sqrt", ""), ("mod", ""),
            ("inv", 'k="3"'), ("pow", 'power="2"'), ("log10", ""), ("dB", 'fact="10"'),
            ("exp", 'base="1.0001" fact="0.2"'),
        ]:
            sources = source_xml()
            if function in ["sum", "diff", "mul", "div", "mean", "min", "max"]:
                sources += source_xml(filename="second.tif")
            band = f'<VRTRasterBand dataType="Float64" band="1" subClass="VRTDerivedRasterBand"><PixelFunctionType>{function}</PixelFunctionType><SourceTransferType>Float64</SourceTransferType><PixelFunctionArguments {arguments}/>{sources}</VRTRasterBand>'
            run(f"VRT derived {function}", write(f"derived-{function}", band))
        for kernel, coefs in [("full", "1 2 1 2 4 2 1 2 1"), ("separable", "1 2 1")]:
            for normalized in [0, 1]:
                for function in [None, "min", "max", "stddev", "median", "mode"]:
                    extra = f'<Kernel normalized="{normalized}"><Size>3</Size><Coefs>{coefs}</Coefs></Kernel>'
                    if function:
                        extra += f"<Function>{function}</Function>"
                    band = f'<VRTRasterBand dataType="Float32" band="1">{source_xml("KernelFilteredSource", extra=extra)}</VRTRasterBand>'
                    name = f"kernel-{kernel}-{normalized}-{function or 'sum'}"
                    run(f"VRT {name}", write(name, band))
        nc = tmp / "source.nc"
        gdal.Translate(str(nc), str(source), format="netCDF", creationOptions=["FORMAT=NC4"])
        selector = f'NETCDF:"{nc}":Band1'
        band = f'<VRTRasterBand dataType="Float32"><SimpleSource><SourceFilename relativeToVRT="0">{selector}</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand>'
        run("VRT netCDF subdataset source", write("netcdf-source", band))
        masked = tmp / "masked.tif"
        ds = gdal.Translate(str(masked), str(source))
        pixels = values.copy()
        pixels[5:12, 7:16] = -9999
        ds.GetRasterBand(1).SetNoDataValue(-9999)
        ds.GetRasterBand(1).WriteArray(pixels)
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        mask = np.full(pixels.shape, 255, dtype="uint8")
        mask[20:30, 25:35] = 0
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        ds = None
        mask_source = source_xml(filename="masked.tif").replace("<SourceBand>1</SourceBand>", "<SourceBand>mask,1</SourceBand>")
        run("VRT source mask", write("mask-band", f'<VRTRasterBand dataType="Byte">{mask_source}</VRTRasterBand>'))
        complex_source = source_xml("ComplexSource", "masked.tif", "<UseMaskBand>true</UseMaskBand>")
        run("VRT complex mask", write("complex-mask", f'<VRTRasterBand dataType="Float32"><NoDataValue>-9999</NoDataValue>{complex_source}</VRTRasterBand>'))
        for kind, coefs in [("full", "1 2 1 2 4 2 1 2 1"), ("separable", "1 2 1")]:
            extra = f'<Kernel normalized="1"><Size>3</Size><Coefs>{coefs}</Coefs></Kernel>'
            kernel = source_xml("KernelFilteredSource", "masked.tif", extra)
            run(f"VRT kernel nodata {kind}", write(f"kernel-nodata-{kind}", f'<VRTRasterBand dataType="Float32"><NoDataValue>-9999</NoDataValue>{kernel}</VRTRasterBand>'))
        for function in ["sum", "mul", "mean", "min", "max", "sqrt", "inv"]:
            for propagate in ["true", "false"]:
                sources = source_xml(filename="masked.tif")
                if function in ["sum", "mul", "mean", "min", "max"]:
                    sources += source_xml(filename="second.tif")
                band = f'<VRTRasterBand dataType="Float64" subClass="VRTDerivedRasterBand"><NoDataValue>-9999</NoDataValue><PixelFunctionType>{function}</PixelFunctionType><PixelFunctionArguments propagateNoData="{propagate}"/>{sources}</VRTRasterBand>'
                run(f"VRT derived nodata {function} {propagate}", write(f"derived-nodata-{function}-{propagate}", band))
        from osgeo import osr
        gcp_srs = osr.SpatialReference()
        gcp_srs.ImportFromEPSG(32613)
        for method, points, gdal_options, cli in [
            ("polynomial", [(0, 0), (96, 0), (0, 80), (96, 80), (48, 40), (24, 60)], {"polynomialOrder": 1}, ["--order", "1"]),
            ("tps", [(0, 0), (96, 0), (0, 80), (96, 80), (48, 40), (24, 60)], {"tps": True}, ["--tps"]),
            ("homography", [(0, 0), (96, 0), (0, 80), (96, 80)], {}, []),
        ]:
            src = tmp / f"{method}-source.tif"
            ds = gdal.Translate(str(src), str(source))
            ds.SetGCPs([gdal.GCP(500000 + x*30, 4400000 - y*30, 0, x, y) for x, y in points], gcp_srs.ExportToWkt())
            ds = None
            path = tmp / f"{method}-reference.vrt"
            gdal.Warp(str(path), str(src), format="VRT", width=48, height=40, **gdal_options)
            run(f"VRT GDAL {method}", path)
            path = tmp / f"{method}-authored.vrt"
            process = subprocess.run([args.terrio, "warp", str(src), str(path), "--ts", "48", "40", *cli], capture_output=True, text=True, env=env)
            if process.returncode:
                records.append({"name": f"VRT authored {method}", "error": process.stderr})
            else:
                run(f"VRT authored {method}", path)
        src = tmp / "rpc-source.tif"
        ds = gdal.GetDriverByName("GTiff").Create(str(src), 96, 80, 1, gdal.GDT_Float32)
        ds.GetRasterBand(1).WriteArray(values)
        rpc = {"LINE_OFF":"40", "SAMP_OFF":"48", "LAT_OFF":"40", "LONG_OFF":"-106", "HEIGHT_OFF":"0", "LINE_SCALE":"40", "SAMP_SCALE":"48", "LAT_SCALE":".1", "LONG_SCALE":".1", "HEIGHT_SCALE":"100", "LINE_NUM_COEFF":"0 0 -1 " + "0 "*17, "SAMP_NUM_COEFF":"0 1 0 " + "0 "*17, "LINE_DEN_COEFF":"1 " + "0 "*19, "SAMP_DEN_COEFF":"1 " + "0 "*19}
        ds.SetMetadata(rpc, "RPC")
        ds = None
        path = tmp / "rpc-reference.vrt"
        gdal.Warp(str(path), str(src), format="VRT", rpc=True, width=48, height=40)
        run("VRT GDAL rpc", path)
        path = tmp / "rpc-authored.vrt"
        process = subprocess.run([args.terrio, "warp", str(src), str(path), "--ts", "48", "40", "--rpc"], capture_output=True, text=True, env=env)
        if process.returncode:
            records.append({"name": "VRT authored rpc", "error": process.stderr})
        else:
            run("VRT authored rpc", path)
        multiband = tmp / "multiband.tif"
        ds = gdal.GetDriverByName("GTiff").Create(str(multiband), 96, 80, 3, gdal.GDT_Float32)
        ds.SetGeoTransform(transform)
        ds.SetProjection(projection)
        for band in range(1, 4):
            pixels = values.copy() + band * 10
            pixels[7 * band:7 * band + 8, 9 * band:9 * band + 9] = -9999
            ds.GetRasterBand(band).WriteArray(pixels)
            ds.GetRasterBand(band).SetNoDataValue(-9999)
        ds = None
        for label, options in [("auto", {}), ("ignore", {"srcNodata": "None"}),
                               ("unified", {"srcNodata": -9999}),
                               ("per-band", {"warpOptions": ["UNIFIED_SRC_NODATA=NO"]})]:
            path = tmp / f"multiband-{label}.vrt"
            gdal.Warp(str(path), str(multiband), format="VRT", width=64, height=56,
                      resampleAlg="bilinear", **options)
            run(f"VRT multiband nodata {label}", path)
            actual = tmp / "direct-warp.tif"
            command = [args.terrio, "warp", str(multiband), str(actual), "--ts", "64", "56", "-r", "bilinear"]
            if label == "ignore":
                command += ["--srcnodata", "none"]
            elif label == "unified":
                command += ["--srcnodata", "-9999"]
            elif label == "per-band":
                command += ["--wo", "UNIFIED_SRC_NODATA=NO"]
            process = subprocess.run(command, capture_output=True, text=True, env=env)
            record = compare(str(path), str(actual), 1e-5) if process.returncode == 0 else {"error": process.stderr}
            record["name"] = f"VRT shared materialized warper nodata {label}"
            records.append(record)

        for algorithm in ["near", "cubic", "cubicspline", "lanczos", "average", "mode", "min", "max", "med", "q1", "q3", "rms", "sum"]:
            for label, options in [("partial", {}), ("no", {"warpOptions": ["UNIFIED_SRC_NODATA=NO"]}),
                                   ("yes", {"srcNodata": -9999}), ("ignore", {"srcNodata": "None"})]:
                path = tmp / f"multiband-{algorithm}-{label}.vrt"
                gdal.Warp(str(path), str(multiband), format="VRT", width=64, height=56,
                          resampleAlg=algorithm, **options)
                reference_path = None
                if algorithm == "sum":
                    # Removing only the identity CRS transformer enables GDAL's conservative
                    # affine sum. Its generic polygon clipping otherwise drops boundary areas.
                    root = ET.parse(path).getroot()
                    transformer = root.find(".//GenImgProjTransformer")
                    for child in list(transformer):
                        if child.tag == "ReprojectTransformer":
                            transformer.remove(child)
                    reference_path = tmp / f"sum-affine-reference-{label}.vrt"
                    ET.ElementTree(root).write(reference_path, encoding="unicode")
                run(f"VRT multiband {algorithm} nodata {label}", path, reference_path=reference_path,
                    tolerance=0.002 if algorithm == "sum" else 1e-5)

        for label, source_bands, source_nodata, destination_nodata in [
            ("reordered", [3, 1, 2], [-9999, -9998, -9997], [-9999] * 3),
            ("destination", [1, 2, 3], [-9999] * 3, [-101, -202, -303]),
            ("duplicate", [1, 1, 1], [-9999] * 3, [-101, -202, -303]),
        ]:
            root = ET.parse(tmp / "multiband-auto.vrt").getroot()
            for index, mapping in enumerate(root.find("GDALWarpOptions/BandList")):
                mapping.set("src", str(source_bands[index]))
                mapping.find("SrcNoDataReal").text = str(source_nodata[index])
                mapping.find("DstNoDataReal").text = str(destination_nodata[index])
            for index, band in enumerate(root.findall("VRTRasterBand")):
                band.find("NoDataValue").text = str(destination_nodata[index])
            path = tmp / f"multiband-{label}.vrt"
            ET.ElementTree(root).write(path, encoding="unicode")
            run(f"VRT multiband mapping {label}", path)
        # Mixed-band buffers use GDALDataTypeUnion; Python's default ReadAsArray
        # chooses Float32 for every mixed dataset, which can discard Float64 precision.
        mixed_source = tmp / "mixed-source.tif"
        mixed = gdal.GetDriverByName("GTiff").Create(str(mixed_source), 96, 80, 2, gdal.GDT_Float64)
        mixed.SetGeoTransform(transform)
        mixed.SetProjection(projection)
        samples = values.astype(np.float64) / 1000.0
        samples[0, :3] = [0., 0.25, -9999.]
        for band in [1, 2]:
            mixed.GetRasterBand(band).WriteArray(samples + (1.250000000001 if band == 2 else 0.))
            mixed.GetRasterBand(band).SetNoDataValue(-9999.)
        mixed = None

        def mixed_compare(name, path):
            record = {"name": name}
            try:
                reference = gdal.Open(str(path))
                dtype = reference.GetRasterBand(1).DataType
                for band in range(2, reference.RasterCount + 1):
                    dtype = gdal.DataTypeUnion(dtype, reference.GetRasterBand(band).DataType)
                expected = reference.ReadAsArray(buf_type=dtype)
                actual_path = tmp / "mixed-actual.tif"
                result = subprocess.run([args.terrio, "convert", str(path), str(actual_path)], capture_output=True, text=True, env=env)
                if result.returncode:
                    raise RuntimeError(result.stderr)
                actual = gdal.Open(str(actual_path))
                observed = actual.ReadAsArray()
                np.testing.assert_array_equal(observed, expected)
                assert all(actual.GetRasterBand(band).DataType == dtype for band in range(1, actual.RasterCount + 1))
                assert actual.GetGeoTransform() == reference.GetGeoTransform()
                assert actual.GetSpatialRef().IsSame(reference.GetSpatialRef())
                assert all(same_value(actual.GetRasterBand(band).GetNoDataValue(), reference.GetRasterBand(band).GetNoDataValue())
                    for band in range(1, reference.RasterCount + 1))
                record.update(exact=1., within_tol=1., same_type=True, same_nodata=True, gt_diff_px=0.)
            except Exception as error:
                record["error"] = str(error)
            records.append(record)

        for native in ["Float32", "Float64"]:
            path = write("mixed-plain-" + native,
                f'<VRTRasterBand dataType="Byte" band="1">{source_xml(filename="mixed-source.tif")}</VRTRasterBand>'
                f'<VRTRasterBand dataType="{native}" band="2">{source_xml(filename="mixed-source.tif")}</VRTRasterBand>')
            mixed_compare("VRT mixed Byte+" + native, path)
        for working in ["Float32", "Float64"]:
            path = tmp / f"mixed-warp-{working}.vrt"
            warped = gdal.Warp(str(path), str(mixed_source), format="VRT", width=96, height=80,
                srcNodata=-9999., dstNodata=0., warpOptions=["UNIFIED_SRC_NODATA=PARTIAL"])
            warped = None
            root = ET.parse(path)
            native = root.getroot().findall("VRTRasterBand")
            native[0].set("dataType", "Byte")
            native[1].set("dataType", "Float64")
            root.find(".//WorkingDataType").text = working
            root.write(path)
            mixed_compare("VRT mixed warped working " + working, path)

        for resampling in ["near", "bilinear", "cubic", "average"]:
            path = tmp / f"warped-{resampling}.vrt"
            gdal.Warp(str(path), str(source), format="VRT", dstSRS="EPSG:3857", width=64, height=56, resampleAlg=resampling)
            run(f"VRT GDAL warped {resampling}", path)
            path = tmp / f"authored-{resampling}.vrt"
            process = subprocess.run([args.terrio, "warp", str(source), str(path), "--t-srs", "EPSG:3857", "--ts", "64", "56", "-r", resampling], capture_output=True, text=True, env=env)
            if process.returncode:
                records.append({"name": f"VRT authored {resampling}", "error": process.stderr})
            else:
                run(f"VRT authored {resampling}", path, True)
    if args.json:
        print(json.dumps(records, indent=2))
    else:
        for record in records:
            print(record["name"], record.get("exact", record.get("error")))
    return int(any("error" in r or r.get("within_tol") != 1 or not r.get("same_type") or not r.get("same_nodata") or r.get("gt_diff_px") != 0 or r.get("max_float32_ulp", 0) > 1 for r in records))


if __name__ == "__main__":
    raise SystemExit(main())
