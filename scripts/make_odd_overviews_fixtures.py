"""Writes `crates/terrio/tests/data/odd_overviews/expected.json`: digests of the average overviews
GDAL computes of a raster whose overview ratios are not integers.

Usage:
    python scripts/make_odd_overviews_fixtures.py

The raster is 8191 x 511 pixels of two pixel-interleaved Float32 bands, tiled (256) and DEFLATE
compressed, with nodata -9999. `GDALRegenerateOverviewsEx` computes overviews band by band from
full-width chunks of 256 rows at this size, and clamps each overview row's source window to its
chunk: overview row 128 is computed from source rows 256 and 257 only, not from row 255 too. GDAL
computes the external overviews (`gdaladdo -ro -r average --config COMPRESS_OVERVIEW DEFLATE`) that
way, as it writes their `.ovr` band-separate, and the internal ones of the pixel-interleaved image
(`gdaladdo -r average`) block by block from whole windows (`GDALRegenerateOverviewsMultiBand`).

The values are integers below 2^11 and the ratios (8191 / 4096, 511 / 256, then 2) have at most 12
fractional bits, so every product and sum of GDAL's weighted average is exact and only its final
division rounds: the results do not depend on whether the compiler fuses the multiply-adds (arm64
builds of GDAL do, x86-64 builds and terrio do not).

`expected.json` holds, for the external and the internal overviews, each level's size and, for
each band, the FNV-1a 64-bit hash of its values (little-endian Float32, row by row), their sum
over the valid pixels and the count of nodata pixels; for the external ones, the `.ovr`'s
interleaving. Needs GDAL's Python bindings (3.13).
"""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal

gdal.UseExceptions()

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "odd_overviews"
WIDTH, HEIGHT, BANDS, NODATA = 8191, 511, 2, -9999.0
LEVELS = [2, 4, 8, 16]


def values(band: int) -> np.ndarray:
    """Band `band` (from 1), as `odd_size_average_overviews_are_gdal_overviews` writes it."""
    y, x = np.mgrid[:HEIGHT, :WIDTH].astype(np.int64)
    v = ((x * 7 + y * 13 * band) % 1000 + y).astype(np.float32)
    v[((x // 37 + y // 23) % 11 == 0) | ((x * band + y) % 101 == 0)] = NODATA
    return v


def fnv1a64(data: bytes) -> str:
    h = 0xCBF29CE484222325
    for byte in data:
        h = ((h ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"


def digest(a: np.ndarray) -> dict:
    valid = a != NODATA
    return {
        "fnv1a64": fnv1a64(a.astype("<f4").tobytes()),
        "sum": float(a[valid].astype(np.float64).sum()),
        "nodata": int((~valid).sum()),
    }


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    cases = []
    with tempfile.TemporaryDirectory() as tmp:
        source = Path(tmp) / "source.tif"
        ds = gdal.GetDriverByName("GTiff").Create(
            str(source), WIDTH, HEIGHT, BANDS, gdal.GDT_Float32,
            ["TILED=YES", "BLOCKXSIZE=256", "BLOCKYSIZE=256", "COMPRESS=DEFLATE"],
        )
        ds.SetGeoTransform([0, 1, 0, HEIGHT, 0, -1])
        for b in range(1, BANDS + 1):
            band = ds.GetRasterBand(b)
            band.SetNoDataValue(NODATA)
            band.WriteArray(values(b))
        ds = None
        assert gdal.Open(str(source)).GetMetadataItem("INTERLEAVE", "IMAGE_STRUCTURE") == "PIXEL"
        for external in [True, False]:
            path = Path(tmp) / f"{'external' if external else 'internal'}.tif"
            shutil.copyfile(source, path)
            subprocess.run(
                ["gdaladdo", "-q", *(["-ro", "--config", "COMPRESS_OVERVIEW", "DEFLATE"] if external else []),
                 "-r", "average", str(path), *map(str, LEVELS)],
                check=True,
            )
            ds = gdal.Open(str(path))
            levels = []
            for i in range(len(LEVELS)):
                ovr = [ds.GetRasterBand(b).GetOverview(i) for b in range(1, BANDS + 1)]
                levels.append({
                    "size": [ovr[0].XSize, ovr[0].YSize],
                    "bands": [digest(o.ReadAsArray()) for o in ovr],
                })
            case = {"external": external, "levels": levels}
            if external:
                ovr_ds = gdal.Open(str(path) + ".ovr")
                case["ovr_interleave"] = ovr_ds.GetMetadataItem("INTERLEAVE", "IMAGE_STRUCTURE")
                case["ovr_compression"] = ovr_ds.GetMetadataItem("COMPRESSION", "IMAGE_STRUCTURE")
            cases.append(case)
    expected = {
        "gdal_version": gdal.__version__,
        "width": WIDTH,
        "height": HEIGHT,
        "bands": BANDS,
        "nodata": NODATA,
        "factors": LEVELS,
        "cases": cases,
    }
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()
