#!/usr/bin/env python3
"""Compare dataset file-set operations with GDAL 3.13 using real-data derivatives."""

import argparse
import hashlib
import json
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal

gdal.UseExceptions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []

    def call(command, *paths, **flags):
        command = [args.terrio, command, *map(str, paths), "--json"]
        command.extend("--" + name.replace("_", "-") for name, enabled in flags.items() if enabled)
        process = subprocess.run(command, capture_output=True, text=True)
        if process.returncode:
            raise AssertionError(process.stderr.strip())
        return json.loads(process.stdout)

    def raster_snapshot(path):
        dataset = gdal.OpenEx(str(path), gdal.OF_RASTER)
        bands = []
        for index in range(1, dataset.RasterCount + 1):
            band = dataset.GetRasterBand(index)
            table = band.GetColorTable()
            bands.append(
                dict(
                    type=band.DataType,
                    pixels=band.ReadRaster(),
                    nodata=band.GetNoDataValue(),
                    mask=band.GetMaskBand().ReadRaster(),
                    metadata=band.GetMetadata(),
                    colors=[table.GetColorEntry(i) for i in range(table.GetCount())]
                    if table
                    else None,
                    overviews=[
                        (
                            band.GetOverview(i).XSize,
                            band.GetOverview(i).YSize,
                            band.GetOverview(i).ReadRaster(),
                        )
                        for i in range(band.GetOverviewCount())
                    ],
                )
            )
        return dict(
            size=[dataset.RasterXSize, dataset.RasterYSize],
            bands=bands,
            transform=dataset.GetGeoTransform(can_return_null=True),
            crs=dataset.GetProjection(),
            metadata=dataset.GetMetadata(),
            rpc=dataset.GetMetadata("RPC"),
        )

    def vector_snapshot(path):
        path = Path(path)
        if path.is_dir() and (path / "metadata.json").is_file():
            metadata = json.loads((path / "metadata.json").read_text())
            path = path / str(metadata.get("minzoom", 0))
        dataset = gdal.OpenEx(str(path), gdal.OF_VECTOR)
        result = []
        for layer in dataset:
            reference = layer.GetSpatialRef()
            fields = layer.GetLayerDefn()
            result.append(
                dict(
                    crs=reference.ExportToWkt() if reference else None,
                    fields=[
                        (
                            fields.GetFieldDefn(i).GetName(),
                            fields.GetFieldDefn(i).GetType(),
                        )
                        for i in range(fields.GetFieldCount())
                    ],
                    features=[json.loads(feature.ExportToJson()) for feature in layer],
                )
            )
        return result

    with tempfile.TemporaryDirectory(prefix="terrio-dataset-files-") as temporary:
        root = Path(temporary)
        fixtures = []
        for driver, extension in [
            ("GTiff", "tif"),
            ("EHdr", "bil"),
            ("ENVI", "dat"),
            ("ERS", "ers"),
            ("SAGA", "sdat"),
            ("RST", "rst"),
            ("HFA", "img"),
            ("PNG", "png"),
            ("JPEG", "jpg"),
            ("AAIGrid", "asc"),
            ("GSBG", "grd"),
            ("Zarr", "zarr"),
        ]:
            folder = root / driver
            folder.mkdir()
            path = folder / ("source." + extension)
            options = ["WORLDFILE=YES"] if driver in ("PNG", "JPEG") else []
            dataset = gdal.Translate(
                str(path),
                str(args.data / "landsat.tif"),
                format=driver,
                srcWin=[0, 0, 32, 32],
                bandList=[1],
                outputType=gdal.GDT_Byte,
                creationOptions=options,
            )
            dataset = None
            if driver == "RST":
                # GDAL's RST writer can emit uninitialized tiny WGS84 deltas when
                # the source has no explicit datum shift. This source is WGS84.
                reference_file = path.with_suffix(".ref")
                if reference_file.exists():
                    reference_file.write_text(
                        "".join(
                            "delta WGS84 : 0 0 0\n"
                            if line.lower().startswith("delta wgs84")
                            else line
                            for line in reference_file.read_text().splitlines(keepends=True)
                        )
                    )
            if driver == "GTiff":
                dataset = gdal.Open(str(path), gdal.GA_ReadOnly)
                dataset.BuildOverviews("NEAREST", [2, 4])
                dataset.GetRasterBand(1).SetStatistics(0, 255, 100, 10)
                dataset = None
                mask = gdal.GetDriverByName("GTiff").Create(
                    str(path) + ".msk", 32, 32, 1, gdal.GDT_Byte
                )
                mask.SetMetadataItem("INTERNAL_MASK_FLAGS_1", "2")
                mask.GetRasterBand(1).Fill(255)
                mask = None
                path.with_suffix(".tfw").write_text("1\n0\n0\n-1\n0\n0\n")
            fixtures.append((driver, path, raster_snapshot))
        for label, driver, extension, options in [
            ("ENVI img", "ENVI", "img", []),
            ("HFA spill", "HFA", "img", ["USE_SPILL=YES"]),
        ]:
            folder = root / label
            folder.mkdir()
            path = folder / ("source." + extension)
            dataset = gdal.Translate(
                str(path),
                str(args.data / "landsat.tif"),
                format=driver,
                srcWin=[0, 0, 32, 32],
                bandList=[1],
                outputType=gdal.GDT_Byte,
                creationOptions=options,
            )
            dataset = None
            fixtures.append((label, path, raster_snapshot))
        # Real pixel values with world-file-only georeferencing.
        folder = root / "GTiff world file"
        folder.mkdir()
        path = folder / "source.tif"
        original = gdal.Open(str(args.data / "landsat.tif"))
        values = original.GetRasterBand(1).ReadRaster(0, 0, 32, 32, buf_type=gdal.GDT_Byte)
        dataset = gdal.GetDriverByName("GTiff").Create(str(path), 32, 32, 1, gdal.GDT_Byte)
        dataset.GetRasterBand(1).WriteRaster(0, 0, 32, 32, values)
        dataset = original = None
        path.with_suffix(".tfw").write_text("2\n0\n0\n-2\n10\n100\n")
        fixtures.append(("GTiff world file", path, raster_snapshot))
        folder = root / "GTiff RPC"
        folder.mkdir()
        path = folder / "source.tif"
        dataset = gdal.GetDriverByName("GTiff").Create(
            str(path), 32, 32, 1, gdal.GDT_Byte, options=["RPB=YES"]
        )
        dataset.GetRasterBand(1).WriteRaster(0, 0, 32, 32, values)
        rpc = {key: "0" for key in ["LINE_OFF", "SAMP_OFF", "LAT_OFF", "LONG_OFF", "HEIGHT_OFF"]}
        rpc.update(
            {
                key: "1"
                for key in [
                    "LINE_SCALE",
                    "SAMP_SCALE",
                    "LAT_SCALE",
                    "LONG_SCALE",
                    "HEIGHT_SCALE",
                ]
            }
        )
        rpc.update(
            {
                key: "1 " + "0 " * 19
                for key in [
                    "LINE_NUM_COEFF",
                    "LINE_DEN_COEFF",
                    "SAMP_NUM_COEFF",
                    "SAMP_DEN_COEFF",
                ]
            }
        )
        dataset.SetMetadata(rpc, "RPC")
        dataset = None
        fixtures.append(("GTiff RPC", path, raster_snapshot))
        for label, driver, extension, options in [
            ("Shapefile", "ESRI Shapefile", "shp", []),
            ("MapInfo TAB", "MapInfo File", "tab", []),
            ("MapInfo MIF", "MapInfo File", "mif", []),
            ("CSV", "CSV", "csv", ["CREATE_CSVT=YES", "GEOMETRY=AS_WKT"]),
            ("GeoPackage", "GPKG", "gpkg", []),
            ("GML", "GML", "gml", []),
        ]:
            folder = root / label
            folder.mkdir()
            path = folder / ("source." + extension)
            dataset = gdal.VectorTranslate(
                str(path),
                str(args.data / "countries.geojson"),
                format=driver,
                limit=3,
                layerCreationOptions=options,
            )
            dataset = None
            fixtures.append((label, path, vector_snapshot))
        folder = root / "MVT directory"
        folder.mkdir()
        path = folder / "source"
        dataset = gdal.VectorTranslate(
            str(path),
            str(args.data / "countries.geojson"),
            format="MVT",
            limit=3,
            datasetCreationOptions=["MINZOOM=0", "MAXZOOM=0"],
        )
        dataset = None
        fixtures.append(("MVT directory", path, vector_snapshot))
        for label, source, snapshot in fixtures:
            stage = "source snapshot"
            try:
                reference = snapshot(source)
                dataset = gdal.OpenEx(str(source / "0" if label == "MVT directory" else source))
                expected_files = dataset.GetFileList()
                dataset = None
                stage = "owned file inventory"
                inventory = call("dataset-files", source)
                difference = ""
                if not source.is_dir():
                    actual_names = {Path(p).name for p in inventory["files"]}
                    expected_names = {Path(p).name for p in expected_files}
                    if label == "CSV" and actual_names == expected_names | {"source.prj"}:
                        difference = (
                            "Terrio retains the CSV CRS .prj omitted by GDAL GetFileList/copy"
                        )
                    elif label == "GML" and actual_names == expected_names | {"source.xsd"}:
                        difference = "Terrio retains and rebases GML XSD schema omitted by GDAL GetFileList/copy"
                    else:
                        assert actual_names == expected_names, (
                            inventory["files"],
                            expected_files,
                        )
                unrelated = source.parent / "source.unrelated.txt"
                unrelated.write_text("preserve unrelated sibling")
                output_folder = source.parent / "output"
                output_folder.mkdir()
                target = output_folder / ("copied" + source.suffix)
                reference_target = output_folder / ("gdal" + source.suffix)
                reference_command = [
                    "gdal",
                    "dataset",
                    "copy",
                    str(source),
                    str(reference_target),
                ]
                if label == "MVT directory":
                    # GDAL opens a zoom-level directory, not the whole pyramid root.
                    reference_command = [
                        "gdal",
                        "vsi",
                        "copy",
                        "--recursive",
                        str(source),
                        str(reference_target),
                    ]
                process = subprocess.run(reference_command, capture_output=True, text=True)
                assert process.returncode == 0, process.stderr
                stage = "GDAL reference copy"
                copied_reference = snapshot(reference_target)
                if difference and label == "CSV":
                    assert all(layer["crs"] is None for layer in copied_reference)
                    assert copied_reference == [dict(layer, crs=None) for layer in reference]
                elif difference and label == "GML":
                    assert not reference_target.with_suffix(".xsd").exists()
                elif label == "RST" and copied_reference != reference:
                    assert dict(copied_reference, crs=None) == dict(reference, crs=None)
                    difference = "Terrio rebases the Idrisi .rdc reference-system link; GDAL copy loses the CRS despite copying .ref"
                else:
                    assert copied_reference == reference
                stage = "Terrio copy snapshot"
                call("dataset-copy", source, target)
                assert snapshot(target) == reference
                if label == "GML":
                    import xml.etree.ElementTree as ET

                    schema = (
                        ET.parse(target)
                        .getroot()
                        .attrib["{http://www.w3.org/2001/XMLSchema-instance}schemaLocation"]
                        .split()[-1]
                    )
                    assert (target.parent / schema).read_bytes() == source.with_suffix(
                        ".xsd"
                    ).read_bytes()
                if not source.is_dir():
                    for original in inventory["files"]:
                        original = Path(original)
                        full_suffix = original.name.removeprefix(source.name)
                        name = (
                            (target.name + full_suffix)
                            if original.name.startswith(source.name + ".") or original == source
                            else (target.stem + original.name.removeprefix(source.stem))
                        )
                        copied = target.with_name(name)
                        if original.suffix not in (".ers", ".rdc", ".gml"):
                            assert (
                                hashlib.sha256(copied.read_bytes()).digest()
                                == hashlib.sha256(original.read_bytes()).digest()
                            )
                stage = "Terrio rename snapshot"
                moved = output_folder / ("moved" + source.suffix)
                call("dataset-rename", target, moved)
                assert not target.exists()
                assert snapshot(moved) == reference
                call("dataset-delete", moved)
                assert not moved.exists()
                assert not list(output_folder.glob("moved*"))
                assert unrelated.read_text() == "preserve unrelated sibling"
                assert snapshot(source) == reference
                results.append(
                    dict(
                        name=label,
                        status="tolerance" if difference else "identical",
                        detail=difference
                        or "owned files, encoded payloads, data/metadata and copy/rename/delete",
                    )
                )
            except Exception as error:
                results.append(dict(name=label, status="differs", detail=f"{stage}: {error}"))
        # VRT dependencies are copied with regular basenames; rename moves only owned definitions.
        for selector in [False, True]:
            label = "VRT netCDF reference" if selector else "VRT TIFF reference"
            try:
                folder = root / label
                folder.mkdir()
                data = folder / ("source.nc" if selector else "source.tif")
                dataset = gdal.Translate(
                    str(data),
                    str(args.data / "landsat.tif"),
                    format="netCDF" if selector else "GTiff",
                    srcWin=[0, 0, 32, 32],
                    bandList=[1],
                )
                dataset = None
                selected = f'NETCDF:"{data}":Band1' if selector else str(data)
                source = folder / "source.vrt"
                dataset = gdal.Translate(str(source), selected, format="VRT")
                dataset = None
                expected = raster_snapshot(source)
                output = folder / "output"
                output.mkdir()
                target = output / "copied.vrt"
                call("dataset-copy", source, target)
                assert raster_snapshot(target) == expected
                assert (output / ("copied" + data.suffix)).read_bytes() == data.read_bytes()
                reference_folder = output / "gdal"
                reference_folder.mkdir()
                reference_target = reference_folder / "copied.vrt"
                reference_command = [
                    "gdal",
                    "dataset",
                    "copy",
                    str(source),
                    str(reference_target),
                ]
                if label == "MVT directory":
                    # GDAL opens a zoom-level directory, not the whole pyramid root.
                    reference_command = [
                        "gdal",
                        "vsi",
                        "copy",
                        "--recursive",
                        str(source),
                        str(reference_target),
                    ]
                process = subprocess.run(reference_command, capture_output=True, text=True)
                reference_copy_failed = process.returncode != 0
                hidden = data.with_name("hidden" + data.suffix)
                data.rename(hidden)
                try:
                    assert raster_snapshot(target) == expected
                    try:
                        independent_reference = (
                            not reference_copy_failed
                            and raster_snapshot(reference_target) == expected
                        )
                    except Exception:
                        independent_reference = False
                finally:
                    hidden.rename(data)
                moved = output / "moved.vrt"
                call("dataset-rename", target, moved)
                assert raster_snapshot(moved) == expected
                assert not target.exists()
                assert (output / ("copied" + data.suffix)).exists()
                call("dataset-delete", moved)
                assert (output / ("moved" + data.suffix)).exists()
                assert data.exists()
                results.append(
                    dict(
                        name=label,
                        status="identical" if independent_reference else "tolerance",
                        detail="VRT references use copied assets (including on rename); GDAL's copy is not independent of the original source"
                        if not independent_reference
                        else "VRT reference rebasing and source-independent data exact",
                    )
                )
            except Exception as error:
                results.append(dict(name=label, status="differs", detail=str(error)))
        try:
            import threading
            from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
            from urllib.parse import urlsplit

            source = root / "GTiff" / "source.tif"
            with source.open("r+b") as stream:
                stream.truncate(8 * 1024 * 1024)
            expected = raster_snapshot(source)
            source_dataset = gdal.Open(str(source))
            owned = [Path(p) for p in source_dataset.GetFileList()]
            source_dataset = None
            payloads = {"/" + p.name: p for p in owned}
            counts = dict(gets=0, heads=0, missing=0, bytes=0)
            opened = set()

            class Handler(BaseHTTPRequestHandler):
                def log_message(self, *_):
                    pass

                def do_HEAD(self):
                    counts["heads"] += 1
                    path = payloads.get(urlsplit(self.path).path)
                    if path is None:
                        self.send_error(404)
                        return
                    self.send_response(200)
                    self.send_header("Content-Length", str(path.stat().st_size))
                    self.end_headers()

                def do_GET(self):
                    counts["gets"] += 1
                    path = payloads.get(urlsplit(self.path).path)
                    if path is None:
                        counts["missing"] += 1
                        self.send_response(404)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    opened.add(path.name)
                    size = path.stat().st_size
                    first, last = 0, size - 1
                    value = self.headers.get("Range")
                    if value:
                        a, b = value.removeprefix("bytes=").split("-", 1)
                        if a:
                            first = int(a)
                            last = min(int(b), last) if b else last
                        else:
                            first = max(0, size - int(b))
                        self.send_response(206)
                        self.send_header("Content-Range", f"bytes {first}-{last}/{size}")
                    else:
                        self.send_response(200)
                    self.send_header("Content-Length", str(last - first + 1))
                    self.end_headers()
                    with path.open("rb") as stream:
                        stream.seek(first)
                        remaining = last - first + 1
                        while remaining:
                            data = stream.read(min(remaining, 65536))
                            self.wfile.write(data)
                            counts["bytes"] += len(data)
                            remaining -= len(data)

            with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
                worker = threading.Thread(target=server.serve_forever, daemon=True)
                worker.start()
                try:
                    target = root / "remote.tif"
                    report = call(
                        "dataset-copy",
                        f"http://127.0.0.1:{server.server_port}/source.tif",
                        target,
                    )
                    assert raster_snapshot(target) == expected
                    assert {Path(p).name for p in report["files"]} == {
                        p.name.replace("source", "remote", 1) for p in owned
                    }
                finally:
                    server.shutdown()
                    worker.join()
            encoded = sum(p.stat().st_size for p in owned)
            assert counts["bytes"] <= encoded + 2 * 65536, counts
            assert opened == {p.name for p in owned}
            assert counts["gets"] - counts["missing"] <= 2 * len(owned) + 2, counts
            # Nine optional companions, plus an HTTP directory probe when the input
            # response has no media type. Known files are not probed again for copying.
            assert counts["missing"] <= 10, counts
            assert report["bytes"] == encoded
            results.append(
                dict(
                    name="HTTP copy with overview/mask/PAM sidecars",
                    status="identical",
                    detail=f"Encoded bytes={encoded}; served={counts['bytes']}; GETs={counts['gets']} (404 probes={counts['missing']}); HEADs={counts['heads']}; existing files opened={len(opened)}; data, overviews, masks, metadata and georeferencing exact",
                )
            )
            import zipfile

            for method, compression in [
                ("stored", zipfile.ZIP_STORED),
                ("deflated", zipfile.ZIP_DEFLATED),
            ]:
                archive = root / f"{method}.zip"
                with zipfile.ZipFile(archive, "w", compression=compression) as writer:
                    writer.write(source, "source.tif")
                payloads.clear()
                payloads["/" + archive.name] = archive
                counts.update(gets=0, heads=0, missing=0, bytes=0)
                opened.clear()
                with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
                    worker = threading.Thread(target=server.serve_forever, daemon=True)
                    worker.start()
                    try:
                        uri = f"/vsizip/{{http://127.0.0.1:{server.server_port}/{archive.name}}}/source.tif"
                        inventory = call("dataset-files", uri)
                        assert inventory["driver"] == "GTiff" and inventory["files"] == [uri]
                    finally:
                        server.shutdown()
                        worker.join()
                # The TIFF's first IFD and transform are near the header. This
                # metadata operation must not fetch the whole 8 MiB stored member.
                assert counts["bytes"] <= 512 * 1024, counts
                assert opened == {archive.name}
                results.append(
                    dict(
                        name=f"HTTP {method} ZIP TIFF metadata inventory",
                        status="identical",
                        detail=f"GDAL source TIFF has embedded transform; uncompressed member={source.stat().st_size}; archive={archive.stat().st_size}; served={counts['bytes']}; GETs={counts['gets']}; metadata inventory only, without whole-member CRC validation",
                    )
                )
            # Rename moves the owned VRT but only copies its dependencies,
            # including sources on read-only HTTP servers.
            import xml.etree.ElementTree as ET

            payloads.clear()
            payloads.update({"/" + p.name: p for p in owned})
            counts.update(gets=0, heads=0, missing=0, bytes=0)
            opened.clear()
            definition = root / "source.vrt"
            gdal.Translate(str(definition), str(source), format="VRT").Close()
            expected_vrt = raster_snapshot(definition)
            with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
                worker = threading.Thread(target=server.serve_forever, daemon=True)
                worker.start()
                try:
                    document = ET.parse(definition)
                    for element in document.iter("SourceFilename"):
                        element.text = f"http://127.0.0.1:{server.server_port}/source.tif"
                        element.set("relativeToVRT", "0")
                    document.write(definition, encoding="utf-8", xml_declaration=True)
                    moved = root / "remote-reference.vrt"
                    report = call("dataset-rename", definition, moved)
                    assert not definition.exists()
                    assert raster_snapshot(moved) == expected_vrt
                    assert all(path.exists() for path in owned)
                    assert report["bytes"] >= source.stat().st_size
                finally:
                    server.shutdown()
                    worker.join()
            results.append(
                dict(
                    name="VRT rename preserves read-only HTTP dependencies",
                    status="identical",
                    detail="owned VRT moved; referenced HTTP raster and companions copied; source files retained; GDAL snapshots exact",
                )
            )
        except Exception as error:
            results.append(
                dict(
                    name="HTTP copy with overview/mask/PAM sidecars",
                    status="differs",
                    detail=str(error),
                )
            )
    if args.json:
        print(json.dumps(results))
    else:
        for result in results:
            print(result["status"], result["name"], result["detail"])
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
