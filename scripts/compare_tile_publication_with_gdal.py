#!/usr/bin/env python3
"""Cross-read real tile publications with GDAL 3.13.3, checking its metadata bugs."""

import argparse
import json
import math
from pathlib import Path
import tempfile
import xml.etree.ElementTree as ET

import numpy as np
from osgeo import gdal

from compare_raster_tiles_with_gdal import compare_folders, run

gdal.UseExceptions()


def compare_kml(reference, actual):
    names = {p.relative_to(reference) for p in reference.rglob("*.kml")} - {
        Path("doc.kml")
    }
    others = {p.relative_to(actual) for p in actual.rglob("*.kml")} - {Path("doc.kml")}
    if not names or names != others:
        raise AssertionError("KML tile layouts differ")
    ns = {"k": "http://www.opengis.net/kml/2.2"}
    for name in names:
        a, b = [ET.parse(root / name) for root in (reference, actual)]
        for bound in ("north", "south", "east", "west"):
            values = []
            for tree in (a, b):
                element = tree.find(
                    f".//k:GroundOverlay/k:LatLonBox/k:{bound}", ns
                )
                if element is None or element.text is None:
                    raise AssertionError(f"{name}: missing KML overlay {bound}")
                values.append(float(element.text))
            if abs(values[0] - values[1]) > 1e-10:
                raise AssertionError(f"{name}: KML {bound} differs")
    for root in (reference, actual):
        for path in root.rglob("*.kml"):
            for link in ET.parse(path).findall(".//k:href", ns):
                if not (path.parent / link.text).is_file():
                    raise AssertionError(
                        f"{path.name}: missing linked file {link.text}"
                    )
    return f"{len(names)} matching overlay bounds and complete local KML links"


