#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Validate CityJSON writer output against published schemas and source XYZ.

GDAL 3.13.3 has no CityJSON driver. Source/output flattening reuses reference()
from the reader's Python conformance oracle, independently of the Rust writer.
XYZ tolerance is half an output quantization step plus four floating-point ULPs.
Object metadata and face classification are exact.
"""
import argparse
from collections import defaultdict
from copy import deepcopy
import json
import hashlib
import sys
import math
from pathlib import Path
import subprocess
import tempfile

from compare_cityjson import FIXTURES, fetch, reference, schema_version, validate_schema


def rows(doc):
    grouped = defaultdict(list)
    source = reference(doc)
    spatial = {props["city_id"] for props, geometry in source if geometry is not None}
    nonspatial = set()
    for props, geometry in source:
        identity = props["city_id"]
        if geometry is None:
            if identity in spatial or identity in nonspatial:
                continue
            nonspatial.add(identity)
        sem = deepcopy(props.get("surface_semantics"))
        if sem:
            sem.pop("parent", None)
            sem.pop("children", None)
        metadata = dict(city_id=identity, city_type=props["city_type"],
                        parents=props.get("parents") or [], attributes=props.get("attributes") or {},
                        lod=str(props.get("lod") or "0") if geometry else None, surface_semantics=sem)
        if geometry and geometry["type"] == "Polygon":
            rings = [ring[:-1] if len(ring) > 1 and ring[0] == ring[-1] else ring for ring in geometry["coordinates"]]
            coordinates = [position for ring in rings for position in ring]
            shape = ("Polygon", tuple(map(len, rings)))
        elif geometry and geometry["type"] == "LineString":
            coordinates = geometry["coordinates"]
            shape = ("LineString", len(coordinates))
        elif geometry:
            coordinates = [geometry["coordinates"]]
            shape = ("Point",)
        else:
            coordinates, shape = [], (None,)
        key = (json.dumps(metadata, sort_keys=True, ensure_ascii=False), shape)
        grouped[key].append(coordinates)
    return grouped


def compare(source, output):
    assert source["CityObjects"].keys() == output["CityObjects"].keys(), "object identities differ"
    source_crs = source.get("metadata", {}).get("referenceSystem")
    output_crs = output.get("metadata", {}).get("referenceSystem")
    def authority(value):
        parts = value.rstrip("/").split("/") if value else []
        return (parts[-3], parts[-1]) if parts else None
    assert authority(source_crs) == authority(output_crs), "CRS differs"
    for vertex in output["vertices"]:
        assert len(vertex) == 3 and all(isinstance(v, int) and not isinstance(v, bool) and abs(v) <= 2**53-1 for v in vertex), "unsafe integer vertex"
    expected, actual = rows(source), rows(output)
    assert expected.keys() == actual.keys(), "geometry structure, LoD, object metadata or face classification differs"
    transform = output["transform"]
    low, high = [math.inf]*3, [-math.inf]*3
    for primitives in expected.values():
        for coordinates in primitives:
            for position in coordinates:
                for axis, value in enumerate(position):
                    low[axis], high[axis] = min(low[axis], value), max(high[axis], value)
    constant = [a == b for a, b in zip(low, high)]
    maximum = 0.0
    count = 0
    for key, wanted in expected.items():
        observed = actual[key]
        assert len(wanted) == len(observed), f"primitive count differs for {key}"
        count += len(wanted)
        for before, after in zip(wanted, observed):
            assert len(before) == len(after), "coordinate count differs"
            for p, q in zip(before, after):
                assert len(p) == len(q) == 3, "XYZ dimension differs"
                for axis, (a, b) in enumerate(zip(p, q)):
                    assert math.isfinite(b), "nonfinite XYZ"
                    offset = transform["translate"][axis]
                    step = 0 if constant[axis] else abs(transform["scale"][axis]) / 2
                    bound = step + 4 * max(math.ulp(a), math.ulp(b), math.ulp(offset))
                    error = abs(a-b)
                    assert error <= bound, f"XYZ axis {axis}: {a} vs {b}, error {error} > {bound}"
                    maximum = max(maximum, error)
    return count, maximum


def run_cli(command):
    try:
        subprocess.run(command, check=True, text=True, capture_output=True)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"writer command failed ({error.returncode}): {error.stderr.strip()}") from error


def core_type_source(folder):
    """Exercise every core class listed by the published schema, not the writer."""
    definitions = json.loads((folder / "cityobjects.schema.json").read_bytes())
    objects = {}
    for name, definition in definitions.items():
        if not name[:1].isupper() or name == "ExtensionObject":
            continue
        required = {field for item in definition.get("allOf", []) for field in item.get("required", [])}
        obj = dict(type=name, attributes=dict(name=name), geometry=[dict(
            type="CompositeSurface" if name == "TINRelief" else "MultiSurface",
            lod="2", boundaries=[[[0, 1, 2]]])])
        if "parents" in required:
            family = next(family for family in ["Building", "Bridge", "Tunnel"] if name.startswith(family))
            obj["parents"] = ["core-" + family]
        if "children" in required:
            obj["children"] = []
        objects["core-" + name] = obj
    if len(objects) != 33:
        raise ValueError(f"expected 33 core classes in the pinned CityJSON 2.0.2 schema, found {len(objects)}")
    for identity, obj in list(objects.items()):
        for parent in obj.get("parents", []):
            objects[parent].setdefault("children", []).append(identity)
    return dict(type="CityJSON", version="2.0", transform=dict(scale=[1, 1, 1], translate=[0, 0, 0]),
                vertices=[[0, 0, 0], [0, 1, 0], [0, 1, 1]], CityObjects=objects)


def compatible_source(source, version):
    """Select a labelled 1.1 subset without changing classifications or links."""
    removed = {identity for identity, obj in source["CityObjects"].items()
               if version == "1.1" and obj["type"] == "GenericCityObject"}
    if not removed:
        return source, removed
    for identity, obj in source["CityObjects"].items():
        refs = set(obj.get("parents", [])) | set(obj.get("children", []))
        if (identity not in removed and refs & removed) or (identity in removed and refs - removed):
            raise ValueError("cannot make a GenericCityObject-free subset without changing object links")
    subset = deepcopy(source)
    subset["CityObjects"] = {i: obj for i, obj in subset["CityObjects"].items() if i not in removed}
    return subset, removed


def node_write_command(package, source, target, version):
    code = """const t=require(process.argv[1]),fs=require('fs');
