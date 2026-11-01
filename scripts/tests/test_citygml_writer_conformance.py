"""Independent native TIN checks must retain and validate per-patch metadata."""
import json
from pathlib import Path
import sys

import pytest

etree = pytest.importorskip("lxml.etree")
pytest.importorskip("pyproj")
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_citygml_writer import xml_rows


def tin():
    doc = etree.ElementTree(etree.fromstring(b'''<core:CityModel xmlns:core="http://www.opengis.net/citygml/3.0" xmlns:dem="http://www.opengis.net/citygml/relief/3.0" xmlns:gen="http://www.opengis.net/citygml/generics/3.0" xmlns:gml="http://www.opengis.net/gml/3.2"><core:cityObjectMember><dem:TINRelief gml:id="tin"><core:genericAttribute><gen:GenericAttributeSet><gen:codeSpace>urn:terrio:citygml:flat-object</gen:codeSpace><gen:genericAttribute><gen:StringAttribute><gen:value/></gen:StringAttribute></gen:genericAttribute></gen:GenericAttributeSet></core:genericAttribute><dem:lod>2</dem:lod><dem:tin><gml:TriangulatedSurface><gml:name codeSpace="urn:terrio:citygml:flat-patches"/><gml:trianglePatches><gml:Triangle><gml:exterior><gml:LinearRing><gml:posList>0 0 0 1 0 1 0 1 2 0 0 0</gml:posList></gml:LinearRing></gml:exterior></gml:Triangle><gml:Triangle><gml:exterior><gml:LinearRing><gml:posList>1 0 1 1 1 3 0 1 2 1 0 1</gml:posList></gml:LinearRing></gml:exterior></gml:Triangle></gml:trianglePatches></gml:TriangulatedSurface></dem:tin></dem:TINRelief></core:cityObjectMember></core:CityModel>'''))
    value = next(n for n in doc.iter() if etree.QName(n).localname == 'value')
    value.text = json.dumps(dict(version=1, city_id='tin', city_type='TINRelief', attributes={}))
    name = next(n for n in doc.iter() if etree.QName(n).localname == 'name')
    name.text = json.dumps(dict(version=1, patches=[dict(version=1, lod=lod, surface_semantics=dict(type='GroundSurface', material=material))
                                                 for lod, material in [('2.1', 'rock'), ('2.2', 'sand')]]))
    return doc, name


def test_native_tin_has_distinct_fractional_lods_and_semantics():
    rows, crs = xml_rows(tin()[0])
    assert not crs
    assert [p['lod'] for p, _ in rows] == ['2.1', '2.2']
    assert [p['surface_semantics']['material'] for p, _ in rows] == ['rock', 'sand']
    assert rows[1][1]['coordinates'][0][1] == [1, 1, 3]


@pytest.mark.parametrize('change', ['count', 'version', 'patch_version', 'lod', 'duplicate', 'kind'])
def test_rejects_corrupt_native_patch_annotations(change):
    doc, name = tin()
    table = json.loads(name.text)
    if change == 'count': table['patches'].pop()
    elif change == 'version': table['version'] = 2
    elif change == 'patch_version': table['patches'][0]['version'] = 2
    elif change == 'lod': table['patches'][0]['lod'] = '3.1'
    elif change == 'duplicate':
        clone = etree.SubElement(name.getparent(), name.tag, codeSpace=name.get('codeSpace'))
        clone.text = name.text
    elif change == 'kind':
        triangle = next(n for n in doc.iter() if etree.QName(n).localname == 'Triangle')
        triangle.tag = triangle.tag.replace('Triangle', 'Polygon')
    name.text = json.dumps(table)
    with pytest.raises(AssertionError):
        xml_rows(doc)


@pytest.mark.parametrize('kind', ['missing', 'ignores_version', 'supported'])
def test_optional_node_probe_requires_citygml_method_and_version(tmp_path, kind):
    import shutil
    from conformance import node_citygml_writer_available

    if not shutil.which('node'):
        pytest.skip('Node is not installed')
    module = tmp_path / 'terrio.js'
    module.with_name('terrio_bg.wasm').write_bytes(b'presence marker')
    method = '' if kind == 'missing' else (
        'toBytes(name,options){return Buffer.from(\'<core:CityModel xmlns:core="http://www.opengis.net/citygml/\'+'
        + ("'3.0'" if kind == 'ignores_version' else 'options.citygmlVersion')
        + '+\'"/>\');},')
    module.write_text('module.exports={open(){return {' + method + 'free(){}};}};')
    assert node_citygml_writer_available(module) == (kind == 'supported')


def source_tin():
    return dict(type='CityJSON', version='2.0', transform=dict(scale=[1, 1, 1], translate=[0, 0, 0]),
                vertices=[[0, 0, 0], [1, 0, 1], [0, 1, 2], [1, 1, 3]],
                CityObjects={'tin': dict(type='TINRelief', geometry=[
                    dict(type='MultiSurface', lod=lod, boundaries=[[face]],
                         semantics=dict(surfaces=[dict(type='GroundSurface', material=material)], values=[0]))
                    for lod, face, material in [('2.1', [0, 1, 2], 'rock'), ('2.2', [1, 3, 2], 'sand')]])})


def test_native_tin_matches_independent_cityjson_source():
    from compare_citygml_writer import compare_json
    assert compare_json(source_tin(), xml_rows(tin()[0])[0]) == 2


@pytest.mark.parametrize('change', ['xyz', 'attribute', 'semantic', 'identity', 'class', 'lod'])
def test_independent_source_comparison_rejects_corrupted_native_tin(change):
    from compare_citygml_writer import compare_json
    doc, name = tin()
    value = next(n for n in doc.iter() if etree.QName(n).localname == 'value')
    obj = json.loads(value.text)
    table = json.loads(name.text)
    if change == 'xyz':
        positions = next(n for n in doc.iter() if etree.QName(n).localname == 'posList')
        positions.text = positions.text.replace('1 0 1', '1 0 9')
    elif change == 'attribute': obj['attributes']['invented'] = True
    elif change == 'semantic': table['patches'][0]['surface_semantics']['material'] = 'water'
    elif change == 'identity': obj['city_id'] = 'another'
    elif change == 'class':
        owner = next(n for n in doc.iter() if etree.QName(n).localname == 'TINRelief')
        owner.tag = owner.tag.replace('TINRelief', 'MassPointRelief')
    elif change == 'lod': table['patches'][0]['lod'] = '2.9'
    value.text = json.dumps(obj)
    name.text = json.dumps(table)
    with pytest.raises(AssertionError):
        compare_json(source_tin(), xml_rows(doc)[0])
