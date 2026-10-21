"""Check that Python wheels and sdists carry terrio's license files.

Usage: python .github/scripts/check-license-files.py python/dist/*.whl python/dist/*.tar.gz

A wheel must hold LICENSE, NOTICE and LICENSE-THIRD-PARTY.md in its ``.dist-info/licenses/``
directory, and an sdist in its ``python/`` directory, where maturin finds them.
"""

import sys
import tarfile
import zipfile

NAMES = ("LICENSE", "NOTICE", "LICENSE-THIRD-PARTY.md")


def members(path):
    """Lists the files of a wheel or an sdist.

    Args:
        path: The path of a ``.whl`` or ``.tar.gz`` file.

    Returns:
        The archive's member names.
    """
    if path.endswith(".whl"):
        with zipfile.ZipFile(path) as wheel:
            return wheel.namelist()
    with tarfile.open(path) as sdist:
        return sdist.getnames()


def main():
    """Checks each archive given on the command line and exits non-zero if one lacks a file."""
    missing = []
    for path in sys.argv[1:]:
        names = members(path)
        for name in NAMES:
            if path.endswith(".whl"):
                found = any(n.endswith(f".dist-info/licenses/{name}") for n in names)
            else:
                found = any(n.split("/", 1)[-1] == f"python/{name}" for n in names)
            if not found:
                missing.append(f"{path}: {name}")
    if not sys.argv[1:]:
        missing.append("no wheel or sdist given")
    for line in missing:
        print(f"missing {line}", file=sys.stderr)
    sys.exit(1 if missing else 0)


if __name__ == "__main__":
    main()
