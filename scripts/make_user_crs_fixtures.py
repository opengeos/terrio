"""Writes the GeoTIFFs of `crates/terrio/tests/data/user_crs` with GDAL (through rasterio).

Usage:
    python scripts/make_user_crs_fixtures.py

Each file is a 2x2 GeoTIFF whose CRS has no EPSG code, written as GDAL writes it: GeoKeys for
every projection method GDAL maps to one (`ProjCoordTransGeoKey` 1, 3, 7-28 and 9815), an ESRI PE
string, and `.aux.xml` sidecars for a CRS GeoKeys cannot hold and for one that overrides the
GeoKeys. `expected.json` holds, for each file, a point and its coordinates in the CRS GDAL reads
back, transformed with pyproj (PROJ). Needs rasterio and pyproj.
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np
import pyproj
import rasterio
from rasterio.transform import from_origin

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "user_crs"
W = "+datum=WGS84 +units=m +no_defs"
# name: (CRS, a point (lon, lat) in its domain)
CASES = {
    "tmerc": (f"+proj=tmerc +lat_0=10 +lon_0=-103 +k=0.9996 +x_0=500000 +y_0=100000 {W}", (-101, 14)),
    "omerc_a": (f"+proj=omerc +no_uoff +lat_0=4 +lonc=115 +alpha=53.31582047 +gamma=53.13010236 +k=0.99984 +x_0=590476.87 +y_0=442857.65 {W}", (116, 5)),
    "omerc_b": (f"+proj=omerc +lat_0=4 +lonc=115 +alpha=53.31582047 +gamma=53.13010236 +k=0.99984 +x_0=590476.87 +y_0=442857.65 {W}", (116, 5)),
    "merc": (f"+proj=merc +lon_0=10 +k=0.997 {W}", (14, 50)),
    "lcc_2sp": (f"+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 {W}", (-90, 41)),
    "lcc_1sp": (f"+proj=lcc +lat_1=40 +lat_0=40 +lon_0=-96 +k_0=0.9998 {W}", (-90, 41)),
    "laea": (f"+proj=laea +lat_0=60 +lon_0=-40 {W}", (-30, 62)),
    "aea": (f"+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 {W}", (-90, 41)),
    "aeqd": (f"+proj=aeqd +lat_0=40 +lon_0=-100 {W}", (-95, 44)),
    "eqdc": (f"+proj=eqdc +lat_0=40 +lon_0=-96 +lat_1=33 +lat_2=45 {W}", (-90, 41)),
    "stere": (f"+proj=stere +lat_0=45 +lon_0=10 +k=0.9999 {W}", (12, 47)),
    "stere_polar": (f"+proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 {W}", (5, 75)),
    "sterea": ("+proj=sterea +lat_0=52.15616055555555 +lon_0=5.38763888888889 +k=0.9999079 +x_0=155000 +y_0=463000 +ellps=bessel +units=m +no_defs", (6, 52.5)),
    "eqc": (f"+proj=eqc +lat_ts=30 +lon_0=10 {W}", (14, 50)),
    "cass": (f"+proj=cass +lat_0=10.44166666666667 +lon_0=-61.33333333333334 +x_0=86501.46392052001 +y_0=65379.0134283 {W}", (-61, 10.6)),
    "gnom": (f"+proj=gnom +lat_0=40 +lon_0=-100 {W}", (-95, 44)),
    "mill": (f"+proj=mill +lon_0=10 {W}", (14, 50)),
    "ortho": (f"+proj=ortho +lat_0=40 +lon_0=-100 {W}", (-95, 44)),
    "poly": (f"+proj=poly +lat_0=0 +lon_0=-54 +x_0=5000000 +y_0=10000000 {W}", (-50, -10)),
    "robin": (f"+proj=robin +lon_0=10 {W}", (14, 50)),
    "sinu": (f"+proj=sinu +lon_0=10 {W}", (14, 50)),
    "vandg": (f"+proj=vandg +lon_0=10 {W}", (14, 50)),
    "nzmg": ("+proj=nzmg +lat_0=-41 +lon_0=173 +x_0=2510000 +y_0=6023150 +ellps=intl +units=m +no_defs", (174, -40)),
    "tmerc_south": (f"+proj=tmerc +axis=wsu +lat_0=-22 +lon_0=17 +k=1 {W}", (18, -23)),
    "cea": (f"+proj=cea +lat_ts=30 +lon_0=10 {W}", (14, 50)),
    # GeoKeys cannot hold it: GDAL writes it to the .aux.xml sidecar.
    "eqearth_sidecar": (f"+proj=eqearth +lon_0=10 {W}", (14, 50)),
    # GeoKeys and an ESRI PE string with no root AUTHORITY.
    "geos_esri_pe": ("+proj=geos +h=35786023 +lon_0=-75 +sweep=x +ellps=GRS80 +units=m +no_defs", (-60, 20)),
}


def write(path: Path, crs: str | None) -> None:
    """Writes a 2x2 GeoTIFF.

    Args:
        path: The file.
        crs: Its CRS, or None.
    """
    with rasterio.open(path, "w", driver="GTiff", width=2, height=2, count=1, dtype="uint8", crs=crs,
                       transform=from_origin(0, 0, 1000, 1000)) as d:
        d.write(np.zeros((1, 2, 2), "uint8"))


def sidecar(path: Path, crs: str) -> None:
    """Writes a `.aux.xml` sidecar holding a CRS, as GDAL does (WKT2).

    Args:
        path: The GeoTIFF.
        crs: The CRS.
    """
    wkt = pyproj.CRS(crs).to_wkt().replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")
    Path(f"{path}.aux.xml").write_text(f'<PAMDataset>\n  <SRS dataAxisToSRSAxisMapping="1,2">{wkt}</SRS>\n</PAMDataset>\n')


def main() -> None:
    """Writes the files and `expected.json`."""
    OUT.mkdir(parents=True, exist_ok=True)
    for f in OUT.glob("*.tif*"):
        f.unlink()
    expected = {}
    for name, (crs, point) in CASES.items():
        write(OUT / f"{name}.tif", crs)
    # A sidecar overriding the GeoKeys (GDAL reads the sidecar's), and one for a file without CRS.
    write(OUT / "sidecar_override.tif", "EPSG:32613")
    sidecar(OUT / "sidecar_override.tif", "EPSG:32614")
    CASES["sidecar_override"] = ("EPSG:32614", (-99, 40))
    write(OUT / "sidecar_only.tif", None)
    sidecar(OUT / "sidecar_only.tif", "EPSG:32613")
    CASES["sidecar_only"] = ("EPSG:32613", (-105, 40))
    for name, (_, (lon, lat)) in CASES.items():
        with rasterio.open(OUT / f"{name}.tif") as s:
            wkt = s.crs.to_wkt()
        x, y = pyproj.Transformer.from_crs("EPSG:4326", wkt, always_xy=True).transform(lon, lat)
        expected[name] = {"gdal": pyproj.CRS(wkt).to_proj4(), "lon": lon, "lat": lat, "x": x, "y": y}
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()
