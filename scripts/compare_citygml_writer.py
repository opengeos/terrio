#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Check native CityGML writer XML against OGC schemas and independent source data.

Coordinates are read from the actual emitted GML primitives, never an annotation.
Standard generic annotations retain the flat JSON fields; native class, owning
hierarchy, integer LoD and CRS are checked separately from those annotations.
GDAL 3.13.3 is informational. PROJ's registry supplies CRS definitions and axes.
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

from lxml import etree
from pyproj import CRS

from compare_citygml import FIXTURES, GML, checked_fixture, compare as compare_gml, source_faces, validate_schema
from compare_cityjson import reference
from compare_cityjson_writer import core_type_source, run_cli

OBJECT_SPACE = 'urn:terrio:citygml:flat-object'
PRIMITIVE_SPACE = 'urn:terrio:citygml:flat-primitive'
PATCH_SPACE = 'urn:terrio:citygml:flat-patches'


def check(value, message):
    if not value:
        raise AssertionError(message)


def local(node):
    return etree.QName(node).localname


def freeze(value):
    return tuple(map(freeze, value)) if isinstance(value, list) else value


def metadata(node):
    if not any(node.get(f'{{{namespace}}}id') for namespace in GML):
        return None
    sets = [child for child in node if local(child).lower() == 'genericattributeset']
    sets += [item for child in node if local(child) == 'genericAttribute' for item in child
             if local(item) == 'GenericAttributeSet']
    found = []
    for group in sets:
        code = group.get('codeSpace') or next((child.text for child in group if local(child) == 'codeSpace'), None)
        if code == OBJECT_SPACE:
            values = [child.text for child in group.iter() if local(child) == 'value']
            check(len(values) == 1, 'flat-object annotation has ambiguous values')
            found.append(json.loads(values[0]))
    check(len(found) <= 1, 'ambiguous object annotations')
    return found[0] if found else None


def native_class(source_type, version):
    aliases = {'TunnelHollowSpace': 'HollowSpace', 'TunnelRoom': 'HollowSpace', 'BuildingStorey': 'Storey', 'TransportSquare': 'Square'}
    aliases |= ({'GenericCityObject': 'GenericOccupiedSpace', 'Room': 'BuildingRoom',
                 'BridgeConstructionElement': 'BridgeConstructiveElement', 'IntBuildingInstallation': 'BuildingInstallation',
                 'IntBridgeInstallation': 'BridgeInstallation', 'IntTunnelInstallation': 'TunnelInstallation'} if version == '3.0'
                else {'GenericOccupiedSpace': 'GenericCityObject', 'GenericUnoccupiedSpace': 'GenericCityObject',
                      'GenericLogicalSpace': 'GenericCityObject', 'BuildingRoom': 'Room', 'BridgeConstructiveElement': 'BridgeConstructionElement'})
    return aliases.get(source_type, source_type)


