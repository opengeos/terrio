"""Check terrio's raster drivers against GDAL, pixel by pixel, in both directions.

Usage: python scripts/raster_roundtrip.py [--keep DIR] [FORMAT ...]

For every format (GDAL short names, such as ``GTiff``, ``AAIGrid`` or ``JP2OpenJPEG``), and for
every test raster the format can hold:

- read: GDAL writes the raster in the format, terrio reads it (``terrio convert`` to a GeoTIFF),
  and the pixels, data type, geotransform, CRS and nodata must match what GDAL reads;
- write: terrio writes the raster in the format (``terrio convert`` from a GeoTIFF written by
  GDAL), GDAL reads it back, and the same properties must match the source.

The test rasters are synthetic (every data type, several bands, NaN and finite nodata, geographic,
projected and user-defined CRSs). Needs GDAL's Python bindings with NumPy, and a built ``terrio``
binary (``target/release/terrio``, or the path in ``TERRIO_BIN``). Prints a table and exits
non-zero when any check fails.
"""

import argparse
import math
import os
import shutil
import subprocess
import sys
import tempfile

import numpy as np
from osgeo import gdal, osr

gdal.UseExceptions()

TERRIO = os.environ.get("TERRIO_BIN", os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))

# A custom Lambert Conformal Conic CRS with no EPSG code.
CUSTOM_WKT = (
    'PROJCS["custom_lcc",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic_2SP"],'
    'PARAMETER["standard_parallel_1",33],PARAMETER["standard_parallel_2",45],PARAMETER["latitude_of_origin",39],'
    'PARAMETER["central_meridian",-96],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1]]'
)

# Format: (extension, GDAL creation options, data types it holds, max bands, lossy, terrio writes).
FORMATS = {
    "GTiff": (".tif", [], None, 8, False, True),
    "COG": (".tif", [], None, 8, False, True),
    "AAIGrid": (".asc", [], {"Byte", "Int16", "UInt16", "Int32", "Float32"}, 1, False, True),
    "EHdr": (".bil", [], {"Byte", "Int8", "Int16", "UInt16", "Int32", "UInt32", "Float32"}, 8, False, True),
    "ENVI": (".envi", [], {"Byte", "Int16", "UInt16", "Int32", "UInt32", "Float32", "Float64", "Int64", "UInt64"}, 8, False, True),
    "GSBG": (".grd", [], {"Float32"}, 1, False, True),
    "SAGA": (".sdat", [], {"Byte", "Int16", "UInt16", "Int32", "UInt32", "Float32", "Float64"}, 1, False, True),
    "RST": (".rst", [], {"Byte", "Int16", "Float32"}, 1, False, True),
    "ERS": (".ers", [], {"Byte", "Int8", "Int16", "UInt16", "Int32", "UInt32", "Float32", "Float64"}, 8, False, True),
    "HFA": (".img", [], {"Byte", "Int8", "Int16", "UInt16", "Int32", "UInt32", "Float32", "Float64"}, 8, False, False),
    "PNG": (".png", ["WORLDFILE=YES"], {"Byte", "UInt16"}, 4, False, True),
    "JPEG": (".jpg", ["WORLDFILE=YES", "QUALITY=95"], {"Byte"}, 3, True, True),
    "JP2OpenJPEG": (".jp2", ["REVERSIBLE=YES", "QUALITY=100"], {"Byte", "Int16", "UInt16"}, 4, False, True),
    "XYZ": (".xyz", [], {"Byte", "Int16", "UInt16", "Int32", "Float32"}, 1, False, True),
    "Zarr": (".zarr", [], None, 8, False, True),
    "netCDF": (".nc", ["FORMAT=NC4"], {"Byte", "Int8", "Int16", "UInt16", "Int32", "UInt32", "Int64", "UInt64", "Float32", "Float64"}, 8, False, True),
    "GPKG": (".gpkg", [], {"Byte"}, 4, False, False),
}

