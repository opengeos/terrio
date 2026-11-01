#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Check real CityJSON against published schemas and an independent flat XYZ oracle.

GDAL 3.13.3 has no CityJSON driver. The specifications, not GDAL, define this
comparison. Download fixtures and schemas outside the repository with --fetch.
Checks CLI, Python paths/bytes, and optionally the Node/WebAssembly package.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import subprocess
import tempfile
from urllib.request import urlopen

import jsonschema
from referencing import Registry, Resource

FIXTURES = ["DH_01_subs.city.json", "delft.json", "rotterdam/rotterdam_subset.json"]
CJIO_COMMIT = "35728539127f71b1d5ae458169ff84050e5e20ad"
SCHEMAS = ["cityjson", "cityobjects", "geomprimitives", "geomtemplates", "metadata", "appearance"]


def fetch(data):
    root = data / "citymodels"
    root.mkdir(parents=True, exist_ok=True)
    for name in FIXTURES:
        path = root / Path(name).name
        if not path.exists():
            url = f"https://raw.githubusercontent.com/cityjson/cjio/{CJIO_COMMIT}/tests/data/{name}"
            path.write_bytes(urlopen(url, timeout=30).read())
    for version in ["1.1.3", "2.0.2"]:
        folder = root / "schemas" / version
        folder.mkdir(parents=True, exist_ok=True)
        for name in SCHEMAS:
            path = folder / f"{name}.schema.json"
            if not path.exists():
                url = f"https://raw.githubusercontent.com/cityjson/specs/{version}/schemas/{name}.schema.json"
                path.write_bytes(urlopen(url, timeout=30).read())


def validate_schema(doc, folder):
    registry = Registry()
    schemas = {}
    for name in SCHEMAS:
        schema = json.loads((folder / f"{name}.schema.json").read_text())
        schemas[name] = schema
        resource = Resource.from_contents(schema)
        registry = registry.with_resource(schema["$id"], resource)
    jsonschema.Draft7Validator(schemas["cityjson"], registry=registry).validate(doc)


def reference(doc):
    """Independent boundary traversal; no Terrio geometry or serialization involved."""
    transform = doc.get("transform", {"scale": [1, 1, 1], "translate": [0, 0, 0]})
    vertices = [
        [v[i] * transform["scale"][i] + transform["translate"][i] for i in range(3)]
        for v in doc["vertices"]
    ]
    result = []

    def walk(value, depth):
        if depth == 0:
            yield value
        else:
            for child in value:
                yield from walk(child, depth - 1)

    for identity, obj in doc["CityObjects"].items():
        base = dict(city_id=identity, city_type=obj["type"], parents=obj.get("parents"), attributes=obj.get("attributes"))
        geometries = obj.get("geometry", [])
        if not geometries:
            result.append((base | dict(lod=None, geometry_type=None, semantics=None, surface_semantics=None), None))
        for g in geometries:
            props = base | dict(lod=g.get("lod"), geometry_type=g["type"], semantics=g.get("semantics"), surface_semantics=None)
            kind = g["type"]
            if kind in ("MultiPoint", "MultiLineString"):
                for part in g["boundaries"]:
                    geometry = {"type": "Point", "coordinates": vertices[part]} if kind == "MultiPoint" else {
                        "type": "LineString", "coordinates": [vertices[i] for i in part]}
                    result.append((props, geometry))
            else:
                depth = {"MultiSurface": 1, "CompositeSurface": 1, "Solid": 2, "MultiSolid": 3, "CompositeSolid": 3}[kind]
                faces = list(walk(g["boundaries"], depth))
                semantics = g.get("semantics")
                semantic_values = list(walk(semantics["values"], depth)) if semantics else [None] * len(faces)
                assert len(semantic_values) == len(faces)
                if not faces:
                    result.append((props, None))
                for face, semantic in zip(faces, semantic_values):
                    rings = [[vertices[i] for i in ring] for ring in face]
                    face_props = props | dict(surface_semantics=semantics["surfaces"][semantic] if semantic is not None else None)
                    result.append((face_props, {"type": "Polygon", "coordinates": rings}))
    return result


def key(props, geometry):
    props = props.copy()
    for name in ("parents", "attributes", "semantics", "surface_semantics"):
        if isinstance(props.get(name), str):
            props[name] = json.loads(props[name])
    if props.get("lod") is not None:
        props["lod"] = str(props["lod"])
    if geometry is not None and geometry["type"] == "Polygon":
        rings = []
        for ring in geometry["coordinates"]:
            if len(ring) > 1 and ring[0] == ring[-1]:
                ring = ring[:-1]
            rings.append(ring)
        geometry = geometry | {"coordinates": rings}
    def freeze(value):
        if isinstance(value, dict):
            return tuple((k, freeze(v)) for k, v in sorted(value.items()))
        if isinstance(value, list):
            return tuple(freeze(v) for v in value)
        return value

    return freeze([props, geometry])


def compare(expected, fc):
    actual = Counter(key(f["properties"], f["geometry"]) for f in fc["features"])
    assert actual == expected, f"{sum((expected - actual).values())} missing / {sum((actual - expected).values())} unexpected primitives"


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
        path = args.data / "citymodels" / Path(name).name
        doc = json.loads(path.read_bytes())
        version = "1.1.3" if doc["version"] == "1.1" else "2.0.2"
        validate_schema(doc, args.data / "citymodels" / "schemas" / version)
        expected = Counter(key(*row) for row in reference(doc))
        native = terrio.open_vector(str(path))[0]
        crs = doc.get("metadata", {}).get("referenceSystem")
        expected_epsg = int(crs.rsplit("/", 1)[1]) if crs else None
        assert native.crs == (f"EPSG:{expected_epsg}" if crs else None), native.crs
        for surface, fc in [
            ("Python path", native.to_geojson()),
            ("Python bytes", terrio.open_bytes(path.read_bytes(), filename=path.name)[0].to_geojson()),
        ]:
            compare(expected, fc)
            results.append(dict(name=f"{path.name} {surface}", status="identical", detail=f"schema valid; {sum(expected.values())} primitives, XYZ, holes, attributes, identity, LoD, parents and semantics exact; CRS checked"))
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "out.geojson"
            subprocess.run([args.terrio, "convert", str(path), str(target)], check=True, capture_output=True, text=True)
            compare(expected, json.loads(target.read_bytes()))
            results.append(dict(name=f"{path.name} CLI", status="identical", detail=f"{sum(expected.values())} primitives exact"))
        if args.node_package:
            script = """const t=require(process.argv[1]);const fs=require('fs');
const d=t.open(fs.readFileSync(process.argv[2]),process.argv[2]);
console.log(JSON.stringify(d.toGeoJSON()));d.free();"""
            output = subprocess.check_output(["node", "-e", script, str(args.node_package.resolve()), str(path)], text=True)
            compare(expected, json.loads(output))
            results.append(dict(name=f"{path.name} Node/WASM", status="identical", detail=f"{sum(expected.values())} primitives exact"))
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['detail']}" for r in results))


if __name__ == "__main__":
    main()