def xml_rows(doc):
    """Read emitted native primitives independently of Terrio's parser."""
    version = etree.QName(doc.getroot()).namespace.rsplit('/', 1)[-1]
    objects = {node: value for node in doc.iter() if isinstance(node.tag, str) and (value := metadata(node)) is not None}
    identities = [value['city_id'] for value in objects.values()]
    check(len(identities) == len(set(identities)), 'duplicate logical identity')
    for node, value in objects.items():
        check(value['version'] == 1, 'unknown annotation version')
        check(local(node) == native_class(value['city_type'], version), 'native class differs from source classification')
    patch_metadata = {}
    for surface in doc.iter():
        if not isinstance(surface.tag, str) or etree.QName(surface).namespace not in GML or local(surface) != 'TriangulatedSurface':
            continue
        tables = [json.loads(child.text) for child in surface
                  if local(child) == 'name' and child.get('codeSpace') == PATCH_SPACE]
        check(len(tables) <= 1, 'ambiguous patch annotations')
        if tables:
            table = tables[0]
            containers = [child for child in surface if local(child) == 'trianglePatches']
            check(len(containers) == 1, 'annotated TIN needs one trianglePatches container')
            triangles = list(containers[0])
            check(table['version'] == 1 and isinstance(table['patches'], list), 'invalid patch annotation table')
            check(len(table['patches']) == len(triangles), 'patch annotation count differs from native triangles')
            check(all(local(child) == 'Triangle' and etree.QName(child).namespace in GML for child in triangles), 'unexpected native patch')
            patch_metadata.update(zip(triangles, table['patches']))
    result, used, references = [], set(), []
    for node in doc.iter():
        if not isinstance(node.tag, str) or etree.QName(node).namespace not in GML or local(node) not in {'Point', 'LineString', 'Polygon', 'Triangle'}:
            continue
        owner = next((ancestor for ancestor in node.iterancestors() if ancestor in objects), None)
        check(owner is not None, 'primitive has no native object owner')
        used.add(owner)
        obj = objects[owner]
        parent = next((ancestor for ancestor in owner.iterancestors() if ancestor in objects), None)
        parents = [objects[parent]['city_id']] if parent is not None else None
        ancestors = [node, *node.iterancestors()]
        name = next((ancestor.get('srsName') for ancestor in ancestors if ancestor.get('srsName')), None)
        crs = CRS.from_user_input(name) if name else None
        if crs:
            references.append(crs)
        swap = crs is not None and crs.is_geographic and crs.axis_info[0].direction in {'north', 'south'}

        def positions(container):
            points = []
            for item in container.iter():
                if local(item) in {'pos', 'posList'}:
                    values = list(map(float, item.text.split()))
                    check(len(values) % 3 == 0, 'coordinate list is not XYZ')
                    points += [values[i:i+3] for i in range(0, len(values), 3)]
            if swap:
                points = [[p[1], p[0], p[2]] for p in points]
            return points

        kind = local(node)
        if kind in {'Polygon', 'Triangle'}:
            rings = [positions(boundary) for boundary in node if local(boundary) in {'exterior', 'interior'}]
            for ring in rings:
                check(len(ring) >= 4 and ring[0] == ring[-1], 'native ring is not closed')
                ring.pop()
            shape = dict(type='Polygon', coordinates=rings)
        elif kind == 'Point':
            points = positions(node)
            check(len(points) == 1, 'native Point has multiple positions')
            shape = dict(type='Point', coordinates=points[0])
        else:
            shape = dict(type='LineString', coordinates=positions(node))
        annotations = [json.loads(child.text) for child in node
                       if local(child) == 'name' and child.get('codeSpace') == PRIMITIVE_SPACE]
        if node in patch_metadata:
            check(not annotations, 'triangle has conflicting annotations')
            annotations = [patch_metadata[node]]
        native_lod = next((local(a)[3:4] for a in ancestors if local(a).startswith('lod') and local(a)[3:4].isdigit()), None)
        if native_lod is None:
            native_lod = next((child.text for child in owner if local(child) == 'lod'), None)
        check(native_lod is not None, 'primitive has no native LoD property')
        if annotations:
            check(len(annotations) == 1 and annotations[0]['version'] == 1, 'ambiguous primitive annotations')
            annotation = annotations[0]
            check(annotation['lod'].split('.')[0] == native_lod, 'annotated LoD contradicts native LoD')
            lod, semantic = annotation['lod'], annotation['surface_semantics']
        else:
            lod, semantic = native_lod, None
        boundary = next((local(a) for a in ancestors[1:ancestors.index(owner)] if a not in objects
                         and (etree.QName(a).namespace or '').startswith('http://www.opengis.net/citygml/')
                         and local(a).endswith('Surface') and not local(a).startswith('lod')), None)
        if boundary:
            expected_boundary = {'Window':'WindowSurface','Door':'DoorSurface'}.get(semantic['type'],semantic['type']) if semantic else None
            if semantic:
                check(boundary == expected_boundary, 'native thematic class contradicts source classification')
            else:
                check(boundary == 'GenericThematicSurface' or (version == '2.0' and boundary == 'WaterSurface' and obj['city_type']=='WaterBody'),
                      'unclassified face acquired a native classification')
        result.append((dict(city_id=obj['city_id'], city_type=obj['city_type'], parents=parents,
                            attributes=obj['attributes'], lod=lod, surface_semantics=semantic), shape))
    for owner, obj in objects.items():
        if owner not in used:
            parent = next((a for a in owner.iterancestors() if a in objects), None)
            result.append((dict(city_id=obj['city_id'], city_type=obj['city_type'],
                                parents=[objects[parent]['city_id']] if parent is not None else None,
                                attributes=obj['attributes'], lod=None, surface_semantics=None), None))
    if not references:
        references = [CRS.from_user_input(node.text) for node in doc.getroot()
                      if local(node) == 'name' and node.get('codeSpace') == 'urn:terrio:citygml:crs']
    return result, references


