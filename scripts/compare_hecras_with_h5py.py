"""Compare terrio's HECRAS vector driver with an independent h5py reading of the same files.

GDAL cannot read HEC-RAS geometry or plan HDF files, so the reference is h5py: ras-commander,
the open-source Python library for HEC-RAS, reads these datasets with h5py and NumPy, and
``hecras_oracle.py`` reads them the same way, without terrio or its HEC-RAS engine. This script
finds the HEC-RAS files of a corpus directory, has the oracle digest them, and runs terrio's
check of the same files (``crates/terrio/tests/hecras_corpus.rs``) against the digests. Every
compared value must be identical: geometry coordinates, identities and joined results are
compared bit for bit, and a refusal is only accepted where the test lists it.

Only the files HEC-RAS 7.0.1 wrote are compared by default (``--version``): that is the version
terrio's HECRAS driver is tested on. Older files are read where the engine supports them, best
effort, and are not part of this comparison.

Usage:
    python scripts/compare_hecras_with_h5py.py CORPUS_DIR [--version 7.0.1] [--cargo cargo]

Prints one line per file, ``OK <file>`` or ``DIFF <file>: <what differs>``, then a summary line.
Exits 1 when a file differs, 2 when the comparison cannot run. Needs h5py, NumPy and cargo.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import h5py

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import hecras_oracle  # noqa: E402

# The root `File Type` values HEC-RAS writes; which of them a file carries does not say whether
# it is a geometry or a plan file.
FILE_TYPES = {"HEC-RAS Geometry", "HEC-RAS Results"}


def hecras_files(corpus, version):
    """The HEC-RAS files of ``corpus`` that ``version`` wrote, relative to it, sorted.

    Args:
        corpus: The corpus directory.
        version: The HEC-RAS version (``7.0.1``), matched against the stored ``File Version``
            (``HEC-RAS 7.0.1 June 2026``); ``None`` for every version.

    Returns:
        A list of relative POSIX paths.
    """
    out = []
    for path in sorted(Path(corpus).rglob("*")):
        if path.suffix.lower() not in (".hdf", ".h5") or not path.is_file():
            continue
        try:
            with h5py.File(path, "r") as f:
                kind = hecras_oracle.text(f.attrs.get("File Type", b""))
                stored = hecras_oracle.text(f.attrs.get("File Version", b""))
        except OSError:
            continue
        if kind not in FILE_TYPES:
            continue
        if version and not re.match(rf"HEC-RAS {re.escape(version)}(\s|$)", stored):
            continue
        out.append(path.relative_to(corpus).as_posix())
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("corpus", help="directory of HEC-RAS geometry and plan HDF files (searched recursively)")
    ap.add_argument("--version", default="7.0.1", help="HEC-RAS version to compare, or 'all' (default 7.0.1)")
    ap.add_argument("--cargo", default="cargo", help="cargo command (default cargo)")
    args = ap.parse_args()
    corpus = os.path.abspath(args.corpus)
    files = hecras_files(corpus, None if args.version == "all" else args.version)
    if not files:
        print(f"no HEC-RAS {args.version} files in {corpus}", file=sys.stderr)
        return 2
    with tempfile.TemporaryDirectory(prefix="terrio-hecras-") as tmp:
        oracle = os.path.join(tmp, "oracle.json")
        hecras_oracle.main(corpus, oracle, files)
        env = dict(os.environ, TERRIO_HECRAS_TEST_DATA=corpus, TERRIO_HECRAS_ORACLE=oracle)
        cmd = [args.cargo, "test", "--release", "-p", "terrio", "--features", "hecras", "--test", "hecras_corpus", "--", "--nocapture"]
        p = subprocess.run(cmd, cwd=ROOT, env=env, capture_output=True, text=True, check=False)
    out = p.stdout + p.stderr
    results = []
    for status, line in re.findall(r"^(OK|DIFF) (.*)$", out, re.M):
        # A file name can contain ": ": the longest listed name the line starts with is the file.
        file = max((f for f in files if line == f or line.startswith(f + ": ")), key=len, default=line)
        results.append((status, file, line[len(file) + 2 :] if line.startswith(file + ": ") else ""))
    for status, file, detail in results:
        print(f"{status} {file}" + (f": {detail}" if detail else ""))
    summary = re.search(r"^checked .*$", out, re.M)
    if summary:
        print(summary[0])
    reported = {file for _, file, _ in results}
    missing = [f for f in files if f not in reported]
    for f in missing:
        print(f"DIFF {f}: not checked ({'the test failed to run' if p.returncode else 'not reported'})")
    if p.returncode and not results:
        print(out[-4000:], file=sys.stderr)
        return 2
    return 1 if missing or any(s == "DIFF" for s, _, _ in results) or p.returncode else 0


if __name__ == "__main__":
    sys.exit(main())
