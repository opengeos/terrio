#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Validate real CityGML 2.0/3.0 using published XSDs and source XML coordinates.

The two public citygml4j examples contain explicit polygon geometry. The oracle
checks every face's XYZ, holes, owner/type, LoD and thematic class independently
of Terrio. Synthetic Rust tests cover implicit geometry and invalid references.
GDAL's partial reader is informational and is not the correctness reference.
"""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import json
import hashlib
from pathlib import Path
import re
import subprocess
import tempfile
from urllib.parse import urljoin, urlparse
from urllib.request import urlopen

from lxml import etree

if not __debug__:
    raise RuntimeError("CityGML conformance checks require assertions; remove Python -O")

COMMIT = "203325705c79bb4a30483f1f279385674bb953fd"
FIXTURES = ["lod3_building_v2.gml", "lod2_buildings_v3.gml"]
FIXTURE_SHA256 = {'lod3_building_v2.gml': 'b6c11f7f57f8f934ef4a8e48e40c727e15d15b016e5c977e7be59b86430183c4', 'lod2_buildings_v3.gml': 'bac5481415cabedafb492001588f3fc8a4ab8797e1c70391c95114b6cc428246'}

XSD = "http://www.w3.org/2001/XMLSchema"
XSI = "http://www.w3.org/2001/XMLSchema-instance"
GML = {"http://www.opengis.net/gml", "http://www.opengis.net/gml/3.2"}
# The explicit city-object types present in these reference files.
OBJECTS = {"Building", "BuildingInstallation", "Window", "Door"}


def canonical(url):
    parsed = urlparse(url)
    if parsed.netloc not in {"schemas.opengis.net", "www.w3.org", "docs.oasis-open.org"}:
        raise ValueError(f"unexpected schema host {url}")
    if ".." in parsed.path.split("/"):
        raise ValueError(f"schema path escapes its cache: {url}")
    return "https://" + parsed.netloc + parsed.path


def cache_path(root, url):
    parsed = urlparse(canonical(url))
    return root / parsed.netloc / parsed.path.lstrip("/")


def schema_locations(doc):
    value = doc.getroot().get(f"{{{XSI}}}schemaLocation")
    if not value or len(value.split()) % 2:
        raise ValueError("schemaLocation must contain namespace/URL pairs")
    tokens = value.split()
    return list(zip(tokens[::2], map(canonical, tokens[1::2])))


def checked_fixture(path):
    payload = path.read_bytes()
    if hashlib.sha256(payload).hexdigest() != FIXTURE_SHA256[path.name]:
        raise ValueError(f"pinned CityGML fixture checksum differs: {path.name}")
    return payload


def fetch(data):
    folder = data / "citymodels"
    folder.mkdir(parents=True, exist_ok=True)
    for name in FIXTURES:
        path = folder / name
        if not path.exists():
            url = f"https://raw.githubusercontent.com/citygml4j/citygml4j/{COMMIT}/citygml4j-samples/input/datasets/{name}"
            payload = urlopen(url, timeout=30).read()
            if hashlib.sha256(payload).hexdigest() != FIXTURE_SHA256[name]:
                raise ValueError(f"downloaded CityGML fixture checksum differs: {name}")
            path.write_bytes(payload)
        checked_fixture(path)
    root = folder / "gml-schemas"
    pending = {url for name in FIXTURES for _, url in schema_locations(etree.parse(str(folder / name), etree.XMLParser(no_network=True, resolve_entities=False)))}
    seen = set()

    def load(url):
        path = cache_path(root, url)
        if not path.exists():
            payload = urlopen(url, timeout=30).read()
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(payload)
        schema = etree.fromstring(path.read_bytes(), etree.XMLParser(no_network=True, resolve_entities=False))
        return [canonical(urljoin(url, n.get("schemaLocation"))) for n in schema
                if n.tag in {f"{{{XSD}}}import", f"{{{XSD}}}include", f"{{{XSD}}}redefine"} and n.get("schemaLocation")]

    with ThreadPoolExecutor(max_workers=6) as pool:
        while pending:
            batch = sorted(pending - seen)
            if not batch:
                break
            seen.update(batch)
            pending = {url for refs in pool.map(load, batch) for url in refs} - seen
    return len(seen)


def validate_schema(doc, root):
    class Offline(etree.Resolver):
        def resolve(self, url, public_id, context):
            url = canonical(url)
            return self.resolve_string(cache_path(root, url).read_bytes(), context, base_url=url)

    parser = etree.XMLParser(no_network=True, resolve_entities=False)
    parser.resolvers.add(Offline())
    wrapper = etree.Element(f"{{{XSD}}}schema", nsmap={"xs": XSD})
    for namespace, url in schema_locations(doc):
        etree.SubElement(wrapper, f"{{{XSD}}}import", namespace=namespace, schemaLocation=url)
    schema = etree.XMLSchema(etree.fromstring(etree.tostring(wrapper), parser))
    schema.assertValid(doc)


def identity(node):
    return next((node.get(f"{{{ns}}}id") for ns in GML if node.get(f"{{{ns}}}id")), None)


def source_faces(doc):
    expected = []
    source_owners = {}
    for node in doc.iter():
        if not isinstance(node.tag, str):
            continue
        qname = etree.QName(node)
        if qname.localname in OBJECTS and (qname.namespace or "").startswith("http://www.opengis.net/citygml/"):
            source_owners[identity(node)] = node
        if qname.namespace not in GML or qname.localname != "Polygon":
            continue
        ancestors = list(node.iterancestors())
        owner = next((n for n in ancestors if etree.QName(n).localname in OBJECTS), None)
        if owner is None:
            raise ValueError("source Polygon has no supported city object owner")
        lod = next((re.match(r"lod(\d+)", etree.QName(n).localname).group(1)
                    for n in ancestors if re.match(r"lod(\d+)", etree.QName(n).localname)), None)
        if lod is None:
            raise ValueError("source Polygon has no LoD property")
        semantic = None
        for ancestor in ancestors:
            if ancestor is owner:
                break
            name = etree.QName(ancestor).localname
            if name.endswith("Surface") and not name.startswith("lod") and "citygml" in (etree.QName(ancestor).namespace or ""):
                semantic = name
                break
        rings = []
        for boundary in node:
            if etree.QName(boundary).localname not in {"exterior", "interior"}:
                continue
            coordinates = []
            for pos in boundary.iter():
                if etree.QName(pos).localname not in {"pos", "posList"}:
                    continue
                # Both published fixtures declare three-dimensional coordinate lists.
                values = [float(v) for v in pos.text.split()]
                assert len(values) % 3 == 0
                coordinates += [values[i:i + 3] for i in range(0, len(values), 3)]
            if coordinates[0] == coordinates[-1]:
                coordinates.pop()
            rings.append(coordinates)
        expected.append((identity(owner), etree.QName(owner).localname, lod, semantic, rings))
    return expected, source_owners


def freeze(value):
    return tuple(freeze(x) for x in value) if isinstance(value, list) else value


def compare(expected, owners, fc):
    rows = []
    for feature in fc["features"]:
        props = feature["properties"]
        owner = owners[props["city_id"]]
        attrs = json.loads(props["attributes"])
        # Collect source occurrences independently, including repeatable function/usage.
        source_values = {}
        for child in owner:
            name = etree.QName(child).localname
            if (len(child) == 0 and name not in OBJECTS
                    and name not in {"boundedBy", "boundary", "appearance", "appearanceMember"}
                    and child.get("{http://www.w3.org/1999/xlink}href") is None):
                expected_value = (child.text or "").strip()
                units = {k: child.get(k) for k in ("uom", "codeSpace") if child.get(k) is not None}
                if units:
                    expected_value = {"value": expected_value} | units
                source_values.setdefault(name, []).append(expected_value)
        for name, values in source_values.items():
            assert attrs[name] == (values[0] if len(values) == 1 else values), (name, attrs)
        allowed = {etree.QName(child).localname for child in owner}
        allowed |= {child.get("name") for child in owner if etree.QName(child).localname.endswith("Attribute") and child.get("name")}
        for child in owner:
            if etree.QName(child).localname == "genericAttribute":
                allowed |= {name.text for item in child for name in item if etree.QName(name).localname == "name"}
        assert set(attrs) <= allowed, (set(attrs) - allowed, attrs)
        if feature["geometry"] is None:
            assert not any(row[0] == props["city_id"] for row in expected)
            continue
        assert feature["geometry"]["type"] == "Polygon"
        rings = []
        for ring in feature["geometry"]["coordinates"]:
            if ring[0] == ring[-1]:
                ring = ring[:-1]
            rings.append(ring)
        semantics = json.loads(props["surface_semantics"]) if props["surface_semantics"] else None
        rows.append((props["city_id"], props["city_type"], props["lod"], semantics.get("type") if semantics else None, freeze(rings)))
    wanted = Counter((a,b,c,d,freeze(e)) for a,b,c,d,e in expected)
    actual = Counter(rows)
    assert wanted == actual, f"{sum((wanted-actual).values())} missing / {sum((actual-wanted).values())} unexpected faces"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--fetch", action="store_true")
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--node-package", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if args.fetch:
        fetch(args.data)
    import terrio
    results = []
    for name in FIXTURES:
        path = args.data / "citymodels" / name
        doc = etree.ElementTree(etree.fromstring(checked_fixture(path), etree.XMLParser(no_network=True, resolve_entities=False)))
        validate_schema(doc, args.data / "citymodels/gml-schemas")
        expected, owners = source_faces(doc)
        source = terrio.open_vector(str(path))
        assert source.driver == "CityGML"
        # The fixtures use the legacy OGC compound URN 3068 + 5783.
        crs = source[0].crs
        assert crs and "3068" in crs and "5783" in crs, crs
        surfaces = [("Python path", source[0].to_geojson()),
                    ("Python bytes", terrio.open_bytes(path.read_bytes(), filename=name)[0].to_geojson())]
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "faces.geojson"
            subprocess.run([args.terrio, "convert", str(path), str(target)], capture_output=True, text=True, check=True)
            surfaces.append(("CLI", json.loads(target.read_bytes())))
        if args.node_package:
            code = "const t=require(process.argv[1]),fs=require('fs');const d=t.open(fs.readFileSync(process.argv[2]),process.argv[2]);console.log(JSON.stringify(d.toGeoJSON()));d.free();"
            output = subprocess.check_output(["node", "-e", code, str(args.node_package.resolve()), str(path)], text=True)
            surfaces.append(("Node/WASM", json.loads(output)))
        for surface, fc in surfaces:
            compare(expected, owners, fc)
            results.append(dict(name=f"{name} {surface}", status="identical", detail=f"schema valid; {len(expected)} faces; exact XYZ, holes, owner/type, LoD, thematic class and direct scalar source properties; source compound CRS checked"))
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['detail']}" for r in results))


if __name__ == "__main__":
    main()
