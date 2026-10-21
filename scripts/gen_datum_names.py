"""Generate the names of EPSG geodetic datums terrio identifies WKT datums by.

Writes ``crates/terrio/data/datums.tsv.zst``, a zstd-compressed text table with lines
``code<TAB>rank<TAB>name``: a name of the EPSG datum ``code``, from its official name and the
name GDAL writes in WKT 1 (rank 0), its EPSG aliases (1), PROJ's (2), ESRI's (3: ``D_ETRS_1989``
for ETRS89) and the former EPSG and ESRI ones (4), from PROJ's database (``alias_name``), and the
datum name ESRI writes for CRSs on another datum (5: ``D_ETRS_1989`` for ETRS89-NOR), which terrio
looks on only when no CRS on the datums of ranks 0 to 4 matches. A WKT
datum named as several datums is taken for those of the best rank. Names equal after terrio's
normalization (letters, digits and ``+`` only, case-insensitive, without ESRI's ``D_`` prefix) are
listed once, with their best rank.

Usage: python scripts/gen_datum_names.py

Needs pyproj and the ``zstd`` command.
"""

import os
import re
import sqlite3
import subprocess

import pyproj

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio", "data", "datums.tsv.zst")

# The rank of each source of aliases.
RANKS = {"EPSG": 1, "PROJ": 2, "ESRI": 3, "EPSG_OLD": 4, "ESRI_OLD": 4}


def normalize(name):
    """Normalize a datum name as terrio does.

    Args:
        name: The datum name.

    Returns:
        Its lowercase ASCII letters, digits and ``+``, without a leading ``D_``.
    """
    if name.startswith("D_"):
        name = name[2:]
    return re.sub(r"[^a-z0-9+]", "", name.lower())


def wkt1_names(db):
    """The datum names GDAL writes in WKT 1, by EPSG datum code.

    Args:
        db: A connection to PROJ's database.

    Returns:
        A dict from datum code to the name.
    """
    names = {}
    query = (
        "SELECT code, datum_code FROM geodetic_crs WHERE auth_name = 'EPSG' "
        "AND datum_auth_name = 'EPSG' AND type = 'geographic 2D' ORDER BY deprecated, code"
    )
    for crs, datum in db.execute(query):
        if datum in names:
            continue
        try:
            wkt = pyproj.CRS.from_epsg(int(crs)).to_wkt("WKT1_GDAL")
        except pyproj.exceptions.CRSError:
            continue
        m = re.search(r'DATUM\["([^"]+)"', wkt or "")
        if m:
            names[datum] = m.group(1)
    return names


def rows():
    """The table's rows.

    Yields:
        Tuples ``(code, rank, name)``.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    names = []
    for code, name in db.execute("SELECT code, name FROM geodetic_datum WHERE auth_name = 'EPSG'"):
        names.append((int(code), 0, name))
    for code, name in wkt1_names(db).items():
        names.append((int(code), 0, name))
    query = (
        "SELECT code, alt_name, source FROM alias_name WHERE table_name = 'geodetic_datum' "
        "AND auth_name = 'EPSG'"
    )
    for code, name, source in db.execute(query):
        names.append((int(code), RANKS.get(source, 4), name))
    best = {}
    for code, rank, name in names:
        key = (code, normalize(name))
        if key[1] and (key not in best or rank < best[key][0]):
            best[key] = (rank, name)
    names.extend(esri_written(db, best))
    for code, rank, name in names:
        key = (code, normalize(name))
        if key[1] and (key not in best or rank < best[key][0]):
            best[key] = (rank, name)
    for (code, _), (rank, name) in sorted(best.items(), key=lambda kv: (kv[0][0], kv[1][0], kv[1][1])):
        yield code, rank, name


def esri_written(db, best):
    """The datum names ESRI writes for CRSs on another datum (rank 5).

    ESRI writes ``D_ETRS_1989`` in the WKT of ETRS89-NOR [EUREF89] / NTM zone 18 (EPSG:5118),
    whose datum is ETRS89-NOR, and GDAL identifies that WKT by the CRS's ESRI name. terrio looks
    on these datums when none on the datum the name stands for matches.

    Args:
        db: A connection to PROJ's database.
        best: The names of ranks 0 to 4, as ``{(code, normalized name): (rank, name)}``.

    Yields:
        Tuples ``(code, 5, name)``.
    """
    by_name = {}
    for (code, key), (rank, _) in best.items():
        r, codes = by_name.get(key, (99, set()))
        if rank < r:
            by_name[key] = (rank, {code})
        elif rank == r:
            codes.add(code)
    query = (
        "SELECT p.code, g.datum_code FROM projected_crs p JOIN geodetic_crs g "
        "ON g.auth_name = p.geodetic_crs_auth_name AND g.code = p.geodetic_crs_code "
        "WHERE p.auth_name = 'EPSG' AND p.deprecated = 0 AND g.datum_auth_name = 'EPSG' "
        "UNION SELECT code, datum_code FROM geodetic_crs WHERE auth_name = 'EPSG' "
        "AND deprecated = 0 AND type = 'geographic 2D' AND datum_auth_name = 'EPSG'"
    )
    for crs, datum in db.execute(query).fetchall():
        try:
            wkt = pyproj.CRS.from_epsg(int(crs)).to_wkt("WKT1_ESRI")
        except pyproj.exceptions.CRSError:
            continue
        m = re.search(r'DATUM\["([^"]+)"', wkt or "")
        if m and int(datum) not in by_name.get(normalize(m.group(1)), (0, set()))[1]:
            yield int(datum), 5, m.group(1)


def main():
    """Write the compressed table."""
    lines = ["\t".join(map(str, r)) + "\n" for r in rows()]
    header = f"# EPSG datum names and aliases, via PROJ {pyproj.proj_version_str}. Generated by scripts/gen_datum_names.py.\n"
    data = (header + "".join(lines)).encode()
    out = subprocess.run(["zstd", "-19", "-q", "-c"], input=data, capture_output=True, check=True).stdout
    with open(OUT, "wb") as f:
        f.write(out)
    print(f"{len(lines)} names, {len(data)} bytes, {len(out)} compressed")


if __name__ == "__main__":
    main()
