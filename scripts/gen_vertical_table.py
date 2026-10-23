"""Generate crates/terrio/data/epsg_vertical.tsv.zst, the transformations between EPSG vertical CRSs.

The table lists, for each ordered pair of EPSG vertical CRSs that the EPSG dataset connects
directly or through one intermediate vertical CRS, the transformations PROJ lists between them
(``projinfo`` with the options GDAL passes to PROJ, as ``scripts/gen_epsg_table.py`` runs it):
vertical offsets, offsets and slopes (``vertoffset``) and grids such as VERTCON (``vgridshift``),
chained through the intermediate as PROJ chains them. Ballpark transformations are left out: terrio
keeps heights when none applies, as PROJ's ballpark does.

Lines are tab-separated, after a ``#`` header:

- ``O<TAB>id<TAB>bounds<TAB>accuracy<TAB>flags<TAB>steps<TAB>name<TAB>chain accuracy<TAB>chain
  bounds<TAB>interpolation CRS``: a transformation, as in ``epsg.tsv.zst``, its steps applied to
  heights (or depths) in the source CRS's axis direction and in metres (``axisswap
  order=1,2,-3`` turns heights into depths), and the EPSG code of the geographic CRS it
  interpolates in (PROJ chains the horizontal transformation to it into compound CRSs on other
  datums), or nothing;
- ``W<TAB>source<TAB>target<TAB>ops<TAB>grid ops``: the transformations from a vertical CRS to
  another, without grids and with every grid of the PROJ CDN available, in PROJ's order.

Usage: python scripts/gen_vertical_table.py

Needs pyproj, GDAL's Python bindings, ``projinfo`` and the ``zstd`` command, with the PROJ and GDAL
versions of ``epsg.tsv.zst``.
"""

import os
import sqlite3
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

import pyproj

sys.path.insert(0, os.path.dirname(__file__))
import gen_epsg_table as epsg  # noqa: E402

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio", "data", "epsg_vertical.tsv.zst")


def vertical_pairs():
    """List the ordered pairs of EPSG vertical CRSs connected directly or through one other.

    Returns:
        Sorted ``(source code, target code)`` pairs.
    """
    db = sqlite3.connect(os.path.join(pyproj.datadir.get_data_dir(), "proj.db"))
    query = """
        SELECT t.sc, t.tc FROM (
            SELECT source_crs_auth_name sa, source_crs_code sc, target_crs_auth_name ta, target_crs_code tc
                FROM grid_transformation WHERE deprecated = 0
            UNION ALL SELECT source_crs_auth_name, source_crs_code, target_crs_auth_name, target_crs_code
                FROM other_transformation WHERE deprecated = 0
            UNION ALL SELECT source_crs_auth_name, source_crs_code, target_crs_auth_name, target_crs_code
                FROM concatenated_operation WHERE deprecated = 0) t
        JOIN vertical_crs a ON a.auth_name = t.sa AND a.code = t.sc
        JOIN vertical_crs b ON b.auth_name = t.ta AND b.code = t.tc
        WHERE t.sa = 'EPSG' AND t.ta = 'EPSG'
    """
    adjacent = {}
    for a, b in db.execute(query):
        a, b = int(a), int(b)
        adjacent.setdefault(a, set()).add(b)
        adjacent.setdefault(b, set()).add(a)
    pairs = set()
    for a, near in adjacent.items():
        for b in near:
            pairs.add((a, b))
            pairs.update((a, c) for c in adjacent[b] if c != a)
    return sorted(pairs)


def rows():
    """Yield the table's rows.

    Yields:
        ``O`` rows, each before the first ``W`` row using it, and ``W`` rows.
    """
    pairs = vertical_pairs()

    def ops(pair, grids):
        """List the non-ballpark transformations of a pair.

        Args:
            pair: The source and target codes.
            grids: Whether every grid of the PROJ CDN counts as available.

        Returns:
            ``(steps, bounds, accuracy, flags, name, chain accuracy, chain bounds)`` tuples.
        """
        found = epsg.transformations(f"EPSG:{pair[0]}", f"EPSG:{pair[1]}", 0.0, 0.0, grids, vertical=True)
        # PROJ names its vertical ballparks "Transformation from A to B (ballpark vertical
        # transformation)", which the main generator's test for "Ballpark" does not catch.
        return [t for t in found if "b" not in t[3] and "ballpark" not in t[4].lower()]

    with ThreadPoolExecutor(os.cpu_count()) as pool:
        plain = list(pool.map(lambda p: ops(p, False), pairs))
        with_grids = list(pool.map(lambda p: ops(p, True), pairs))
    ids = {}

    def id_of(t):
        """The id of a transformation, yielding its ``O`` row when new.

        Args:
            t: The transformation.

        Returns:
            ``(id, row or None)``.
        """
        if t in ids:
            return ids[t], None
        ids[t] = len(ids) + 1
        steps, bounds, accuracy, flags, name, chain_accuracy, chain_bounds, interpolation = t
        bounds = ",".join(repr(round(v, 6)) for v in bounds)
        return ids[t], ("O", ids[t], bounds, repr(accuracy), flags, steps, name, chain_accuracy, chain_bounds, interpolation)

    for (a, b), transfos, transfos_grids in zip(pairs, plain, with_grids):
        if not transfos and not transfos_grids:
            continue
        lists = []
        for list_ in (transfos, transfos_grids if transfos_grids != transfos else []):
            numbers = []
            for t in list_:
                n, row = id_of(t)
                if row is not None:
                    yield row
                numbers.append(n)
            lists.append(",".join(map(str, numbers)))
        yield ("W", a, b, *lists)


def main():
    """Write the compressed table."""
    lines = ["\t".join(map(str, r)) + "\n" for r in rows()]
    header = (
        f"# Transformations between EPSG vertical CRSs (https://epsg.org), via PROJ {pyproj.proj_version_str}. "
        "Generated by scripts/gen_vertical_table.py.\n"
    )
    data = (header + "".join(lines)).encode()
    out = subprocess.run(["zstd", "-19", "-q", "-c"], input=data, capture_output=True, check=True).stdout
    with open(OUT, "wb") as f:
        f.write(out)
    kinds = {}
    for line in lines:
        kinds[line[0]] = kinds.get(line[0], 0) + 1
    print(f"{kinds}, {len(data)} bytes, {len(out)} compressed")


if __name__ == "__main__":
    main()
