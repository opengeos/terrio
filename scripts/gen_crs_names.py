# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate the CRS names PROJ looks geographic 2D and projected CRSs up by.

Writes ``crates/terrio-crs/data/crs_names.tsv.zst``, a zstd-compressed text table with lines
``kind<TAB>name<TAB>code``. Each line gives the CRS that PROJ 9.9.0's ``getResolvedCRS`` and
``buildCRSIds`` find for a CRS without ID, by its name: the EPSG ``code``, or 0 when PROJ finds no
single CRS (several, or one of another authority). The kinds are:

- ``g``: a geographic 2D CRS. ``createObjectsFromName`` gets the name, case-insensitively, among
  the names and aliases of every authority's geographic 2D CRSs, keeps its first two rows, and
  needs them to be a single CRS. The name has its ASCII letters in lowercase.
- ``G``: the name of an ESRI WKT 1 ``GEOGCS`` starting with ``GCS_``, exactly as written. PROJ's WKT
  reader first replaces it by the official name of the CRS it is an ESRI alias of
  (``getOfficialNameFromAlias``), then looks that name up as for ``g``.
- ``p`` and ``P``: the same for projected CRSs (``P``: the name of a ``PROJCS`` of an ESRI-style
  WKT 1).

A ``g`` or ``p`` name absent from the table stands for the EPSG CRS whose official name it is
(whatever the case of its ASCII letters) when PROJ finds that CRS alone, and for none otherwise.
The table lists only the names that are not, together with the ESRI names (``G``, ``P``).

Usage: python scripts/gen_crs_names.py

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
    os.path.dirname(__file__), "..", "crates", "terrio-crs", "data", "crs_names.tsv.zst"
)

GEOG_2D = "geographic 2D"

# SQLite's NOCASE folds ASCII letters only.
FOLD = str.maketrans(string.ascii_uppercase, string.ascii_lowercase)


def tables(kind):
    """The table and type constraint of a kind of CRS.

    Args:
        kind: ``"geographic"`` or ``"projected"``.

    Returns:
        ``(table, type)``, with ``type`` None for projected CRSs.
    """
    return (
        ("geodetic_crs", GEOG_2D) if kind == "geographic" else ("projected_crs", None)
    )


def equivalent_name(a, b):
    """Whether PROJ's ``Identifier::isEquivalentName(a, b, false)`` holds, near enough.

    Args:
        a: A name.
        b: Another.

    Returns:
        Whether they are equal without spaces, ``_-/().&,``, case and ``19`` before two digits.
    """

    def canonical(n):
        n = re.sub(r"[ _\-/().&,]", "", n.translate(FOLD))
        return re.sub(r"(?<![0-9])19([0-9]{2})", r"\1", n)

    return canonical(a) == canonical(b)


def lookup(db, name, kind):
    """The CRS PROJ's ``createObjectsFromName(name, {kind}, false, 2)`` finds alone.

    PROJ's query keeps its first two rows (``LIMIT 2``, in SQL). Of those, PROJ then drops the
    repeats of a CRS and the deprecated CRSs after current ones, and keeps the ones whose
    official name is the searched one, if any.

    Args:
        db: A connection to PROJ's database.
        name: The searched name.
        kind: ``"geographic"`` or ``"projected"``.

    Returns:
        ``(auth_name, code)``, or None when PROJ finds no single CRS.
    """
    table, typ = tables(kind)
    own_type = f"AND type = '{typ}' " if typ else ""
    alias_type = f"AND ov.type = '{typ}' " if typ else ""
    sql = (
        "SELECT table_name, auth_name, code, name, deprecated, is_alias FROM ("
        f"SELECT '{table}' AS table_name, auth_name, code, name, deprecated, 0 AS is_alias "
        f"FROM {table} WHERE 1 = 1 {own_type}AND name = ? COLLATE NOCASE "
        f"UNION SELECT '{table}' AS table_name, ov.auth_name AS auth_name, ov.code AS code, "
        "a.alt_name AS name, ov.deprecated AS deprecated, 1 as is_alias "
        f"FROM {table} ov JOIN alias_name a ON ov.auth_name = a.auth_name AND ov.code = a.code "
        f"WHERE a.source != 'EPSG_OLD' AND a.table_name = '{table}' {alias_type}"
        "AND a.alt_name = ? COLLATE NOCASE"
        ") ORDER BY deprecated, is_alias, length(name), name LIMIT 2"
    )
    found = []
    first_deprecated = None
    for _, auth, code, _, deprecated, _ in db.execute(sql, (name, name)):
        if any((auth, code) == (f[0], f[1]) for f in found):
            continue
        if first_deprecated is None:
            first_deprecated = deprecated == 1
        if deprecated == 1 and found and not first_deprecated:
            break
        official = db.execute(
            f"SELECT name FROM {table} WHERE auth_name = ? AND code = ?", (auth, code)
        ).fetchone()[0]
        found.append((auth, code, official))
    exact = [f for f in found if equivalent_name(f[2], name)]
    if exact:
        found = exact
    return (found[0][0], found[0][1]) if len(found) == 1 else None


