"""Freeze every IAU_2015 projected CRS against GDAL/PROJ, in GIS order.

Both forward and inverse oracle outputs are retained: a projection's own
approximate inverse need not recover its input exactly (notably Robinson).
"""
import json
import math
import subprocess
from pathlib import Path

from osgeo import gdal, osr

ROOT = Path(__file__).resolve().parents[1]


def main():
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    raw = subprocess.check_output(['zstd', '-dc', str(ROOT / 'crates/terrio-crs/data/iau.tsv.zst')], text=True)
    rows = {int(row[0]): row for row in (line.split('\t', 8) for line in raw.splitlines())}
    cases = []
    maximum_roundtrip = 0.0
    worst = None
    inverse_refusals = []
    for code, row in sorted(rows.items()):
        if row[3] != 'p':
            continue
        base = int(row[2])
        parameters = dict(token[1:].split('=', 1) for token in row[5].split() if '=' in token)
        lon0, lat0 = (float(parameters.get(key, 0)) for key in ['lon_0', 'lat_0'])
        points = []
        for dx, dy in [(3, 5), (-7, -8)]:
            lon, lat = lon0 + dx, max(-85.0, min(85.0, lat0 + dy))
            values = dict(e=lon, w=-lon, n=lat, s=-lat)
            points.append([values[axis] for axis in rows[base][4][:2]])
        references = []
        for value in [base, code]:
            srs = osr.SpatialReference()
            srs.SetFromUserInput(f'IAU_2015:{value}')
            srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            references.append(srs)
        forward = osr.CreateCoordinateTransformation(*references)
        projected = [list(forward.TransformPoint(*point)[:2]) for point in points]
        reverse = osr.CreateCoordinateTransformation(*reversed(references))
        inverse, errors = [], []
        for index, point in enumerate(projected):
            try:
                inverse.append(list(reverse.TransformPoint(*point)[:2]))
                errors.append(None)
            except RuntimeError as error:
                inverse.append(None)
                errors.append(str(error))
                inverse_refusals.append([code,index,str(error)])
        assert all(math.isfinite(value) for point in projected + [p for p in inverse if p is not None] for value in point), code
        for original, back in zip(points, inverse, strict=True):
            if back is None:
                continue
            for i, axis in enumerate(rows[base][4][:2]):
                error = back[i] - original[i]
                if axis in 'ew':
                    error = (error + 180) % 360 - 180
                if abs(error) > maximum_roundtrip:
                    maximum_roundtrip, worst = abs(error), code
        case = dict(code=code, base=base, longitude_axis=next(i for i,a in enumerate(rows[base][4][:2]) if a in "ew"), points=points, forward=projected, inverse=inverse)
        if any(errors):
            case["inverse_errors"] = errors
        cases.append(case)
    extra_definitions = [
        ('Mars kilometres and offsets', 'IAU_2015:49900', '+proj=eqc +R=3396190 +lat_0=10 +lon_0=30 +x_0=40000 +y_0=-20000 +units=km +type=crs', [[137.4,-4.6],[35,20]]),
        ('Mars US survey feet', 'IAU_2015:49900', '+proj=eqc +R=3396190 +units=us-ft +type=crs', [[10,45],[-7,-8]]),
        ('Mars polar-radius sphere', 'IAU_2015:49900', '+proj=eqc +R=3376200 +units=m +type=crs', [[10,45],[-7,-8]]),
        ('Mars conformal sphere at nonzero latitude', 'IAU_2015:49901', '+proj=merc +R_C +lat_0=25 +lon_0=17 +a=3396190 +b=3376200 +units=km +type=crs', [[5,-3],[-8,7]]),
    ]
    extras = []
    for name, source, target, points in extra_definitions:
        references = []
        for text in [source,target]:
            srs = osr.SpatialReference()
            srs.SetFromUserInput(text)
            srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            references.append(srs)
        forward = osr.CreateCoordinateTransformation(*references)
        projected = [list(forward.TransformPoint(*point)[:2]) for point in points]
        inverse = osr.CreateCoordinateTransformation(*reversed(references))
        back = [list(inverse.TransformPoint(*point)[:2]) for point in projected]
        extras.append(dict(name=name, source=source,target=target,longitude_axis=next(i for i,a in enumerate(rows[int(source.split(":")[-1])][4][:2]) if a in "ew"),points=points,forward=projected,inverse=back,metres_per_unit=references[1].GetLinearUnits()))
    environment = dict(gdal=gdal.VersionInfo('RELEASE_NAME'), proj='.'.join(map(str, [osr.GetPROJVersionMajor(), osr.GetPROJVersionMinor(), osr.GetPROJVersionMicro()])), network=False,
                       axis_order='GDAL traditional GIS order', projected_tolerance_metres=0.001, inverse_tolerance_degrees=1e-8,
                       max_oracle_roundtrip_degrees=maximum_roundtrip, worst_oracle_roundtrip_code=worst, inverse_refusals=inverse_refusals)
    destination = ROOT / 'crates/terrio-crs/tests/data/iau_reference.json'
    destination.write_text('{"environment":' + json.dumps(environment) + ',"cases":[\n' + ',\n'.join(json.dumps(case, separators=(',', ':')) for case in cases) + '\n],"extras":' + json.dumps(extras,separators=(',', ':')) + '}\n')
    print(f'{len(cases)} CRS pairs, {len(cases)*2} points; {destination.stat().st_size} bytes; max GDAL roundtrip {maximum_roundtrip} degrees at {worst}')


if __name__ == '__main__':
    main()
