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
from copy import deepcopy
import json
import re
from pathlib import Path
import subprocess
import tempfile
from urllib.request import urlopen
from urllib.parse import urlparse

FIXTURES = ["DH_01_subs.city.json", "delft.json", "rotterdam/rotterdam_subset.json"]
CJIO_COMMIT = "35728539127f71b1d5ae458169ff84050e5e20ad"
SCHEMA_COMMITS = {"1.1.3": "e242f3e59cb62e2001811f0874c48438b0ae2858",
                  "2.0.2": "877ee49e16c82a4bc7ab598926efcb9ba1075374"}
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
                url = f"https://raw.githubusercontent.com/cityjson/specs/{SCHEMA_COMMITS[version]}/schemas/{name}.schema.json"
                path.write_bytes(urlopen(url, timeout=30).read())


def fetch_3dbag(data):
    """Cache the reported live API bbox as per-page, single-transform CityJSON files."""
    root = data / "citymodels"
    root.mkdir(parents=True, exist_ok=True)
    url = "https://api.3dbag.nl/collections/pand/items?bbox=85000,446700.034,85300.011,447000&limit=1000"

    def shift(value, offset):
        return [shift(v, offset) for v in value] if isinstance(value, list) else value + offset

    for page_number in range(1, 11):
        parsed = urlparse(url)
        if parsed.scheme != "https" or parsed.netloc != "api.3dbag.nl" or parsed.path != "/collections/pand/items":
            raise ValueError(f"unexpected 3DBAG page link {url}")
        suffix = "" if page_number == 1 else f"-page{page_number}"
        cached = root / f"3dbag-degenerate-api{suffix}.json"
        if not cached.exists():
            cached.write_bytes(urlopen(url, timeout=30).read())
        page = json.loads(cached.read_bytes())
        doc = deepcopy(page["metadata"])
        for feature in page["features"]:
            offset = len(doc["vertices"])
            doc["vertices"].extend(feature["vertices"])
            for identity, original in feature["CityObjects"].items():
                obj = deepcopy(original)
                for geometry in obj.get("geometry", []):
                    geometry["boundaries"] = shift(geometry["boundaries"], offset)
                if identity in doc["CityObjects"]:
                    raise ValueError(f"duplicate API object {identity}")
                doc["CityObjects"][identity] = obj
        (root / f"3dbag-degenerate-page{page_number}.city.json").write_text(json.dumps(doc))
        next_urls = [link["href"] for link in page["links"] if link["rel"] == "next"]
        if not next_urls:
            return
        url = next_urls[0]
    raise ValueError("3DBAG bbox pagination exceeded ten pages")


def validate_schema(doc, folder):
    import jsonschema
    from referencing import Registry, Resource

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
        base = dict(city_id=identity, city_type=obj["type"], parents=obj.get("parents"), attributes=obj.get("attributes"), skipped_rings=0, skipped_faces=0)
        geometries = obj.get("geometry", [])
        if not geometries:
            result.append((base | dict(lod=None, geometry_type=None, semantics=None, surface_semantics=None), None))
        for g in geometries:
            source = g
            local_vertices = vertices
            if g["type"] == "GeometryInstance":
                source = doc["geometry-templates"]["templates"][g["template"]]
                anchor = vertices[g["boundaries"][0]]
                matrix = g["transformationMatrix"]
                local_vertices = [
                    [sum(matrix[row * 4 + col] * (point + [1])[col] for col in range(4)) + anchor[row]
                     for row in range(3)]
                    for point in doc["geometry-templates"]["vertices-templates"]
                ]
            props = base | dict(lod=g.get("lod", source.get("lod")), geometry_type=g["type"], semantics=source.get("semantics"), surface_semantics=None)
            kind = source["type"]
            if kind in ("MultiPoint", "MultiLineString"):
                for part in source["boundaries"]:
                    geometry = {"type": "Point", "coordinates": local_vertices[part]} if kind == "MultiPoint" else {
                        "type": "LineString", "coordinates": [local_vertices[i] for i in part]}
                    result.append((props, geometry))
            else:
                depth = {"MultiSurface": 1, "CompositeSurface": 1, "Solid": 2, "MultiSolid": 3, "CompositeSolid": 3}[kind]
                faces = list(walk(source["boundaries"], depth))
                semantics = source.get("semantics")
                semantic_values = list(walk(semantics["values"], depth)) if semantics else [None] * len(faces)
                assert len(semantic_values) == len(faces)
                candidates = []
                skipped_rings = 0
                for face in faces:
                    rings = [[local_vertices[i] for i in ring] for ring in face]
                    valid = [len(set(map(tuple, ring))) >= 3 for ring in rings]
                    skipped_rings += sum(not value for value in valid)
                    candidates.append([ring for ring, keep in zip(rings, valid) if keep] if valid and valid[0] else None)
                props = props | dict(skipped_rings=skipped_rings, skipped_faces=sum(r is None for r in candidates))
                if not any(r is not None for r in candidates):
                    result.append((props, None))
                for rings, semantic in zip(candidates, semantic_values):
                    if rings is None:
                        continue
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


def schema_version(version):
    """Use only the published schema versions supported by the reader."""
    versions = {"1.1": "1.1.3", "2.0": "2.0.2"}
    if version not in versions:
        raise ValueError(f"no published schema for CityJSON version {version!r}")
    return versions[version]


def convert_cli(terrio, source, target):
    """Keep the converter diagnostic in conformance reports when conversion fails."""
    try:
        subprocess.run([str(terrio), "convert", str(source), str(target)], check=True, capture_output=True, text=True)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"{source}: terrio convert failed ({error.returncode}): {error.stderr.strip()}") from error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--fetch", action="store_true")
    parser.add_argument("--fetch-3dbag", action="store_true")
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--node-package", type=Path)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--extra-fixture", type=Path, action="append", default=[])
    args = parser.parse_args()
    if args.fetch:
        fetch(args.data)
    if args.fetch_3dbag:
        fetch_3dbag(args.data)
    import terrio

    results = []
    paths = [args.data / "citymodels" / Path(name).name for name in FIXTURES]
    paths += sorted((args.data / "citymodels").glob("3dbag-degenerate-page*.city.json"))
    paths += [p for p in args.extra_fixture if p not in paths]
    for path in paths:
        doc = json.loads(path.read_bytes())
        version = schema_version(doc["version"])
        validate_schema(doc, args.data / "citymodels" / "schemas" / version)
        expected = Counter(key(*row) for row in reference(doc))
        native = terrio.open_vector(str(path))[0]
        crs = doc.get("metadata", {}).get("referenceSystem")
        expected_epsg = int(re.split(r"[:/]", crs)[-1]) if crs else None
        assert native.crs == (f"EPSG:{expected_epsg}" if crs else None), native.crs
        for surface, fc in [
            ("Python path", native.to_geojson()),
            ("Python bytes", terrio.open_bytes(path.read_bytes(), filename=path.name)[0].to_geojson()),
        ]:
            compare(expected, fc)
            results.append(dict(name=f"{path.name} {surface}", status="identical", detail=f"schema valid; {sum(expected.values())} primitives, surviving XYZ/holes, diagnostics, attributes, identity, LoD, parents and semantics exact; CRS checked"))
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "out.geojson"
            convert_cli(args.terrio, path, target)
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
