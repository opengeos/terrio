"""Writes `crates/terrio/tests/data/ovr_rebuilds`: the external average overviews GDAL builds of
masked rasters in several compressed levels, and of requested levels an `.ovr` already has (issue
#1260), with what GDAL reads of them.

Usage:
    python scripts/make_ovr_rebuild_fixtures.py

The sources are the issue's 67 x 45 Byte image: `plain.tif` without a mask, `masked.tif` with an
internal per-dataset mask and `sidecar.tif` with the same mask in a `.msk` sidecar
(`GDAL_TIFF_INTERNAL_MASK=NO`), which leaves out 7 x 7 blocks and scattered pixels. Each case
copies a source and runs `gdaladdo -ro --config COMPRESS_OVERVIEW <compression> -r <method>` once
per step (average unless a step names another method). Compressed, GDAL computes a single band's
new levels block by block, each from the previous one, and weighs the previous level's pixels by
its mask band, all valid in an `.ovr`, because the source has a mask. When a step requests levels
the `.ovr` already has, GDAL builds the new ones first (uncompressed, band by band, it recomputes
every level of the `.ovr` with them), then recomputes the requested existing ones band by band:
from full resolution, or each from the previous one for averages of a raster without a mask.

`wide` is a 10300 x 520 Float32 raster in DEFLATE tiles of 256, with an internal mask, which
`wide_masked_ovr_rebuilds_take_the_sidecar_order` writes the same way: so wide that band by band
GDAL computes its levels block by block (`GDALRegenerateOverviewsMultiBand`), in the order the
`.ovr` lists them, each from the previous one when that one is wider. It builds `2 8`, then
`2 4 8 16` uncompressed: the sidecar's `2 8`, then the new `4 16`, then `2 8` again.

`expected.json` holds, for each case, its source, its steps and, for each level in the order GDAL
lists them, its size, the mask GDAL reads of it (`GetMaskBand`, 1 valid and 0 not) and its data,
row by row; for `wide`, each level's size and the FNV-1a 64-bit hash of its values (little-endian
Float32, row by row). Its weighted averages are not all exact: generate it with an x86-64 build of
GDAL, which does not fuse their multiply-adds (arm64 builds do, 1 ulp apart), as terrio does not.
Needs GDAL's Python bindings (3.13).
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

OUT = Path(__file__).resolve().parent.parent / "crates/terrio/tests/data/ovr_rebuilds"
WIDTH, HEIGHT = 67, 45

CASES = [
    ("masked", [("DEFLATE", [2, 4, 8])]),
    ("sidecar", [("DEFLATE", [2, 4, 8])]),
    ("plain", [("DEFLATE", [3, 5]), ("DEFLATE", [2, 3])]),
    ("plain", [("NONE", [3, 5]), ("NONE", [2, 3])]),
    ("masked", [("DEFLATE", [3, 5]), ("DEFLATE", [2, 3])]),
    ("sidecar", [("NONE", [3, 5]), ("NONE", [2, 3])]),
    # Band by band, the new level's pass recomputes the nearest level as an average.
    ("masked", [("NONE", [2], "nearest"), ("NONE", [4])]),
]


def sources() -> None:
    data = (np.arange(WIDTH * HEIGHT).reshape(HEIGHT, WIDTH) * 3 % 251).astype(np.uint8)
    rng = np.random.default_rng(0)
    small = rng.random((HEIGHT // 7 + 2, WIDTH // 7 + 2)) > 0.45
    blocks = np.kron(small, np.ones((7, 7), bool))[:HEIGHT, :WIDTH]
    mask = np.where(blocks ^ (rng.random((HEIGHT, WIDTH)) > 0.97), 255, 0).astype(np.uint8)
    for name, internal in [("plain", None), ("masked", "YES"), ("sidecar", "NO")]:
        tif = OUT / f"{name}.tif"
        for suffix in ("", ".msk"):
            Path(f"{tif}{suffix}").unlink(missing_ok=True)
        gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", internal)
        try:
            ds = gdal.GetDriverByName("GTiff").Create(str(tif), WIDTH, HEIGHT, 1, gdal.GDT_Byte)
            ds.GetRasterBand(1).WriteArray(data)
            if internal:
                ds.CreateMaskBand(gdal.GMF_PER_DATASET)
                ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
            ds = None
        finally:
            gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", None)


WIDE = (10300, 520)
WIDE_STEPS = [[2, 8], [2, 4, 8, 16]]


def wide_values() -> tuple[np.ndarray, np.ndarray]:
    """The values and mask (255 valid) of `wide`, as the Rust test computes them."""
    y, x = np.mgrid[:WIDE[1], :WIDE[0]].astype(np.int64)
    values = ((x * 7 + y * 13) % 1000 + y).astype(np.float32)
    invalid = ((x // 37 + y // 23) % 11 == 0) | ((x + y) % 101 == 0)
    return values, np.where(invalid, 0, 255).astype(np.uint8)


def fnv1a64(data: bytes) -> str:
    h = 0xCBF29CE484222325
    for byte in data:
        h = ((h ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"


def wide(tmp: Path) -> list:
    values, mask = wide_values()
    tif = tmp / "wide.tif"
    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "YES")
    try:
        ds = gdal.GetDriverByName("GTiff").Create(
            str(tif), WIDE[0], WIDE[1], 1, gdal.GDT_Float32,
            ["TILED=YES", "BLOCKXSIZE=256", "BLOCKYSIZE=256", "COMPRESS=DEFLATE"],
        )
        ds.GetRasterBand(1).WriteArray(values)
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        ds = None
    finally:
        gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", None)
    for levels in WIDE_STEPS:
        subprocess.run(["gdaladdo", "-q", "-ro", "-r", "average", str(tif), *map(str, levels)], check=True)
    ds = gdal.Open(str(tif))
    band = ds.GetRasterBand(1)
    return [
        {"size": [o.XSize, o.YSize], "fnv1a64": fnv1a64(o.ReadAsArray().astype("<f4").tobytes())}
        for o in map(band.GetOverview, range(band.GetOverviewCount()))
    ]


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    sources()
    cases = []
    with tempfile.TemporaryDirectory() as tmp:
        for i, (source, steps) in enumerate(CASES):
            tif = Path(tmp) / f"case{i}.tif"
            shutil.copyfile(OUT / f"{source}.tif", tif)
            if source == "sidecar":
                shutil.copyfile(OUT / "sidecar.tif.msk", f"{tif}.msk")
            for compression, levels, *method in steps:
                subprocess.run(
                    ["gdaladdo", "-q", "-ro", "--config", "COMPRESS_OVERVIEW", compression,
                     "-r", (method or ["average"])[0], str(tif), *map(str, levels)],
                    check=True,
                )
            ds = gdal.Open(str(tif))
            band = ds.GetRasterBand(1)
            levels = []
            for overview in map(band.GetOverview, range(band.GetOverviewCount())):
                levels.append({
                    "size": [overview.XSize, overview.YSize],
                    "mask": ["".join("1" if v else "0" for v in row) for row in overview.GetMaskBand().ReadAsArray()],
                    "data": [" ".join(map(str, row)) for row in overview.ReadAsArray().tolist()],
                })
            ds = None
            cases.append({
                "source": source,
                "steps": [{"compression": c, "levels": lv, "method": (m or ["average"])[0]} for c, lv, *m in steps],
                "levels": levels,
            })
        wide_levels = wide(Path(tmp))
    expected = {
        "gdal": gdal.__version__,
        "cases": cases,
        "wide": {"size": list(WIDE), "steps": WIDE_STEPS, "levels": wide_levels},
    }
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()
