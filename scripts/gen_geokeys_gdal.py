"""Freeze the GeoKeys GDAL writes for CRSs, and the CRS GDAL reads from them, for
``SpatialRef::from_geokeys`` and ``SpatialRef::to_geokeys`` (``tests/geokeys_gdal.rs``).

GDAL 3.12.1 (rasterio 1.5.0) writes a one-pixel GeoTIFF of each CRS (``GTIFSetFromOGISDefnEx``)
and reads it back (``GTIFGetOGISDefnAsOSR``). The rows give the GeoKeys it wrote (tifffile reads
the GeoKeyDirectoryTag, GeoDoubleParamsTag and GeoAsciiParamsTag) and its WKT 1 of the CRS it
read. The CRSs are every 20th EPSG projected and geographic 2D code, the codes behind each rule,
PROJ strings of every projection method GeoTIFF names (ProjCoordTransGeoKey 1, 3, 7-28 and
9815) in other units and on other datums, ESRI codes (written as ESRI PE citations) and compound
CRSs.

Usage: python scripts/gen_geokeys_gdal.py (needs rasterio 1.5.0, with GDAL 3.12.1, pyproj 3.8.0,
numpy and tifffile)
"""

import importlib.util
import json
import os
import tempfile
import warnings
from pathlib import Path

# rasterio's GDAL reads the PROJ database of its own PROJ, which another PROJ_DATA (pyproj's)
# hides; pyproj keeps its own.
os.environ["PROJ_DATA"] = str(
    Path(importlib.util.find_spec("rasterio").origin).parent / "proj_data"
)
os.environ["PROJ_NETWORK"] = "OFF"

import numpy
import pyproj
import rasterio
import tifffile
from pyproj.database import query_crs_info
from pyproj.enums import PJType
from rasterio.crs import CRS
from rasterio.transform import from_origin

pyproj.datadir.set_data_dir(
    str(Path(pyproj.__file__).parent / "proj_dir" / "share" / "proj")
)

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/geokeys_gdal.tsv"
)

# Projected CRSs of each method, geographic CRSs on other datums, ellipsoids and meridians, units,
# compound and vertical CRSs.
RULES = [
    "EPSG:4326",
    "EPSG:4269",
    "EPSG:4267",
    "EPSG:4258",
    "EPSG:4152",
    "EPSG:4807",
    "EPSG:32633",
    "EPSG:32733",
    "EPSG:26915",
    "EPSG:27700",
    "EPSG:2056",
    "EPSG:3857",
    "EPSG:3031",
    "EPSG:3413",
    "EPSG:5041",
    "EPSG:3035",
    "EPSG:2154",
    "EPSG:3078",
    "EPSG:2193",
    "EPSG:2246",
    "EPSG:6350",
    "EPSG:6931",
    "EPSG:4326+5773",
    "EPSG:32633+5773",
    "EPSG:2056+5728",
    "ESRI:102003",
    "ESRI:54009",
    "ESRI:54030",
    "ESRI:53035",
]

GRS80 = "+ellps=GRS80 +towgs84=0,0,0"
PROJ_STRINGS = [
    # Geographic CRSs: named datums, other ellipsoids, a prime meridian, a shift.
    "+proj=longlat +datum=WGS84",
    "+proj=longlat +datum=NAD83",
    "+proj=longlat +ellps=intl",
    "+proj=longlat +ellps=bessel +pm=paris",
    "+proj=longlat +a=6378000 +rf=300",
    "+proj=longlat +R=6371000",
    "+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7",
    # The methods GeoTIFF names, in metres on WGS 84.
    "+proj=tmerc +lat_0=10 +lon_0=-3 +k=0.9996 +x_0=500000 +y_0=100 +datum=WGS84",
    "+proj=omerc +no_uoff +lat_0=4 +lonc=102.25 +alpha=323.0257905 +gamma=323.1301023611 +k=0.99984 +x_0=804671 +y_0=0 +datum=WGS84",
    "+proj=omerc +lat_0=4 +lonc=115 +alpha=53.31582047222 +gamma=53.13010236111 +k=0.99984 +x_0=590476.87 +y_0=442857.65 +datum=WGS84",
    "+proj=merc +lon_0=10 +k=0.997 +x_0=3900000 +y_0=900000 +datum=WGS84",
    "+proj=merc +lon_0=10 +lat_ts=20 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=lcc +lat_1=46.8 +lat_0=46.8 +lon_0=2.3 +k_0=0.99987742 +x_0=600000 +y_0=2200000 +datum=WGS84",
    "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +datum=WGS84",
    "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=37.5 +lon_0=-96 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=aeqd +lat_0=47 +lon_0=8 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=eqdc +lat_1=20 +lat_2=60 +lat_0=40 +lon_0=-96 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=stere +lat_0=40 +lon_0=-100 +k=0.9999 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=stere +lat_0=-90 +lon_0=0 +k=0.994 +x_0=2000000 +y_0=2000000 +datum=WGS84",
    "+proj=sterea +lat_0=52.15616055555555 +lon_0=5.38763888888889 +k=0.9999079 +x_0=155000 +y_0=463000 +datum=WGS84",
    "+proj=eqc +lat_ts=30 +lat_0=0 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=cass +lat_0=10.44166666666667 +lon_0=-61.33333333333334 +x_0=86501.46392052 +y_0=65379.0134283 +datum=WGS84",
    "+proj=gnom +lat_0=30 +lon_0=10 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=mill +lat_0=0 +lon_0=0 +x_0=0 +y_0=0 +R_A +datum=WGS84",
    "+proj=ortho +lat_0=40 +lon_0=-100 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=poly +lat_0=0 +lon_0=-54 +x_0=5000000 +y_0=10000000 +datum=WGS84",
    "+proj=robin +lon_0=10 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=sinu +lon_0=10 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=vandg +lon_0=10 +x_0=0 +y_0=0 +R=6371000",
    "+proj=nzmg +lat_0=-41 +lon_0=173 +x_0=2510000 +y_0=6023150 +ellps=intl +towgs84=59.47,-5.04,187.44,0.47,-0.1,1.024,-4.5993",
    "+proj=tmerc +axis=wsu +lat_0=-22 +lon_0=19 +k=1 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=cea +lat_ts=30 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=somerc +lat_0=46.9524055555556 +lon_0=7.43958333333333 +k_0=1 +x_0=2600000 +y_0=1200000 +datum=WGS84",
    # Units and datums.
    "+proj=utm +zone=17 +datum=NAD83 +units=us-ft",
    "+proj=utm +zone=33 +datum=WGS84 +units=ft",
    "+proj=utm +zone=33 +datum=WGS84 +units=km",
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=500000 +y_0=0 +datum=WGS84 +to_meter=0.3047972654",
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=500000 +y_0=0 +datum=WGS84 +to_meter=1.5",
    "+proj=utm +zone=33 +datum=NAD27",
    f"+proj=utm +zone=33 {GRS80}",
    "+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 +ellps=airy",
    "+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=0 +y_0=0 +ellps=bessel +pm=paris",
    "+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +x_0=0 +y_0=0 +a=6378000 +rf=300",
    "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +R=6371000",
]


