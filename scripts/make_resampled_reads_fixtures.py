"""Writes the rasters of `crates/terrio/tests/data/resampled_reads` and what GDAL reads of them
into outputs of other sizes.

Usage:
    python scripts/make_resampled_reads_fixtures.py
    python scripts/make_resampled_reads_fixtures.py --check-rasterio
    python scripts/make_resampled_reads_fixtures.py --mask-control

The rasters are small GeoTIFFs with overviews: Byte RGB with an internal mask whose overviews have
masks of their own, and the same with overviews built before the mask (none has one); UInt16 of odd
size, Int16 and Float32 with nodata values (NaN included); Float32 with external overviews; RGBA
with a Byte alpha band and two bands with a UInt16 one; a paletted band; a `.msk` sidecar mask;
two bands all valid, without overviews; and Byte RGB with external overviews that RasterIO
chooses among twice.

`expected.json.gz` holds, for each case (a file, bands, a window, an output size, a resampling and an
overview selection), the pixels the recorded `gdal_version` reads, in the raster's type, and the masks, as the
hexadecimal of their little-endian bytes. Pixels of several bands are read through the dataset
(`GDALDatasetRasterIOEx`, as rasterio reads them), a band alone through the band, and masks through
`GetMaskBand()` with the same window, size and resampling. A selection other than `AUTO` is read
as `gdal_translate -ovr` reads it, from the overview dataset of that level alone, the masks not
compared. Fractional window sizes have fractions under 0.5, for which GDAL's Python bindings and
rasterio give GDAL the same integer window. Needs GDAL's Python bindings (exactly 3.13.3).

With `--check-rasterio`, the files and `expected.json.gz` are only read: each `AUTO` case is compared
with rasterio's `read(window=..., out_shape=..., resampling=...)` and `read_masks`, which needs
rasterio rather than GDAL's bindings.

The `--mask-control` mode writes a separate GDAL 3.13.3 control for the masked external
cubic 26×41 case. Only its first image/mask flags are normalized from 1/5 to 0/4,
so GDAL uses the stored mask. Historical raw references are kept unchanged.

Generation requires GDAL 3.13.3 and refreshes only the RGB, mixed-metadata and masked external
fixtures plus the mixed internal fixture. Existing GDAL 3.13.2 cases and their TIFFs are kept
unchanged. Every case records its oracle version; reproducing the historical cases requires
that version and the generator from their original commit.
"""

from __future__ import annotations

import argparse
import gzip
import json
import shutil
import struct
import sys
import tempfile
from pathlib import Path
from typing import Any

import numpy as np

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "resampled_reads"
METHODS = ["nearest", "bilinear", "cubic", "cubicspline", "lanczos", "average", "rms", "mode"]


def pattern(rng: np.random.Generator, bands: int, rows: int, cols: int, low: float, high: float) -> np.ndarray:
    """Smooth waves with noise, `(bands, rows, cols)`, between `low` and `high`."""
    y, x = np.mgrid[0:rows, 0:cols]
    waves = [np.sin(x / (3.0 + b)) * 0.3 + np.cos(y / 4.0) * 0.3 + 0.5 + rng.normal(0, 0.08, x.shape) for b in range(bands)]
    return low + (high - low) * np.clip(np.stack(waves), 0, 1)


