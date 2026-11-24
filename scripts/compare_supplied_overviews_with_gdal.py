"""Supplied COG overviews against GDAL's COG driver with OVERVIEWS=FORCE_USE_EXISTING (reference
GDAL 3.13.3).

Each case is a GeoTIFF whose overview bands GDAL creates (`BuildOverviews("NONE", ...)`, at
gdaladdo's ceil(n / factor) sizes) and this script then writes by hand, values no resampler
produces. GDAL converts it to a COG with OVERVIEWS=FORCE_USE_EXISTING; terrio writes the
full-resolution array with the same levels supplied as arrays (`Raster.write(overviews=[...])`),
with no intermediate file. Every level is compared: sizes and samples exactly for lossless
compression; for LERC with MAX_Z_ERROR, each terrio level (and GDAL's) against the supplied level
within the bound plus one Float32 spacing of each value, the rounding of the decoded sample (the
two LERC encoders' compressed bytes and decoded values are not expected to be equal).
Nodata and dataset metadata items are compared too. No data is downloaded or modified.
"""

import argparse
import json
from pathlib import Path
import tempfile

import numpy as np
from osgeo import gdal

import terrio

CASES = {
    # name: (width, height, bands, GDAL type, nodata, factors, creation options)
    "Float32 nodata DEFLATE, ceil sizes": (2001, 1501, 1, gdal.GDT_Float32, -9999.0, [2, 4, 8], {"COMPRESS": "DEFLATE"}),
    "Float32 LERC_DEFLATE MAX_Z_ERROR=0.01": (
        2001, 1501, 1, gdal.GDT_Float32, -9999.0, [2, 4], {"COMPRESS": "LERC_DEFLATE", "MAX_Z_ERROR": "0.01"},
    ),
    "UInt16 3 bands ZSTD": (1300, 1100, 3, gdal.GDT_UInt16, None, [2, 4], {"COMPRESS": "ZSTD", "PREDICTOR": "2"}),
    "Byte LZW, factor 3": (1025, 1023, 1, gdal.GDT_Byte, 0, [3, 9], {"COMPRESS": "LZW"}),
}


def hand_level(shape, level, dtype, nodata, seed):
    """Values marking the level, with nodata holes, that no resampling of another level gives."""
    rng = np.random.default_rng(seed)
    bands, rows, cols = shape
    y, x = np.mgrid[0:rows, 0:cols]
    planes = []
    for b in range(bands):
        v = (x * (3 + b) + y * 7 + level * 101) % 200 + rng.random((rows, cols)) * 0.5
        planes.append(v)
    out = np.stack(planes)
    if np.dtype(dtype).kind != "f":
        out = np.round(out)
    out = out.astype(dtype)
    if nodata is not None:
        out[:, (x + y * 3) % 17 == 0] = nodata
    return out


