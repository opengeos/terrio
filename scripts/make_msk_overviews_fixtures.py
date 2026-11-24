"""Writes `crates/terrio/tests/data/msk_overviews`: a GeoTIFF with a per-dataset `.msk` sidecar,
and the external overviews GDAL builds for it read-only, with what GDAL reads of them.

Usage:
    python scripts/make_msk_overviews_fixtures.py

`masked.tif` is a 67 x 45 Byte image whose `.msk` (`GDAL_TIFF_INTERNAL_MASK=NO`) leaves out
blocks and scattered pixels, so that overview windows straddle its edges. `gdaladdo -ro -r average
masked.tif 2 3` (uncompressed) writes `masked.tif.ovr` and `masked.tif.msk.ovr`: GDAL computes
the mask's levels band by band, each from the previous one. `expected.json` holds, for each level
(its size, largest first), the mask GDAL reads of it (`GetMaskBand`, 1 valid and 0 not, row by
row), its mask flags, and the data GDAL reads; and the files GDAL lists for the dataset.

Generated with GDAL 3.13.2.
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np
from osgeo import gdal

gdal.UseExceptions()

OUT = Path(__file__).resolve().parent.parent / "crates/terrio/tests/data/msk_overviews"
WIDTH, HEIGHT = 67, 45


def mask() -> np.ndarray:
    rows, cols = np.indices((HEIGHT, WIDTH))
    valid = ((cols // 7 + rows // 5) % 3 != 0) & ((5 * cols + 3 * rows) % 17 != 0)
    return valid.astype(np.uint8) * 255


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    tif = OUT / "masked.tif"
    for suffix in ("", ".msk", ".ovr", ".msk.ovr"):
        Path(f"{tif}{suffix}").unlink(missing_ok=True)
    gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "NO")
    try:
        ds = gdal.GetDriverByName("GTiff").Create(
            str(tif), WIDTH, HEIGHT, 1, gdal.GDT_Byte
        )
        ds.GetRasterBand(1).WriteArray(
            (np.arange(WIDTH * HEIGHT).reshape(HEIGHT, WIDTH) * 7 % 251).astype(
                np.uint8
            )
        )
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask())
        ds = None
        ds = gdal.Open(str(tif))
        ds.BuildOverviews("AVERAGE", [2, 3])
        ds = None
    finally:
        gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", None)
    ds = gdal.Open(str(tif))
    band = ds.GetRasterBand(1)
    levels = []
    for overview in sorted(
        map(band.GetOverview, range(band.GetOverviewCount())), key=lambda o: -o.XSize
    ):
        levels.append(
            {
                "size": [overview.XSize, overview.YSize],
                "mask_flags": overview.GetMaskFlags(),
                "mask": [
                    "".join("1" if v else "0" for v in row)
                    for row in overview.GetMaskBand().ReadAsArray()
                ],
                "data": overview.ReadAsArray().ravel().tolist(),
            }
        )
    files = [Path(f).name for f in ds.GetFileList()]
    expected = {"gdal": gdal.__version__, "levels": levels, "files": files}
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()