def write_files(gdal: Any) -> list[dict[str, Any]]:
    """Writes the rasters.

    Args:
        gdal: GDAL's Python bindings (`osgeo.gdal`).

    Returns:
        The cases: file, bands (empty for all), window `(x, y, width, height)` or None, output
        size `(width, height)`, resampling and overview selection.
    """
    OUT.mkdir(parents=True, exist_ok=True)
    for old in OUT.iterdir():
        old.unlink()
    rng = np.random.default_rng(429)
    # Overviews in compressed tiles of 16 x 16 pixels: the files stay small.
    gdal.SetConfigOption("COMPRESS_OVERVIEW", "DEFLATE")
    gdal.SetConfigOption("GDAL_TIFF_OVR_BLOCKSIZE", "16")

    def tiff(name: str, dtype: int, data: np.ndarray, nodata: Any = None, options: tuple[str, ...] = (),
             colors: Any = None) -> Any:
        bands, rows, cols = data.shape
        ds = gdal.GetDriverByName("GTiff").Create(str(OUT / name), cols, rows, bands, dtype,
                                                  ["TILED=YES", "BLOCKXSIZE=16", "BLOCKYSIZE=16", "COMPRESS=DEFLATE", *options])
        ds.SetGeoTransform([500000.0, 30.0, 0.0, 4100000.0, 0.0, -30.0])
        if colors is not None:
            ds.GetRasterBand(1).SetRasterColorTable(colors)
        for i in range(bands):
            band = ds.GetRasterBand(i + 1)
            band.WriteArray(data[i])
            if nodata is not None:
                band.SetNoDataValue(nodata)
        return ds

    def case(file: str, size: tuple[int, int], window: tuple[float, float, float, float] | None = None,
             resampling: str = "nearest", bands: tuple[int, ...] = (), overview: str = "AUTO") -> dict[str, Any]:
        return {"file": file, "bands": list(bands), "window": None if window is None else list(window),
                "size": list(size), "resampling": resampling, "overview": overview}

    cases = []
    # Byte RGB with an internal mask, overviews built after it: each level has a mask of its own.
    mask = np.full((48, 64), 255, np.uint8)
    mask[:, :11] = 0
    mask[20:29, 30:41] = 0
    rgb = pattern(rng, 3, 48, 64, 1, 250).astype(np.uint8)
    for name, mask_first in [("rgb_mask.tif", True), ("rgb_mask_plain_overviews.tif", False)]:
        ds = tiff(name, gdal.GDT_Byte, rgb, options=("PHOTOMETRIC=RGB",))
        if not mask_first:
            ds.BuildOverviews("AVERAGE", [2, 4, 8])
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        if mask_first:
            ds.BuildOverviews("AVERAGE", [2, 4, 8])
        ds = None
        for m in METHODS:
            cases += [case(name, (30, 22), resampling=m), case(name, (13, 9), (3, 5, 54, 38), m),
                      case(name, (7, 5), (10.25, 6.4, 40.3, 30.2), m), case(name, (24, 18), (2, 3, 10, 8), m)]
        cases += [case(name, (32, 24)), case(name, (16, 12), resampling="bilinear"), case(name, (40, 9), resampling="cubic"),
                  case(name, (40, 12), (0, 0, 40, 30), "bilinear"), case(name, (21, 16), bands=(2,), resampling="bilinear"),
                  case(name, (21, 16), bands=(3, 1), resampling="cubic"), case(name, (10, 7), (20, 10, 30, 22), "average", (2, 3))]
    # UInt16 of odd size with a nodata value and average overviews (of the valid pixels).
    u16 = pattern(rng, 1, 53, 67, 1, 60000).astype(np.uint16)
    u16[0, rng.random(u16.shape[1:]) < 0.04] = 0
    u16[0, 10:18, 40:52] = 0
    ds = tiff("u16_nodata.tif", gdal.GDT_UInt16, u16, nodata=0)
    ds.BuildOverviews("AVERAGE", [2, 3, 4])
    ds = None
    for m in METHODS:
        cases += [case("u16_nodata.tif", (25, 20), resampling=m), case("u16_nodata.tif", (11, 13), (5.4, 2.2, 50.3, 40.1), m),
                  case("u16_nodata.tif", (36, 28), (10, 10, 15, 12), m)]
    # Level choices about the thresholds: 67 / 34 and 67 / 23 against 1.2 and 1.0.
    for w in (30, 31, 32, 33, 34, 35, 26, 27, 28):
        cases += [case("u16_nodata.tif", (w, max(1, round(w * 53 / 67))), resampling=m) for m in ("nearest", "bilinear")]
    # Int16 with a nodata value, two bands, no overviews.
    i16 = pattern(rng, 2, 40, 50, -20000, 20000).astype(np.int16)
    i16[:, rng.random(i16.shape[1:]) < 0.05] = -32768
    ds = tiff("i16_nodata.tif", gdal.GDT_Int16, i16, nodata=-32768)
    ds = None
    for m in ("nearest", "bilinear", "cubic", "lanczos", "average", "mode"):
        cases += [case("i16_nodata.tif", (17, 13), resampling=m), case("i16_nodata.tif", (24, 18), (4.3, 3.2, 10.2, 8.1), m)]
    # Float32 with a NaN nodata value and overviews.
    f32 = pattern(rng, 1, 44, 60, -5, 5).astype(np.float32)
    f32[0, rng.random(f32.shape[1:]) < 0.05] = np.nan
    f32[0, 5:12, 20:31] = np.nan
    ds = tiff("f32_nan.tif", gdal.GDT_Float32, f32, nodata=float("nan"))
    ds.BuildOverviews("AVERAGE", [2, 4])
    ds = None
    for m in METHODS:
        cases += [case("f32_nan.tif", (23, 17), resampling=m), case("f32_nan.tif", (9, 8), (12.2, 4.4, 30.3, 25.2), m)]
    # Float32 with a nodata value and external overviews (a `.ovr`).
    f32b = pattern(rng, 2, 36, 48, 0, 100).astype(np.float32)
    f32b[:, 3:9, 3:9] = -9999
    tiff("f32_external_overviews.tif", gdal.GDT_Float32, f32b, nodata=-9999)
    ds = gdal.Open(str(OUT / "f32_external_overviews.tif"))
    ds.BuildOverviews("AVERAGE", [2, 4])
    ds = None
    for m in ("nearest", "bilinear", "cubic", "average"):
        cases += [case("f32_external_overviews.tif", (21, 15), resampling=m),
                  case("f32_external_overviews.tif", (7, 6), (2.3, 1.2, 40.4, 30.3), m, (2,))]
    # RGBA: a Byte alpha band of four, with partial transparency.
    rgba = pattern(rng, 4, 36, 48, 0, 255).astype(np.uint8)
    rgba[3] = np.where(np.arange(48)[None, :] < 12, 0, rgba[3])
    ds = tiff("rgba.tif", gdal.GDT_Byte, rgba, options=("PHOTOMETRIC=RGB", "ALPHA=YES"))
    ds.BuildOverviews("AVERAGE", [2, 4])
    ds = None
    for m in ("nearest", "bilinear", "cubic", "average", "mode"):
        cases += [case("rgba.tif", (20, 15), resampling=m), case("rgba.tif", (17, 11), (6.2, 3.1, 30.2, 22.4), m, (1, 2, 3)),
                  case("rgba.tif", (24, 18), (0, 0, 10, 8), m)]
    # Two bands, the second a UInt16 alpha band.
    ua = pattern(rng, 2, 30, 40, 0, 65535).astype(np.uint16)
    ua[1, :, :8] = 0
    ua[1, :, 8:12] = 200
    ds = tiff("u16_alpha.tif", gdal.GDT_UInt16, ua, options=("ALPHA=YES",))
    ds.BuildOverviews("AVERAGE", [2])
    ds = None
    for m in ("nearest", "bilinear", "average"):
        cases += [case("u16_alpha.tif", (17, 13), resampling=m), case("u16_alpha.tif", (20, 15), (0, 0, 8, 6), m)]
    # A paletted band: bilinear and cubic fall back to nearest neighbour, average does not.
    pal = (pattern(rng, 1, 30, 40, 0, 15)).astype(np.uint8)
    ct = gdal.ColorTable()
    for i in range(16):
        ct.SetColorEntry(i, (i * 16, 255 - i * 16, (i * 37) % 256, 255))
    ds = tiff("paletted.tif", gdal.GDT_Byte, pal, colors=ct)
    ds.BuildOverviews("NEAREST", [2])
    ds = None
    # From the overview (which has no color table in GDAL), and at full resolution, where GDAL's
    # average of colors is refused.
    for m in ("nearest", "bilinear", "cubic", "average", "mode"):
        cases.append(case("paletted.tif", (17, 13), resampling=m))
    for m in ("nearest", "bilinear", "cubic", "mode"):
        cases.append(case("paletted.tif", (22, 16), (0, 0, 8, 6), m))
    # A `.msk` sidecar mask, with overviews of its own.
    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "NO")
    side = pattern(rng, 2, 36, 48, 1, 250).astype(np.uint8)
    ds = tiff("sidecar_mask.tif", gdal.GDT_Byte, side)
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    smask = np.full((36, 48), 255, np.uint8)
    smask[10:20, 5:30] = 0
    ds.GetRasterBand(1).GetMaskBand().WriteArray(smask)
    ds.BuildOverviews("AVERAGE", [2, 4])
    ds = None
    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", None)
    for m in ("nearest", "bilinear", "cubic", "average"):
        cases += [case("sidecar_mask.tif", (20, 15), resampling=m), case("sidecar_mask.tif", (9, 7), (4.4, 6.3, 30.2, 20.4), m)]
    # Two bands all valid, without overviews: resampled together.
    valid = pattern(rng, 2, 32, 40, 0, 255).astype(np.uint8)
    tiff("valid.tif", gdal.GDT_Byte, valid)
    for m in METHODS:
        cases += [case("valid.tif", (15, 12), resampling=m), case("valid.tif", (21, 16), (3.2, 2.1, 8.4, 6.3), m),
                  case("valid.tif", (40, 9), resampling=m)]
    # Byte RGB with external overviews of 49 x 42 and 25 x 21: GDAL's band of the `.ovr`'s image
    # has the file's other image as an overview, which RasterIO chooses again from the first.
    name = "rgb_external_overviews.tif"
    tiff(name, gdal.GDT_Byte, pattern(rng, 3, 83, 97, 1, 250).astype(np.uint8))
    ds = gdal.Open(str(OUT / name))
    ds.BuildOverviews("AVERAGE", [2, 4])
    ds = None
    # Band by band (a band alone, nearest neighbour and average), and through the dataset (the
    # interpolating kernels, which choose an overview of the `.ovr`'s image when the buffer is
    # smaller on both axes, else read its bands band by band).
    cases += [case(name, (31, 27), bands=(1,)), case(name, (29, 25), (2.3, 3.4, 90.2, 78.3), bands=(2,)),
              case(name, (26, 22), resampling="bilinear", bands=(1,)), case(name, (24, 20), (2, 3, 90, 78), "average", (3,)),
              case(name, (31, 27)), case(name, (26, 22), resampling="average"), case(name, (26, 22), resampling="bilinear"),
              case(name, (26, 22), resampling="cubic"), case(name, (26, 41), resampling="cubic"),
              case(name, (24, 20), (2, 3, 90, 78), "lanczos")]
    # Mixed metadata: GDAL 3.13.3 excludes only the marked candidate, at each level.
    name = "mixed_external_overviews.tif"
    ds = tiff(name, gdal.GDT_Byte, pattern(rng, 1, 83, 97, 1, 250).astype(np.uint8))
    ds = None
    ds = gdal.Open(str(OUT / name))
    ds.BuildOverviews("AVERAGE", [2, 4])
    ds = None
    ds = gdal.Open(str(OUT / (name + ".ovr")), gdal.GA_Update)
    ds.GetRasterBand(1).GetOverview(0).SetMetadataItem("RESAMPLING", "AVERAGE_BIT2GRAYSCALE")
    ds = None
    cases += [case(name, (31, 27)), case(name, (26, 22), resampling="bilinear"),
              case(name, (20, 17), resampling="cubic")]
    # Internal mixed markers follow the same per-candidate exclusion rule.
    name = "mixed_internal_overviews.tif"
    ds = tiff(name, gdal.GDT_Byte, np.ones((1, 64, 64), dtype=np.uint8))
    ds.BuildOverviews("AVERAGE", [2, 4])
    ds.GetRasterBand(1).GetOverview(0).Fill(10)
    ds.GetRasterBand(1).GetOverview(1).Fill(20)
    ds.GetRasterBand(1).GetOverview(1).SetMetadataItem("RESAMPLING", "AVERAGE_BIT2GRAYSCALE")
    ds = None
    cases += [case(name, (16, 16), resampling=m) for m in ("nearest", "bilinear", "cubic")]
    # External overviews with dataset masks: invalid samples must affect interpolation after
    # the second level selection, including for shared readers.
    name = "masked_external_overviews.tif"
    ds = tiff(name, gdal.GDT_Byte, pattern(rng, 3, 83, 97, 1, 250).astype(np.uint8))
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    mask = np.full((83, 97), 255, np.uint8)
    mask[12:55, 16:65] = 0
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
    ds = None
    ds = gdal.Open(str(OUT / name))
    ds.BuildOverviews("AVERAGE", [2, 4])
    ds = None
    ds = gdal.Open(str(OUT / (name + ".ovr")), gdal.GA_Update)
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    mask = np.full((ds.RasterYSize, ds.RasterXSize), 255, np.uint8)
    mask[6:28, 8:33] = 0
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
    ds.BuildOverviews("AVERAGE", [2])
    ds = None
    cases += [case(name, (31, 27), bands=(1,)), case(name, (26, 22), resampling="bilinear"),
              case(name, (26, 22), resampling="cubic"), case(name, (26, 41), resampling="cubic")]
    # Levels read alone, as gdal_translate -ovr reads them.
    for overview in ("NONE", "0", "1", "9", "AUTO-1"):
        cases += [case("rgb_mask.tif", (20, 15), resampling="bilinear", overview=overview),
                  case("rgb_mask.tif", (12, 9), (3, 5, 54, 38), "average", overview=overview),
                  case("u16_nodata.tif", (25, 20), resampling="nearest", overview=overview),
                  case("u16_nodata.tif", (11, 13), (5.4, 2.2, 50.3, 40.1), "cubic", overview=overview)]
    gdal.SetConfigOption("COMPRESS_OVERVIEW", None)
    gdal.SetConfigOption("GDAL_TIFF_OVR_BLOCKSIZE", None)
    return cases