const source=t.open(fs.readFileSync(process.argv[2]),process.argv[2]);
fs.writeFileSync(process.argv[3],source.toBytes('output.city.json',{cityjsonVersion:process.argv[4]}));source.free();"""
    return ["node", "-e", code, str(package.resolve()), str(source), str(target), version]


def rejected_generic_outputs(args, path, layer, folder):
    """All public surfaces must reject a full model unrepresentable in 1.1."""
    from terrio.errors import InvalidArgumentError

    target = folder / "rejected.city.json"
    commands = [("CLI", [args.terrio, "convert", str(path), str(target), "--cityjson-version", "1.1"])]
    if args.node_package:
        commands.append(("Node/WASM", node_write_command(args.node_package, path, target, "1.1")))
    for surface, command in commands:
        result = subprocess.run(command, text=True, capture_output=True)
        assert result.returncode and "GenericCityObject" in result.stderr and "1.1" in result.stderr, result.stderr
        assert not target.exists(), "failed writer created an output file"
        yield surface
    for surface, write in [
        ("Python path", lambda: layer.write(str(target), cityjson_version="1.1")),
        ("Python bytes", lambda: layer.to_bytes("output.city.json", cityjson_version="1.1")),
    ]:
        try:
            write()
        except InvalidArgumentError as error:
            assert "GenericCityObject" in str(error) and "1.1" in str(error), str(error)
        else:
            raise AssertionError(f"{surface} accepted GenericCityObject in CityJSON 1.1")
        assert not target.exists(), "failed writer created an output file"
        yield surface


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

    root = args.data / "citymodels"
    paths = [root / Path(name).name for name in FIXTURES]
    paths += sorted(root.glob("3dbag-degenerate-page*.city.json"))
    with tempfile.TemporaryDirectory() as sample_directory:
        synthetic = Path(sample_directory) / "writer-core-types.city.json"
        synthetic.write_text(json.dumps(core_type_source(root / "schemas/2.0.2")))
        paths.append(synthetic)
        results = []
        for path in paths:
            original = json.loads(path.read_bytes())
            validate_schema(original, root / "schemas" / schema_version(original["version"]))
            verified = {}
            for version in ["1.1", "2.0"]:
                source, removed = compatible_source(original, version)
                with tempfile.TemporaryDirectory() as temporary:
                    folder = Path(temporary)
                    input_path = path
                    label = path.name
                    if removed:
                        layer = terrio.open_vector(str(path))[0]
                        for surface in rejected_generic_outputs(args, path, layer, folder):
                            results.append(dict(name=f"{path.name} 1.1 {surface} rejection", status="expected rejection",
                                                detail=f"explicitly rejected {len(removed)} GenericCityObjects unsupported by 1.1; no output file created"))
                        validate_schema(source, root / "schemas" / schema_version(source["version"]))
                        input_path = folder / "supported-subset.city.json"
                        input_path.write_text(json.dumps(source))
                        label += f" supported subset (excluded {len(removed)} GenericCityObjects)"
                    layer = terrio.open_vector(str(input_path))[0]
                    outputs = []
                    cli = folder / "cli.city.json"
                    run_cli([args.terrio, "convert", str(input_path), str(cli), "--cityjson-version", version])
                    outputs.append(("CLI", cli.read_bytes()))
                    python = folder / "python.city.json"
                    layer.write(str(python), cityjson_version=version)
                    outputs.append(("Python path", python.read_bytes()))
                    outputs.append(("Python bytes", layer.to_bytes("output.city.json", cityjson_version=version)))
                    if args.node_package:
                        target = folder / "node.city.json"
                        run_cli(node_write_command(args.node_package, input_path, target, version))
                        outputs.append(("Node/WASM", target.read_bytes()))
                    for surface, data in outputs:
                        output = json.loads(data)
                        assert output["version"] == version
                        digest = hashlib.sha256(data).digest()
                        if digest not in verified:
                            validate_schema(output, root / "schemas" / schema_version(version))
                            verified[digest] = compare(source, output)
                        count, error = verified[digest]
                        results.append(dict(name=f"{label} {version} {surface}", status="within tolerance",
                                            detail=f"schema valid; {count} flat primitives; exact identity/type, parents, attributes, LoD and face classification; CRS checked; max XYZ error {error:.12g}, within half a quantization step + 4 ULPs"))
                print(f"validated {path.name} -> {version}", file=sys.stderr, flush=True)
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['detail']}" for r in results))


if __name__ == "__main__":
    main()
