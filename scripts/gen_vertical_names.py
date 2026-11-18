# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate the names PROJ identifies vertical CRSs without ID by.

Writes ``crates/terrio-crs/data/vertical_names.tsv.zst``, a zstd-compressed text table with lines
``kind<TAB>name<TAB>code``, from PROJ 9.9.0's database. PROJ identifies a vertical CRS without ID
(``VerticalCRS::identify``) among the CRSs its name finds (``createObjectsFromName``: the names and
aliases of the database's vertical CRSs, whatever the case of their ASCII letters), keeping the one
on the same datum (by name) with the same unit and direction. Its ESRI WKT reader first replaces
the name of a ``VERTCS`` and of its ``VDATUM`` by the official name they are an ESRI alias of
(``getOfficialNameFromAlias``), and an ESRI name of no alias is read with spaces for its
underscores. The kinds are:

- ``c``: a name or alias of the EPSG vertical CRS ``code``, its ASCII letters in lowercase.
- ``n``: a name or alias of the EPSG vertical CRS ``code`` as PROJ compares names (see ``d``),
  which identifies a CRS no name finds (ESRI's ``GLLAT_2025_depth``, ``GLLAT(2025) depth``).
- ``e``: an ESRI alias of a vertical CRS, as written, and the EPSG vertical CRS of the official
  name it stands for, as ``c`` finds it.
- ``d``: a name or alias of an EPSG vertical datum (or datum ensemble), as PROJ compares names
  (``Identifier::isEquivalentName``: without spaces, ``_-/().&,``, case and ``19`` before two
  digits), and the EPSG vertical CRS ``code`` on it.

A name can stand for several codes, on as many lines.

Usage: python scripts/gen_vertical_names.py

Needs pyproj 3.8.0 (PROJ 9.9.0) and the ``zstd`` command.
"""

import os
import re
import sqlite3
import string
import subprocess

import pyproj

assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str

OUT = os.path.join(
    os.path.dirname(__file__),
    "..",
    "crates",
    "terrio-crs",
    "data",
    "vertical_names.tsv.zst",
)

# SQLite's NOCASE folds ASCII letters only.
FOLD = str.maketrans(string.ascii_uppercase, string.ascii_lowercase)


def canonical(name):
    """A name as PROJ's ``Identifier::isEquivalentName`` compares it, near enough."""
    n = re.sub(r"[ _\-/().&,]", "", name.translate(FOLD))
    return re.sub(r"(?<![0-9])19([0-9]{2})", r"\1", n)


def rows():
    """The table's rows.

    Yields:
        Tuples ``(kind, name, code)``.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    crs = {}
    for code, name in db.execute(
        "SELECT code, name FROM vertical_crs WHERE auth_name = 'EPSG'"
    ):
        crs.setdefault(name.translate(FOLD), set()).add(int(code))
    aliases = db.execute(
        "SELECT code, alt_name, source FROM alias_name WHERE table_name = 'vertical_crs' "
        "AND auth_name = 'EPSG' AND source != 'EPSG_OLD'"
    ).fetchall()
    official = dict(
        db.execute("SELECT code, name FROM vertical_crs WHERE auth_name = 'EPSG'")
    )
    for code, alt, _ in aliases:
        crs.setdefault(alt.translate(FOLD), set()).add(int(code))
    for name in sorted(crs):
        for code in sorted(crs[name]):
            yield "c", name, code
    equivalent = {}
    for name, codes in crs.items():
        equivalent.setdefault(canonical(name), set()).update(codes)
    for name in sorted(equivalent):
        for code in sorted(equivalent[name]):
            yield "n", name, code
    # The official name an ESRI alias stands for: that of a current CRS before a deprecated one
    # (`getOfficialNameFromAlias`).
    deprecated = dict(
        db.execute("SELECT code, deprecated FROM vertical_crs WHERE auth_name = 'EPSG'")
    )
    esri = {}
    for code, alt, source in aliases:
        if source in ("ESRI", "ESRI_OLD"):
            best = esri.get(alt)
            if best is None or deprecated[code] < deprecated[best]:
                esri[alt] = code
    for name in sorted(esri):
        for code in sorted(crs.get(official[esri[name]].translate(FOLD), set())):
            yield "e", name, code
    # The vertical CRSs on each datum (or datum ensemble), by the datum's names.
    on = {}
    for code, datum in db.execute(
        "SELECT code, datum_code FROM vertical_crs WHERE auth_name = 'EPSG' "
        "AND datum_auth_name = 'EPSG'"
    ):
        on.setdefault(datum, set()).add(int(code))
    datums = {}
    for code, name in db.execute(
        "SELECT code, name FROM vertical_datum WHERE auth_name = 'EPSG'"
    ):
        datums.setdefault(canonical(name), set()).update(on.get(code, ()))
    for code, alt in db.execute(
        "SELECT code, alt_name FROM alias_name WHERE table_name = 'vertical_datum' "
        "AND auth_name = 'EPSG'"
    ):
        datums.setdefault(canonical(alt), set()).update(on.get(code, ()))
    for name in sorted(datums):
        for code in sorted(datums[name]):
            yield "d", name, code


def main():
    """Write the table."""
    lines = [
        f"# Vertical CRS and datum names of PROJ {pyproj.proj_version_str}. "
        "Generated by scripts/gen_vertical_names.py."
    ]
    for kind, name, code in rows():
        assert "\t" not in name and "\n" not in name, name
        lines.append(f"{kind}\t{name}\t{code}")
    text = ("\n".join(lines) + "\n").encode()
    subprocess.run(["zstd", "-19", "-q", "-f", "-o", OUT], input=text, check=True)
    print(f"{len(lines) - 1} names, {len(text)} bytes")


if __name__ == "__main__":
    main()
