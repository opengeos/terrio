"""Writes ``grib1.tsv.zst`` (next to this script): GDAL's GRIB1 parameter tables (code table 2,
per center and table version) and level types (code table 3), as degrib's ``grib1tab.cpp``
declares them. terrio's GRIB driver names GRIB1 bands from them as GDAL does.

    python gen_grib1_tables.py <gdal>/frmts/grib/degrib/degrib/grib1tab.cpp

Lines (tab-separated): ``P table index name comment unit conversion`` and
``S index name comment unit two_part``. Needs the ``zstd`` command. Generated from GDAL 3.12.1.
"""

import os
import re
import subprocess
import sys

TOKEN = re.compile(r'"((?:[^"\\]|\\.)*)"|([A-Za-z_][A-Za-z_0-9]*)|(-?\d+)|([{}\[\];,=])')


def tokens(text):
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    text = re.sub(r"//[^\n]*", " ", text)
    out = []
    for m in TOKEN.finditer(text):
        s, ident, num, punct = m.groups()
        if s is not None:
            value = bytes(s, "latin-1").decode("unicode_escape")
            # Adjacent literals are one string.
            if out and out[-1][0] == "str":
                out[-1] = ("str", out[-1][1] + value)
            else:
                out.append(("str", value))
        elif ident is not None:
            out.append(("id", ident))
        elif num is not None:
            out.append(("num", num))
        else:
            out.append(("p", punct))
    return out


def tables(toks):
    """(type, name, entries) for each `const <type> <name>[256] = { {...}, ... };`."""
    i = 0
    while i < len(toks):
        if toks[i] == ("id", "const") and toks[i + 1][1] in ("GRIB1ParmTable", "GRIB1SurfTable"):
            kind, name = toks[i + 1][1], toks[i + 2][1]
            j = i + 3
            while toks[j] != ("p", "="):
                j += 1
            j += 2  # '=' '{'
            entries = []
            while toks[j] != ("p", "}"):
                assert toks[j] == ("p", "{"), (name, toks[j])
                k = j + 1
                fields = []
                while toks[k] != ("p", "}"):
                    if toks[k][0] != "p":
                        fields.append(toks[k][1])
                    k += 1
                entries.append(fields)
                j = k + 1
                if toks[j] == ("p", ","):
                    j += 1
            yield kind, name, entries
            i = j
        i += 1


def main():
    text = open(sys.argv[1], encoding="latin-1").read()
    lines = []
    for kind, name, entries in tables(tokens(text)):
        assert len(entries) == 256, (name, len(entries))
        for i, f in enumerate(entries):
            assert len(f) == 4, (name, i, f)
            assert not any("\t" in x or "\n" in x for x in f)
            if kind == "GRIB1ParmTable":
                lines.append("\t".join(["P", name.removeprefix("parm_table_"), str(i), f[0], f[1], f[2], f[3]]))
            else:
                lines.append("\t".join(["S", str(i), f[0], f[1], f[2], f[3]]))
    text = "\n".join(lines) + "\n"
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "grib1.tsv.zst")
    data = subprocess.run(["zstd", "-19", "-q", "-c"], input=text.encode(), capture_output=True, check=True).stdout
    with open(out, "wb") as f:
        f.write(data)
    print(f"{len(lines)} lines, {len(text)} bytes, {len(data)} compressed: {out}")


if __name__ == "__main__":
    main()
