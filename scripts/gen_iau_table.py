"""Generate the IAU_2015 registry from PROJ 9.8.1, including celestial bodies.

The pinned source is PROJ's data/sql/iau.sql (Git blob
900564ccf2e1f77fb708ddc2c8544843b29bc24a), generated from the IAU 2015 report:
https://doi.org/10.1007/s10569-017-9805-5 . Definitions retain their official WKT2.

Rows: code, body, base geographic code, kind (g/c/p), GIS axis, PROJ, name, WKT.
'c' is an ellipsoidal planetocentric geographic CS; its synthetic +geoc PROJ
representation requires latitude conversion in the shared engine. Projected
CRSs use their ordinary inverse/forward projection's geodetic latitudes.
"""
import json
import sqlite3
import subprocess
from pathlib import Path

from osgeo import gdal, osr

ROOT = Path(__file__).resolve().parents[1]
AUTHORITY = 'IAU_2015'


def main():
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    database = next(Path(p) / 'proj.db' for p in osr.GetPROJSearchPaths() if (Path(p) / 'proj.db').is_file())
    connection = sqlite3.connect(database)
    metadata = dict(connection.execute('SELECT key,value FROM metadata'))
    if metadata['PROJ.VERSION'] != '9.8.1':
        raise RuntimeError('Regenerate with the pinned PROJ 9.8.1 database')
    geodetic = connection.execute('''
        SELECT g.code, e.celestial_body_code, g.code, cs.type, e.semi_major_axis,
               e.semi_minor_axis, e.inv_flattening, g.datum_code
        FROM geodetic_crs g
        JOIN geodetic_datum d ON g.datum_auth_name=d.auth_name AND g.datum_code=d.code
        JOIN ellipsoid e ON d.ellipsoid_auth_name=e.auth_name AND d.ellipsoid_code=e.code
        JOIN coordinate_system cs ON g.coordinate_system_auth_name=cs.auth_name AND g.coordinate_system_code=cs.code
        WHERE g.auth_name=? ORDER BY CAST(g.code AS INTEGER)
    ''', (AUTHORITY,)).fetchall()
    rows = {int(r[0]): r for r in geodetic}
    projected = connection.execute('''
        SELECT code,geodetic_crs_code FROM projected_crs WHERE auth_name=? ORDER BY CAST(code AS INTEGER)
    ''', (AUTHORITY,)).fetchall()
    for code, base in projected:
        source = rows[int(base)]
        rows[int(code)] = (code, source[1], base, 'projected', *source[4:])
    output = []
    kinds = {'g': 0, 'c': 0, 'p': 0}
    for code, row in sorted(rows.items()):
        _, body, base, cs_type, a, b, rf, datum = row
        srs = osr.SpatialReference()
        srs.SetFromUserInput(f'{AUTHORITY}:{code}')
        srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
        definition = json.loads(srs.ExportToPROJJSON())
        axes = definition['coordinate_system']['axis']
        kind = 'p' if cs_type == 'projected' else 'c' if cs_type == 'spherical' else 'g'
        kinds[kind] += 1
        if any('meridian' in axis for axis in axes):
            axis = 'enu'
        else:
            directions = {'east': 'e', 'west': 'w', 'north': 'n', 'south': 's'}
            opposite = {'e': 'w', 'w': 'e', 'n': 's', 's': 'n'}
            normalized = []
            for index in srs.GetDataAxisToSRSAxisMapping():
                direction = directions[axes[abs(index)-1]['direction']]
                normalized.append(opposite[direction] if index < 0 else direction)
            axis = ''.join(normalized) + 'u'
        if kind == 'c':
            minor = b if b is not None else a * (1 - 1 / rf)
            proj = f'+proj=longlat +a={a!r} +b={minor!r} +geoc'
        else:
            proj = ' '.join(t for t in srs.ExportToProj4().split() if not t.startswith('+axis=') and t not in ('+no_defs', '+type=crs'))
        if axis != 'enu':
            proj += f' +axis={axis}'
        assert not any(p in proj for p in ('+datum=', '+towgs84=', '+nadgrids=')), proj
        fields = [str(code), str(body), str(base), kind, axis, proj, srs.GetName(), str(datum), srs.ExportToWkt(['FORMAT=WKT2_2019'])]
        assert all('\t' not in value and '\n' not in value for value in fields)
        output.append('\t'.join(fields))
    data = ('\n'.join(output) + '\n').encode()
    compressed = subprocess.run(['zstd', '-19', '--stdout'], input=data, capture_output=True, check=True).stdout
    destination = ROOT / 'crates/terrio-crs/data/iau.tsv.zst'
    destination.write_bytes(compressed)
    info = dict(authority=AUTHORITY, version=2015, proj=metadata['PROJ.VERSION'], gdal=gdal.VersionInfo('RELEASE_NAME'),
                source='https://github.com/OSGeo/PROJ/blob/9.8.1/data/sql/iau.sql', source_git_blob='900564ccf2e1f77fb708ddc2c8544843b29bc24a',
                source_report='https://doi.org/10.1007/s10569-017-9805-5', entries=len(rows), kinds=kinds)
    destination.with_name('iau-source.json').write_text(json.dumps(info, indent=2) + '\n')
    print(f'{len(rows)} definitions ({kinds}), {len(compressed)} compressed bytes')


if __name__ == '__main__':
    main()
