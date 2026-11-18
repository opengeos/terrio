"""Generate crates/terrio-crs/tests/data/wkt_root_code.json: where PROJ 9.9.0 places points of
WKTs whose root `AUTHORITY` or `ID` is not the CRS their nodes describe (opengeos/terrio#1012,
item 16), or whose name is that of an Esri CRS on another datum (item 14), and the code PROJ
identifies (`to_epsg()`, and an ESRI one by `to_authority()`). Controls: WKTs of the right code,
an Esri `.prj` identified by its name and datum, and PROJ's WKT 2 of a method terrio identifies by
its code (Belgian Lambert 72) and a geocentric CRS in simplified WKT 2, under its own code and 4326,
and ETRS89 declaring NAD83.

PROJ keeps a WKT's content and identifies a code only when the content is that CRS. Each case is a
WKT, a point in WGS 84 and that point in the CRS by `Transformer.from_crs("EPSG:4326", wkt,
always_xy=True)`, or `null` where PROJ refuses the transformation.

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt_root_code.py (needs pyproj 3.8.0)
"""

import json
import os
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/wkt_root_code.json"
)

BNG = (
    'PROJCS["OSGB 1936 / British National Grid",GEOGCS["OSGB 1936",DATUM["OSGB_1936",'
    'SPHEROID["Airy 1830",6377563.396,299.3249646,AUTHORITY["EPSG","7001"]],'
    'TOWGS84[{towgs84}],AUTHORITY["EPSG","6277"]],'
    'PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],'
    'AUTHORITY["EPSG","4277"]],PROJECTION["{method}"],PARAMETER["latitude_of_origin",49],'
    'PARAMETER["central_meridian",-2],PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",400000],'
    'PARAMETER["false_northing",-100000],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],'
    'AXIS["Northing",NORTH],AUTHORITY["EPSG","{code}"]]'
)
BNG_SHIFT = "446.448,-125.157,542.06,0.15,0.247,0.842,-20.489"
NTF_2D = (
    'GEOGCS["unnamed ellipsoid",DATUM["unknown",SPHEROID["unnamed",6378249.2,293.466021294],'
    'TOWGS84[-168,-60,320,0,0,0,0]],PRIMEM["Paris",2.337229166666667],UNIT["degree",0.0174532925199433],'
    'AUTHORITY["EPSG","{code}"]]'
)


def sub(wkt, old, new):
    """`wkt` with `old`, which must occur once, replaced by `new`."""
    assert wkt.count(old) == 1, old
    return wkt.replace(old, new)


def relabel(wkt, code, new):
    """The WKT 2 with its root `ID` replaced."""
    old = f'ID["EPSG",{code}]'
    i = wkt.rindex(old)
    return wkt[:i] + f'ID["EPSG",{new}]' + wkt[i + len(old) :]


def lo19(code):
    return relabel(CRS.from_epsg(2048).to_wkt(), 2048, code)


METRE_UP = 'UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Gravity-related height",UP]'
FOOT_UP = (
    'UNIT["US survey foot",0.304800609601219,AUTHORITY["EPSG","9003"]],'
    'AXIS["Gravity-related height",UP]'
)

LOCAL_AIRY = (
    'GEOGCS["x",DATUM["My_Local",SPHEROID["Airy 1830",6377563.396,299.3249646]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4277"]]'
)
HARN = (
    'GEOGCS["NAD83(HARN)",DATUM["NAD83_High_Accuracy_Reference_Network",'
    'SPHEROID["GRS 1980",6378137,298.257222101]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4269"]]'
)


def wgs84_on(datum):
    """WGS 84 on a datum named `datum`, declaring EPSG:4326."""
    wkt = sub(LOCAL_GRS80, "My_Local_Datum", datum)
    wkt = sub(wkt, "GRS 1980", "WGS 84")
    wkt = sub(wkt, "298.257222101", "298.257223563")
    return sub(wkt, '"4258"', '"4326"')


def greenland(code):
    """EPSG:4747's WKT 1 with its datum by name only, declaring `code`."""
    wkt = CRS.from_epsg(4747).to_wkt("WKT1_GDAL")
    wkt = sub(wkt, ',AUTHORITY["EPSG","1421"]', "")
    return sub(wkt, 'AUTHORITY["EPSG","4747"]]', f'AUTHORITY["EPSG","{code}"]]')


LOCAL_GRS80 = (
    'GEOGCS["x",DATUM["My_Local_Datum",SPHEROID["GRS 1980",6378137,298.257222101]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4258"]]'
)


