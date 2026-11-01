#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Generate native property-order tables from the published CityGML schemas.

Usage: python scripts/generate_citygml_writer_profiles.py SCHEMA_CACHE OUTPUT
SCHEMA_CACHE is the schemas.opengis.net/citygml directory downloaded by the
CityGML conformance fetcher. No schema files are bundled with Terrio.
The output contains schema names and structural facts, not schema implementations.
"""
import argparse
import json
from pathlib import Path
from lxml import etree

X = '{http://www.w3.org/2001/XMLSchema}'
PREFIXES = {'building':'bldg', 'bridge':'brid', 'tunnel':'tun', 'generics':'gen',
            'construction':'con', 'cityfurniture':'frn', 'cityobjectgroup':'grp',
            'landuse':'luse', 'vegetation':'veg', 'relief':'dem', 'transportation':'tran',
            'waterbody':'wtr'}


def generate(root, version):
    docs = [etree.parse(str(p), etree.XMLParser(no_network=True, resolve_entities=False))
            for p in sorted(root.glob(f'*/{version}/*.xsd'))]
    core = root / version / ('core.xsd' if version == '3.0' else 'cityGMLBase.xsd')
    docs.append(etree.parse(str(core), etree.XMLParser(no_network=True, resolve_entities=False)))
    elements, types, prefixes = {}, {}, {}
    for doc in docs:
        schema = doc.getroot()
        ns = schema.get('targetNamespace', '')
        if not ns.startswith('http://www.opengis.net/citygml/'):
            continue
        module = ns.split('/')[-2]
        prefix = PREFIXES.get(module, 'core')
        prefixes[ns] = prefix
        for node in schema:
            if node.get('name'):
                if node.tag == X + 'element':
                    elements[(ns, node.get('name'))] = node
                elif node.tag == X + 'complexType':
                    types[(ns, node.get('name'))] = node

    def qname(node, name):
        if not name:
            return None
        if ':' in name:
            p, name = name.split(':', 1)
            return node.nsmap[p], name
        return node.nsmap.get(None), name

    def tag(key):
        return prefixes[key[0]] + ':' + key[1]

    def derives(key, ancestor):
        seen = set()
        while key in elements and key not in seen:
            if key == ancestor:
                return True
            seen.add(key)
            node = elements[key]
            key = qname(node, node.get('substitutionGroup'))
        return False

    city = ('http://www.opengis.net/citygml/' + version, '_CityObject' if version == '2.0' else 'AbstractCityObject')
    concrete = {key for key, node in elements.items() if node.get('abstract') != 'true' and derives(key, city)}

    def props(key, seen=()):
        if key not in types or key in seen:
            return []
        node = types[key]
        ext = node.find('./' + X + 'complexContent/' + X + 'extension')
        inherited = props(qname(ext, ext.get('base')), (*seen, key)) if ext is not None else []
        body = ext if ext is not None else node
        return inherited + [p for seq in body if seq.tag in {X+'sequence', X+'choice', X+'all'}
                            for p in seq if p.tag == X+'element' and p.get('name')]

    profiles = []
    for key in sorted(concrete, key=tag):
        node = elements[key]
        entries = []
        for prop in props(qname(node, node.get('type'))):
            name = prop.get('name')
            if name in {'generalizesTo', 'relatedTo'}:
                continue
            schema = prop.getroottree().getroot()
            ns = schema.get('targetNamespace')
            prop_tag = tag((ns, name))
            typ = qname(prop, prop.get('type'))
            local_type = typ[1] if typ else ''
            geometry = {'GeometryPropertyType':'Any', 'MultiSurfacePropertyType':'Surface', 'SurfacePropertyType':'SurfaceSingle',
                        'MultiCurvePropertyType':'Curve', 'MultiPointPropertyType':'Points',
                        'PointPropertyType':'Point'}.get(local_type)
            if name == 'tin':
                geometry = 'Tin'
            refs = []
            if typ in types:
                refs += [qname(e, e.get('ref')) for e in types[typ].iter(X+'element') if e.get('ref')]
            refs += [qname(e, e.get('ref')) for e in prop.iter(X+'element') if e.get('ref')]
            children = sorted({tag(ref) for ref in refs if ref and ref[0] in prefixes and any(derives(c, ref) for c in concrete)})
            if geometry or children or name == 'lod':
                entries.append((prop_tag, geometry or 'None', children,
                                prop.get('minOccurs', '1') != '0', prop.get('maxOccurs', '1') != '1'))
        ancestors = []
        current = key
        while current in elements and current[0] in prefixes:
            ancestors.append(tag(current))
            current = qname(elements[current], elements[current].get('substitutionGroup'))
        profiles.append((tag(key), ancestors, entries))
    return profiles


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cache', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    out = ['// SPDX-' + 'FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors',
           '// SPDX-' + 'License-Identifier: MIT', '',
           '// Generated by scripts/generate_citygml_writer_profiles.py from published OGC',
           '// CityGML 2.0/3.0 schemas: https://schemas.opengis.net/citygml/.',
           '// Structural names and ordering only; schemas remain the conformance authority.',
           'use super::{GeometryProperty as G, Profile, Property};', '',
           'pub(super) fn profiles(version: super::CityGmlVersion) -> &\'static [Profile] {',
           '    match version {']
    for version, variant in [('2.0','V2_0'), ('3.0','V3_0')]:
        out.append(f'        super::CityGmlVersion::{variant} => &[')
        for name, ancestors, entries in generate(args.cache, version):
            ancestors = ', '.join(map(json.dumps, ancestors))
            out.append(f'            Profile {{ name: {json.dumps(name)}, ancestors: &[{ancestors}], properties: &[')
            for name, geom, children, required, many in entries:
                children = ', '.join(map(json.dumps, children))
                out.append(f'                Property {{ name: {json.dumps(name)}, geometry: G::{geom}, children: &[{children}], required: {str(required).lower()}, many: {str(many).lower()} }},')
            out.append('            ] },')
        out.append('        ],')
    out += ['    }', '}', '']
    args.output.write_text('\n'.join(out))


if __name__ == '__main__':
    main()