def compare(name, case, directory):
    width, height, bands, gdt, nodata, factors, co = case
    dtype = gdal.GetDataTypeName(gdt).lower()
    dtype = {"byte": "uint8"}.get(dtype, dtype)
    src_path = directory / f"{name}.src.tif".replace(" ", "_").replace(",", "")
    ds = gdal.GetDriverByName("GTiff").Create(str(src_path), width, height, bands, gdt, ["TILED=YES", "COMPRESS=DEFLATE"])
    ds.SetGeoTransform((400000, 10, 0, 4500000, 0, -10))
    ds.SetProjection("EPSG:32615")
    base = hand_level((bands, height, width), 0, dtype, nodata, 1)
    for b in range(bands):
        if nodata is not None:
            ds.GetRasterBand(b + 1).SetNoDataValue(nodata)
        ds.GetRasterBand(b + 1).WriteArray(base[b])
    ds.BuildOverviews("NONE", factors)
    levels = []
    for i in range(ds.GetRasterBand(1).GetOverviewCount()):
        ovr = ds.GetRasterBand(1).GetOverview(i)
        level = hand_level((bands, ovr.YSize, ovr.XSize), i + 1, dtype, nodata, i + 2)
        for b in range(bands):
            ds.GetRasterBand(b + 1).GetOverview(i).WriteArray(level[b])
        levels.append(level)
    ds.SetMetadataItem("PROVENANCE", "hand-written overviews")
    ds = None

    gdal_path = directory / "gdal.tif"
    gdal.Translate(str(gdal_path), str(src_path), format="COG",
                   creationOptions=[f"{k}={v}" for k, v in co.items()] + ["OVERVIEWS=FORCE_USE_EXISTING"])
    ours_path = directory / "terrio.tif"
    transform = [400000, 10, 0, 4500000, 0, -10]
    raster = terrio.Raster.from_array(base, transform, "EPSG:32615", nodata)
    raster.write(str(ours_path), driver="COG", compress=co["COMPRESS"],
                 options={k: v for k, v in co.items() if k != "COMPRESS"},
                 overviews=levels, metadata={"PROVENANCE": "hand-written overviews"})

    g, t = gdal.Open(str(gdal_path)), gdal.Open(str(ours_path))
    tolerance = float(co.get("MAX_Z_ERROR", 0))
    worst = worst_gdal = 0.0
    sizes = []
    # Every band has exactly the supplied levels: an extra one would go unexamined below.
    for b in range(bands):
        counts = (t.GetRasterBand(b + 1).GetOverviewCount(), g.GetRasterBand(b + 1).GetOverviewCount())
        if counts != (len(levels), len(levels)):
            raise ValueError(f"band {b + 1}: terrio {counts[0]} overviews, GDAL {counts[1]}, {len(levels)} supplied")
    for i in range(len(levels) + 1):
        gb = [g.GetRasterBand(b + 1) if i == 0 else g.GetRasterBand(b + 1).GetOverview(i - 1) for b in range(bands)]
        tb = [t.GetRasterBand(b + 1) if i == 0 else t.GetRasterBand(b + 1).GetOverview(i - 1) for b in range(bands)]
        if [(b.XSize, b.YSize) for b in tb] != [(b.XSize, b.YSize) for b in gb]:
            raise ValueError(f"level {i}: terrio {tb[0].XSize} x {tb[0].YSize}, GDAL {gb[0].XSize} x {gb[0].YSize}")
        sizes.append(f"{tb[0].XSize}x{tb[0].YSize}")
        expected = base if i == 0 else levels[i - 1]
        for b in range(bands):
            ours, theirs = tb[b].ReadAsArray(), gb[b].ReadAsArray()
            if tolerance == 0:
                if not np.array_equal(ours, theirs) or not np.array_equal(ours, expected[b]):
                    raise ValueError(f"level {i} band {b + 1}: samples differ")
            else:
                valid = expected[b] != nodata
                if not np.array_equal(ours == nodata, ~valid):
                    raise ValueError(f"level {i} band {b + 1}: nodata pixels differ")
                e = expected[b][valid].astype("float64")
                # LERC quantizes within MAX_Z_ERROR; the Float32 sample it decodes to rounds once
                # more, by up to one Float32 spacing of the value. GDAL's output is held to it too.
                bound = tolerance + np.spacing(np.abs(expected[b][valid]).astype("float32")).astype("float64")
                for who, values in (("terrio", ours), ("GDAL", theirs)):
                    d = np.abs(values[valid].astype("float64") - e)
                    # A NaN difference compares false with the bound below; the supplied valid
                    # samples are finite, so a non-finite difference is a wrong decoded sample.
                    if not np.isfinite(d).all():
                        raise ValueError(f"level {i} band {b + 1}: {who} decodes a non-finite valid sample")
                    if who == "terrio":
                        worst = max(worst, float(d.max()))
                    else:
                        worst_gdal = max(worst_gdal, float(d.max()))
                    if (d > bound).any():
                        raise ValueError(f"level {i} band {b + 1}: {who} {d.max()} from the source, over MAX_Z_ERROR={tolerance} + one Float32 spacing")
    if t.GetRasterBand(1).GetNoDataValue() != g.GetRasterBand(1).GetNoDataValue():
        raise ValueError("nodata differs")
    if t.GetMetadataItem("PROVENANCE") != g.GetMetadataItem("PROVENANCE"):
        raise ValueError("metadata item differs")
    if t.GetMetadataItem("LAYOUT", "IMAGE_STRUCTURE") != "COG":
        raise ValueError("not a COG layout")
    g = t = None
    status = "identical" if tolerance == 0 else "within tolerance"
    detail = f"levels {', '.join(sizes)}; "
    detail += "samples exact against GDAL and the supplied levels" if tolerance == 0 else (
        f"every level within MAX_Z_ERROR={tolerance} plus one Float32 spacing of the supplied levels "
        f"(max {worst:.7g}; GDAL's {worst_gdal:.7g}), nodata exact")
    detail += f"; nodata and metadata as GDAL's; GDAL {gdal.VersionInfo('RELEASE_NAME')}"
    return dict(name=name, status=status, detail=detail)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path, nargs="?")
    parser.add_argument("--terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    rows = []
    for name, case in CASES.items():
        with tempfile.TemporaryDirectory(prefix="terrio-supplied-") as directory:
            try:
                rows.append(compare(name, case, Path(directory)))
            except Exception as error:
                rows.append(dict(name=name, status="differs", detail=str(error)))
    print(json.dumps(rows, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']}: {r['detail']}" for r in rows))
    return int(any(r["status"] == "differs" for r in rows))


if __name__ == "__main__":
    raise SystemExit(main())