def resample_alg(gdal: Any, name: str) -> int:
    """The `GRIORA_*` value of a resampling."""
    return {"nearest": gdal.GRIORA_NearestNeighbour, "bilinear": gdal.GRIORA_Bilinear, "cubic": gdal.GRIORA_Cubic,
            "cubicspline": gdal.GRIORA_CubicSpline, "lanczos": gdal.GRIORA_Lanczos, "average": gdal.GRIORA_Average,
            "rms": gdal.GRIORA_RMS, "mode": gdal.GRIORA_Mode}[name]


def hexa(values: np.ndarray) -> str:
    """Values as the hexadecimal of their little-endian bytes."""
    return np.ascontiguousarray(values).astype(values.dtype.newbyteorder("<")).tobytes().hex()


class RasterIO:
    """GDAL's `GDALDatasetRasterIOEx` and `GDALRasterIOEx`, called as rasterio calls them: with the
    floating-point window always given (`bFloatingPointWindowValidity`), and the integer window
    its truncation. GDAL's Python bindings give the floating-point window only when the window is
    not whole, and GDAL then reads an overview by nearest neighbour from the integer window rounded
    to it rather than from the floating-point one scaled.
    """

    def __init__(self, gdal: Any) -> None:
        import ctypes
        import ctypes.util

        self.ctypes = ctypes

        class ExtraArg(ctypes.Structure):
            # GDALRasterIOExtraArg, version 3 (GDAL 3.13).
            _fields_ = [("nVersion", ctypes.c_int), ("eResampleAlg", ctypes.c_int), ("pfnProgress", ctypes.c_void_p),
                        ("pProgressData", ctypes.c_void_p), ("bFloatingPointWindowValidity", ctypes.c_int),
                        ("dfXOff", ctypes.c_double), ("dfYOff", ctypes.c_double), ("dfXSize", ctypes.c_double),
                        ("dfYSize", ctypes.c_double), ("bUseOnlyThisScale", ctypes.c_int), ("bOperateInBufType", ctypes.c_int)]

        self.ExtraArg = ExtraArg
        lib = ctypes.CDLL(ctypes.util.find_library("gdal") or "libgdal.dylib")
        i64, ptr, c_int = ctypes.c_int64, ctypes.c_void_p, ctypes.c_int
        lib.GDALDatasetRasterIOEx.argtypes = [ptr, c_int, c_int, c_int, c_int, c_int, ptr, c_int, c_int, c_int, c_int,
                                              ctypes.POINTER(c_int), i64, i64, i64, ctypes.POINTER(ExtraArg)]
        lib.GDALRasterIOEx.argtypes = [ptr, c_int, c_int, c_int, c_int, c_int, ptr, c_int, c_int, c_int, i64, i64,
                                       ctypes.POINTER(ExtraArg)]
        lib.GDALGetMaskBand.argtypes = [ptr]
        lib.GDALGetMaskBand.restype = ptr
        self.lib = lib
        self.gdal = gdal

    def extra(self, window: tuple[float, float, float, float], alg: int) -> Any:
        x, y, w, h = window
        return self.ExtraArg(3, alg, None, None, 1, x, y, w, h, 0, 1)

    @staticmethod
    def integer(window: tuple[float, float, float, float]) -> tuple[int, int, int, int]:
        x, y, w, h = window
        return int(x), int(y), int(max(1, w)), int(max(1, h))

    def read(self, ds: Any, bands: list[int], window: tuple[float, float, float, float], size: tuple[int, int],
             alg: int) -> np.ndarray:
        """Pixels of `bands`: through the dataset for several, through the band for one."""
        bw, bh = size
        dtype = self.gdal.GetDataTypeName(ds.GetRasterBand(bands[0]).DataType)
        out = np.zeros((len(bands), bh, bw), self.gdal_array_type(dtype))
        extra = self.extra(window, alg)
        buf = out.ctypes.data_as(self.ctypes.c_void_p)
        etype = ds.GetRasterBand(bands[0]).DataType
        if len(bands) == 1:
            err = self.lib.GDALRasterIOEx(int(ds.GetRasterBand(bands[0]).this), 0, *self.integer(window), buf, bw, bh, etype,
                                          0, 0, self.ctypes.byref(extra))
        else:
            band_map = (self.ctypes.c_int * len(bands))(*bands)
            err = self.lib.GDALDatasetRasterIOEx(int(ds.this), 0, *self.integer(window), buf, bw, bh, etype, len(bands), band_map,
                                                 0, 0, 0, self.ctypes.byref(extra))
        assert err == 0, (window, size)
        return out

    def read_mask(self, ds: Any, band: int, window: tuple[float, float, float, float], size: tuple[int, int], alg: int) -> np.ndarray:
        """The mask band of a band (`GetMaskBand()`), read into Byte values."""
        bw, bh = size
        out = np.zeros((bh, bw), np.uint8)
        mask = self.lib.GDALGetMaskBand(int(ds.GetRasterBand(band).this))
        extra = self.extra(window, alg)
        err = self.lib.GDALRasterIOEx(mask, 0, *self.integer(window), out.ctypes.data_as(self.ctypes.c_void_p), bw, bh,
                                      self.gdal.GDT_Byte, 0, 0, self.ctypes.byref(extra))
        assert err == 0, (window, size)
        return out

    @staticmethod
    def gdal_array_type(name: str) -> Any:
        return {"Byte": np.uint8, "UInt16": np.uint16, "Int16": np.int16, "Float32": np.float32}[name]


