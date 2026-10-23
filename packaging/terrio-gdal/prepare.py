"""Prepares python/ to build the terrio-gdal wheels, from a GDAL built with vcpkg.

Usage: python packaging/terrio-gdal/prepare.py VCPKG_PREFIX

VCPKG_PREFIX is vcpkg's install directory for the triplet (``<vcpkg>/installed/x64-linux-terrio``).
Run from the repository's root, after build-gdal.sh has built GDAL; it:

- renames the distribution in python/pyproject.toml to ``terrio-gdal`` (the import package stays
  ``terrio``) and builds it with the ``gdal`` cargo feature;
- copies GDAL's data files and PROJ's database into the package (``terrio/gdal_data``,
  ``terrio/proj_data``), where ``terrio._gdal_env`` points GDAL at them on import;
- gathers the licenses of GDAL and the libraries built with it into python/LICENSE-GDAL.txt,
  which maturin puts in the wheel's ``.dist-info/licenses`` with terrio's own;
- on Windows, copies GDAL's import library ``gdal.lib`` to ``gdal_i.lib``, the name gdal-sys
  links with there.

The changes are for the build machine only: they are not to be committed.
"""

import shutil
import sys
from pathlib import Path

PYTHON = Path("python")
PACKAGE = PYTHON / "python" / "terrio"
DESCRIPTION = "Pure-Rust geospatial raster and vector data I/O, without GDAL"
GDAL_DESCRIPTION = (
    "terrio with the optional GDAL backend: geospatial data I/O in Rust, with GDAL bundled "
    "for the formats terrio does not read itself"
)


def rewrite_pyproject(path: Path) -> None:
    """Renames the distribution to terrio-gdal and turns the ``gdal`` cargo feature on.

    Args:
        path: The path of python/pyproject.toml.

    Raises:
        SystemExit: If the file does not have the lines this function rewrites.
    """
    text = path.read_text(encoding="utf-8")
    replacements = [
        ('name = "terrio"\n', 'name = "terrio-gdal"\n'),
        (f'description = "{DESCRIPTION}"\n', f'description = "{GDAL_DESCRIPTION}"\n'),
        ("[tool.maturin]\n", '[tool.maturin]\nfeatures = ["gdal"]\n'),
    ]
    for old, new in replacements:
        if text.count(old) != 1:
            sys.exit(f"{path}: expected one line {old.strip()!r}")
        text = text.replace(old, new)
    path.write_text(text, encoding="utf-8")


def copy_data(prefix: Path) -> None:
    """Copies GDAL's data files and PROJ's database into the ``terrio`` package.

    Args:
        prefix: vcpkg's install directory for the triplet.

    Raises:
        SystemExit: If GDAL's or PROJ's data files are missing.
    """
    for source, name, marker in (
        (prefix / "share" / "gdal", "gdal_data", "header.dxf"),
        (prefix / "share" / "proj", "proj_data", "proj.db"),
    ):
        if not (source / marker).is_file():
            sys.exit(f"{source / marker} not found")
        target = PACKAGE / name
        if target.exists():
            shutil.rmtree(target)
        # Only data: vcpkg also leaves CMake files and its own copyright notice there.
        shutil.copytree(
            source,
            target,
            ignore=shutil.ignore_patterns("*.cmake", "copyright", "usage", "vcpkg*"),
        )


def gather_licenses(prefix: Path, path: Path) -> None:
    """Writes the licenses of GDAL and the libraries built with it into one file.

    Args:
        prefix: vcpkg's install directory for the triplet, whose ``share/<port>/copyright``
            files hold each library's license.
        path: The file to write.
    """
    parts = [
        "The terrio-gdal wheels bundle GDAL and the libraries it is built with, by vcpkg, under\n"
        "the licenses that follow.\n"
    ]
    for copyright in sorted((prefix / "share").glob("*/copyright")):
        port = copyright.parent.name
        if port.startswith("vcpkg-"):
            continue
        text = copyright.read_text(encoding="utf-8", errors="replace").strip()
        parts.append(f"{'=' * 79}\n{port}\n{'=' * 79}\n\n{text}\n")
    path.write_text("\n".join(parts), encoding="utf-8")


def main() -> None:
    """Prepares python/ from the vcpkg install directory given on the command line."""
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    prefix = Path(sys.argv[1])
    if not (prefix / "include" / "gdal.h").is_file():
        sys.exit(f"no GDAL in {prefix}")
    rewrite_pyproject(PYTHON / "pyproject.toml")
    copy_data(prefix)
    gather_licenses(prefix, PYTHON / "LICENSE-GDAL.txt")
    import_lib = prefix / "lib" / "gdal.lib"
    if import_lib.is_file():
        shutil.copyfile(import_lib, prefix / "lib" / "gdal_i.lib")
    print(f"python/ prepared for terrio-gdal, with GDAL from {prefix}")


if __name__ == "__main__":
    main()
