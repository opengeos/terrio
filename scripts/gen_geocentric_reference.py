#!/usr/bin/env python3
"""Freeze GDAL/PROJ XYZ oracle cases for geocentric endpoints (no network/grids)."""
import json
import re
from pathlib import Path

from osgeo import gdal, osr


def srs(text):
    value = osr.SpatialReference()
    value.SetFromUserInput(text)
    value.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    return value


def main():
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    cases = []

    def add(name, src, dst, points, geographic=False, pipeline=None, source_epoch=None):
        options = osr.CoordinateTransformationOptions()
        if pipeline:
            options.SetOperation(pipeline)
        source = srs(src)
        if source_epoch is not None:
            source.SetCoordinateEpoch(source_epoch)
        operation = osr.CreateCoordinateTransformation(source, srs(dst), options)
        cases.append({"name": name, "src": src, "dst": dst, "pipeline": pipeline, "source_epoch": source_epoch, "points": points,
                      "expected": [list(operation.TransformPoint(*p)) for p in points],
                      "tolerance": [1e-8, 1e-8, 0.001] if geographic else [0.001] * 3})

    points = [[147, -42, 100], [0, 0, 0], [90, 0, 1000], [0, 90, 50], [0, -90, -20], [-73, 40, 100], [180, 0, -100], [12, 45, 10000]]
    forward = osr.CoordinateTransformation(srs("EPSG:4979"), srs("EPSG:4978"))
    xyz = [list(forward.TransformPoint(*p)) for p in points]
    add("WGS84 XYZ", "EPSG:4979", "EPSG:4978", points)
    add("inverse, poles and interior", "EPSG:4978", "EPSG:4979", xyz + [[0, 0, 0], [1, 0, 0], [0, 0, -1], [1, 2, 3], [1e6, 1e6, 1e6]], True)
    add("projected to XYZ", "EPSG:32633", "EPSG:4978", [[500000, 5000000, 123], [300000, 4000000, -50]])
    add("XYZ to projected", "EPSG:4978", "EPSG:32633", xyz[:2])
    for geographic, cartesian, point in [(4937, 4936, [12, 45, 100]), (4955, 4954, [-75, 45, 100])]:
        add(f"ellipsoid EPSG:{cartesian}", f"EPSG:{geographic}", f"EPSG:{cartesian}", [point])
    nad = osr.CoordinateTransformation(srs("EPSG:4955"), srs("EPSG:4954"))
    add("CSRS datum XYZ", "EPSG:4954", "EPSG:4978", [list(nad.TransformPoint(-75, 45, 100))])
    airy = "+proj=geocent +ellps=airy +towgs84=446.448,-125.157,542.06,0.15,0.247,0.842,-20.489 +units=m"
    local = osr.CoordinateTransformation(srs("+proj=longlat +ellps=airy"), srs("+proj=geocent +ellps=airy"))
    local_xyz = [list(local.TransformPoint(-2, 53, 100))]
    # PROJ 9.8.1's CRS factory silently skips a geocentric BoundCRS's TOWGS84.
    # Use the declared operation explicitly, rather than certify a lost datum shift.
    helmert = "+proj=pipeline +step +proj=helmert +x=446.448 +y=-125.157 +z=542.06 +rx=0.15 +ry=0.247 +rz=0.842 +s=-20.489 +convention=position_vector"
    add("Helmert XYZ to geographic", airy, "EPSG:4979", local_xyz, True,
        helmert + " +step +inv +proj=cart +ellps=WGS84 +step +proj=unitconvert +xy_in=rad +xy_out=deg +step +proj=axisswap +order=2,1")
    add("Helmert XYZ to projected", airy, "EPSG:32630", local_xyz, False,
        helmert + " +step +inv +proj=cart +ellps=WGS84 +step +proj=utm +zone=30 +ellps=WGS84")
    add("Helmert geographic to XYZ", "+proj=longlat +ellps=airy +towgs84=446.448,-125.157,542.06,0.15,0.247,0.842,-20.489", "EPSG:4978", [[-2, 53, 100]])
    for suffix in ["+units=km", "+units=ft", "+axis=neu", "+axis=end", "+pm=paris"]:
        target = "+proj=geocent +datum=WGS84 " + suffix
        add("Cartesian option " + suffix, "EPSG:4979", target, points[:2])
        operation = osr.CoordinateTransformation(srs("EPSG:4979"), srs(target))
        add("inverse option " + suffix, target, "EPSG:4979", [list(operation.TransformPoint(*p)) for p in points[:2]], True)
    itrf = osr.CoordinateTransformation(srs("EPSG:7912"), srs("EPSG:7789"))
    add("ITRF2014 XYZ epoch to GDA2020", "EPSG:7789", "EPSG:7842", [list(itrf.TransformPoint(135, -25, 100))], source_epoch=2020.0)
    add("sphere", "+proj=longlat +R=1000000", "+proj=geocent +R=1000000 +units=m", points[:3])
    for fmt in ["WKT1_GDAL", "WKT2_2019"]:
        wkt = srs("EPSG:4978").ExportToWkt(["FORMAT=" + fmt])
        without_ids = re.sub(r',?\b(?:ID|AUTHORITY)\[[^\]]*\]', '', wkt)
        add(fmt + " without IDs", "EPSG:4979", without_ids, points[:2])
    output = {"generator": "scripts/gen_geocentric_reference.py", "gdal": gdal.VersionInfo("RELEASE_NAME"),
              "proj": f"{osr.GetPROJVersionMajor()}.{osr.GetPROJVersionMinor()}.{osr.GetPROJVersionMicro()}", "network": False, "cases": cases}
    path = Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/geocentric_reference.json"
    path.write_text(json.dumps(output, indent=2) + "\n")
    print(f"{len(cases)} cases, {sum(len(c['points']) for c in cases)} points: {path}")


if __name__ == "__main__":
    main()