# What a format cannot hold, so GDAL's read-back of a file differs from the source whoever
# writes it: the data type (types inferred from the values), or the nodata value.
NO_TYPE = {"AAIGrid", "XYZ"}
NO_NODATA = {"XYZ", "GSBG", "SAGA", "PNG", "JPEG", "GPKG"}

# Types a format stores as another one (GDAL does the same): source type -> type read back.
TYPE_CHANGES = {"netCDF": {"UInt32": "Float64", "Int64": "Float64", "UInt64": "Float64"}}

# Without a nodata value, GDAL's netCDF driver reports the type's default fill value, whoever wrote
# the file.
NETCDF_DEFAULT_FILL = {"Byte": None, "Int8": -127.0, "Int16": -32767.0, "UInt16": 65535.0, "Int32": -2147483647.0, "Float32": 9.969209968386869e36, "Float64": 9.969209968386869e36}

# Zarr v3 needs a fill value, which GDAL reads as nodata: terrio writes 0 for integers and NaN
# for floats when the source has no nodata (GDAL writes an invalid null fill value).
ZARR_V3_DEFAULT_FILL = {t: 0.0 for t in ("Byte", "Int8", "Int16", "UInt16", "Int32", "UInt32", "Int64", "UInt64")} | {"Float32": math.nan, "Float64": math.nan}

# Extra ``terrio convert`` options to try when writing, per format.
WRITE_VARIANTS = {
    "GTiff": [[], ["--compress", "LZW"], ["--compress", "DEFLATE", "--tile-size", "256"], ["--compress", "ZSTD", "--bigtiff"], ["--compress", "PACKBITS"]],
    "COG": [[], ["--compress", "DEFLATE"], ["--compress", "ZSTD", "--tile-size", "256", "--bigtiff"], ["--compress", "NONE"]],
    "Zarr": [[], ["--compress", "ZLIB"], ["--co", "FORMAT=ZARR_V3"], ["--co", "FORMAT=ZARR_V3", "--compress", "ZSTD"], ["--co", "FORMAT=ZARR_V3", "--compress", "GZIP"]],
    "netCDF": [[], ["--co", "FORMAT=NC2"]],
}

# More GDAL creation options to read files written with, per format (besides those in FORMATS).
READ_VARIANTS = {
    "netCDF": [["FORMAT=NC"]],
    "Zarr": [["FORMAT=ZARR_V3"], ["FORMAT=ZARR_V3", "CHUNK_MEMORY_LAYOUT=F", "BLOCKSIZE=1,16,16"], ["CHUNK_MEMORY_LAYOUT=F"], ["COMPRESS=BLOSC", "BLOSC_SHUFFLE=BIT"]],
}

# GDAL driver name -> terrio format name for writing.
TERRIO_NAMES = {"JP2OpenJPEG": "JP2", "netCDF": "netCDF"}