def official_from_esri_alias(db, name, table):
    """The name PROJ's ESRI WKT reader gives a CRS of an ESRI name (``getOfficialNameFromAlias``).

    Args:
        db: A connection to PROJ's database.
        name: The name in the WKT.
        table: ``"geodetic_crs"`` or ``"projected_crs"``.

    Returns:
        The official name of the CRS it is an ESRI alias of, else None.
    """
    rows = db.execute(
        "SELECT auth_name, code FROM alias_name WHERE alt_name = ? AND table_name = ? "
        "AND source IN ('ESRI', 'ESRI_OLD')",
        (name, table),
    ).fetchall()
    best = None
    for auth, code in rows:
        r = db.execute(
            f"SELECT name, deprecated FROM {table} WHERE auth_name = ? AND code = ?",
            (auth, code),
        ).fetchone()
        if r and (best is None or r[1] < best[1]):
            best = r
    return best[0] if best else None


def epsg_code(found):
    """The EPSG code of a lookup's result, 0 for none or another authority's CRS."""
    return int(found[1]) if found and found[0] == "EPSG" else 0


def rows():
    """The table's rows.

    Yields:
        Tuples ``(kind, name, code)``.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    for kind, lower, upper in (("geographic", "g", "G"), ("projected", "p", "P")):
        table, typ = tables(kind)
        own_type = f"AND type = '{typ}'" if typ else ""
        alias_type = f"AND ov.type = '{typ}'" if typ else ""
        official = {}
        for auth, code, name in db.execute(
            f"SELECT auth_name, code, name FROM {table} WHERE 1 = 1 {own_type}"
        ):
            if auth == "EPSG":
                official.setdefault(name.translate(FOLD), set()).add(int(code))
        names = {
            n for (n,) in db.execute(f"SELECT name FROM {table} WHERE 1 = 1 {own_type}")
        }
        aliases = db.execute(
            f"SELECT a.alt_name, a.source FROM {table} ov JOIN alias_name a "
            "ON ov.auth_name = a.auth_name "
            f"AND ov.code = a.code WHERE a.table_name = '{table}' {alias_type}"
        ).fetchall()
        names |= {n for n, source in aliases if source != "EPSG_OLD"}
        out = {}
        for name in names:
            key = name.translate(FOLD)
            if key in out:
                continue
            code = epsg_code(lookup(db, name, kind))
            fallback = (
                next(iter(official[key])) if len(official.get(key, ())) == 1 else 0
            )
            # Absent names stand for the EPSG CRS of that official name, else for none.
            if code != fallback or (code == 0 and key in official):
                out[key] = code
        for key in sorted(out):
            yield lower, key, out[key]
        # ESRI names, as written: the official name of the CRS they stand for, looked up.
        esri = sorted({n for n, source in aliases if source in ("ESRI", "ESRI_OLD")})
        for name in esri:
            if kind == "geographic" and not name.startswith("GCS_"):
                continue
            if name == "GCS_WGS_1984":
                mapped = "WGS 84"
            else:
                mapped = official_from_esri_alias(db, name, table) or name
            yield upper, name, epsg_code(lookup(db, mapped, kind))


def main():
    """Write the table."""
    lines = [
        f"# CRS names PROJ {pyproj.proj_version_str} looks CRSs without ID up by. Generated by scripts/gen_crs_names.py."
    ]
    for kind, name, code in rows():
        assert "\t" not in name and "\n" not in name, name
        lines.append(f"{kind}\t{name}\t{code}")
    text = ("\n".join(lines) + "\n").encode()
    subprocess.run(["zstd", "-19", "-q", "-f", "-o", OUT], input=text, check=True)
    print(f"{len(lines) - 1} names, {len(text)} bytes")


if __name__ == "__main__":
    main()
