"""Verify initial package hashes, or the immutable references during implementation."""

import argparse
import hashlib
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--initial", action="store_true", help="also check mutable candidate scaffold hashes")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    manifest = json.loads((root / "MANIFEST.json").read_text())
    failures = []
    listed = {item["path"] for item in manifest["files"]}
    for path in root.rglob("*"):
        relative = path.relative_to(root).as_posix()
        mutable = relative.startswith(("candidate/", "notes/"))
        if path.is_symlink():
            failures.append(relative + ": symlink is outside the reviewed package policy")
        elif path.is_file() and relative not in listed and relative != "MANIFEST.json" and not mutable:
            failures.append(relative + ": unexpected immutable file")
    for item in manifest["files"]:
        if item["role"] == "mutable" and not args.initial:
            continue
        path = root / item["path"]
        if path.is_symlink() or not path.is_file():
            failures.append(item["path"] + ": missing, nonregular, or symlink")
        elif hashlib.sha256(path.read_bytes()).hexdigest() != item["sha256"]:
            failures.append(item["path"] + ": hash mismatch")
    if failures:
        raise SystemExit("\n".join(failures))
    print("PASS: approved package hashes" + (" including original scaffold" if args.initial else " (mutable candidate excluded)"))


if __name__ == "__main__":
    main()
