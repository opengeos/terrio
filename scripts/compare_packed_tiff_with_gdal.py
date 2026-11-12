# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Packed unsigned GTiff/COG writes and copies, independently decoded by GDAL 3.13.3.

Synthetic rows exercise byte padding and tile edges; an available WorldCover
crop checks real classifications, nodata and a reduced palette. No compressed-byte
equality or file-size ratio is claimed. Outputs live in a temporary directory.
"""

import argparse
import json
from pathlib import Path
import subprocess
import struct
import tempfile
import warnings

import numpy as np
from osgeo import gdal, osr
import terrio
import terrio.rio as rio
from terrio.rio.shutil import copy
from terrio.rio.transform import Affine, from_origin


def verify(path, values, bits, transform, crs, nodata=None, palette=None, require_overviews=False):
    """Compare stored TIFF samples and metadata against GDAL references."""
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
        if len(palette) < 1 << bits:
            raise ValueError("source palette has fewer than 2**NBITS entries")
        actual = band.GetRasterColorTable()
        if actual is None or actual.GetCount() != 1 << bits:
            raise ValueError("palette does not have 2**NBITS entries")
        if any(actual.GetColorEntry(i) != color for i, color in enumerate(palette[:1 << bits])):
            raise ValueError("palette entries changed")
    with rio.open(path) as native:
        np.testing.assert_array_equal(native.read(1), values)
        np.testing.assert_array_equal(native.read_masks(1), expected_mask)
    if require_overviews and band.GetOverviewCount() == 0:
        raise ValueError("COG has no overviews")
    for i in range(band.GetOverviewCount()):
        if band.GetOverview(i).GetMetadataItem("NBITS", "IMAGE_STRUCTURE") != str(bits):
            raise ValueError(f"overview {i}: stored width is not {bits}")
        with rio.open(path, overview_level=i) as native:
            np.testing.assert_array_equal(native.read(1), band.GetOverview(i).ReadAsArray())
    return f"NBITS={bits}; exact samples, masks, container type, nodata, affine and CRS; {band.GetOverviewCount()} packed overviews decoded by both readers"



def source_preservation_cases(directory):
    """Precision and source validity controls, including GDAL's overview-only-mask loss."""
    rows = []
    for driver in ("GTiff", "COG"):
        name = f"JP2 15-bit AUTO to {driver}"
        if gdal.GetDriverByName("JP2OpenJPEG") is None:
            rows.append(dict(name=name, status="not compared", detail="GDAL JP2OpenJPEG is unavailable"))
            continue
        try:
            values = (np.arange(256 * 256, dtype="uint32").reshape(256, 256) % 32768).astype("uint16")
            transform, crs = from_origin(500000, 5800000, 10, 10), "EPSG:32633"
            memory = gdal.GetDriverByName("MEM").Create("", 256, 256, 1, gdal.GDT_UInt16)
            memory.WriteArray(values)
            memory.SetGeoTransform(transform.to_gdal())
            srs = osr.SpatialReference()
            srs.SetFromUserInput(crs)
            memory.SetProjection(srs.ExportToWkt())
            source = directory / f"precision-{driver}.jp2"
            encoded = gdal.Translate(str(source), memory, format="JP2OpenJPEG",
                                     creationOptions=["NBITS=15", "REVERSIBLE=YES", "QUALITY=100"])
            encoded.FlushCache()
            encoded = None
            reference = gdal.Translate("", str(source), format="MEM", width=128, height=128)
            target = directory / f"precision-{driver}.tif"
            terrio.translate(str(source), str(target), format=driver, outsize=(128, 128))
            detail = verify(target, reference.ReadAsArray(), 15, Affine.from_gdal(*reference.GetGeoTransform()), crs)
            rows.append(dict(name=name, status="identical", detail=detail + "; independent GDAL AUTO-resampled JP2 values"))
        except Exception as error:
            rows.append(dict(name=name, status="differs", detail=str(error)))
    name = "COG overview-only source masks"
    try:
        source = directory / "overview-only.tif"
        transform, crs = from_origin(500000, 5800000, 10, 10), "EPSG:32633"
        previous = gdal.GetConfigOption("GDAL_TIFF_INTERNAL_MASK")
        gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "YES")
        try:
            ds = gdal.GetDriverByName("GTiff").Create(str(source), 64, 64, 1, gdal.GDT_UInt16,
                                                     options=["NBITS=9", "ENDIANNESS=LITTLE"])
            ds.SetGeoTransform(transform.to_gdal())
            srs = osr.SpatialReference()
            srs.SetFromUserInput(crs)
            ds.SetProjection(srs.ExportToWkt())
            ds.GetRasterBand(1).Fill(7)
            ds.CreateMaskBand(gdal.GMF_PER_DATASET)
            ds.GetRasterBand(1).GetMaskBand().Fill(255)
            ds.BuildOverviews("NEAREST", [2, 4])
            ds.GetRasterBand(1).GetOverview(0).GetMaskBand().Fill(0)
            ds = None
        finally:
            gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", previous)
        # Unlink only the base mask IFD from GDAL's classic little-endian TIFF.
        raw = bytearray(source.read_bytes())
        assert raw[:4] == b"II*\0"
        offset, previous_slot = struct.unpack_from("<I", raw, 4)[0], 4
        while offset:
            count = struct.unpack_from("<H", raw, offset)[0]
            tags = {}
            for index in range(count):
                entry = offset + 2 + 12 * index
                tag, kind, length = struct.unpack_from("<HHI", raw, entry)
                if length == 1:
                    tags[tag] = struct.unpack_from("<H" if kind == 3 else "<I", raw, entry + 8)[0]
            next_slot = offset + 2 + 12 * count
            following = struct.unpack_from("<I", raw, next_slot)[0]
            if tags.get(254, 0) & 4 and tags.get(256) == 64:
                struct.pack_into("<I", raw, previous_slot, following)
            else:
                previous_slot = next_slot
            offset = following
        source.write_bytes(raw)
        original = gdal.Open(str(source))
        assert original.GetRasterBand(1).GetMaskFlags() == gdal.GMF_ALL_VALID
        target = directory / "overview-only-copy.tif"
        copy(source, target, driver="COG")
        detail = verify(target, original.ReadAsArray(), 9, transform, crs, require_overviews=True)
        output = gdal.Open(str(target))
        assert output.GetRasterBand(1).GetMaskFlags() == original.GetRasterBand(1).GetMaskFlags()
        assert output.GetRasterBand(1).GetOverviewCount() == original.GetRasterBand(1).GetOverviewCount()
        for i in range(original.GetRasterBand(1).GetOverviewCount()):
            np.testing.assert_array_equal(output.GetRasterBand(1).GetOverview(i).GetMaskBand().ReadAsArray(),
                                          original.GetRasterBand(1).GetOverview(i).GetMaskBand().ReadAsArray())
        rows.append(dict(name=name, status="identical", detail=detail + "; exact SOURCE overview validity; "
                         "GDAL 3.13.3 COG copying drops these masks; Terrio retains the absent base mask"))
    except Exception as error:
        rows.append(dict(name=name, status="differs", detail=str(error)))
    return rows