def gdal_case(gdal: Any, rasterio_like: RasterIO, c: dict[str, Any]) -> dict[str, Any]:
    """What GDAL reads of a case: pixels and (for `AUTO`) masks."""
    path = str(OUT / c["file"])
    ds = gdal.Open(path)
    bands = c["bands"] or list(range(1, ds.RasterCount + 1))
    x, y, w, h = c["window"] or (0, 0, ds.RasterXSize, ds.RasterYSize)
    bw, bh = c["size"]
    alg = resample_alg(gdal, c["resampling"])
    out = {**c}
    if c["overview"] == "AUTO":
        data = rasterio_like.read(ds, bands, (x, y, w, h), (bw, bh), alg)
        masks = np.stack([rasterio_like.read_mask(ds, b, (x, y, w, h), (bw, bh), alg) for b in bands])
        out["masks"] = hexa(masks)
    else:
        name = {"nearest": "near"}.get(c["resampling"], c["resampling"])
        translated = gdal.Translate("/vsimem/level.tif", ds, srcWin=[x, y, w, h], width=bw, height=bh, resampleAlg=name,
                                    overviewLevel=c["overview"], bandList=bands)
        data = translated.ReadAsArray()
        data = data[None] if data.ndim == 2 else data
        translated = None
        gdal.Unlink("/vsimem/level.tif")
    out["dtype"] = data.dtype.name
    out["data"] = hexa(data)
    return out


