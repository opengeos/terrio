"""Writes the rasters of `crates/terrio/tests/data/float_reads` and what GDAL reads of them as
floating-point values, invalid pixels NaN.

Usage:
    python scripts/make_float_reads_fixtures.py
    python scripts/make_float_reads_fixtures.py --check-rasterio

The rasters have nodata values in each integer type and both floating-point types (at the ends of
the types' ranges, NaN, fractional, outside the type's range, Float32's -3.4e38), per-band nodata
values (in a PAM `.aux.xml`), internal and sidecar masks over nodata values, overviews with masks of
their own, without (sampled from the full-resolution mask) and with nodata values only, a virtual
raster of bands of several types, a GeoTIFF with a band scale and offset, and a netCDF variable
packed with `scale_factor`, `add_offset` and `_FillValue`.

`expected.json` holds, for each case (a file, bands, a window, an overview and whether values are
unscaled), the values GDAL reads with `ReadAsArray` into `Float32` and `Float64` buffers, NaN where
`GetMaskBand` is 0: the mask band of the overview read, or for an overview without a mask of its
own, the full-resolution mask band read at the overview's size, as `LazyRaster::read_mask` samples
it (GDAL's overview band reports no mask then). Unscaled values are `value * scale + offset` with
the band's `GetScale` and `GetOffset`, computed in `Float64`, then rounded to `Float32`, and checked
against `gdal_translate -unscale`. Values are text (`repr`), NaN and infinities included. Needs
GDAL's Python bindings (3.13).

With `--check-rasterio`, the files and `expected.json` are only read: each case is compared with
rasterio's masked reads (`read(masked=True, out_dtype=...)` filled with NaN; overviews as decimated
reads), which needs rasterio rather than GDAL's bindings.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

import numpy as np

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "float_reads"
COLS, ROWS = 6, 4
N = COLS * ROWS
FULL = (0, 0, COLS, ROWS)


def write_files(gdal: Any) -> list[dict[str, Any]]:
    """Writes the rasters.

    Args:
        gdal: GDAL's Python bindings (`osgeo.gdal`).

    Returns:
        The cases: file, bands (empty for all), window `(x, y, width, height)`, overview, whether
        values are unscaled and, for overviews without a mask of their own, `"sampled": True`.
    """

    def tiff(name: str, dtype: int, bands: list[np.ndarray], cols: int = COLS, rows: int = ROWS, nodata: Any = None,
             options: tuple[str, ...] = ()) -> Any:
        ds = gdal.GetDriverByName("GTiff").Create(str(OUT / name), cols, rows, len(bands), dtype, list(options))
        for i, values in enumerate(bands, 1):
            band = ds.GetRasterBand(i)
            band.WriteArray(np.asarray(values).reshape(rows, cols))
            if isinstance(nodata, int) and dtype == gdal.GDT_Int64:
                band.SetNoDataValueAsInt64(nodata)
            elif isinstance(nodata, int) and dtype == gdal.GDT_UInt64:
                band.SetNoDataValueAsUInt64(nodata)
            elif nodata is not None:
                band.SetNoDataValue(nodata)
        return ds

    def case(file: str, bands: tuple[int, ...] = (), window: tuple[int, int, int, int] = FULL, overview: int | None = None,
             unscale: bool = False, sampled: bool = False) -> dict[str, Any]:
        out = {"file": file, "bands": list(bands), "window": list(window), "overview": overview, "unscale": unscale}
        if sampled:
            out["sampled"] = True
        return out

    cases = []
    ramp = np.arange(N)
    # Overviews are tiles of 128 x 128 pixels: compressed, the files stay small.
    gdal.SetConfigOption("COMPRESS_OVERVIEW", "DEFLATE")

    # Byte: a nodata value shared by both bands, at the end of the type's range.
    b1 = ramp.astype(np.uint8)
    b1[[3, 10]] = 255
    tiff("byte.tif", gdal.GDT_Byte, [b1, (255 - ramp).astype(np.uint8)], nodata=255)
    cases += [case("byte.tif"), case("byte.tif", (2,), (1, 1, 3, 2)), case("byte.tif", (2, 1), (0, 2, 6, 2))]
    # A fractional nodata value, which GDAL truncates to the type (1), and one outside its range.
    tiff("byte_fractional_nodata.tif", gdal.GDT_Byte, [(ramp % 4).astype(np.uint8)], nodata=1.5)
    tiff("byte_nodata_out_of_range.tif", gdal.GDT_Byte, [(ramp % 4).astype(np.uint8)], nodata=-9999)
    cases += [case("byte_fractional_nodata.tif"), case("byte_nodata_out_of_range.tif")]
    # Int8: a negative fractional value, truncated toward zero (-1).
    tiff("int8.tif", gdal.GDT_Int8, [(ramp - 12).astype(np.int8)], nodata=-1.5)
    cases.append(case("int8.tif"))
    u16 = (ramp * 2731).astype(np.uint16)
    u16[[0, 5, 23]] = [65535, 65534, 0]
    tiff("uint16.tif", gdal.GDT_UInt16, [u16], nodata=65535)
    cases += [case("uint16.tif"), case("uint16.tif", window=(0, 0, 3, 2))]
    # Int16 with overviews and no mask: nodata values at both levels.
    i16 = (np.arange(48) * 1361 - 32768).astype(np.int16)
    i16[[0, 2, 17, 47]] = [-32768, -32768, 32767, -32767]
    ds = tiff("int16.tif", gdal.GDT_Int16, [i16], cols=8, rows=6, nodata=-32768)
    ds.BuildOverviews("NEAREST", [2])
    ds = None
    cases += [case("int16.tif", window=(0, 0, 8, 6)), case("int16.tif", overview=0, window=(0, 0, 4, 3)),
              case("int16.tif", overview=0, window=(1, 1, 2, 2))]
    # 32- and 64-bit integers next to their nodata values: rounded alike to f32 (or f64), but valid.
    u32 = np.array([4294967295, 4294967294, 16777217, 16777216, 0, 4294967295] * 4, dtype=np.uint32)
    tiff("uint32.tif", gdal.GDT_UInt32, [u32], nodata=4294967295)
    i32 = np.array([-2147483647, -2147483646, -2147483648, 2147483647, 16777217, -16777217] * 4, dtype=np.int32)
    tiff("int32.tif", gdal.GDT_Int32, [i32], nodata=-2147483647)
    i64 = np.array([2**53 + 1, 2**53, 2**62 + 2**38 + 1, -(2**63), 2**63 - 1, -(2**53) - 1] * 4, dtype=np.int64)
    tiff("int64.tif", gdal.GDT_Int64, [i64], nodata=2**53)
    u64 = np.array([2**64 - 2, 2**64 - 1, 0, 2**53 + 1, 2**63, 1] * 4, dtype=np.uint64)
    tiff("uint64.tif", gdal.GDT_UInt64, [u64], nodata=2**64 - 1)
    cases += [case("uint32.tif"), case("int32.tif"), case("int64.tif"), case("uint64.tif")]
    # Float32: -3.4e38 (f32::MIN within GDAL's tolerance of it), NaN, infinities, subnormals.
    f32 = np.array([-3.4e38, np.finfo(np.float32).min, np.inf, -np.inf, np.nan, 1.5, -0.0, np.finfo(np.float32).max, 1e-45, 0.1,
                    -3.3999998e38, 2.0] * 2, dtype=np.float32)
    tiff("float32.tif", gdal.GDT_Float32, [f32], nodata=-3.4e38)
    tiff("float32_nan.tif", gdal.GDT_Float32, [f32], nodata=float("nan"))
    cases += [case("float32.tif"), case("float32_nan.tif"), case("float32_nan.tif", window=(2, 1, 4, 3))]
    # Float64: values within GDAL's tolerance of the nodata value, and beyond f32's range.
    f64 = np.array([-9999.0, -9999.0000001, -9998.99, 1e39, -1e39, 3.4028235677973366e38, 1e-46, np.nan, 0.1, 2.0**53 + 1, -0.0,
                    9999.0] * 2, dtype=np.float64)
    tiff("float64.tif", gdal.GDT_Float64, [f64], nodata=-9999.0)
    cases.append(case("float64.tif"))
    # Per-band nodata values, kept by GDAL in the PAM `.aux.xml` of a GeoTIFF opened read-only.
    tiff("per_band_nodata.tif", gdal.GDT_Int16, [(ramp % 3 - 1).astype(np.int16), (ramp % 4).astype(np.int16),
                                                 (ramp % 5).astype(np.int16)])
    ds = gdal.Open(str(OUT / "per_band_nodata.tif"))
    # GDAL warns that the GeoTIFF tag holds one value, then keeps both in the `.aux.xml`.
    with gdal.quiet_errors():
        ds.GetRasterBand(1).SetNoDataValue(-1)
        ds.GetRasterBand(2).SetNoDataValue(0)
    ds = None
    cases += [case("per_band_nodata.tif"), case("per_band_nodata.tif", (3, 1), (1, 1, 4, 2))]
    # Bands of several types, each with its nodata value: read together as Float64 by terrio.
    sources = [("mixed_uint32.tif", gdal.GDT_UInt32, np.array([4294967295, 4294967294, 7] * 8, dtype=np.uint32), "UInt32", "4294967295"),
               ("mixed_float32.tif", gdal.GDT_Float32, np.array([np.nan, 1.5, -2.25] * 8, dtype=np.float32), "Float32", "nan"),
               ("mixed_byte.tif", gdal.GDT_Byte, (ramp % 3).astype(np.uint8), "Byte", "1.5")]
    bands = []
    for i, (name, dtype, values, type_name, nodata) in enumerate(sources, 1):
        tiff(name, dtype, [values])
        bands.append(f'  <VRTRasterBand dataType="{type_name}" band="{i}">\n    <NoDataValue>{nodata}</NoDataValue>\n'
                     f'    <SimpleSource>\n      <SourceFilename relativeToVRT="1">{name}</SourceFilename>\n'
                     f'      <SourceBand>1</SourceBand>\n    </SimpleSource>\n  </VRTRasterBand>\n')
    (OUT / "mixed_types.vrt").write_text(f'<VRTDataset rasterXSize="{COLS}" rasterYSize="{ROWS}">\n'
                                         f'  <GeoTransform>0, 1, 0, {ROWS}, 0, -1</GeoTransform>\n{"".join(bands)}</VRTDataset>\n')
    cases += [case("mixed_types.vrt"), case("mixed_types.vrt", (3, 1), (2, 0, 3, 4))]
    # An internal mask over a nodata value, which it replaces: some pixels of the nodata value are
    # valid, some others invalid. Overviews built after the mask have masks of their own.
    mask = np.full((6, 8), 255, np.uint8)
    mask[1, 1:7] = 0
    mask[4:, 0] = 0
    mask[2, 3] = 0
    values = (np.arange(48) % 7).astype(np.uint16)
    ds = tiff("masked.tif", gdal.GDT_UInt16, [values, values + 100], cols=8, rows=6, nodata=0)
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
    ds.BuildOverviews("NEAREST", [2])
    ds = None
    cases += [case("masked.tif", window=(0, 0, 8, 6)), case("masked.tif", (2,), (2, 1, 4, 3)),
              case("masked.tif", overview=0, window=(0, 0, 4, 3)), case("masked.tif", (2,), (1, 1, 3, 2), overview=0)]
    # Overviews built before the mask: none has a mask of its own.
    ds = tiff("masked_sampled_overview.tif", gdal.GDT_Float32, [np.arange(48, dtype=np.float32) / 4], cols=8, rows=6)
    ds.BuildOverviews("NEAREST", [2])
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    # The overview's pixels are sampled from rows 1, 3, 5 and columns 1, 3, 5, 7: columns 1 and 5
    # of those rows invalid, and a pixel no sample reads.
    sampled = np.full((6, 8), 255, np.uint8)
    sampled[1::2, 1::4] = 0
    sampled[0, 0] = 0
    ds.GetRasterBand(1).GetMaskBand().WriteArray(sampled)
    ds = None
    cases += [case("masked_sampled_overview.tif", window=(0, 0, 8, 6)),
              case("masked_sampled_overview.tif", overview=0, window=(0, 0, 4, 3), sampled=True),
              case("masked_sampled_overview.tif", overview=0, window=(1, 1, 2, 2), sampled=True)]
    # A `.msk` sidecar.
    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "NO")
    ds = tiff("sidecar_mask.tif", gdal.GDT_Byte, [ramp.astype(np.uint8)], nodata=5)
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask[:ROWS, :COLS])
    ds = None
    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", None)
    cases += [case("sidecar_mask.tif"), case("sidecar_mask.tif", window=(1, 1, 5, 3))]
    # A band scale and offset (GDAL_METADATA).
    ds = tiff("scaled.tif", gdal.GDT_UInt16, [(ramp * 2311).astype(np.uint16)], nodata=0)
    ds.GetRasterBand(1).SetScale(0.0001)
    ds.GetRasterBand(1).SetOffset(-0.1)
    ds = None
    cases += [case("scaled.tif"), case("scaled.tif", unscale=True), case("scaled.tif", window=(2, 1, 3, 3), unscale=True)]
    # A netCDF variable of two bands (time), packed: Int16 with scale_factor, add_offset and
    # _FillValue (the nodata value).
    ds = gdal.GetDriverByName("netCDF").CreateMultiDimensional(str(OUT / "packed.nc"))
    root = ds.GetRootGroup()
    dims = [root.CreateDimension(name, None, None, size) for name, size in [("time", 2), ("y", ROWS), ("x", COLS)]]
    time = root.CreateMDArray("time", dims[:1], gdal.ExtendedDataType.Create(gdal.GDT_Float64))
    for name, value in [("standard_name", "time"), ("units", "days since 2000-01-01")]:
        time.CreateAttribute(name, [], gdal.ExtendedDataType.CreateString()).Write(value)
    time.Write(np.array([0.0, 1.0]))
    var = root.CreateMDArray("t", dims, gdal.ExtendedDataType.Create(gdal.GDT_Int16))
    var.SetNoDataValueDouble(-32767)
    var.SetScale(0.01)
    var.SetOffset(273.15)
    packed = (np.arange(2 * N) * 1543 - 18000).astype(np.int16).reshape(2, ROWS, COLS)
    packed[0, 0, [0, 4]] = [-32767, -32768]
    packed[1, 2, 2] = -32767
    packed[1, 3, 5] = 32767
    var.Write(packed)
    ds = None
    cases += [case("packed.nc"), case("packed.nc", unscale=True), case("packed.nc", (2,), (1, 1, 4, 3), unscale=True)]
    gdal.SetConfigOption("COMPRESS_OVERVIEW", None)
    return cases


def gdal_path(file: str) -> str:
    """The name GDAL opens a fixture by: a netCDF file's packed variable as a subdataset."""
    path = str(OUT / file)
    return f'NETCDF:"{path}":t' if file.endswith(".nc") else path


