"""Generate terrio's GRIB tables from GDAL's GRIB driver.

Writes ``crates/terrio/data/grib_tables.txt.zst``, a zstd-compressed text file holding the tables
GDAL's GRIB driver (its copy of NOAA's degrib) names GRIB messages with:

- the GRIB2 CSV tables of ``frmts/grib/data`` (parameters by discipline and category, the NCEP,
  NDFD, MRMS, Canadian and HPC local parameters, fixed surfaces, centers and subcenters), each
  as a line ``@<file name>`` followed by the file as it is;
- the GRIB1 parameter tables of ``frmts/grib/degrib/degrib/grib1tab.cpp`` (NCEP, ECMWF, NOHRSC,
  CPTEC, DWD, ...), each as a line ``@grib1:<table>`` followed by ``code<TAB>name<TAB>comment
  <TAB>unit<TAB>conversion`` lines, and its table of level types (``@grib1:surface``, lines
  ``code<TAB>name<TAB>comment<TAB>unit<TAB>two-part flag``).

The tables are the WMO's and NOAA's (public domain), as GDAL distributes them (MIT license), so
that terrio names GRIB bands as GDAL does (``GRIB_ELEMENT``, ``GRIB_COMMENT``, ``GRIB_UNIT``,
``GRIB_SHORT_NAME``, ``GRIB_IDS``).

Usage: python scripts/gen_grib_tables.py [GDAL_SOURCE_DIR]

Reads the files from a GDAL source tree when one is given, otherwise from GitHub at the GDAL
version below. Needs the ``zstd`` command.
"""

import os
import re
import subprocess
import sys
import urllib.request

GDAL_VERSION = "v3.12.2"
RAW = f"https://raw.githubusercontent.com/OSGeo/gdal/{GDAL_VERSION}"
OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio", "data", "grib_tables.txt.zst")

# The CSV files of frmts/grib/data that degrib reads (grib2_process.csv is unused by GDAL).
CSV_FILES = (
    ["grib2_center.csv", "grib2_subcenter.csv", "grib2_table_4_5.csv", "grib2_table_4_2_local_index.csv"]
    + [f"grib2_table_4_2_local_{c}.csv" for c in ("Canada", "HPC", "MRMS", "NCEP", "NDFD")]
    + [
        f"grib2_table_4_2_{d}_{c}.csv"
        for d, cats in (
            (0, (0, 1, 2, 3, 4, 5, 6, 7, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 190, 191)),
            (1, (0, 1, 2)),
            (2, (0, 3, 4, 5, 6, 7)),
            (3, (0, 1, 2, 3, 4, 5, 6)),
            (4, (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10)),
            (10, (0, 1, 2, 3, 4, 191)),
            (20, (0, 1, 2, 3)),
        )
        for c in cats
    ]
)


def read(path, source):
    """Reads a file of GDAL's source tree.

    Args:
        path: The path in the source tree.
        source: The local source tree, or None to read from GitHub.

    Returns:
        The file's text.
    """
    if source:
        with open(os.path.join(source, path), encoding="utf-8") as f:
            return f.read()
    with urllib.request.urlopen(f"{RAW}/{path}") as r:  # noqa: S310 (a fixed https URL)
        return r.read().decode("utf-8")


def c_string(s):
    """Decodes the escapes of a C string literal's contents.

    Args:
        s: The text between the quotes.

    Returns:
        The string.
    """
    return s.replace('\\"', '"').replace("\\\\", "\\")


def grib1_tables(text):
    """Parses the tables of grib1tab.cpp.

    Args:
        text: The file's text.

    Returns:
        A list of ``(name, lines)`` pairs: the parameter tables (``code, name, comment, unit,
        conversion``) and the level table (``surface``: ``code, name, comment, unit, two-part flag``).
    """
    out = []
    string = r'"((?:[^"\\]|\\.)*)"'
    for m in re.finditer(r"const GRIB1(ParmTable|SurfTable) (\w+)\[256\] = \{(.*?)\n\};", text, re.S):
        kind, name, body = m.groups()
        # Comments are ignored: entries are numbered by position, from 0.
        body = re.sub(r"/\*.*?\*/", "", body, flags=re.S)
        # Adjacent string literals ("a" "b") are one string.
        body = re.sub(r'"\s+"', "", body)
        lines = []
        if kind == "ParmTable":
            entries = re.findall(r"\{\s*" + string + r"\s*,\s*" + string + r"\s*,\s*" + string + r"\s*,\s*(\w+)\s*\}", body)
            for code, (n, comment, unit, conv) in enumerate(entries):
                lines.append(f"{code}\t{c_string(n)}\t{c_string(comment)}\t{c_string(unit)}\t{conv}")
            out.append((name, lines, len(entries)))
        else:
            entries = re.findall(r"\{\s*" + string + r"\s*,\s*" + string + r"\s*,\s*" + string + r"\s*,\s*(\d+)\s*\}", body)
            for code, (n, comment, unit, two_part) in enumerate(entries):
                lines.append(f"{code}\t{c_string(n)}\t{c_string(comment)}\t{c_string(unit)}\t{two_part}")
            out.append(("surface" if name == "GRIB1Surface" else name, lines, len(entries)))
    return out


def main():
    """Writes the tables."""
    source = sys.argv[1] if len(sys.argv) > 1 else None
    parts = []
    for name in CSV_FILES:
        text = read(f"frmts/grib/data/{name}", source)
        parts.append(f"@{name}\n{text.rstrip(chr(10))}\n")
    for name, lines, count in grib1_tables(read("frmts/grib/degrib/degrib/grib1tab.cpp", source)):
        if count != 256:
            raise SystemExit(f"grib1tab.cpp: {name} has {count} entries, not 256")
        parts.append(f"@grib1:{name}\n" + "\n".join(lines) + "\n")
    data = "".join(parts).encode("utf-8")
    out = subprocess.run(["zstd", "-19", "-q", "-c"], input=data, capture_output=True, check=True).stdout
    with open(OUT, "wb") as f:
        f.write(out)
    print(f"{OUT}: {len(data)} bytes, {len(out)} compressed")


if __name__ == "__main__":
    main()
