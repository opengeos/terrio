#!/usr/bin/env python3
"""Prepare real Landsat CADRG frames and GDAL's polar sample with GDAL 3.13.3."""

import argparse
import hashlib
from pathlib import Path
import urllib.request

import numpy as np
from osgeo import gdal, osr

gdal.UseExceptions()
POLAR_URL = (
    "https://raw.githubusercontent.com/OSGeo/gdal/c8b4c45fca87d3e6fbf80e7a7898b8a661ad0edc/"
    "autotest/gdrivers/data/rpftoc/zone9/RPF/ZONE9/00027010.ON9"
)

POLAR_SHA256 = "def957230fa374138b6275e8d4aeea2f53e0c1814c314f41cfe74c691e06eb64"


def verify_polar(path):
    if hashlib.sha256(path.read_bytes()).hexdigest() != POLAR_SHA256:
        raise RuntimeError(
            "GDAL polar fixture checksum differs from the pinned reference"
        )


def prepare(data):
    if int(gdal.VersionInfo("VERSION_NUM")) < 3130000:
        raise RuntimeError("RPF fixture creation needs GDAL >= 3.13")
    source = gdal.Translate(
        "",
        str(data / "landsat.tif"),
        format="MEM",
        srcWin=[500, 500, 96, 96],
        bandList=[1, 1, 1],
        colorInterpretation=["red", "green", "blue"],
    )
    for relative, series, scale in [
        ("rpftoc_landsat/ON", "ON", 1000000),
        ("rpftoc_landsat/TP", "TP", 500000),
        ("rpftoc_fractional3500000", "WI", 3500000),
    ]:
        root = data / relative
        if not any(root.rglob(f"*.{series}2")):
            if root.exists():
                raise RuntimeError(
                    f"refusing to overwrite incomplete fixture directory {root}"
                )
            root.parent.mkdir(parents=True, exist_ok=True)
            output = gdal.GetDriverByName("NITF").CreateCopy(
                str(root),
                source,
                options=[
                    "PRODUCT_TYPE=CADRG",
                    f"SERIES_CODE={series}",
                    f"SCALE={scale}",
                    "ZONE=2",
                    "RESAMPLING=NEAREST",
                ],
            )
            output.Close()
    polar = data / "rpftoc_polar/RPF/ZONE9/00027010.ON9"
    if not polar.exists():
        polar.parent.mkdir(parents=True, exist_ok=True)
        temporary = polar.with_suffix(".download")
        try:
            urllib.request.urlretrieve(POLAR_URL, temporary)
            verify_polar(temporary)
            temporary.replace(polar)
        finally:
            temporary.unlink(missing_ok=True)
    verify_polar(polar)

    # Independent synthetic checks for frames containing the geographic poles.
    for label, zone, latitude in [("north", "9", 89.9), ("south", "J", -89.8)]:
        root = data / "rpftoc_pole_centers" / (label + "-rgb")
        if any(root.rglob("*.ON" + zone)):
            continue
        if root.exists():
            raise RuntimeError(f"refusing to overwrite incomplete polar fixture {root}")
        root.parent.mkdir(parents=True, exist_ok=True)
        ds = gdal.GetDriverByName("MEM").Create("", 30, 30, 3, gdal.GDT_Byte)
        ds.SetGeoTransform((-2, 4 / 30, 0, latitude, 0, -0.1 / 30))
        crs = osr.SpatialReference()
        crs.ImportFromEPSG(4326)
        crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
        ds.SetSpatialRef(crs)
        for band, color in enumerate(
            [gdal.GCI_RedBand, gdal.GCI_GreenBand, gdal.GCI_BlueBand], 1
        ):
            ds.GetRasterBand(band).SetColorInterpretation(color)
            ds.GetRasterBand(band).WriteArray(
                (np.arange(900).reshape(30, 30) % 127 + band * 10).astype("uint8")
            )
        output = gdal.GetDriverByName("NITF").CreateCopy(
            str(root),
            ds,
            options=[
                "PRODUCT_TYPE=CADRG",
                "SERIES_CODE=ON",
                "SCALE=1000000",
                "ZONE=" + zone,
            ],
        )
        output.Close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    prepare(parser.parse_args().data.resolve())