def text(values: np.ndarray) -> list[str]:
    """Values as text that Rust's `f64::from_str` reads back exactly (`nan`, `inf`, `-inf`)."""
    return [repr(float(v)) for v in values.ravel()]


def gdal_case(gdal: Any, c: dict[str, Any]) -> dict[str, Any]:
    """What GDAL reads of a case: its `Float32` and `Float64` values, NaN where invalid."""
    ds = gdal.Open(gdal_path(c["file"]))
    x, y, w, h = c["window"]
    out32, out64 = [], []
    # A mask of the dataset's own (internal or sidecar), not one of nodata values.
    flags = ds.GetRasterBand(1).GetMaskFlags()
    mask_of_its_own = flags == gdal.GMF_PER_DATASET
    for b in c["bands"] or range(1, ds.RasterCount + 1):
        band = ds.GetRasterBand(b)
        level = band if c["overview"] is None else band.GetOverview(c["overview"])
        v32 = level.ReadAsArray(x, y, w, h, buf_type=gdal.GDT_Float32)
        v64 = level.ReadAsArray(x, y, w, h, buf_type=gdal.GDT_Float64)
        if c["overview"] is None:
            mask = band.GetMaskBand().ReadAsArray(x, y, w, h)
        else:
            # The full-resolution mask band read at the overview's size: its overview when it has
            # one (the overview's own mask), else sampled by nearest neighbour.
            fx, fy = band.XSize // level.XSize, band.YSize // level.YSize
            decimated = band.GetMaskBand().ReadAsArray(x * fx, y * fy, w * fx, h * fy, buf_xsize=w, buf_ysize=h)
            if c.get("sampled"):
                assert band.GetMaskBand().GetOverviewCount() == 0
                mask = decimated
            else:
                mask = level.GetMaskBand().ReadAsArray(x, y, w, h)
                assert (mask == decimated).all(), c
        if c["unscale"]:
            scale, offset = band.GetScale(), band.GetOffset()
            raw = v64
            v64 = v64 * (1.0 if scale is None else scale) + (0.0 if offset is None else offset)
            v32 = v64.astype(np.float32)
            check_unscale(gdal, c, b, raw, v32, v64, mask)
        out32.append(np.where(mask == 0, np.float32(np.nan), v32))
        out64.append(np.where(mask == 0, np.nan, v64))
    return {**c, "per_dataset_mask": mask_of_its_own, "f32": text(np.stack(out32)), "f64": text(np.stack(out64))}


