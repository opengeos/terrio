"""Compare terrio's reading of GDAL's NBITS GeoTIFFs with GDAL's own, pixel for pixel.

GDAL writes UInt16 and UInt32 bands with ``NBITS`` (9 to 31 bits, as for Sentinel-2's 15-bit
JPEG 2000 bands) packed: the samples are stored MSB first, each row starting on a byte. This
writes every bit depth tested, in every lossless compression, tiled and striped, with one and
three bands, has terrio convert each file to a plain GeoTIFF and compares the pixels with GDAL's
reading of the original.

Usage: python scripts/compare_tiff_nbits_with_gdal.py TERRIO_BIN SCRATCH_DIR [MORE_FILES...]
"""

import itertools
import os
import subprocess
import sys

import numpy as np
from osgeo import gdal

gdal.UseExceptions()


def main():
    """Write NBITS variants with GDAL, convert each with terrio, and compare the pixels."""
    terrio, out, real = sys.argv[1], sys.argv[2], sys.argv[3:]
    rng = np.random.default_rng(0)
    ok = bad = 0
    files = list(real)
    for nbits, comp, tiled, bands in itertools.product((9, 12, 15, 17, 24, 31), ("NONE", "LZW", "DEFLATE", "ZSTD", "PACKBITS", "LZMA"), (True, False), (1, 3)):
        dtype = gdal.GDT_UInt16 if nbits <= 16 else gdal.GDT_UInt32
        w, h = 517, 301
        a = rng.integers(0, 2**nbits, size=(bands, h, w), dtype=np.uint32)
        p = os.path.join(out, f"n{nbits}_{comp}_{'t' if tiled else 's'}_{bands}.tif")
        opts = [f"NBITS={nbits}", f"COMPRESS={comp}"] + (["TILED=YES", "BLOCKXSIZE=128", "BLOCKYSIZE=64"] if tiled else ["BLOCKYSIZE=7"])
        ds = gdal.GetDriverByName("GTiff").Create(p, w, h, bands, dtype, opts)
        ds.SetGeoTransform([500000, 30, 0, 4500000, 0, -30])
        for b in range(bands):
            ds.GetRasterBand(b + 1).WriteArray(a[b])
        ds = None
        files.append(p)
    for p in files:
        q = p + ".terrio.tif"
        r = subprocess.run([terrio, "convert", p, q], capture_output=True, text=True)
        ref = gdal.Open(p).ReadAsArray()
        if r.returncode:
            bad += 1
            print(f"FAIL {os.path.basename(p)}: {r.stderr.strip()[-200:]}")
            continue
        got = gdal.Open(q).ReadAsArray()
        same = got.shape == ref.shape and got.dtype == ref.dtype and np.array_equal(got, ref)
        ok += same
        bad += not same
        if not same:
            print(f"DIFF {os.path.basename(p)}: {ref.dtype}{ref.shape} vs {got.dtype}{got.shape}")
    print(f"{ok} identical, {bad} not")


if __name__ == "__main__":
    main()
