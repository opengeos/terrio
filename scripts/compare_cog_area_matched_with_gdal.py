#!/usr/bin/env python3
"""Area-matched COG overviews written by `terrio convert`, read and checked with GDAL.

GDAL has no area-matched method, so GDAL is the reader and the structural reference. For each
real raster with nodata, `terrio convert -f COG --co OVERVIEW_RESAMPLING=AREA_MATCHED` (or
`AREA_MATCHED_MODE` for a class raster) is compared with GDAL's COG driver writing the same raster
with `average` (`mode`) and `nearest`, `OVERVIEWS=IGNORE_EXISTING`. A case agrees when, as GDAL
reads the files:

- terrio's overviews have the sizes of GDAL's;
- the full-resolution band, and its nodata, are GDAL's;
- every level keeps `round(valid * w * h / (cols * rows))` valid cells (half to even);
- the band items `OVERVIEW_AREA_RATIOS` and `OVERVIEW_BASE_VALID_PIXELS` equal the ratios and the
  count recomputed from the levels (ratios to 1e-12), and `IMAGE_STRUCTURE` records the method;
- GDAL's COG validator reports no error, and GDAL's COG driver keeps the band items when it
  converts terrio's file again.

The overview pixels differ from those of every GDAL method by design, and the ratios are compared
to 1e-12 (they are formatted from integer counts in a different order). The detail of each case
gives the valid area each level keeps for terrio, `average` and `nearest`.

    python scripts/compare_cog_area_matched_with_gdal.py DATA --terrio target/release/terrio [--json]
"""

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal

gdal.UseExceptions()

# (file, method, GDAL's counterpart): Float32 with NaN and with -inf nodata, Float64 with NaN, and
# a paletted Byte class raster (nodata 255).
CASES = [
    ("ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif", "AREA_MATCHED", "AVERAGE"),
    ("las_vegas_dem.tif", "AREA_MATCHED", "AVERAGE"),
    ("dem.tif", "AREA_MATCHED", "AVERAGE"),
    ("OPERA_L3_DSWx_WTR.tif", "AREA_MATCHED_MODE", "MODE"),
]
OPTIONS = ["COMPRESS=DEFLATE", "BLOCKSIZE=256"]


def invalid(values, nodata):
    """The pixels that are not valid: NaN, or the nodata value."""
    out = np.isnan(values) if values.dtype.kind == "f" else np.zeros(values.shape, bool)
    if nodata is not None and not np.isnan(nodata):
        out |= values == np.asarray(nodata).astype(values.dtype)
    return out


def read(path):
    """The full-resolution band, its nodata, the overviews and the band's default-domain items."""
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1)
    levels = [band.GetOverview(i).ReadAsArray() for i in range(band.GetOverviewCount())]
    out = (band.ReadAsArray(), band.GetNoDataValue(), levels, band.GetMetadata() or {}, ds.GetMetadata("IMAGE_STRUCTURE") or {})
    ds = None
    return out


def ratios(base, nodata, levels):
    """The valid area each level keeps over the full-resolution valid area."""
    valid = int((~invalid(base, nodata)).sum())
    return valid, [int((~invalid(lv, nodata)).sum()) * base.size / lv.size / valid for lv in levels]


def round_half_even(num, den):
    q, r = divmod(num, den)
    return q + 1 if 2 * r > den or (2 * r == den and q % 2) else q


def validate_errors(path):
    """The errors of GDAL's COG validator (osgeo_utils), if it is installed."""
    try:
        from osgeo_utils.samples.validate_cloud_optimized_geotiff import validate
    except ImportError:
        return None
    _warnings, errors, _details = validate(str(path), full_check=True)
    return errors