def key(row):
    props, geometry = row
    metadata_key = (props['city_id'], props['city_type'], props['lod'] if geometry else None,
                    tuple(props.get('parents') or []), json.dumps(props.get('attributes') or {}, sort_keys=True, ensure_ascii=True),
                    json.dumps(props.get('surface_semantics'), sort_keys=True, ensure_ascii=True))
    if geometry and geometry['type'] == 'Polygon':
        geometry = geometry | dict(coordinates=[ring[:-1] if len(ring)>1 and ring[0]==ring[-1] else ring for ring in geometry['coordinates']])
    return metadata_key, None if geometry is None else (geometry['type'], freeze(geometry['coordinates']))


def compare_json(original, actual):
    """Compare independently read native XML rows with a CityJSON source."""
    wanted = [(props | dict(lod=str(props['lod']) if props['lod'] is not None else ('0' if shape else None)), shape)
              for props, shape in reference(original)]
    check(Counter(map(key, wanted)) == Counter(map(key, actual)), 'native XML geometry or flat metadata differs from source')
    return len(wanted)


def verify(source, output, schema_root):
    doc = etree.ElementTree(etree.fromstring(output, etree.XMLParser(no_network=True, resolve_entities=False)))
    validate_schema(doc, schema_root)
    actual, references = xml_rows(doc)
    if source.suffix == '.gml':
        original = etree.ElementTree(etree.fromstring(checked_fixture(source), etree.XMLParser(no_network=True, resolve_entities=False)))
        expected, owners = source_faces(original)
        features = []
        for props, shape in actual:
            props = props | {name: json.dumps(props[name]) if props[name] is not None else None
                             for name in ['parents', 'attributes', 'surface_semantics']}
            features.append(dict(type='Feature', properties=props, geometry=shape))
        compare_gml(expected, owners, dict(type='FeatureCollection', features=features))
        expected_crs = next((node.get('srsName') for node in original.iter() if node.get('srsName')), None)
        count = len(expected)
    else:
        original = json.loads(source.read_bytes())
        count = compare_json(original, actual)
        expected_crs = original.get('metadata', {}).get('referenceSystem')
    check(all(crs.equals(CRS.from_user_input(expected_crs), ignore_axis_order=True) for crs in references) if expected_crs else not references,
          'native CRS differs from source')
    check(bool(references) == bool(expected_crs), 'native CRS was dropped or invented')
    return count