def check_rasterio(expected: list[dict[str, Any]]) -> int:
    """Compares the `AUTO` cases with rasterio's reads.

    Returns:
        The number of cases that differ.
    """
    import rasterio
    from rasterio.enums import Resampling
    from rasterio.windows import Window

    differ = 0
    for c in expected:
        if c["overview"] != "AUTO":
            continue
        with rasterio.open(OUT / c["file"]) as src:
            bands = c["bands"] or list(range(1, src.count + 1))
            window = None if c["window"] is None else Window(*c["window"])
            shape = (len(bands), c["size"][1], c["size"][0])
            resampling = Resampling[{"cubicspline": "cubic_spline"}.get(c["resampling"], c["resampling"])]
            data = src.read(bands, window=window, out_shape=shape, resampling=resampling)
            masks = np.stack([src.read_masks(b, window=window, out_shape=shape[1:], resampling=resampling) for b in bands])
        if hexa(data) != c["data"] or hexa(masks) != c["masks"]:
            differ += 1
            print("rasterio differs:", {k: c[k] for k in ("file", "bands", "window", "size", "resampling")})
    return differ


def write_mask_control(gdal: Any) -> None:
    """Pin the one resampled case whose first-image mask GDAL otherwise ignores."""
    cases = json.loads(gzip.decompress((OUT / "expected.json.gz").read_bytes()))
    case = next(c for c in cases if c["file"] == "masked_external_overviews.tif"
                and c["size"] == [26, 41] and c["resampling"] == "cubic"
                and c["overview"] == "AUTO" and not c["bands"] and c["window"] is None)
    with tempfile.TemporaryDirectory(prefix="terrio-mask-control-") as temporary:
        path = Path(temporary) / case["file"]
        shutil.copyfile(OUT / case["file"], path)
        overview = Path(str(path) + ".ovr")
        data = bytearray((OUT / (case["file"] + ".ovr")).read_bytes())
        assert data[:4] == b"II*\0"
        offset = struct.unpack_from("<I", data, 4)[0]
        directories = []
        while offset:
            count = struct.unpack_from("<H", data, offset)[0]
            tags = {struct.unpack_from("<H", data, offset + 2 + i * 12)[0]: offset + 2 + i * 12
                    for i in range(count)}
            directories.append(tags)
            offset = struct.unpack_from("<I", data, offset + 2 + count * 12)[0]
        def value(tags, tag):
            """Read an inline SHORT or LONG from the little-endian classic TIFF."""
            entry = tags[tag]
            fmt = "<H" if struct.unpack_from("<H", data, entry + 2)[0] == 3 else "<I"
            return struct.unpack_from(fmt, data, entry + 8)[0]
        image, mask = directories[0], directories[2]
        assert (value(image, 254), value(mask, 254), value(mask, 262)) == (1, 5, 4)
        assert [value(image, tag) for tag in (256, 257)] == [value(mask, tag) for tag in (256, 257)]
        struct.pack_into("<I", data, image[254] + 8, 0)
        struct.pack_into("<I", data, mask[254] + 8, 4)
        overview.write_bytes(data)
        control = gdal_case(gdal, RasterIO(gdal), dict(case, file=str(path)))
    control["file"] = case["file"]
    control["gdal_version"] = gdal.__version__
    control["reference_control"] = "Equivalent first image/mask flags 0/4 instead of 1/5; stored pixels and mask unchanged"
    text = json.dumps([control], separators=(",", ":")) + "\n"
    (OUT / "expected_mask_control.json.gz").write_bytes(gzip.compress(text.encode(), mtime=0))


