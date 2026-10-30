# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/enums.py

"""Enumerations, with rasterio's names and values."""

from __future__ import annotations

from enum import Enum, IntEnum


class TransformDirection(IntEnum):
    """The direction of a coordinate transformation."""

    forward = 1
    reverse = 0


class TransformMethod(Enum):
    """How pixel and map coordinates are related."""

    affine = "transform"
    gcps = "gcps"
    rpcs = "rpcs"


class ColorInterp(IntEnum):
    """Raster band color interpretations, as GDAL numbers them."""

    undefined = 0
    gray = 1
    grey = 1
    palette = 2
    red = 3
    green = 4
    blue = 5
    alpha = 6
    hue = 7
    saturation = 8
    lightness = 9
    cyan = 10
    magenta = 11
    yellow = 12
    black = 13
    Y = 14
    Cb = 15
    Cr = 16
    pan = 17
    coastal = 18
    rededge = 19
    nir = 20
    swir = 21
    mwir = 22
    lwir = 23
    tir = 24
    other_ir = 25
    sar_ka = 30
    sar_k = 31
    sar_ku = 32
    sar_x = 33
    sar_c = 34
    sar_s = 35
    sar_l = 36
    sar_p = 37


class Resampling(IntEnum):
    """Resampling methods, as GDAL numbers them."""

    nearest = 0
    bilinear = 1
    cubic = 2
    cubic_spline = 3
    lanczos = 4
    average = 5
    mode = 6
    gauss = 7
    max = 8
    min = 9
    med = 10
    q1 = 11
    q3 = 12
    sum = 13
    rms = 14


class OverviewResampling(IntEnum):
    """Resampling methods for overviews."""

    nearest = 0
    bilinear = 1
    cubic = 2
    cubic_spline = 3
    lanczos = 4
    average = 5
    mode = 6
    gauss = 7
    rms = 14


class Compression(Enum):
    """Compressions of GeoTIFF files."""

    jpeg = "JPEG"
    lzw = "LZW"
    packbits = "PACKBITS"
    deflate = "DEFLATE"
    ccittrle = "CCITTRLE"
    ccittfax3 = "CCITTFAX3"
    ccittfax4 = "CCITTFAX4"
    lzma = "LZMA"
    none = "NONE"
    zstd = "ZSTD"
    lerc = "LERC"
    lerc_deflate = "LERC_DEFLATE"
    lerc_zstd = "LERC_ZSTD"
    webp = "WEBP"
    jpeg2000 = "JPEG2000"


class Interleaving(Enum):
    """How the bands of a dataset are interleaved."""

    pixel = "PIXEL"
    line = "LINE"
    band = "BAND"
    tile = "TILE"


class MaskFlags(IntEnum):
    """Where a band's mask comes from."""

    all_valid = 1
    per_dataset = 2
    alpha = 4
    nodata = 8


class PhotometricInterp(Enum):
    """Photometric interpretations of TIFF files."""

    black = "MINISBLACK"
    white = "MINISWHITE"
    rgb = "RGB"
    cmyk = "CMYK"
    ycbcr = "YCbCr"
    cielab = "CIELAB"
    icclab = "ICCLAB"
    itulab = "ITULAB"


class MergeAlg(Enum):
    """How rasterized values combine."""

    replace = "REPLACE"
    add = "ADD"


class WktVersion(Enum):
    """WKT versions."""

    WKT2_2015 = "WKT2_2015"
    WKT2 = "WKT2"
    WKT2_2019 = "WKT2_2018"
    WKT1_GDAL = "WKT1_GDAL"
    WKT1 = "WKT1"
    WKT1_ESRI = "WKT1_ESRI"

    @classmethod
    def _missing_(cls, value: object) -> WktVersion:
        """Accepts `WKT2_2019` as an alias.

        Args:
            value: The value looked up.

        Returns:
            WktVersion: The member.

        Raises:
            ValueError: If the value is not a WKT version.
        """
        if value == "WKT2_2019":
            return WktVersion.WKT2_2019
        raise ValueError(f"Invalid value for WktVersion: {value}")


__all__ = [
    "ColorInterp",
    "Compression",
    "Interleaving",
    "MaskFlags",
    "MergeAlg",
    "OverviewResampling",
    "PhotometricInterp",
    "Resampling",
    "TransformDirection",
    "TransformMethod",
    "WktVersion",
]
