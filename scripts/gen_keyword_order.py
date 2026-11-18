"""Generate crates/terrio-crs/tests/data/keyword_order.json: WKT 2 CRSs with two nodes where
PROJ 9.9.0 reads one, which it takes in the CRS's own order whatever their keywords
(opengeos/terrio#1058 step 1b): two datum nodes (`TRF`, `GEODETICDATUM` and `DATUM`), two
ellipsoid nodes (`SPHEROID` and `ELLIPSOID`), and two base CRSs (`BASEGEODCRS` and
`BASEGEOGCRS`), one on WGS 84 and one on OSGB36's Airy 1830.

Each case is British National Grid (PROJ's WKT 2 of EPSG:27700 without its `ID`) with its base
CRS changed, the ellipsoid PROJ reads, and a point from WGS 84 by
`Transformer.from_crs("EPSG:4326", wkt, always_xy=True)`.

Usage: PROJ_NETWORK=OFF python scripts/gen_keyword_order.py (needs pyproj 3.8.0)
"""

import json
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/keyword_order.json"
)
POINT = [-1.0, 53.0]
AIRY = 'ELLIPSOID["Airy 1830",6377563.396,299.3249646,LENGTHUNIT["metre",1]]'
WGS84 = 'ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]'
OSGB = f'DATUM["Ordnance Survey of Great Britain 1936",{AIRY}]'
WGS = f'DATUM["World Geodetic System 1984",{WGS84}]'
PM = 'PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]'


def bng(base):
    """British National Grid on the base CRS given, without its `ID`."""
    wkt = CRS.from_epsg(27700).to_wkt("WKT2_2019")
    start = wkt.index("BASEGEOGCRS")
    end = wkt.index(",CONVERSION")
    out = wkt[:start] + base + wkt[end:]
    i = out.rindex(',ID["EPSG",27700]')
    return out[:i] + out[i + len(',ID["EPSG",27700]') :]


def keyword(node, key):
    """The node with its keyword replaced."""
    return key + node[node.index("[") :]


def main():
    cases = [
        (
            "TRF (WGS 84) before DATUM (OSGB36)",
            f'BASEGEOGCRS["x",{keyword(WGS, "TRF")},{OSGB},{PM}]',
        ),
        (
            "DATUM (WGS 84) before TRF (OSGB36)",
            f'BASEGEOGCRS["x",{WGS},{keyword(OSGB, "TRF")},{PM}]',
        ),
        (
            "GEODETICDATUM (WGS 84) before DATUM (OSGB36)",
            f'BASEGEOGCRS["x",{keyword(WGS, "GEODETICDATUM")},{OSGB},{PM}]',
        ),
        (
            "SPHEROID (Airy) before ELLIPSOID (WGS 84)",
            f'BASEGEOGCRS["x",DATUM["d",{keyword(AIRY, "SPHEROID")},{WGS84}],{PM}]',
        ),
        (
            "ELLIPSOID (WGS 84) before SPHEROID (Airy)",
            f'BASEGEOGCRS["x",DATUM["d",{WGS84},{keyword(AIRY, "SPHEROID")}],{PM}]',
        ),
        (
            "BASEGEODCRS (WGS 84) before BASEGEOGCRS (OSGB36)",
            f'BASEGEODCRS["w",{WGS},{PM}],BASEGEOGCRS["o",{OSGB},{PM}]',
        ),
        (
            "BASEGEOGCRS (OSGB36) before BASEGEODCRS (WGS 84)",
            f'BASEGEOGCRS["o",{OSGB},{PM}],BASEGEODCRS["w",{WGS},{PM}]',
        ),
    ]
    out = []
    for name, base in cases:
        wkt = bng(base)
        crs = CRS.from_wkt(wkt)
        t = Transformer.from_crs("EPSG:4326", wkt, always_xy=True)
        out.append(
            {
                "name": name,
                "wkt": wkt,
                "a": crs.ellipsoid.semi_major_metre,
                "datum": crs.datum.name,
                "proj": list(t.transform(*POINT)),
            }
        )
    data = {"proj": pyproj.proj_version_str, "point": POINT, "cases": out}
    OUT.write_text(json.dumps(data, indent=1) + "\n")
    print(f"{len(out)} cases to {OUT}")


if __name__ == "__main__":
    main()