def case(data, terrio, root, name, method, reference):
    src = data / name
    out = root / f"{src.stem}_terrio.tif"
    cmd = [terrio, "convert", str(src), str(out), "-f", "COG", "--compress", "DEFLATE",
           "--co", "BLOCKSIZE=256", "--co", f"OVERVIEW_RESAMPLING={method}"]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode:
        return {"name": f"{method.lower()} {name}", "status": "differs", "detail": proc.stderr.strip()[-400:]}
    refs = {}
    for m in (reference, "NEAREST"):
        path = root / f"{src.stem}_gdal_{m.lower()}.tif"
        gdal.Translate(str(path), str(src), format="COG",
                       creationOptions=[*OPTIONS, f"OVERVIEW_RESAMPLING={m}", "OVERVIEWS=IGNORE_EXISTING"])
        refs[m] = read(path)
    base, nodata, levels, items, structure = read(out)
    g_base, g_nodata, g_levels, _, _ = refs[reference]
    problems = []
    sizes = [lv.shape for lv in levels]
    if sizes != [lv.shape for lv in g_levels]:
        problems.append(f"overview sizes {sizes} vs GDAL's {[lv.shape for lv in g_levels]}")
    if not (np.array_equal(invalid(base, nodata), invalid(g_base, g_nodata))
            and np.array_equal(base[~invalid(base, nodata)], g_base[~invalid(g_base, g_nodata)])):
        problems.append("the full-resolution band differs from GDAL's")
    valid, kept = ratios(base, nodata, levels)
    for lv, r in zip(levels, kept):
        want = round_half_even(valid * lv.size, base.size)
        got = int((~invalid(lv, nodata)).sum())
        if got != want:
            problems.append(f"{lv.shape[1]}x{lv.shape[0]}: {got} valid cells, not {want}")
    stored = [float(v) for v in items.get("OVERVIEW_AREA_RATIOS", "").split(",") if v]
    if len(stored) != len(kept) or any(abs(a - b) > 1e-12 for a, b in zip(stored, kept)):
        problems.append(f"OVERVIEW_AREA_RATIOS {stored} vs recomputed {kept}")
    if items.get("OVERVIEW_BASE_VALID_PIXELS") != str(valid):
        problems.append(f"OVERVIEW_BASE_VALID_PIXELS {items.get('OVERVIEW_BASE_VALID_PIXELS')} vs {valid}")
    if structure.get("OVERVIEW_RESAMPLING") != method or structure.get("LAYOUT") != "COG":
        problems.append(f"IMAGE_STRUCTURE {structure}")
    errors = validate_errors(out)
    if errors:
        problems.append(f"COG validator: {errors}")
    copy = root / f"{src.stem}_gdal_copy.tif"
    gdal.Translate(str(copy), str(out), format="COG", creationOptions=OPTIONS)
    copied = read(copy)[3]
    if any(copied.get(k) != items.get(k) for k in ("OVERVIEW_AREA_RATIOS", "OVERVIEW_BASE_VALID_PIXELS")):
        problems.append("GDAL's COG driver dropped the band items")
    fmt = lambda rs: ", ".join(f"{r:.4f}" for r in rs)  # noqa: E731
    detail = (f"{base.shape[1]}x{base.shape[0]}, {valid} valid px; area kept per level: terrio {fmt(kept)}; "
              f"GDAL {reference.lower()} {fmt(ratios(g_base, g_nodata, g_levels)[1])}; "
              f"GDAL nearest {fmt(ratios(*refs['NEAREST'][:3])[1])}")
    if errors is None:
        detail += "; COG validator not installed"
    return {"name": f"{method.lower()} {name}", "status": "differs" if problems else "agrees",
            "detail": "; ".join(problems) if problems else detail}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-area-matched-") as directory:
        for name, method, reference in CASES:
            if not (args.data / name).exists():
                results.append({"name": f"{method.lower()} {name}", "status": "differs", "detail": "missing test file"})
                continue
            results.append(case(args.data, args.terrio, Path(directory), name, method, reference))
    if args.json:
        print(json.dumps(results))
    else:
        for r in results:
            print(f"{r['status']:10} {r['name']}: {r['detail']}")
    return 1 if any(r["status"] != "agrees" for r in results) else 0


if __name__ == "__main__":
    sys.exit(main())