def compare_stacta(reference, actual, tile_size):
    # GDAL 3.13.3 records the default 256-pixel matrix scale even when
    # --tile-size changes. Assert that precise error before correcting it.
    path = reference / "stacta.json"
    document = json.loads(path.read_text())
    native = json.loads((actual / "stacta.json").read_text())
    sets = document["properties"]["tiles:tile_matrix_sets"]
    native_sets = native["properties"]["tiles:tile_matrix_sets"]
    if len(sets) != 1 or len(native_sets) != 1:
        raise AssertionError("expected one STACTA matrix set")
    # GDAL names its WebMercator set GoogleMapsCompatible; matrix coordinates
    # and tile limits, rather than internal identifiers, define its meaning.
    matrices = next(iter(sets.values()))["tileMatrix"]
    native_matrices = next(iter(native_sets.values()))["tileMatrix"]
    if len(matrices) != len(native_matrices):
        raise AssertionError("STACTA level counts differ")
    for a, b in zip(matrices, native_matrices, strict=True):
        for key in (
            "identifier",
            "tileWidth",
            "tileHeight",
            "matrixWidth",
            "matrixHeight",
            "topLeftCorner",
        ):
            if a[key] != b[key]:
                raise AssertionError(f"STACTA {key} differs")
        metres_across = 2 * math.pi * 6378137
        native_scale = metres_across / b["matrixWidth"] / tile_size / 0.00028
        if not np.isclose(b["scaleDenominator"], native_scale, rtol=1e-12, atol=0):
            raise AssertionError("native matrix scale does not match the global grid")
        expected = native_scale * tile_size / 256
        if not np.isclose(a["scaleDenominator"], expected, rtol=1e-12, atol=0):
            raise AssertionError(
                "reference scale is not the known default-tile-size defect"
            )
        a["scaleDenominator"] *= 256 / tile_size
    links = document["properties"]["tiles:tile_matrix_links"]
    native_links = native["properties"]["tiles:tile_matrix_links"]
    if (
        len(links) != 1
        or len(native_links) != 1
        or next(iter(links.values()))["limits"]
        != next(iter(native_links.values()))["limits"]
    ):
        raise AssertionError("STACTA tile limits differ")
    normalized = reference / "normalized-stacta.json"
    normalized.write_text(json.dumps(document))
    a, b = gdal.Open(str(normalized)), gdal.Open(str(actual / "stacta.json"))
    if (a.RasterXSize, a.RasterYSize, a.RasterCount) != (
        b.RasterXSize,
        b.RasterYSize,
        b.RasterCount,
    ):
        raise AssertionError("cross-read STACTA dimensions/bands differ")
    if not np.allclose(a.GetGeoTransform(), b.GetGeoTransform(), atol=1e-8, rtol=1e-12):
        raise AssertionError("cross-read STACTA grids differ")
    if not np.array_equal(a.ReadAsArray(), b.ReadAsArray(), equal_nan=True):
        raise AssertionError("cross-read STACTA pixels differ")
    return "GDAL reopens both STACTA mosaics with exact samples and matching grids" + (
        "; verified GDAL default-256 scale defect corrected in reference only"
        if tile_size != 256
        else ""
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-publication-") as directory:
        root = Path(directory)
        source = root / "landsat.tif"
        ds = gdal.Translate(
            str(source),
            str(args.data / "landsat.tif"),
            srcWin=[600, 500, 513, 299],
            bandList=[1, 2, 3],
            outputType=gdal.GDT_Byte,
        )
        for band in range(1, 4):
            ds.GetRasterBand(band).DeleteNoDataValue()
        ds.Close()
        template = root / "template.mapml"
        template.write_text(
            "|".join(
                "${" + key + "}"
                for key in [
                    "TITLE",
                    "TILING_SCHEME",
                    "MINZOOM",
                    "MAXZOOM",
                    "CURZOOM",
                    "MINTILEX",
                    "MAXTILEX",
                    "MINTILEY",
                    "MAXTILEY",
                ]
            )
        )
        for name, scheme, size, convention in [
            ("wgs64", "WorldCRS84Quad", 64, "xyz"),
            ("merc256", "WebMercatorQuad", 256, "xyz"),
            ("tms64", "WorldCRS84Quad", 64, "tms"),
        ]:
            reference, actual = root / f"{name}-gdal", root / f"{name}-native"
            common = [
                "--tiling-scheme",
                scheme,
                "--tile-size",
                str(size),
                "--min-zoom",
                "11",
                "--max-zoom",
                "11",
                "--resampling",
                "nearest",
                "--convention",
                convention,
                "--no-alpha",
                "--kml",
                "--title",
                "Real imagery",
            ]
            viewers = (
                ["--webviewer", "openlayers"]
                if convention == "tms"
                else [
                    "--webviewer",
                    "stac",
                    "--webviewer",
                    "mapml",
                    "--mapml-template",
                    template,
                ]
            )
            try:
                run(
                    [
                        "gdal",
                        "raster",
                        "tile",
                        source,
                        reference,
                        *common,
                        *viewers,
                        "--num-threads",
                        "2",
                    ]
                )
                run([args.terrio, "tile", source, actual, *common, *viewers])
                detail = (
                    compare_folders(reference, actual, "png")
                    + "; "
                    + compare_kml(reference, actual)
                )
                if convention == "xyz":
                    if (reference / "mapml.mapml").read_text() != (
                        actual / "mapml.mapml"
                    ).read_text():
                        raise AssertionError("custom MapML substitutions differ")
                    detail += "; MapML template substitutions match; " + compare_stacta(
                        reference, actual, size
                    )
                results.append(
                    {
                        "name": name,
                        "status": "tolerance"
                        if size != 256 and convention == "xyz"
                        else "identical",
                        "detail": detail,
                    }
                )
            except (AssertionError, ET.ParseError, RuntimeError, ValueError) as error:
                results.append(
                    {"name": name, "status": "differs", "detail": str(error)}
                )
    print(json.dumps(results, indent=None if args.json else 2))
    return int(any(case["status"] == "differs" for case in results))


if __name__ == "__main__":
    raise SystemExit(main())