def main() -> int:
    global OUT
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--check-rasterio", action="store_true")
    parser.add_argument("--mask-control", action="store_true")
    args = parser.parse_args()
    if args.check_rasterio:
        expected = json.loads(gzip.decompress((OUT / "expected.json.gz").read_bytes()))
        differ = check_rasterio(expected)
        print(f"{differ} case(s) differ from rasterio")
        return int(differ > 0)
    from osgeo import gdal

    gdal.UseExceptions()
    if gdal.__version__ != "3.13.3":
        print(f"GDAL 3.13.3 is the pinned generator reference, not {gdal.__version__}", file=sys.stderr)
        return 1
    if args.mask_control:
        write_mask_control(gdal)
        return 0
    destination = OUT
    reference = destination / "expected.json.gz"
    if not reference.is_file():
        parser.error("historical expected.json.gz is required; restore the checked-in fixtures first")
    refreshed = {"rgb_external_overviews.tif", "mixed_external_overviews.tif", "masked_external_overviews.tif", "mixed_internal_overviews.tif"}
    previous = json.loads(gzip.decompress(reference.read_bytes()))
    expected = [dict(c, gdal_version=c.get("gdal_version", "3.13.2")) for c in previous if c["file"] not in refreshed]
    # Generate into scratch space: old TIFFs and their historical oracle values are never rewritten.
    with tempfile.TemporaryDirectory(prefix="terrio-fixtures-") as temporary:
        OUT = Path(temporary)
        try:
            cases = [c for c in write_files(gdal) if c["file"] in refreshed]
            rasterio_like = RasterIO(gdal)
            expected += [dict(gdal_case(gdal, rasterio_like, c), gdal_version=gdal.__version__) for c in cases]
            for name in sorted(refreshed):
                for generated in OUT.glob(name + "*"):
                    shutil.copyfile(generated, destination / generated.name)
        finally:
            OUT = destination
    text = json.dumps(expected, separators=(",", ":")) + "\n"
    (OUT / "expected.json.gz").write_bytes(gzip.compress(text.encode(), mtime=0))
    print(f"{len(expected)} cases in {OUT / 'expected.json.gz'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
