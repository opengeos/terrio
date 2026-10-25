#!/usr/bin/env python3
"""Measure equivalent CRS-only consumers before and after extraction, serially.

Requires Rust's wasm32-unknown-unknown target, GNU time and wasm-opt. Output must
be a new directory on disk. Run without other builds for meaningful timings.
No registry packages are published and no repository checkout is modified.
"""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import time


PROBE = '''use {crate}::{{SpatialRef, Transformer}};

#[unsafe(no_mangle)]
pub extern "C" fn transform(longitude: f64, latitude: f64) -> f64 {{
    let source = SpatialRef::parse("EPSG:4326").unwrap();
    let target = SpatialRef::parse("EPSG:3857").unwrap();
    Transformer::new(&source, &target).unwrap()
        .transform(longitude, latitude).unwrap().0
}}
'''


def capture(command, **kwargs):
    return subprocess.check_output(command, text=True, **kwargs).strip()


def source_state(path):
    return {
        "commit": capture(["git", "-C", str(path), "rev-parse", "HEAD"]),
        "dirty": bool(capture(["git", "-C", str(path), "status", "--porcelain"])),
        "diff_sha256": hashlib.sha256(subprocess.check_output(["git", "-C", str(path), "diff", "HEAD"])).hexdigest(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True, help="Pre-extraction workspace")
    parser.add_argument("--after", type=Path, required=True, help="Workspace with terrio-crs")
    parser.add_argument("--output", type=Path, required=True, help="New disk-backed directory")
    parser.add_argument("--wasm-opt", required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    env.update({
        "CARGO_BUILD_JOBS": "2",
        "CARGO_PROFILE_DEV_DEBUG": "0",
        "CARGO_PROFILE_TEST_DEBUG": "0",
        "CARGO_INCREMENTAL": "0",
        "BINARYEN_CORES": "2",
    })
    # Force compilation rather than timing cache hits. Still populate the shared
    # compiler cache as required by the workspace's local-build instructions.
    if shutil.which("sccache"):
        env.update(RUSTC_WRAPPER="sccache", SCCACHE_RECACHE="1")
    report = {
        "rustc": capture(["rustc", "-Vv"]),
        "wasm_opt": capture([args.wasm_opt, "--version"]),
        "machine": platform.platform(),
        "cpu": platform.processor(),
        "jobs": 2,
        "compiler_cache": "recache" if env.get("SCCACHE_RECACHE") else "disabled",
        "profile": {"opt-level": "s", "lto": True, "codegen-units": 1, "panic": "abort", "strip": True},
        "before": source_state(args.before),
        "after": source_state(args.after),
        "measurements": [],
    }
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    configurations = [
        ("before", args.before.resolve(), "terrio", "terrio::crs", []),
        ("native", args.after.resolve(), "terrio-crs", "terrio_crs", []),
        ("fallback", args.after.resolve(), "terrio-crs", "terrio_crs", ["wbprojection-fallback"]),
        ("terrio-after", args.after.resolve(), "terrio", "terrio::crs", []),
    ]
    for label, workspace, package, rust_crate, features in configurations:
        probe = output / label
        (probe / "src").mkdir(parents=True)
        dependency = json.dumps(str(workspace / "crates" / package))
        (probe / "Cargo.toml").write_text(
            '[package]\nname = "crs-footprint-probe"\nversion = "0.0.0"\nedition = "2024"\npublish = false\n'
            '[lib]\ncrate-type = ["cdylib", "rlib"]\n'
            f'[dependencies]\n{package} = {{ path = {dependency}, default-features = false, features = {json.dumps(features)} }}\n'
            '[profile.release]\nopt-level = "s"\nlto = true\ncodegen-units = 1\npanic = "abort"\nstrip = true\n'
        )
        (probe / "src/lib.rs").write_text(PROBE.format(crate=rust_crate))
        # Seed shared dependency versions from the corresponding source snapshot.
        shutil.copyfile(workspace / "Cargo.lock", probe / "Cargo.lock")
        for target in ["native", "wasm32-unknown-unknown"]:
            target_args = [] if target == "native" else ["--target", target]
            target_dir = probe / ("target-" + target)
            build_env = dict(env, CARGO_TARGET_DIR=str(target_dir))
            tree = capture(["cargo", "tree", "--edges", "normal,build", "--prefix", "none", *target_args], cwd=probe, env=build_env)
            (probe / f"dependencies-{target}.txt").write_text(tree + "\n")
            # Version and name identify each compiled package; paths and repeated
            # feature-expansion markers do not create additional dependencies.
            packages = {tuple(line.split()[:2]) for line in tree.splitlines()}
            packages.discard(("crs-footprint-probe", "v0.0.0"))
            command = ["cargo", "build", "--release", "--locked", *target_args]
            usage = probe / f"usage-{target}.json"
            measured_command = ["/usr/bin/time", "-f", '{"max_rss_kib": %M, "user_seconds": %U, "system_seconds": %S}', "-o", str(usage), *command]
            started = time.perf_counter()
            with (probe / f"build-{target}.log").open("w") as log:
                subprocess.run(measured_command, cwd=probe, env=build_env, stdout=log, stderr=subprocess.STDOUT, check=True)
            elapsed = time.perf_counter() - started
            row = {"configuration": label, "target": target, "packages": len(packages), "elapsed_seconds": elapsed, **json.loads(usage.read_text()), "command": command}
            if target != "native":
                wasm = target_dir / target / "release/crs_footprint_probe.wasm"
                optimized = probe / "optimized.wasm"
                subprocess.run([args.wasm_opt, "-Oz", "--all-features", str(wasm), "-o", str(optimized)], env=build_env, check=True)
                row.update(wasm_bytes=wasm.stat().st_size, optimized_wasm_bytes=optimized.stat().st_size, optimized_gzip_bytes=len(gzip.compress(optimized.read_bytes(), mtime=0)))
            report["measurements"].append(row)
            (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(row), flush=True)


if __name__ == "__main__":
    main()
