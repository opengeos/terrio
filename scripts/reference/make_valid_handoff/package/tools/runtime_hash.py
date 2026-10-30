"""Content hashes for the explicitly approved external reference runtime bindings."""

import hashlib
from pathlib import Path


def hash_path(path):
    path = Path(path)
    if path.is_file():
        return hashlib.sha256(path.read_bytes()).hexdigest()
    tree = hashlib.sha256()
    for item in sorted(path.rglob("*")):
        if item.is_symlink():
            raise ValueError("symlink inside runtime package: " + str(item))
        if item.is_file():
            tree.update(item.relative_to(path).as_posix().encode() + b"\0")
            tree.update(hashlib.sha256(item.read_bytes()).digest())
    return tree.hexdigest()
