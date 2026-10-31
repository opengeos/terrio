"""Generate the GRIB2 code tables terrio names GRIB bands with, as GDAL's GRIB driver names them.

Writes ``crates/terrio/data/grib2.tsv.zst``, a zstd-compressed text table built from GDAL's GRIB2
CSV files (``grib2_*.csv`` in ``GDAL_DATA``: degrib's tables merged with the WMO's), with lines:

- ``P<TAB>discipline<TAB>category<TAB>first<TAB>last<TAB>short_name<TAB>name<TAB>unit<TAB>unit_conv``:
  parameters ``first`` to ``last`` of a category of code table 4.2 (``grib2_table_4_2_D_C.csv``),
  runs of identical rows merged; rows "Reserved for local use" are left out, as GDAL then looks
  in the center's local table;
- ``I<TAB>center<TAB>subcenter<TAB>table``: the local table of a center (any subcenter when
  empty), in GDAL's order (``grib2_table_4_2_local_index.csv``);
- ``L<TAB>table<TAB>discipline<TAB>category<TAB>number<TAB>short_name<TAB>name<TAB>unit<TAB>unit_conv``:
  the rows of a local table;
- ``V<TAB>code<TAB>short_name<TAB>name<TAB>unit``: code table 4.5, fixed surfaces;
- ``C<TAB>code<TAB>name`` and ``S<TAB>center<TAB>subcenter<TAB>name``: originating centers and
  subcenters.

Usage: python scripts/gen_grib_tables.py [GDAL_DATA directory]

The directory defaults to rasterio's bundled ``gdal_data``. Needs the ``zstd`` command.
"""

import csv
import glob
import os
import re
import subprocess
import sys

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio", "data", "grib2.tsv.zst")


def gdal_data() -> tuple[str, str]:
    """The `GDAL_DATA` directory and a name for its GDAL."""
    if len(sys.argv) > 1:
        return sys.argv[1], sys.argv[1]
    import rasterio

    return os.path.join(os.path.dirname(rasterio.__file__), "gdal_data"), f"GDAL {rasterio.__gdal_version__}"


def rows(path: str) -> list[list[str]]:
    with open(path, newline="", encoding="utf-8") as f:
        out = list(csv.reader(f))
    # The header, then GDAL's comment rows (negative codes).
    return [r for r in out[1:] if r and not r[0].startswith("-")]


def clean(v: str) -> str:
    return v.replace("\t", " ").replace("\n", " ")


def main() -> None:
    d, name = gdal_data()
    lines = [f"# GRIB2 code tables of {name}: see scripts/gen_grib_tables.py"]
    for path in sorted(glob.glob(os.path.join(d, "grib2_table_4_2_*_*.csv"))):
        m = re.match(r"grib2_table_4_2_(\d+)_(\d+)\.csv$", os.path.basename(path))
        if not m:
            continue
        disc, cat = m.groups()
        run = None
        for r in rows(path):
            num = int(r[0])
            key = tuple(clean(v) for v in r[1:5])
            if key[1] == "Reserved for local use":
                if run:
                    lines.append("\t".join(["P", disc, cat, str(run[0]), str(run[1]), *run[2]]))
                run = None
                continue
            if run and run[2] == key and run[1] + 1 == num:
                run[1] = num
                continue
            if run:
                lines.append("\t".join(["P", disc, cat, str(run[0]), str(run[1]), *run[2]]))
            run = [num, num, key]
        if run:
            lines.append("\t".join(["P", disc, cat, str(run[0]), str(run[1]), *run[2]]))
    tables = []
    for r in rows(os.path.join(d, "grib2_table_4_2_local_index.csv")):
        table = r[2].removeprefix("grib2_table_4_2_local_").removesuffix(".csv")
        lines.append("\t".join(["I", r[0], r[1], table]))
        if table not in tables:
            tables.append(table)
    for table in tables:
        for r in rows(os.path.join(d, f"grib2_table_4_2_local_{table}.csv")):
            lines.append("\t".join(["L", table, *(clean(v) for v in r[:7])]))
    for r in rows(os.path.join(d, "grib2_table_4_5.csv")):
        lines.append("\t".join(["V", *(clean(v) for v in r[:4])]))
    for r in rows(os.path.join(d, "grib2_center.csv")):
        lines.append("\t".join(["C", r[0], clean(r[1])]))
    for r in rows(os.path.join(d, "grib2_subcenter.csv")):
        lines.append("\t".join(["S", r[0], r[1], clean(r[2])]))
    text = "\n".join(lines) + "\n"
    data = subprocess.run(["zstd", "-19", "-q", "-c"], input=text.encode(), capture_output=True, check=True).stdout
    with open(OUT, "wb") as f:
        f.write(data)
    print(f"{len(lines)} lines, {len(text)} bytes, {len(data)} compressed -> {OUT}")


if __name__ == "__main__":
    main()
