#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Require a newer catalog version when its recorded digest differs from a base commit."""

import argparse
import json
from pathlib import Path
import re
import subprocess

SCHEMA = "schemas/cli-usage-v0.1.schema.json"
GOLDEN = "crates/terrio-cli/tests/usage_golden.rs"


def digest(text):
    """Read the digest already checked against the live CLI by the Rust golden test."""
    match = re.search(r'const CATALOG_SHA256: &str = "([a-f0-9]{64})";', text)
    if not match:
        raise ValueError("cannot read CATALOG_SHA256 from the golden test")
    return match[1]


def version(text):
    """Read and normalize the schema version, including the original two-part 0.1."""
    value = json.loads(text)["properties"]["schema_version"]["const"]
    if not isinstance(value, str):
        raise ValueError(f"invalid catalog version: {value!r}")
    parts = value.split(".")
    if len(parts) not in (2, 3) or any(not part.isdecimal() for part in parts):
        raise ValueError(f"invalid catalog version: {value!r}")
    return tuple(int(part) for part in parts) + (0,) * (3 - len(parts))


def check(base, repo):
    """Compare working-tree metadata with the immutable base, without building either CLI."""
    resolved = subprocess.run(
        ["git", "rev-parse", "--verify", "--end-of-options", base + "^{commit}"],
        cwd=repo, capture_output=True, text=True,
    )
    if resolved.returncode:
        raise ValueError(f"base revision is not available: {base}")
    base = resolved.stdout.strip()

    def before(path, required=True):
        result = subprocess.run(["git", "show", f"{base}:{path}"], cwd=repo, capture_output=True, text=True)
        if result.returncode:
            if required:
                raise ValueError(f"cannot read {path} from base revision {base}")
            return None
        return result.stdout

    old_version = version(before(SCHEMA))
    old_golden = before(GOLDEN, required=False)
    old_digest = digest(old_golden) if old_golden is not None else None
    new_version = version((repo / SCHEMA).read_text())
    new_digest = digest((repo / GOLDEN).read_text())
    if new_digest != old_digest and new_version <= old_version:
        raise ValueError(
            "catalog digest changed without a newer schema_version; bump the catalog patch version "
            "and update its schema, version assertions and golden digest together"
        )


def main():
    """Check a PR against the base SHA supplied by GitHub's pull-request event."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="the protected base commit SHA")
    args = parser.parse_args()
    try:
        check(args.base, Path(__file__).resolve().parents[1])
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"{error}\n")
    print("Catalog version agrees with the protected base comparison.")


if __name__ == "__main__":
    main()
