#!/usr/bin/env python3
"""Freeze the supported PROJJSON parsing subset against GDAL/PROJ, offline."""
import json
from pathlib import Path

from osgeo import gdal, osr

ROOT = Path(__file__).resolve().parents[1]


def strip_ids(value):
    if isinstance(value, dict):
        return {k: strip_ids(v) for k, v in value.items() if k not in ('id', 'ids')}
    if isinstance(value, list):
        return list(map(strip_ids, value))
    return value


def main():
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    cases = []
    definitions = [
        ('British National Grid', 'OGC:CRS84', 'EPSG:27700', [[-1.5, 52.5], [-3, 55], [0, 51]]),
        ('Web Mercator', 'EPSG:4326', 'EPSG:3857', [[0, 0], [12, 45], [-105, 40]]),
        ('North-first projected axes', 'EPSG:3044', 'EPSG:4258', [[500000, 5000000], [400000, 5500000]]),
        ('West and south projected axes', 'EPSG:2046', 'EPSG:4148', [[-100000, 3000000], [-200000, 3100000]]),
        ('US survey feet', 'EPSG:2227', 'EPSG:4269', [[6300000, 2100000], [6400000, 2200000]]),
        ('Paris prime meridian and grads', 'EPSG:4807', 'EPSG:4275', [[0, 50], [2, 55]]),
        ('Bound zero translation', '+proj=longlat +ellps=bessel +towgs84=0,0,0 +type=crs', 'EPSG:4326', [[12, 45], [10, 50]]),
        ('Bound geographic translation', '+proj=longlat +ellps=bessel +towgs84=100,-50,25 +type=crs', 'EPSG:4326', [[12, 45], [10, 50]]),
        ('Bound projected seven parameters', '+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996 +x_0=400000 +y_0=-100000 +ellps=airy +towgs84=446.448,-125.157,542.06,0.1502,0.247,0.8421,-20.4894 +units=m +type=crs', 'EPSG:4326', [[400000, 300000], [500000, 200000]]),
    ]
    for name, src, dst, points in definitions:
        refs = []
        for text in [src, dst]:
            s = osr.SpatialReference()
            s.SetFromUserInput(text)
            s.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            refs.append(s)
        operation = osr.CreateCoordinateTransformation(*refs)
        expected = [list(operation.TransformPoint(*p)[:2]) for p in points]
        for free in [False, True]:
            encoded = [json.loads(s.ExportToPROJJSON()) for s in refs]
            if free:
                encoded = list(map(strip_ids, encoded))
            case = dict(name=name + (' without IDs' if free else ' with IDs'), src=encoded[0], dst=encoded[1], points=points, expected=expected,
                        src_wkt=refs[0].ExportToWkt(['FORMAT=WKT2_2019']), dst_wkt=refs[1].ExportToWkt(['FORMAT=WKT2_2019']),
                        tolerance=[1e-8, 1e-8] if refs[1].IsGeographic() else [0.001, 0.001])
            # PROJ strings preserve these custom bound operations and the Web
            # Mercator pair. Other exports can lose datum/axis-unit information.
            if name.startswith('Bound') or name == 'Web Mercator':
                case.update(src_proj=refs[0].ExportToProj4(), dst_proj=refs[1].ExportToProj4())
            cases.append(case)
    output = dict(environment=dict(gdal=gdal.VersionInfo('RELEASE_NAME'), proj='.'.join(map(str, [osr.GetPROJVersionMajor(), osr.GetPROJVersionMinor(), osr.GetPROJVersionMicro()])), network=False), cases=cases)
    path = ROOT / 'crates/terrio-crs/tests/data/projjson_reference.json'
    path.write_text(json.dumps(output, indent=2, ensure_ascii=False) + '\n')
    print(f'{len(cases)} cases written to {path}')


if __name__ == '__main__':
    main()