def codes():
    """Every 20th EPSG projected and geographic 2D code, not deprecated."""
    out = []
    for kind in (PJType.PROJECTED_CRS, PJType.GEOGRAPHIC_2D_CRS):
        infos = query_crs_info(
            auth_name="EPSG", pj_types=[kind], allow_deprecated=False
        )
        out += [f"EPSG:{c}" for c in sorted(int(i.code) for i in infos)[::20]]
    return out


def gdal_keys(definition, directory):
    """GDAL's GeoKeys of a CRS and its WKT 1 of the CRS it reads from them; None when GDAL
    cannot write the CRS in GeoKeys (it writes it in the `.aux.xml` sidecar)."""
    path = Path(directory) / "crs.tif"
    try:
        crs = CRS.from_user_input(definition)
    except rasterio.errors.CRSError:
        return None
    profile = {
        "driver": "GTiff",
        "width": 1,
        "height": 1,
        "count": 1,
        "dtype": "uint8",
        "crs": crs,
        "transform": from_origin(0, 1, 1, 1),
    }
    with rasterio.open(path, "w", **profile) as dst:
        dst.write(numpy.zeros((1, 1, 1), dtype="uint8"))
    with tifffile.TiffFile(path) as tif:
        tags = tif.pages[0].tags
        keys = list(tags[34735].value) if 34735 in tags else []
        doubles = list(tags[34736].value) if 34736 in tags else []
        ascii = tags[34737].value if 34737 in tags else ""
    sidecar = Path(f"{path}.aux.xml")
    # A CRS GeoKeys cannot hold goes to the PAM sidecar: no GeoKeys to compare.
    in_sidecar = sidecar.exists()
    with rasterio.open(path) as src:
        read = src.crs.to_wkt() if src.crs else ""
    path.unlink()
    sidecar.unlink(missing_ok=True)
    return None if in_sidecar else (keys, doubles, ascii, read)


def main():
    assert rasterio.__gdal_version__ == "3.12.1", (
        f"GDAL {rasterio.__gdal_version__}, not 3.12.1"
    )
    warnings.simplefilter("ignore")
    rows = []
    with tempfile.TemporaryDirectory() as tmp:
        for kind, definitions in (("code", RULES + codes()), ("proj", PROJ_STRINGS)):
            for definition in dict.fromkeys(definitions):
                got = gdal_keys(definition, tmp)
                if got is None:
                    print(f"left out: {definition} (GDAL writes no GeoKeys of it)")
                    continue
                keys, doubles, ascii, read = got
                row = [
                    kind,
                    definition,
                    ",".join(map(str, keys)),
                    ",".join(repr(d) for d in doubles),
                    json.dumps(ascii),
                    read,
                ]
                assert not any("\t" in f or "\n" in f for f in row), definition
                rows.append("\t".join(row))
    header = (
        "# GeoKeys GDAL 3.12.1 (rasterio 1.5.0) writes for a CRS, and its WKT 1 of the CRS it reads from them.\n"
        "# Generated by scripts/gen_geokeys_gdal.py.\n"
        "# kind\tdefinition\tGeoKeyDirectoryTag\tGeoDoubleParamsTag\tGeoAsciiParamsTag (JSON)\tGDAL's WKT 1\n"
    )
    OUT.write_text(header + "\n".join(rows) + "\n")
    print(f"{len(rows)} rows -> {OUT}")


if __name__ == "__main__":
    main()
