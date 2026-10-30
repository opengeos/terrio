"""Custodian-only builder for a source-filtered MakeValid implementation package.

Run the builder through hq. Refuses to replace an existing destination. It reads
only the pinned public caller/encoder files, never the deferred repair module.
Generated encoder source and its audit ledger remain outside the handoff.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


BASELINE = "0281ec0add52008c051cd024ab9a23ca855055d7"
REPO = Path(__file__).resolve().parents[2]
TEMPLATES = REPO / "scripts/reference/make_valid_handoff/package"
sys.dont_write_bytecode = True
sys.path.insert(0, str(TEMPLATES / "tools"))
from runtime_hash import hash_path


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(arguments, *, cwd=None, env=None):
    return subprocess.run(arguments, cwd=cwd, env=env, check=True, capture_output=True, text=True).stdout


def pinned(path):
    return run(["git", "show", f"{BASELINE}:{path}"], cwd=REPO)


def between(text, start, end):
    if text.count(start) != 1:
        raise ValueError(f"source marker is not unique: {start}")
    return text.split(start, 1)[1].split(end, 1)[0]


def encoder_source():
    vector_path = "crates/terrio/src/tiles/vector.rs"
    mvt_path = "crates/terrio/src/tiles/mvt.rs"
    vector, mvt = pinned(vector_path), pinned(mvt_path)
    parts = {
        "orientation": "fn ogr_is_clockwise(" + between(vector, "fn ogr_is_clockwise(", "\n/// How points"),
        "coordinate_adapter": "#[derive(Clone, Copy)]\nenum ToTile" + between(vector, "#[derive(Clone, Copy)]\nenum ToTile", "\n/// Encodes a polygon ring"),
        "ring_encoder": "fn encode_ring(" + between(vector, "fn encode_ring(", "\n/// Encodes a polygon's rings"),
        "polygon_encoder": "fn encode_polygon(" + between(vector, "fn encode_polygon(", "\n/// Encodes polygons in a tile"),
        "zigzag": "pub(crate) fn zigzag(" + between(mvt, "pub(crate) fn zigzag(", "\nfn unzigzag"),
    }
    source = '''// Custodian-only exact caller extraction; not a repair implementation.
#![forbid(unsafe_code)]
#![allow(dead_code)]
use serde_json::{Value, json};
use std::io::{self, Read};
type Pt = (f64, f64);
struct TileFrame { top_x: f64, top_y: f64, dim: f64 }
mod mvt {
    pub const MOVE_TO: u32 = 1;
    pub const LINE_TO: u32 = 2;
    pub const CLOSE_PATH: u32 = 7;
''' + parts["zigzag"] + "\n}\n" + "\n".join(parts[k] for k in ("orientation", "coordinate_adapter", "ring_encoder", "polygon_encoder"))
    source += '''
fn encode(polys: &[Vec<Vec<Pt>>]) -> Value {
    let mut out = Vec::new();
    let mut cursor = (0, 0);
    let mut ok = false;
    let mut encoded = Vec::new();
    for rings in polys {
        let (survived, points) = encode_polygon(&mut out, rings, ToTile::Truncate, &mut cursor);
        ok |= survived;
        if survived { encoded.push(points); }
    }
    json!({"status":"ok", "present":ok && !out.is_empty(), "commands":out, "polygons":encoded})
}
fn request() -> Result<Value, String> {
    let mut text = String::new();
    io::stdin().take(16 * 1024 * 1024 + 1).read_to_string(&mut text).map_err(|e| e.to_string())?;
    if text.len() > 16 * 1024 * 1024 { return Err("transport request exceeds 16 MiB".into()); }
    let request: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let polys: Vec<Vec<Vec<Pt>>> = serde_json::from_value(request.get("polygons").cloned().ok_or("missing polygons")?).map_err(|e| e.to_string())?;
    if polys.iter().flatten().flatten().any(|p| !p.0.is_finite() || !p.1.is_finite() || p.0 < i32::MIN as f64 || p.0 > i32::MAX as f64 || p.1 < i32::MIN as f64 || p.1 > i32::MAX as f64) {
        return Err("outside finite i32 coordinate domain".into());
    }
    Ok(encode(&polys))
}
fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let response = match std::panic::catch_unwind(request) {
        Ok(Ok(value)) => value,
        Ok(Err(message)) => json!({"status":"error", "message":message}),
        Err(payload) => {
            let text = payload.downcast_ref::<String>().map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied()).unwrap_or("");
            json!({"status":if text.contains("overflow") { "encoder_overflow" } else { "encoder_panic" }})
        },
    };
    println!("{response}");
}
'''
    if "super::valid" in source or "fn make_valid" in source:
        raise ValueError("forbidden repair reference in extracted adapter")
    ledger = {
        "baseline": BASELINE,
        "source_files": [{"path": p, "sha256": hashlib.sha256(s.encode()).hexdigest()} for p, s in ((vector_path, vector), (mvt_path, mvt))],
        "exact_extractions": {name: hashlib.sha256(text.encode()).hexdigest() for name, text in parts.items()},
        "source_sha256": hashlib.sha256(source.encode()).hexdigest(),
        "excluded": "No deferred repair module, repair helper, validity implementation, source history, or unreviewed fixture was read or copied by this builder.",
        "license_basis": "Terrio MIT caller; orientation/encoding follows GDAL MIT code. Attribution is retained in package NOTICE. Test reference only.",
    }
    return source, ledger


def runtime_bindings(environment):
    site = environment / "lib/python3.13/site-packages"
    bindings = [{"host": str(site / name), "sandbox": "/reference/" + name} for name in ("numpy", "shapely")]
    libraries = {}
    pending = sorted((site / "numpy").rglob("*.so")) + sorted((site / "shapely").rglob("*.so"))
    visited = set()
    while pending:
        path = pending.pop()
        if path in visited:
            continue
        visited.add(path)
        for line in run(["ldd", str(path)]).splitlines():
            match = re.match(r"\s*(\S+) => (/\S+)", line)
            if not match:
                continue
            name, filename = match.groups()
            resolved = Path(filename).resolve()
            if resolved.is_relative_to(environment):
                if "/" in name:
                    raise ValueError("invalid library soname")
                previous = libraries.get(name)
                if previous and digest(previous) != digest(resolved):
                    raise ValueError("conflicting reference runtime library: " + name)
                libraries[name] = resolved
                pending.append(resolved)
    bindings += [{"host": str(path), "sandbox": "/reference/lib/" + name} for name, path in sorted(libraries.items())]
    return bindings


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", required=True, type=Path)
    parser.add_argument("--scratch", type=Path, default=Path("/mnt/scratch/terrio/make-valid-handoff"))
    parser.add_argument("--reference-environment", type=Path, default=Path("/mnt/scratch/terrio/tools/.pixi/envs/default"))
    args = parser.parse_args()
    destination = args.destination.resolve()
    if destination.exists():
        parser.error("destination already exists; refusing to overwrite it")
    args.scratch.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix="prepare-", dir=args.scratch))
    package, adapter = stage / "package", stage / "custodian-adapter"
    shutil.copytree(TEMPLATES, package, ignore=shutil.ignore_patterns("__pycache__", "target", "Cargo.lock"))
    for path in package.rglob("*"):
        if path.is_symlink():
            raise ValueError("template symlink not allowed: " + str(path))
    (package / "SPEC.md").write_bytes((REPO / "docs/make-valid-clean-room-spec.md").read_bytes())
    (package / "fixtures").mkdir()
    (package / "fixtures/observations.json").write_bytes((REPO / "docs/make-valid-clean-room-observations.json").read_bytes())
    (package / "reference").mkdir()
    (package / "reference/capture_make_valid_contract.py").write_bytes((REPO / "scripts/reference/capture_make_valid_contract.py").read_bytes())
    source, ledger = encoder_source()
    (adapter / "src").mkdir(parents=True)
    (adapter / "src/main.rs").write_text(source)
    (adapter / "Cargo.toml").write_text('''[package]
name = "tile-encoder"
version = "0.0.0"
edition = "2024"
license = "MIT"
publish = false
[dependencies]
serde_json = "=1.0.154"
[profile.release]
debug = 0
strip = "symbols"
overflow-checks = true
''')
    env = dict(os.environ)
    env.update(CARGO_TARGET_DIR=str(stage / "target"), CARGO_BUILD_JOBS="2", CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0")
    env.pop("RUSTC_WRAPPER", None)
    env["RUSTFLAGS"] = f"--remap-path-prefix={stage}=/handoff"
    run(["cargo", "build", "--offline", "--release"], cwd=adapter, env=env)
    shutil.copy2(stage / "target/release/tile-encoder", package / "reference/tile-encoder")
    run(["cargo", "generate-lockfile", "--offline"], cwd=package, env=env)
    vendor_config = run(["cargo", "vendor", "--offline", "--locked", "--versioned-dirs", "vendor"], cwd=package, env=env)
    (package / ".cargo").mkdir()
    (package / ".cargo/config.toml").write_text(vendor_config.replace(str(package / "vendor"), "vendor"))
    dependencies = json.loads(run(["cargo", "metadata", "--offline", "--locked", "--format-version", "1"], cwd=package, env=env))["packages"]
    approved_dependencies = []
    for dependency in dependencies:
        if dependency["name"] == "tile-repair-candidate":
            continue
        if dependency["license"] not in {"MIT", "MIT OR Apache-2.0", "Apache-2.0 OR MIT", "Unlicense OR MIT", "(MIT OR Apache-2.0) AND Unicode-3.0"}:
            raise ValueError("dependency needs a license decision: " + dependency["name"])
        selected = "MIT AND Unicode-3.0" if dependency["name"] == "unicode-ident" else "MIT"
        approved_dependencies.append({**{k: dependency[k] for k in ("name", "version", "license")}, "selected_license": selected})
    ledger["dependencies"] = approved_dependencies
    ledger["builder_sha256"] = digest(Path(__file__))
    ledger["rustc"] = run(["rustc", "--version"]).strip()
    ledger["binary_sha256"] = digest(package / "reference/tile-encoder")
    ledger["binary_profile"] = "release; overflow-checks=true; no debug symbols; stripped; generated-source paths remapped"
    ledger["generated_source_directory"] = str(adapter)
    (stage / "custodian-ledger.json").write_text(json.dumps(ledger, indent=2) + "\n")
    toolchain = run(["rustc", "--print", "sysroot"]).strip()
    runtime = {
        "toolchain": toolchain,
        "host_queue": shutil.which("hq"),
        "build_directory": str(args.scratch / ("candidate-target-" + stage.name)),
        "reference_bindings": runtime_bindings(args.reference_environment.resolve()),
    }
    for binding in runtime["reference_bindings"]:
        binding["sha256"] = hash_path(binding["host"])
    if not runtime["host_queue"]:
        raise ValueError("host queue missing")
    (package / "HOST-RUNTIME.json").write_text(json.dumps(runtime, indent=2) + "\n")
    license_text = (REPO / "LICENSE").read_text().split("\n---\n", 1)[0].rstrip() + "\n"
    (package / "LICENSE").write_text(license_text)
    notice = '''Independent tile repair scaffold and test handoff, prepared 2026-10-29.
Original scaffold, transport, package tools and independently authored numeric
fixtures are supplied under the accompanying MIT license.

The reference/tile-encoder executable contains ONLY selected Terrio tile caller
functions at commit 0281ec0add52008c051cd024ab9a23ca855055d7, under MIT, with
GDAL-derived orientation/encoding behavior. It contains no repair algorithm.
Terrio contributors: https://github.com/opengeos/terrio
GDAL/OGR: https://github.com/OSGeo/gdal/tree/v3.13.3
Copyright (c) 1999, Frank Warmerdam
Copyright (c) 2008-2014, Even Rouault
Copyright (c) 2018, Even Rouault
The MIT permission and warranty terms in LICENSE apply to this reference too.
The exact source-boundary ledger remains with the custodian, outside the handoff.

Vendored Rust dependencies retain their upstream license files and copyright
notices in vendor/. Their exact names, versions and licenses are in HANDOFF.json.

The externally mounted reference runtime uses Shapely/NumPy and GEOS for tests
only; it is not redistributed in this archive or linked into candidate code.
GEOS is LGPL licensed: https://libgeos.org/ . No GEOS source is included.
'''
    (package / "NOTICE").write_text(notice)
    handoff = {
        "schema_version": 1, "date": "2026-10-29", "source_baseline": BASELINE,
        "status": "prepared; candidate unimplemented; no implementation agent launched",
        "review_scope": "Source-filtered package preparation by source-exposed custodian. No claim of legal clearance or agent-wide T3 isolation.",
        "adapter": {"sha256": ledger["binary_sha256"], "profile": ledger["binary_profile"], "kind": "exact production caller extraction; no repair"},
        "dependencies": approved_dependencies,
        "known_encoder_domain_exclusion": "wide-i32-bowtie; kernel comparison mandatory; native encoder overflows i32 deltas with checks enabled",
        "reference_cases": 51,
        "isolation": {"kind": "Bubblewrap per-command boundary", "network": "disabled", "agent_wide": "not configured by package", "writable": ["candidate", "notes", "external SSD build directory"]},
    }
    (package / "HANDOFF.json").write_text(json.dumps(handoff, indent=2) + "\n")
    # Check all transitive package files; vendored crates may not introduce symlinks.
    files = []
    for path in sorted(package.rglob("*")):
        if path.is_symlink():
            raise ValueError("package symlink not permitted: " + str(path))
        if not path.is_file():
            continue
        relative = path.relative_to(package).as_posix()
        if relative == ".git" or "/.git/" in "/" + relative or relative.endswith("/valid.rs"):
            raise ValueError("prohibited source/history path: " + relative)
        files.append({"path": relative, "sha256": digest(path), "role": "mutable" if relative.startswith(("candidate/", "notes/")) else "immutable"})
    manifest = {"schema_version": 1, "source_baseline": BASELINE, "files": files}
    (package / "MANIFEST.json").write_text(json.dumps(manifest, indent=2) + "\n")
    build = Path(runtime["build_directory"])
    build.mkdir(exist_ok=False)
    (build / ".handoff-manifest-sha256").write_text(digest(package / "MANIFEST.json") + "\n")
    (stage / "custodian-ledger.json").write_text(json.dumps({**ledger, "manifest_sha256": digest(package / "MANIFEST.json")}, indent=2) + "\n")
    shutil.copytree(package, destination)
    print(json.dumps({"destination": str(destination), "custodian_ledger": str(stage / "custodian-ledger.json"), "manifest_sha256": digest(destination / "MANIFEST.json"), "files": len(files)}, indent=2))


if __name__ == "__main__":
    main()
