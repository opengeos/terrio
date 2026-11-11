"""Packed unsigned GTiff/COG writes and copies, independently decoded by GDAL 3.13.3.

Synthetic rows exercise byte padding and tile edges; an available WorldCover
crop checks real classifications, nodata and a reduced palette. No compressed-byte
equality or file-size ratio is claimed. Outputs live in a temporary directory.
"""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile

import numpy as np
from osgeo import gdal, osr
import terrio.rio as rio
from terrio.rio.shutil import copy
from terrio.rio.transform import Affine, from_origin


def verify(path, values, bits, transform, crs, nodata=None, palette=None):
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1)
    if band.GetMetadataItem("NBITS", "IMAGE_STRUCTURE") != str(bits):
        raise ValueError(f"{path.name}: stored width is not {bits}")
    if band.DataType != gdal.GetDataTypeByName({"uint8": "Byte", "uint16": "UInt16", "uint32": "UInt32"}[str(values.dtype)]):
        raise ValueError("sample container type changed")
    np.testing.assert_array_equal(band.ReadAsArray(), values)
    if band.GetNoDataValue() != nodata or ds.GetGeoTransform() != transform.to_gdal():
        raise ValueError("nodata or affine changed")
    expected_crs = osr.SpatialReference()
    expected_crs.SetFromUserInput(crs)
    if not ds.GetSpatialRef().IsSame(expected_crs):
        raise ValueError("CRS changed")
    expected_mask = np.full(values.shape, 255, dtype="uint8") if nodata is None else np.where(values == nodata, 0, 255).astype("uint8")
    np.testing.assert_array_equal(band.GetMaskBand().ReadAsArray(), expected_mask)
    if palette is not None:
        actual = band.GetRasterColorTable()
        if actual is None or actual.GetCount() != 1 << bits:
            raise ValueError("palette does not have 2**NBITS entries")
        if any(actual.GetColorEntry(i) != color for i, color in enumerate(palette[:1 << bits])):
            raise ValueError("palette entries changed")
    with rio.open(path) as native:
        np.testing.assert_array_equal(native.read(1), values)
        np.testing.assert_array_equal(native.read_masks(1), expected_mask)
    for i in range(band.GetOverviewCount()):
        with rio.open(path, overview_level=i) as native:
            np.testing.assert_array_equal(native.read(1), band.GetOverview(i).ReadAsArray())
    return f"NBITS={bits}; exact samples, masks, container type, nodata, affine and CRS; {band.GetOverviewCount()} packed overviews decoded by both readers"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    rows = []
    with tempfile.TemporaryDirectory(prefix="terrio-packed-") as directory:
        directory = Path(directory)
        for bits, dtype in [(1, "uint8"), (4, "uint8"), (7, "uint8"), (12, "uint16"), (24, "uint32"), (31, "uint32")]:
            y, x = np.mgrid[:97, :131]
            values = ((x * 104729 + y * 15485863) % (1 << bits)).astype(dtype)
            transform, crs = from_origin(500000, 5800000, 10, 10), "EPSG:32633"
            for driver in ["GTiff", "COG"]:
                name = f"{driver} {bits}-bit"
                try:
                    path = directory / f"{driver}-{bits}.tif"
                    with rio.open(path, "w", driver=driver, width=131, height=97, count=1, dtype=dtype,
                                  crs=crs, transform=transform, compress="DEFLATE", nbits=bits,
                                  tiled=True, blockxsize=32, blockysize=32, blocksize=32) as dst:
                        dst.write(values, 1)
                    detail = verify(path, values, bits, transform, crs)
                    copied = directory / f"copy-{driver}-{bits}.tif"
                    copy(path, copied, driver=driver, compress="LZW")
                    verify(copied, values, bits, transform, crs)
                    cli = directory / f"cli-{driver}-{bits}.tif"
                    subprocess.run([args.terrio, "translate", str(path), str(cli), "-f", driver, "--compress", "DEFLATE"], check=True, capture_output=True)
                    verify(cli, values, bits, transform, crs)
                    rows.append(dict(name=name, status="identical", detail=detail + "; Python and CLI copies retain the bit width"))
                except Exception as error:
                    rows.append(dict(name=name, status="differs", detail=str(error)))
        source = args.data / "esa_worldcover_colormap.tif"
        if source.exists():
            try:
                ds = gdal.Open(str(source))
                size = min(1024, ds.RasterXSize, ds.RasterYSize)
                xoff, yoff = (ds.RasterXSize - size) // 2, (ds.RasterYSize - size) // 2
                values = ds.ReadAsArray(xoff, yoff, size, size)
                transform = Affine.from_gdal(*ds.GetGeoTransform()) * Affine.translation(xoff, yoff)
                crs, nodata = ds.GetProjection(), ds.GetRasterBand(1).GetNoDataValue()
                table = ds.GetRasterBand(1).GetRasterColorTable()
                palette = [table.GetColorEntry(i) for i in range(table.GetCount())]
                path = directory / "WorldCover.tif"
                with rio.open(path, "w", driver="COG", width=size, height=size, count=1, dtype="uint8",
                              crs=crs, transform=transform, nodata=nodata, nbits=7, compress="DEFLATE", blocksize=128) as dst:
                    dst.write(values, 1)
                    dst.write_colormap(1, dict(enumerate(palette)))
                detail = verify(path, values, 7, transform, crs, nodata, palette)
                rows.append(dict(name="WorldCover palette", status="identical", detail=detail + "; exact first 128 palette entries"))
            except Exception as error:
                rows.append(dict(name="WorldCover palette", status="differs", detail=str(error)))
    print(json.dumps(rows, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']}: {r['detail']}" for r in rows))
    return int(any(row["status"] == "differs" for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