def check_unscale(gdal: Any, c: dict[str, Any], band: int, raw: np.ndarray, v32: np.ndarray, v64: np.ndarray, mask: np.ndarray) -> None:
    """Checks unscaled values against `gdal_translate -unscale`, on the valid pixels.

    GDAL computes `value * scale + offset` as C++ does: where its compiler contracts the expression
    into one fused multiply-add (clang on arm64), `Float64` results are rounded once rather than
    twice, and can be one ULP from the values here, which round twice as a build that does not
    fuse them does. They are then checked against `math.fma` (Python 3.13). `Float32` results are
    rounded alike.
    """
    import math

    x, y, w, h = c["window"]
    ds = gdal.Open(gdal_path(c["file"]))
    scale, offset = ds.GetRasterBand(band).GetScale(), ds.GetRasterBand(band).GetOffset()
    scale, offset = 1.0 if scale is None else scale, 0.0 if offset is None else offset
    valid = mask != 0
    fused = np.array([math.fma(v, scale, offset) for v in raw[valid]])
    for dtype, values in [(gdal.GDT_Float32, v32), (gdal.GDT_Float64, v64)]:
        out = gdal.Translate("/vsimem/unscaled.tif", gdal_path(c["file"]), bandList=[band], unscale=True, outputType=dtype,
                             srcWin=[x, y, w, h])
        translated = out.GetRasterBand(1).ReadAsArray()[valid]
        out = None
        gdal.Unlink("/vsimem/unscaled.tif")
        same = translated.astype(values.dtype) == values[valid]
        if dtype == gdal.GDT_Float64:
            same |= translated == fused
        assert same.all(), (c, dtype)