def tin_source():
    """Exercise native triangle patches with distinct fractional flat fields."""
    return dict(type='CityJSON', version='2.0', transform=dict(scale=[1, 1, 1], translate=[0, 0, 0]),
                vertices=[[0, 0, 0], [1, 0, 1], [0, 1, 2], [1, 1, 3]],
                CityObjects={'terrain': dict(type='ReliefFeature', children=['tin']),
                             'tin': dict(type='TINRelief', parents=['terrain'], geometry=[
                    dict(type='MultiSurface', lod=lod, boundaries=[[face]],
                         semantics=dict(surfaces=[dict(type='GroundSurface', material=material)], values=[0]))
                    for lod, face, material in [('2.1', [0, 1, 2], 'rock'), ('2.2', [1, 3, 2], 'sand')]])})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', type=Path)
    parser.add_argument('--terrio', default='target/release/terrio')
    parser.add_argument('--node-package', type=Path)
    parser.add_argument('--cli-only', action='store_true', help='development check without Python/Node outputs')
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args()
    root = args.data / 'citymodels'
    with tempfile.TemporaryDirectory() as generated_directory:
        generated = Path(generated_directory)
        (generated/'writer-core-types.city.json').write_text(json.dumps(core_type_source(root/'schemas/2.0.2')))
        paths = [(root/name, [version]) for name, version in zip(FIXTURES, ['2.0', '3.0'])]
        paths += [(root/name, ['2.0', '3.0']) for name in ['DH_01_subs.city.json', 'delft.json', 'rotterdam_subset.json']]
        paths += [(path, ['2.0', '3.0']) for path in sorted(root.glob('3dbag-degenerate-page*.city.json'))]
        paths += [(generated/'writer-core-types.city.json', ['3.0'])]
        (generated/'writer-tin.city.json').write_text(json.dumps(tin_source()))
        paths += [(generated/'writer-tin.city.json', ['2.0', '3.0'])]
        lod0 = dict(type='CityJSON', version='2.0', transform=dict(scale=[1,1,1], translate=[0,0,0]),
                    vertices=[[0,0,2],[1,0,2],[0,1,2]], CityObjects={'house':dict(type='Building',
                        geometry=[dict(type='MultiSurface',lod='0',boundaries=[[[0,1,2]]])])})
        (generated/'writer-lod0.city.json').write_text(json.dumps(lod0))
        paths += [(generated/'writer-lod0.city.json', [('2.0', 'footprint'), ('2.0', 'roof-edge')])]

        results = []
        if not args.cli_only:
            import terrio
        for path, versions in paths:
            check(path.is_file(), f'missing source fixture {path}')
            verified = {}
            # A flat CityJSON LoD 0 face has no native FootPrint/RoofEdge selector.
            # Make the corpus conversion choice explicit, without changing its XYZ.
            needs_lod0_choice = False
            if path.suffix != '.gml':
                source_objects = json.loads(path.read_bytes()).get('CityObjects', {})
                needs_lod0_choice = any(obj['type'] in {'Building', 'BuildingPart'}
                                        and any(str(g.get('lod')).split('.')[0] == '0' for g in obj.get('geometry', []))
                                        for obj in source_objects.values())
            for selection in versions:
                version, choice = selection if isinstance(selection, tuple) else (selection, None)
                if version == '2.0' and choice is None and needs_lod0_choice:
                    choice = 'footprint'
                options = dict(citygml_lod0_surface=choice) if choice else {}
                label = version + (f' {choice}' if choice else '')
                with tempfile.TemporaryDirectory() as temporary:
                    folder = Path(temporary)
                    target = folder/'cli.citygml'
                    run_cli([args.terrio, 'convert', str(path), str(target), '--citygml-version', version]
                            + (['--citygml-lod0-surface', choice] if choice else []))
                    outputs = [('CLI', target.read_bytes())]
                    if not args.cli_only:
                        layer = terrio.open_vector(str(path))[0]
                        target = folder/'python.citygml'
                        layer.write(str(target), citygml_version=version, **options)
                        outputs += [('Python path', target.read_bytes()), ('Python bytes', layer.to_bytes('output.citygml', citygml_version=version, **options))]
                        if args.node_package:
                            target = folder/'node.citygml'
                            script = 'const fs=require("fs"),t=require(process.argv[1]);fs.writeFileSync(process.argv[3],t.open(new Uint8Array(fs.readFileSync(process.argv[2])),process.argv[2]).toBytes("model.citygml",{citygmlVersion:process.argv[4],citygmlLod0Surface:process.argv[5]||undefined}));'
                            subprocess.run(['node', '-e', script, str(args.node_package.resolve()), str(path), str(target), version, choice or ''], check=True, capture_output=True)
                            outputs.append(('Node/WASM', target.read_bytes()))
                    for api, output in outputs:
                        doc = etree.fromstring(output, etree.XMLParser(no_network=True, resolve_entities=False))
                        check(etree.QName(doc).namespace == f'http://www.opengis.net/citygml/{version}', 'writer ignored selected version')
                        if isinstance(selection, tuple):
                            native = 'lod0FootPrint' if choice == 'footprint' else 'lod0RoofEdge'
                            check(any(local(n) == native for n in doc.iter()), 'writer ignored selected LoD 0 property')
                        digest = hashlib.sha256(output).digest()
                        if digest not in verified:
                            verified[digest] = verify(path, output, root/'gml-schemas')
                        results.append(dict(name=f'{path.name} {label} {api}', status='identical',
                                            detail=f'schema valid; {verified[digest]} flat primitives; actual native XML XYZ, holes, class, owning hierarchy, LoD and CRS checked; flat JSON annotations checked independently'))
                print(f'validated {path.name} -> {label}', file=sys.stderr, flush=True)
    print(json.dumps(results, indent=2) if args.json else '\n'.join(f"{r['name']}: {r['detail']}" for r in results))


if __name__ == '__main__':
    main()
