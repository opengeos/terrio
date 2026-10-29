"""Writes the GRIB2 files of `crates/terrio/tests/data/grib` and what GDAL reads of them.

Usage:
    python scripts/make_grib_fixtures.py

The files are written by GDAL's GRIB driver (through rasterio), one per data encoding it writes
(simple packing, complex packing with spatial differencing and missing values, IEEE floating
point, PNG, JPEG 2000), on a global 0-360 grid, a Lambert conformal, a polar stereographic and a
Mercator grid, a temperature (which GDAL reads in Celsius) and a probability of temperature (which it
does not convert), a message of two fields with a bitmap, the second reusing the first's (GDAL's
simple packing rewritten: GDAL never writes bitmaps), and a simple packing whose values single
and double precision unpack differently (rewritten too).
`expected.json` holds, for each file, what GDAL reads: size, geotransform, CRS (WKT and EPSG
code), each band's nodata, description, GRIB metadata and pixels. Needs rasterio.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import numpy as np
import rasterio
from rasterio.transform import from_origin

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "terrio" / "tests" / "data" / "grib"

# The Python test's helpers: synthetic fields, GDAL's writing, bitmap rewriting.
_spec = importlib.util.spec_from_file_location("test_grib", ROOT / "python" / "tests" / "test_grib.py")
t = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(t)

LATLON = dict(crs="EPSG:4326", transform=from_origin(-20.0, 60.0, 0.5, 0.5))
LCC = "+proj=lcc +lat_1=25 +lat_2=25 +lat_0=25 +lon_0=-95 +x_0=0 +y_0=0 +a=6371229 +b=6371229 +units=m +no_defs"
POLAR = "+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +x_0=0 +y_0=0 +ellps=WGS84 +units=m +no_defs"
MERCATOR = "+proj=merc +lat_ts=20 +lon_0=0 +x_0=0 +y_0=0 +a=6371229 +b=6371229 +units=m +no_defs"
PROJECTED = from_origin(-1_500_000.0, 1_200_000.0, 12_000.0, 12_000.0)


def bitmap_message(path: Path) -> None:
    src = path.with_suffix(".src.grib2")
    t.write_grib(src, t.field(9, 11, bands=2, seed=13), DATA_ENCODING="SIMPLE_PACKING", **LATLON)
    data = src.read_bytes()
    src.unlink()
    keep = ~t.file_order_mask(t.masked(9, 11, 1))
    msgs = t.messages(data)
    first, second = (dict(t.sections(m)) for m in msgs)
    a5, a7 = t.repack(first[5], first[7], keep)
    b5, b7 = t.repack(second[5], second[7], keep)
    path.write_bytes(
        t.build(msgs[0][:16], [first[1], first[3], first[4], a5, t.bitmap_section(keep), a7, second[4], b5, t.section(6, bytes([254])), b7])
    )


def files() -> dict[str, Path]:
    OUT.mkdir(parents=True, exist_ok=True)
    out = {}

    def w(name, data, **kw):
        out[name] = Path(t.write_grib(OUT / name, data, **kw))

    w("simple.grib2", t.field(7, 9, bands=2, seed=1), DATA_ENCODING="SIMPLE_PACKING", **LATLON)
    w("complex_sd2_nodata.grib2", t.field(7, 9, bands=2, seed=2, nodata=True), nodata=t.NODATA,
      DATA_ENCODING="COMPLEX_PACKING", SPATIAL_DIFFERENCING_ORDER="2", **LATLON)
    w("ieee.grib2", t.field(5, 6, seed=3), DATA_ENCODING="IEEE_FLOATING_POINT", **LATLON)
    w("png.grib2", t.field(7, 9, seed=4), DATA_ENCODING="PNG", **LATLON)
    w("jpeg2000.grib2", t.field(7, 9, seed=5), DATA_ENCODING="JPEG2000", **LATLON)
    w("global_0_360.grib2", t.field(7, 12, seed=6), DATA_ENCODING="COMPLEX_PACKING", SPATIAL_DIFFERENCING_ORDER="1",
      crs="EPSG:4326", transform=from_origin(0.0, 90.0, 30.0, 30.0))
    w("lcc.grib2", t.field(6, 8, seed=7), DATA_ENCODING="SIMPLE_PACKING", crs=LCC, transform=PROJECTED)
    w("polar.grib2", t.field(6, 8, seed=8), DATA_ENCODING="SIMPLE_PACKING", crs=POLAR, transform=PROJECTED)
    w("mercator.grib2", t.field(6, 8, seed=11), DATA_ENCODING="SIMPLE_PACKING", crs=MERCATOR, transform=PROJECTED)
    w("temperature.grib2", t.field(5, 6, seed=9) - 273.0, DATA_ENCODING="SIMPLE_PACKING", PDS_PDTN="0",
      PDS_TEMPLATE_NUMBERS=t.TEMPERATURE, **LATLON)
    pdtn, numbers = t.PRODUCTS["4.5-probability"]
    w("probability.grib2", t.field(5, 6, seed=10), DATA_ENCODING="SIMPLE_PACKING", PDS_PDTN=pdtn,
      PDS_TEMPLATE_NUMBERS=numbers, **LATLON)
    bitmap_message(OUT / "bitmap254.grib2")
    out["bitmap254.grib2"] = OUT / "bitmap254.grib2"
    src = Path(t.write_grib(OUT / "arithmetic.src.grib2", t.field(9, 11, seed=21), DATA_ENCODING="SIMPLE_PACKING", **LATLON))
    out["arithmetic.grib2"] = Path(t.packing_arithmetic(src, OUT / "arithmetic.grib2"))
    src.unlink()
    for p in OUT.glob("*.aux.xml"):
        p.unlink()
    return out


def main() -> None:
    expected = {}
    for name, path in files().items():
        with rasterio.open(path) as ds:
            expected[name] = {
                "width": ds.width,
                "height": ds.height,
                "geo_transform": list(ds.transform.to_gdal()),
                "crs_wkt": ds.crs.to_wkt() if ds.crs else None,
                "epsg": ds.crs.to_epsg() if ds.crs else None,
                "bands": [
                    {
                        "nodata": ds.nodatavals[b - 1],
                        "description": ds.descriptions[b - 1],
                        "tags": {k: v for k, v in ds.tags(b).items() if k in t.TAGS},
                        "pixels": [float(v) for v in ds.read(b).reshape(-1)],
                    }
                    for b in ds.indexes
                ],
            }
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")
    print(f"{len(expected)} files -> {OUT}")


if __name__ == "__main__":
    main()