def unknown_wkt2(identifier):
    """PROJ's WKT 2 of WGS 84 on PROJ's placeholder datum `unknown`, its root `ID` replaced."""
    return sub(
        CRS.from_wkt(wgs84_on("unknown")).to_wkt(), 'ID["EPSG",4326]]', identifier + "]"
    )


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.9.0", (
        pyproj.proj_version_str
    )
    assert os.environ.get("PROJ_NETWORK") == "OFF"
    monte_mario = CRS.from_epsg(3003).to_wkt("WKT1_ESRI")
    cases = [
        (
            "British National Grid with its own code",
            BNG.format(towgs84=BNG_SHIFT, method="Transverse_Mercator", code=27700),
            (-2.0, 54.0),
        ),
        (
            "British National Grid declaring EPSG:4277",
            BNG.format(towgs84=BNG_SHIFT, method="Transverse_Mercator", code=4277),
            (-2.0, 54.0),
        ),
        (
            "an unknown method declaring EPSG:27700",
            BNG.format(towgs84="375,-111,431,0,0,0,0", method="Unheard_Of", code=27700),
            (-2.0, 54.0),
        ),
        ("Hartebeesthoek94 / Lo19 declaring EPSG:1", lo19(1), (18.5, -33.9)),
        ("Hartebeesthoek94 / Lo19 with its own code", lo19(2048), (18.5, -33.9)),
        (
            "a 2D NTF (Paris) in degrees declaring EPSG:7400",
            NTF_2D.format(code=7400),
            (2.35, 48.86),
        ),
        ("Esri Monte_Mario_Italy_1", monte_mario, (12.5, 41.9)),
        (
            "Esri Monte_Mario_Italy_1 on a datum renamed D_Local",
            sub(monte_mario, "D_Monte_Mario", "D_Local"),
            (12.5, 41.9),
        ),
        (
            "Esri USA_Contiguous_Albers_Equal_Area_Conic",
            CRS.from_user_input("ESRI:102003").to_wkt("WKT1_ESRI"),
            (-96.0, 40.0),
        ),
        (
            "Belge 1972 / Belgian Lambert 72 with its own code",
            CRS.from_epsg(31300).to_wkt(),
            (4.4, 50.8),
        ),
        (
            "Belge 1972 / Belgian Lambert 72 declaring EPSG:27700",
            relabel(CRS.from_epsg(31300).to_wkt(), 31300, 27700),
            (4.4, 50.8),
        ),
        (
            "WGS 84 geocentric in simplified WKT 2 with its own code",
            CRS.from_epsg(4978).to_wkt("WKT2_2019_SIMPLIFIED"),
            (10.0, 50.0),
        ),
        (
            "WGS 84 geocentric in simplified WKT 2 declaring EPSG:4326",
            relabel(CRS.from_epsg(4978).to_wkt("WKT2_2019_SIMPLIFIED"), 4978, 4326),
            (10.0, 50.0),
        ),
        (
            "ETRS89 declaring NAD83",
            relabel(CRS.from_epsg(4258).to_wkt(), 4258, 4269),
            (-100.0, 40.0),
        ),
        (
            "ETRS89 in WKT 1 declaring NAD83",
            sub(
                CRS.from_epsg(4258).to_wkt("WKT1_GDAL"),
                'AUTHORITY["EPSG","4258"]]',
                'AUTHORITY["EPSG","4269"]]',
            ),
            (-100.0, 40.0),
        ),
        (
            "Greenland 1996 by name declaring ETRS89",
            greenland(4258),
            (-45.0, 65.0),
        ),
        (
            "Greenland 1996 by name with its own code",
            greenland(4747),
            (-45.0, 65.0),
        ),
        (
            "Greenland 1996 by name in WKT 1 declaring GR96 3D (EPSG:4909)",
            greenland(4909),
            (-45.0, 65.0),
        ),
        (
            "Web Mercator and EGM96 height declaring EPSG:9707",
            CRS.from_user_input("EPSG:3857+5773").to_wkt("WKT1_GDAL")[:-1]
            + ',AUTHORITY["EPSG","9707"]]',
            (10.0, 50.0),
        ),
        (
            "WGS 84 + EGM96 height with its own code",
            CRS.from_epsg(9707).to_wkt("WKT1_GDAL"),
            (10.0, 50.0),
        ),
        (
            "WGS 84 + EGM96 height in US survey feet declaring EPSG:9707",
            sub(CRS.from_epsg(9707).to_wkt("WKT1_GDAL"), METRE_UP, FOOT_UP),
            (10.0, 50.0),
        ),
        (
            "WGS 84 + EGM96 depth declaring EPSG:9707",
            sub(
                CRS.from_epsg(9707).to_wkt("WKT1_GDAL"),
                'AXIS["Gravity-related height",UP]',
                'AXIS["Gravity-related depth",DOWN]',
            ),
            (10.0, 50.0),
        ),
        (
            "WGS 84 without a vertical CRS declaring EPSG:9707",
            sub(CRS.from_epsg(4326).to_wkt("WKT1_GDAL"), '"4326"]]', '"9707"]]'),
            (10.0, 50.0),
        ),
        (
            "a local datum on Airy 1830 declaring OSGB36",
            LOCAL_AIRY,
            (-2.0, 54.0),
        ),
        (
            "British National Grid on a local datum with OSGB36's code inside",
            sub(
                sub(
                    CRS.from_epsg(27700).to_wkt("WKT1_GDAL"),
                    'DATUM["Ordnance_Survey_of_Great_Britain_1936"',
                    'DATUM["My_Local"',
                ),
                ',AUTHORITY["EPSG","6277"]',
                "",
            ),
            (-2.0, 54.0),
        ),
        (
            "British National Grid declaring ESRI:54009",
            sub(
                CRS.from_epsg(27700).to_wkt("WKT1_GDAL"),
                'AUTHORITY["EPSG","27700"]]',
                'AUTHORITY["ESRI","54009"]]',
            ),
            (-2.0, 54.0),
        ),
        (
            "OSGB36 + ODN height whose vertical CRS declares EPSG:5703",
            sub(
                CRS.from_epsg(7405).to_wkt("WKT1_GDAL"),
                'AUTHORITY["EPSG","5701"]',
                'AUTHORITY["EPSG","5703"]',
            ),
            (-2.0, 54.0),
        ),
        (
            "NAD83(HARN) by name declaring NAD83",
            HARN,
            (-100.0, 40.0),
        ),
        (
            "an unknown method in a compound declaring EPSG:7405",
            sub(
                CRS.from_epsg(7405).to_wkt("WKT1_GDAL"),
                'PROJECTION["Transverse_Mercator"]',
                'PROJECTION["Unheard_Of"]',
            ),
            (-2.0, 54.0),
        ),
        (
            "a local datum on GRS 1980 declaring ETRS89",
            LOCAL_GRS80,
            (10.0, 50.0),
        ),
        (
            "WGS 84 on PROJ's placeholder datum unknown with its code",
            wgs84_on("unknown"),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on a datum named Unknown declaring EPSG:4326",
            wgs84_on("Unknown"),
            (10.0, 50.0),
        ),
        (
            "WGS 84 3D declaring EPSG:4326",
            relabel(CRS.from_epsg(4979).to_wkt(), 4979, 4326),
            (10.0, 50.0),
        ),
        (
            "WGS 84 2D declaring EPSG:4979",
            relabel(CRS.from_epsg(4326).to_wkt(), 4326, 4979),
            (10.0, 50.0),
        ),
        (
            "Esri RRAF 1991 of the deprecated EPSG:4640 with its own code",
            CRS.from_epsg(4640).to_wkt("WKT1_ESRI")[:-1] + ',AUTHORITY["EPSG",4640]]',
            (-61.0, 16.0),
        ),
        # Root identifiers anywhere among the root's children (opengeos/terrio#1012 items 31
        # and 57), as PROJ reads them.
        (
            "WGS 84 on the datum unknown with its code before its axes",
            sub(
                wgs84_on("unknown"),
                'AUTHORITY["EPSG","4326"]]',
                'AUTHORITY["EPSG","4326"],AXIS["Latitude",NORTH],AXIS["Longitude",EAST]]',
            ),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code, its ID with a URI",
            unknown_wkt2('ID["EPSG",4326,URI["urn:ogc:def:crs:EPSG::4326"]]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code, its ID with a citation",
            unknown_wkt2('ID["EPSG",4326,CITATION["IOGP"]]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code before a remark",
            unknown_wkt2('ID["EPSG",4326],REMARK["A remark."]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code after another authority's",
            unknown_wkt2('ID["IGNF","WGS84G"],ID["EPSG",4326]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code before another authority's",
            unknown_wkt2('ID["EPSG",4326],ID["IGNF","WGS84G"]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code after an ESRI code that does not exist",
            unknown_wkt2('ID["ESRI",4326],ID["EPSG",4326]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code after an EPSG code that does not exist",
            unknown_wkt2('ID["EPSG",1],ID["EPSG",4326]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code after ESRI:102100",
            unknown_wkt2('ID["ESRI",102100],ID["EPSG",4326]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code after an EPSG code that is no number",
            unknown_wkt2('ID["EPSG","not-a-code"],ID["EPSG",4326]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code after a projected CRS's",
            unknown_wkt2('ID["EPSG",27700],ID["EPSG",4326]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown with its code after ESRI:104000",
            unknown_wkt2('ID["ESRI",104000],ID["EPSG",4326]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown declaring an ESRI and a projected code",
            unknown_wkt2('ID["ESRI",102100],ID["EPSG",27700]'),
            (10.0, 50.0),
        ),
        (
            "WGS 84 on the datum unknown declaring OSGB36 before its code",
            unknown_wkt2('ID["EPSG",4277],ID["EPSG",4326]'),
            (10.0, 50.0),
        ),
        (
            "PROJ's WKT 2 of EPSG:4073, its ID before a remark",
            CRS.from_epsg(4073).to_wkt(),
            (20.5, 44.8),
        ),
        (
            "ETRS89 declaring NAD83 by an ID with a URI",
            sub(
                relabel(CRS.from_epsg(4258).to_wkt(), 4258, 4269),
                'ID["EPSG",4269],',
                'ID["EPSG",4269,URI["urn:ogc:def:crs:EPSG::4269"]],',
            ),
            (-100.0, 40.0),
        ),
    ]
    # Declared codes that are not the CRS; a method terrio's WKT 2 reader does not know
    # (opengeos/terrio#1012 item 43) is refused under one, where PROJ reads its content.
    declared = {
        "British National Grid declaring EPSG:4277": 4277,
        "an unknown method declaring EPSG:27700": 27700,
        "Hartebeesthoek94 / Lo19 declaring EPSG:1": 1,
        "a 2D NTF (Paris) in degrees declaring EPSG:7400": 7400,
        "Belge 1972 / Belgian Lambert 72 declaring EPSG:27700": 27700,
        "WGS 84 geocentric in simplified WKT 2 declaring EPSG:4326": 4326,
        "ETRS89 declaring NAD83": 4269,
        "ETRS89 in WKT 1 declaring NAD83": 4269,
        "Greenland 1996 by name declaring ETRS89": 4258,
        "Web Mercator and EGM96 height declaring EPSG:9707": 9707,
        "WGS 84 + EGM96 height in US survey feet declaring EPSG:9707": 9707,
        "WGS 84 + EGM96 depth declaring EPSG:9707": 9707,
        "WGS 84 without a vertical CRS declaring EPSG:9707": 9707,
        "a local datum on Airy 1830 declaring OSGB36": 4277,
        "British National Grid on a local datum with OSGB36's code inside": 27700,
        "NAD83(HARN) by name declaring NAD83": 4269,
        "an unknown method in a compound declaring EPSG:7405": 7405,
        "a local datum on GRS 1980 declaring ETRS89": 4258,
        "WGS 84 on a datum named Unknown declaring EPSG:4326": 4326,
        "WGS 84 3D declaring EPSG:4326": 4326,
        "WGS 84 2D declaring EPSG:4979": 4979,
        "ETRS89 declaring NAD83 by an ID with a URI": 4269,
        "WGS 84 on the datum unknown declaring OSGB36 before its code": 4277,
    }
    # The code terrio identifies by the content where PROJ's `to_epsg` gives none: British
    # National Grid's `TOWGS84` makes it a bound CRS to PROJ, and PROJ identifies no WKT whose
    # root code is not its content.
    terrio_epsg = {
        "British National Grid with its own code": 27700,
        "British National Grid declaring EPSG:4277": 27700,
        "WGS 84 geocentric in simplified WKT 2 declaring EPSG:4326": 4978,
        "ETRS89 declaring NAD83": 4258,
        "ETRS89 in WKT 1 declaring NAD83": 4258,
        "NAD83(HARN) by name declaring NAD83": 4152,
        "ETRS89 declaring NAD83 by an ID with a URI": 4258,
    }
    refused = {"Belge 1972 / Belgian Lambert 72 declaring EPSG:27700"}
    out = []
    for name, wkt, point in cases:
        crs = CRS.from_wkt(wkt)
        try:
            x, y = Transformer.from_crs("EPSG:4326", crs, always_xy=True).transform(
                *point, errcheck=True
            )
            proj = [x, y]
        except pyproj.exceptions.ProjError:
            proj = None
        authority = crs.to_authority()
        esri = int(authority[1]) if authority and authority[0] == "ESRI" else None
        out.append(
            {
                "name": name,
                "wkt": wkt,
                "point": list(point),
                "proj": proj,
                "epsg": crs.to_epsg(),
                "esri": esri,
                "declared": declared.get(name),
                "terrio_epsg": terrio_epsg.get(name),
                "terrio": "refused" if name in refused else None,
            }
        )
    OUT.write_text(
        json.dumps({"proj": pyproj.proj_version_str, "cases": out}, indent=1) + "\n"
    )


if __name__ == "__main__":
    main()