def main():
    """Run packed TIFF conformance cases and report comparison results."""
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
            nodata = 0 if bits == 4 else None
            for driver in ["GTiff", "COG"]:
                name = f"{driver} {bits}-bit"
                try:
                    path = directory / f"{driver}-{bits}.tif"
                    with rio.open(path, "w", driver=driver, width=131, height=97, count=1, dtype=dtype,
                                  crs=crs, transform=transform, nodata=nodata, compress="DEFLATE", nbits=bits,
                                  tiled=True, blockxsize=32, blockysize=32, blocksize=32) as dst:
                        dst.write(values, 1)
                    detail = verify(path, values, bits, transform, crs, nodata, require_overviews=driver == "COG")
                    copied = directory / f"copy-{driver}-{bits}.tif"
                    copy(path, copied, driver=driver, compress="LZW")
                    verify(copied, values, bits, transform, crs, nodata)
                    cli = directory / f"cli-{driver}-{bits}.tif"
                    result = subprocess.run([args.terrio, "translate", str(path), str(cli), "-f", driver, "--compress", "DEFLATE"], capture_output=True, text=True)
                    if result.returncode:
                        raise RuntimeError(f"terrio translate failed: {result.stderr.strip()}")
                    verify(cli, values, bits, transform, crs, nodata)
                    rows.append(dict(name=name, status="identical", detail=detail + "; Python and CLI copies retain the bit width"))
                except Exception as error:
                    rows.append(dict(name=name, status="differs", detail=str(error)))
        # An even grid gives the same COG and GTiff overview geometry. The odd-position
        # bright edge makes independent unpacked GDAL CUBIC exceed the four-bit maximum.
        try:
            values = np.tile(np.where(np.arange(256) >= 127, 15, 0).astype("uint8"), (128, 1))
            transform, crs = from_origin(500000, 5800000, 10, 10), "EPSG:32633"
            references = []
            for bits in (4, 8):
                reference = gdal.GetDriverByName("GTiff").Create(
                    str(directory / f"cubic-reference-{bits}.tif"), 256, 128, 1,
                    gdal.GDT_Byte, options=[f"NBITS={bits}"],
                )
                reference.WriteArray(values)
                reference.BuildOverviews("CUBIC", [2])
                references.append(reference.GetRasterBand(1).GetOverview(0).ReadAsArray())
                reference = None
            if references[1].max() <= 15:
                raise RuntimeError("bright control must produce cubic overshoot")
            path = directory / "cubic-bright.tif"
            with warnings.catch_warnings(record=True) as captured:
                warnings.simplefilter("always")
                with rio.open(path, "w", driver="COG", width=256, height=128,
                              count=1, dtype="uint8", crs=crs, transform=transform,
                              compress="DEFLATE", nbits=4, blocksize=128) as dst:
                    dst.write(values, 1)
            detail = verify(path, values, 4, transform, crs, require_overviews=True)
            actual = gdal.Open(str(path))
            np.testing.assert_array_equal(actual.GetRasterBand(1).GetOverview(0).ReadAsArray(), references[0])
            actual = None
            messages = [str(item.message) for item in captured]
            rows.append(dict(name="COG 4-bit CUBIC bright edge", status="identical",
                             detail=detail + "; generated overview equals independent GDAL CUBIC; "
                             + "warnings=" + repr(messages)))
        except Exception as error:
            rows.append(dict(name="COG 4-bit CUBIC bright edge", status="differs", detail=str(error)))
        rows.extend(source_preservation_cases(directory))
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
        else:
            rows.append(dict(name="WorldCover palette", status="not compared", detail="esa_worldcover_colormap.tif is unavailable"))
    print(json.dumps(rows, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']}: {r['detail']}" for r in rows))
    return int(any(row["status"] == "differs" for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
