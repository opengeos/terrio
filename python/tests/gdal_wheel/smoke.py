"""Smoke test of an installed terrio-gdal wheel: GDAL is in it, and finds its data files.

Usage: python -I smoke.py [--network]

Run outside the source tree, with the wheel installed, on a machine without GDAL: it checks that
the bundled GDAL writes and reads formats only GDAL has (NITF rasters, ESRI File Geodatabases and
DXF drawings, whose writer needs GDAL's data files), that PROJ's database is found (the CRSs are
identified as EPSG codes), and with ``--network``, that GDAL's HTTPS requests trust the system's
certificates.
"""

import sys
import tempfile
from pathlib import Path

import numpy as np

import terrio


def check(condition: bool, message: str) -> None:
    """Fails the test with a message unless the condition holds.

    Args:
        condition: What must hold.
        message: What went wrong otherwise.

    Raises:
        SystemExit: If the condition does not hold.
    """
    if not condition:
        sys.exit(f"FAILED: {message}")


def check_bundle() -> None:
    """Checks that terrio has GDAL, with its data files bundled in the package."""
    version = terrio.gdal_version()
    print(f"terrio {terrio.__version__}, GDAL {version}, from {Path(terrio.__file__).parent}")
    check(version is not None, "terrio is built without GDAL")
    package = Path(terrio.__file__).parent
    check((package / "gdal_data" / "header.dxf").is_file(), "GDAL's data files are not bundled")
    check((package / "proj_data" / "proj.db").is_file(), "PROJ's database is not bundled")
    gdal = [d for d in terrio.drivers() if d.get("name") == "GDAL"]
    check(bool(gdal), "no GDAL driver in terrio.drivers()")


def check_raster(tmp: Path) -> None:
    """Writes a NITF file through GDAL and reads it back.

    Args:
        tmp: A scratch directory.
    """
    array = np.arange(64 * 48, dtype=np.uint16).reshape(1, 48, 64)
    transform = (500000.0, 30.0, 0.0, 4500000.0, 0.0, -30.0)
    raster = terrio.Raster.from_array(array, transform, crs="EPSG:32618")
    raster.write(str(tmp / "dem.tif"))
    # An extension terrio has no driver for goes to the GDAL driver that claims it.
    terrio.convert(str(tmp / "dem.tif"), str(tmp / "dem.ntf"))
    nitf = terrio.open_raster(str(tmp / "dem.ntf"))
    print(f"NITF: driver {nitf.driver}, CRS {nitf.crs}, shape {nitf.shape}")
    check(nitf.driver == "GDAL", f"the NITF file was opened by {nitf.driver}, not GDAL")
    check(nitf.crs == "EPSG:32618", f"the NITF file's CRS is {nitf.crs}, not EPSG:32618")
    check(np.array_equal(nitf.read(1), array[0]), "the NITF file's pixels differ")
    # ERDAS Imagine, which terrio reads itself but writes through GDAL.
    terrio.convert(str(tmp / "dem.tif"), str(tmp / "dem.img"))
    img = terrio.open_raster(str(tmp / "dem.img"))
    check(img.crs == "EPSG:32618", f"the ERDAS Imagine file's CRS is {img.crs}")
    check(np.array_equal(img.read(1), array[0]), "the ERDAS Imagine file's pixels differ")


def check_vector(tmp: Path) -> None:
    """Writes an ESRI File Geodatabase and a DXF drawing through GDAL and reads them back.

    Args:
        tmp: A scratch directory.
    """
    features = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"name": name, "value": value},
                "geometry": {"type": "Point", "coordinates": [x, y]},
            }
            for name, value, x, y in [("a", 1, -73.98, 40.75), ("b", 2, 2.35, 48.86)]
        ],
    }
    terrio.write_vector(features, str(tmp / "points.geojson"), crs="EPSG:4326")
    terrio.convert(str(tmp / "points.geojson"), str(tmp / "points.gdb"))
    gdb = terrio.open_vector(str(tmp / "points.gdb"))
    layer = gdb.layers[0]
    print(f"File Geodatabase: driver {gdb.driver}, layer {layer.name}, CRS {layer.crs}")
    check(gdb.driver == "GDAL", f"the File Geodatabase was opened by {gdb.driver}, not GDAL")
    check(layer.crs == "EPSG:4326", f"the File Geodatabase's CRS is {layer.crs}")
    check(len(layer) == 2, f"the File Geodatabase has {len(layer)} features, not 2")
    # GDAL's DXF writer copies header.dxf and trailer.dxf from GDAL's data files.
    terrio.convert(str(tmp / "points.geojson"), str(tmp / "points.dxf"))
    dxf = terrio.open_vector(str(tmp / "points.dxf"))
    print(f"DXF: driver {dxf.driver}, {len(dxf.layers[0])} features")
    check(len(dxf.layers[0]) == 2, "the DXF drawing does not have 2 features")


def check_network() -> None:
    """Reads a remote NITF sample from GDAL's autotest data over HTTPS, through GDAL."""
    url = (
        "/vsicurl/https://raw.githubusercontent.com/OSGeo/gdal/v3.12.0/"
        "autotest/gdrivers/data/nitf/rgb.ntf"
    )
    remote = terrio.open_raster(url)
    print(f"HTTPS: {remote.driver}, shape {remote.shape}")
    check(remote.driver == "GDAL", "the remote NITF file was not opened by GDAL")


def main() -> None:
    """Runs the checks."""
    check_bundle()
    with tempfile.TemporaryDirectory() as tmp:
        check_raster(Path(tmp))
        check_vector(Path(tmp))
    if "--network" in sys.argv[1:]:
        check_network()
    print("terrio-gdal smoke test passed")


if __name__ == "__main__":
    main()