def make_sources(tmp):
    """Write the synthetic test rasters as GeoTIFFs with GDAL.

    Args:
        tmp: A directory for the files.

    Returns:
        A list of ``(name, path, data type name, band count)`` tuples.
    """
    rng = np.random.default_rng(42)
    specs = [
        # name, data type, bands, nodata, crs, size
        ("byte_rgb_utm", gdal.GDT_Byte, 3, None, "EPSG:32611", (67, 45)),
        ("byte_nd0_geo", gdal.GDT_Byte, 1, 0, "EPSG:4326", (60, 40)),
        ("int8", gdal.GDT_Int8, 1, -128, "EPSG:4326", (33, 21)),
        ("uint16", gdal.GDT_UInt16, 2, 65535, "EPSG:3857", (50, 30)),
        ("int16_nd", gdal.GDT_Int16, 1, -32768, "EPSG:4326", (64, 48)),
        ("int16_plain", gdal.GDT_Int16, 1, None, "EPSG:4269", (41, 37)),
        ("uint32", gdal.GDT_UInt32, 1, None, "EPSG:32633", (30, 20)),
        ("int32_nd", gdal.GDT_Int32, 1, -9999, "EPSG:3857", (52, 31)),
        ("uint64", gdal.GDT_UInt64, 1, None, "EPSG:4326", (20, 10)),
        ("int64", gdal.GDT_Int64, 1, None, "EPSG:4326", (20, 10)),
        ("float32_nd", gdal.GDT_Float32, 1, -9999.0, "EPSG:26915", (71, 53)),
        ("float32_nan", gdal.GDT_Float32, 2, float("nan"), "EPSG:4326", (45, 35)),
        ("float64_custom", gdal.GDT_Float64, 1, -3.4e38, CUSTOM_WKT, (40, 30)),
        ("float32_big", gdal.GDT_Float32, 1, -9999.0, "EPSG:32610", (1100, 700)),
    ]
    out = []
    for name, dt, bands, nodata, crs, (w, h) in specs:
        path = os.path.join(tmp, f"src_{name}.tif")
        ds = gdal.GetDriverByName("GTiff").Create(path, w, h, bands, dt)
        srs = osr.SpatialReference()
        srs.SetFromUserInput(crs)
        ds.SetProjection(srs.ExportToWkt())
        if srs.IsGeographic():
            ds.SetGeoTransform([-110.5, 0.01, 0, 40.25, 0, -0.01])
        else:
            ds.SetGeoTransform([500000.0, 30.0, 0, 4100000.0, 0, -30.0])
        np_type = gdal.GetDataTypeName(dt)
        for b in range(bands):
            if np_type == "Byte":
                a = rng.integers(1, 256, (h, w))
            elif np_type == "Int8":
                a = rng.integers(-127, 128, (h, w))
            elif np_type.startswith("Float"):
                a = rng.normal(500, 200, (h, w))
                if np_type == "Float32":
                    a = a.astype(np.float32)
            elif np_type == "UInt16":
                a = rng.integers(0, 65535, (h, w))
            elif np_type == "Int16":
                a = rng.integers(-32767, 32767, (h, w))
            elif np_type.startswith("UInt"):
                a = rng.integers(0, 4_000_000_000, (h, w))
            else:
                a = rng.integers(-2_000_000_000, 2_000_000_000, (h, w))
            if nodata is not None:
                a = a.astype(np.float64 if np_type == "Float64" else a.dtype)
                a[: h // 5, : w // 6] = nodata
            band = ds.GetRasterBand(b + 1)
            band.WriteArray(a)
            if nodata is not None:
                band.SetNoDataValue(nodata)
        ds = None
        out.append((name, path, np_type, bands))
    return out


def gdal_open(path):
    """Open a raster with GDAL, or its first subdataset when it has several (netCDF).

    Args:
        path: The dataset path.

    Returns:
        The GDAL dataset.
    """
    ds = gdal.Open(path)
    subdatasets = ds.GetSubDatasets()
    if ds.RasterCount == 0 and subdatasets:
        ds = gdal.Open(subdatasets[0][0])
    return ds


def same_crs(a, b):
    """Whether two GDAL datasets have the same CRS (both empty counts as the same).

    Args:
        a: A GDAL dataset.
        b: Another GDAL dataset.

    Returns:
        True when the CRSs match.
    """
    wa, wb = a.GetProjection(), b.GetProjection()
    if not wa or not wb:
        return not wa and not wb
    sa, sb = osr.SpatialReference(wkt=wa), osr.SpatialReference(wkt=wb)
    return bool(sa.IsSame(sb, ["CRITERION=EQUIVALENT_EXCEPT_AXIS_ORDER_GEOGCRS"]))


def compare(expected, actual, lossy=0, check_crs=True, check_nodata=True, check_type=True, type_changes=None, default_fill=None):
    """Compare two GDAL datasets: size, bands, types, geotransform, CRS, nodata and pixels.

    Args:
        expected: The reference dataset.
        actual: The dataset under test.
        lossy: The largest pixel difference allowed (lossy compressions), or 0.
        check_crs: Compare the CRSs.
        check_nodata: Compare the nodata values.
        check_type: Compare the data types.
        type_changes: Expected data type name by source type name, for types the format changes.
        default_fill: Nodata value by data type name reported by the reader when the source has none.

    Returns:
        A list of differences (empty when they match).
    """
    diffs = []
    if (expected.RasterXSize, expected.RasterYSize) != (actual.RasterXSize, actual.RasterYSize):
        return [f"size {actual.RasterXSize}x{actual.RasterYSize} vs {expected.RasterXSize}x{expected.RasterYSize}"]
    if expected.RasterCount != actual.RasterCount:
        return [f"bands {actual.RasterCount} vs {expected.RasterCount}"]
    ge, ga = expected.GetGeoTransform(), actual.GetGeoTransform()
    if not all(math.isclose(x, y, rel_tol=1e-9, abs_tol=1e-9) for x, y in zip(ge, ga)):
        diffs.append(f"geotransform {list(ga)} vs {list(ge)}")
    # Where GDAL itself loses the CRS (multi-band Zarr), terrio may still recover it.
    if check_crs and expected.GetProjection() and not same_crs(expected, actual):
        diffs.append("CRS differs")
    for i in range(1, expected.RasterCount + 1):
        be, ba = expected.GetRasterBand(i), actual.GetRasterBand(i)
        te, ta = gdal.GetDataTypeName(be.DataType), gdal.GetDataTypeName(ba.DataType)
        if check_type and (type_changes or {}).get(te, te) != ta:
            diffs.append(f"band {i} type {ta} vs {te}")
        ne, na = be.GetNoDataValue(), ba.GetNoDataValue()
        if ne is None and na is not None and default_fill is not None and ta in default_fill:
            d = default_fill[ta]
            if d is not None and (na == d or (math.isnan(na) and math.isnan(d))):
                na = None
        if check_nodata and not (ne == na or (ne is not None and na is not None and math.isnan(ne) and math.isnan(na))):
            diffs.append(f"band {i} nodata {na} vs {ne}")
        ae, aa = be.ReadAsArray(), ba.ReadAsArray()
        if not check_nodata and ne is not None:
            # The format replaces nodata pixels with its own marker: compare the others.
            valid = ~np.isnan(ae) if math.isnan(ne) else ae != ne
            ae, aa = ae[valid], aa[valid]
        if lossy:
            # Decoders may round differently; encoders differ more.
            err = np.abs(ae.astype(np.float64) - aa.astype(np.float64))
            if err.max() > lossy:
                diffs.append(f"band {i} pixels differ by up to {err.max()} (mean {err.mean():.2f})")
        elif not np.array_equal(ae, aa, equal_nan=ae.dtype.kind == "f" and aa.dtype.kind == "f"):
            n = int(np.sum(ae != aa))
            diffs.append(f"band {i} pixels differ ({n} of {ae.size})")
    return diffs


def terrio(*args):
    """Run terrio.

    Args:
        *args: Command-line arguments.

    Returns:
        None on success, or the error message.
    """
    p = subprocess.run([TERRIO, *args], capture_output=True, text=True)
    return None if p.returncode == 0 else (p.stderr.strip() or "failed")[-300:]


def fits(fmt, dtype, bands):
    """Whether a format can hold a raster of this type and band count.

    Args:
        fmt: The GDAL driver name.
        dtype: The GDAL data type name.
        bands: The band count.

    Returns:
        True when it can.
    """
    _, _, types, max_bands, _, _ = FORMATS[fmt]
    return (types is None or dtype in types) and bands <= max_bands


def check_format(fmt, sources, tmp):
    """Run the read and write checks of one format.

    Args:
        fmt: The GDAL driver name.
        sources: The test rasters from :func:`make_sources`.
        tmp: A directory for the files.

    Returns:
        A dict with lists of results under ``read`` and ``write``.
    """
    ext, co, _, _, lossy, writable = FORMATS[fmt]
    results = {"read": [], "write": []}
    for name, src, dtype, bands in sources:
        if not fits(fmt, dtype, bands) or (fmt not in ("GTiff", "COG") and name == "float32_big"):
            continue
        src_ds = gdal.Open(src)
        # GDAL writes, terrio reads.
        for r, read_co in enumerate([co] + READ_VARIANTS.get(fmt, [])):
            label = name if r == 0 else f"{name} {' '.join(read_co)}"
            g_path = os.path.join(tmp, f"gdal_{fmt}_{name}_{r}{ext}")
            if "BLOCKSIZE=1,16,16" in read_co and bands == 1:
                read_co = [c for c in read_co if not c.startswith("BLOCKSIZE")] + ["BLOCKSIZE=16,16"]
            try:
                gdal.Translate(g_path, src_ds, format=fmt, creationOptions=read_co)
                t_tif = os.path.join(tmp, f"terrio_read_{fmt}_{name}_{r}.tif")
                err = terrio("convert", g_path, t_tif)
                if err:
                    results["read"].append((label, [f"terrio: {err}"]))
                else:
                    results["read"].append((label, compare(gdal_open(g_path), gdal.Open(t_tif), lossy=3 if lossy else 0)))
            except RuntimeError as e:
                results["read"].append((label, [f"GDAL cannot write it: {e}"]))
        # terrio writes, GDAL reads.
        if not writable:
            continue
        for v, variant in enumerate(WRITE_VARIANTS.get(fmt, [[]])):
            t_path = os.path.join(tmp, f"terrio_{fmt}_{name}_{v}{ext}")
            err = terrio("convert", src, t_path, "-f", TERRIO_NAMES.get(fmt, fmt), *variant)
            label = f"{name} {' '.join(variant)}".strip()
            if err:
                results["write"].append((label, [f"terrio: {err}"]))
                continue
            try:
                diffs = compare(
                    src_ds,
                    gdal.Open(t_path),
                    lossy=64 if lossy else 0,
                    check_nodata=fmt not in NO_NODATA,
                    check_type=fmt not in NO_TYPE,
                    type_changes=TYPE_CHANGES.get(fmt),
                    default_fill=NETCDF_DEFAULT_FILL if fmt == "netCDF" else ZARR_V3_DEFAULT_FILL if "FORMAT=ZARR_V3" in variant else None,
                )
            except RuntimeError as e:
                diffs = [f"GDAL cannot read it: {e}"]
            if fmt == "COG":
                from osgeo_utils.samples import validate_cloud_optimized_geotiff as cog

                _, errors, _ = cog.validate(t_path, full_check=True)
                diffs += [f"COG: {e}" for e in errors]
            results["write"].append((label, diffs))
    return results


def main():
    """Run the checks and print a summary table."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("formats", nargs="*", help="GDAL driver names (default: all)")
    parser.add_argument("--keep", help="keep the files in this directory")
    parser.add_argument("-v", "--verbose", action="store_true", help="print every difference")
    args = parser.parse_args()
    formats = args.formats or list(FORMATS)
    tmp = args.keep or tempfile.mkdtemp(prefix="terrio_rt_")
    os.makedirs(tmp, exist_ok=True)
    gdal.PushErrorHandler("CPLQuietErrorHandler")
    sources = make_sources(tmp)
    failed = False
    print(f"{'format':<12} {'read':>12} {'write':>12}")
    for fmt in formats:
        res = check_format(fmt, sources, tmp)
        cells = []
        for kind in ("read", "write"):
            items = res[kind]
            ok = sum(1 for _, d in items if not d)
            cells.append(f"{ok}/{len(items)}" if items else "-")
            for name, diffs in items:
                if diffs:
                    failed = True
                    if args.verbose:
                        print(f"  {fmt} {kind} {name}: " + "; ".join(diffs))
        print(f"{fmt:<12} {cells[0]:>12} {cells[1]:>12}")
    if not args.keep:
        shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