def check_rasterio(expected: list[dict[str, Any]]) -> int:
    """Compares the expected values with rasterio's masked reads.

    Returns:
        The number of cases that differ.
    """
    import rasterio
    from rasterio.windows import Window

    with rasterio.Env() as env:
        drivers = env.drivers()
    differ, skipped = 0, 0
    for c in expected:
        path = str(OUT / c["file"])
        if c["file"].endswith(".nc") and "netCDF" not in drivers:
            skipped += 1
            continue
        name = f'NETCDF:"{path}":t' if c["file"].endswith(".nc") else path
        with rasterio.open(name) as src:
            bands = c["bands"] or list(range(1, src.count + 1))
            x, y, w, h = c["window"]
            if c["overview"] is None:
                window, shape = Window(x, y, w, h), None
            else:
                # A decimated read of the full-resolution window, which GDAL reads from the overview.
                f = src.overviews(bands[0])[c["overview"]]
                window, shape = Window(x * f, y * f, w * f, h * f), (h, w)
            wrong = []
            for key, dtype in [("f32", "float32"), ("f64", "float64")]:
                # Band by band: rasterio reads bands of several types one at a time.
                out_dtype = "float64" if c["unscale"] else dtype
                read = np.ma.stack([src.read(b, window=window, out_shape=shape, masked=True, out_dtype=out_dtype) for b in bands])
                if c["unscale"]:
                    scales = np.array([src.scales[b - 1] for b in bands])[:, None, None]
                    offsets = np.array([src.offsets[b - 1] for b in bands])[:, None, None]
                    read = (read * scales + offsets).astype(dtype)
                values = read.filled(np.nan).ravel()
                want = np.array([float(v) for v in c[key]], dtype=dtype)
                same = (values == want) | (np.isnan(values) & np.isnan(want))
                if not same.all():
                    wrong.append(f"{key}: {(~same).sum()} of {same.size} values")
            if wrong:
                differ += 1
                print(f"differs: {c['file']} bands {c['bands']} window {c['window']} overview {c['overview']} unscale {c['unscale']}: "
                      f"{', '.join(wrong)}")
    compared = len(expected) - skipped
    print(f"{compared - differ} of {compared} cases identical to rasterio's reads (rasterio {rasterio.__version__}, "
          f"GDAL {rasterio.__gdal_version__})" + (f"; {skipped} netCDF cases skipped: no netCDF driver" if skipped else ""))
    return differ


def main() -> None:
    """Writes the files and `expected.json`, or checks them against rasterio."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check-rasterio", action="store_true", help="compare expected.json with rasterio's masked reads")
    args = parser.parse_args()
    if args.check_rasterio:
        expected = json.loads((OUT / "expected.json").read_text())
        sys.exit(1 if check_rasterio(expected) else 0)
    from osgeo import gdal

    gdal.UseExceptions()
    OUT.mkdir(parents=True, exist_ok=True)
    for f in OUT.iterdir():
        f.unlink()
    cases = write_files(gdal)
    expected = [gdal_case(gdal, c) for c in cases]
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")
    print(f"{len(expected)} cases, GDAL {gdal.__version__}")


if __name__ == "__main__":
    main()
