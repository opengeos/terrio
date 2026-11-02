#!/usr/bin/env python3
"""Compare terrio's XLSX driver with GDAL's, in both directions.

Reads the same workbooks with both drivers under each ``HEADERS`` and ``FIELD_TYPES`` option, reads
with terrio what ``ogr2ogr`` writes, and reads with GDAL what terrio writes. Field names, feature
counts and every value must match, but for the differences terrio makes on purpose: repeated
headings get unique names, and the cases in ``KNOWN_DIFFERENCES`` are reported as differences
without failing.

Usage:
    compare_xlsx_with_gdal.py --terrio PATH [--json REPORT] [WORKBOOK ...]
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import zipfile

MAIN = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
RELS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"

STYLES = (
    f'<styleSheet xmlns="{MAIN}"><numFmts count="3">'
    '<numFmt numFmtId="170" formatCode="yyyy-mm-dd hh:mm:ss"/>'
    '<numFmt numFmtId="171" formatCode="yyyy-mm-dd hh:mm:ss.000"/>'
    '<numFmt numFmtId="172" formatCode="#,##0.00"/></numFmts>'
    '<cellXfs count="7"><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="170"/><xf numFmtId="18"/>'
    '<xf numFmtId="171"/><xf numFmtId="172"/><xf numFmtId="22"/></cellXfs></styleSheet>'
)

# Differences from GDAL that are intended: sheet, case, the exact differences, and why. Anything else
# in these sheets is still unexpected.
BOOLEAN_WIDENING = ["row 0 flag: '1' != 'true'", "row 1 flag: '5' != 'true'", "row 2 flag: '0' != 'false'"]
BOOLEAN_WHY = "GDAL keeps a boolean column with other numbers boolean, losing those values"
KNOWN_DIFFERENCES = [
    (
        "no references",
        "read HEADERS=DISABLE",
        ["fields ['Field1', 'Field2'] != ['id', 'name']", "2 features != 1"],
        "GDAL ignored HEADERS=DISABLE on this sheet of cells without references",
    ),
    ("widening", "read defaults", BOOLEAN_WIDENING, BOOLEAN_WHY),
    ("widening", "read HEADERS=FORCE", BOOLEAN_WIDENING, BOOLEAN_WHY),
    ("widening", "GDAL reading terrio's fixture copy", BOOLEAN_WIDENING, BOOLEAN_WHY),
]

STRINGS = ["id", "name", "when", "x", "y", " spaced ", "a & b", "ünï", "day", "clock", "flag", "v"]


def s(ref, text):
    """A shared-string cell.

    Args:
        ref: The cell reference.
        text: One of ``STRINGS``.

    Returns:
        The cell's XML.
    """
    return f'<c r="{ref}" t="s"><v>{STRINGS.index(text)}</v></c>'


def n(ref, value, style=None):
    """A number cell.

    Args:
        ref: The cell reference.
        value: The number.
        style: The style index, if any.

    Returns:
        The cell's XML.
    """
    attr = f' s="{style}"' if style is not None else ""
    return f'<c r="{ref}"{attr}><v>{value}</v></c>'


SHEETS = {
    "typed": [
        (1, [s("A1", "id"), s("B1", "name"), s("C1", "day"), s("D1", "when"), s("E1", "clock"), s("F1", "flag"), s("G1", "v")]),
        (2, [n("A2", 1), s("B2", " spaced "), n("C2", 45293, 1), n("D2", 45293.5, 2), n("E2", 0.25, 3), '<c r="F2" t="b"><v>1</v></c>', n("G2", 1.5, 5)]),
        (3, [n("A3", 4294967296), s("B3", "a & b"), n("C3", 45294, 1), n("D3", 45294.75, 6), n("E3", 0.5, 3), '<c r="F3" t="b"><v>0</v></c>', n("G3", 2, 5)]),
        (4, [n("A4", 3), s("B4", "ünï"), n("D4", 45295.123, 4)]),
    ],
    "widening": [
        (1, [s("A1", "x"), s("B1", "y"), s("C1", "name"), s("D1", "when"), s("E1", "flag")]),
        (2, [n("A2", 1), n("B2", 1), s("C2", "a & b"), n("D2", 45293, 1), '<c r="E2" t="b"><v>1</v></c>']),
        (3, [n("A3", 1.5), s("B3", "ünï"), n("C3", 7), n("D3", 45293.5, 2), n("E3", 5)]),
        (4, [n("A4", 2), n("B4", 45293, 1), n("C4", 0.25, 3), n("D4", 45294, 1), '<c r="E4" t="b"><v>0</v></c>']),
    ],
    "all text": [(1, [s("A1", "id"), s("B1", "name")]), (2, [s("A2", "x"), s("B2", "y")])],
    "one row": [(1, [s("A1", "id"), n("B1", 2)])],
    "numbers": [(1, [n("A1", 1), n("B1", 2)]), (2, [n("A2", 3.5), n("B2", 4)])],
    "empty": [],
    "gaps": [
        (2, [s("B2", "x"), s("D2", "y")]),
        (3, [n("B3", 1), n("D3", 2)]),
        (6, [n("B6", 3), n("F6", 9)]),
        (7, []),
    ],
    "blank headings": [(1, [s("A1", "id"), s("C1", "name")]), (2, [n("A2", 1), n("B2", 2), n("C2", 3), n("D2", 4)])],
    "formulas": [
        (1, [s("A1", "x"), s("B1", "y"), s("C1", "name")]),
        (2, ['<c r="A2"><f>1+1</f><v>2</v></c>', '<c r="B2" t="str"><f>"q"</f><v>q</v></c>', '<c r="C2" t="e"><f>1/0</f><v>#DIV/0!</v></c>']),
        (3, ['<c r="A3"><f>2+2</f><v>4</v></c>', '<c r="B3"><f>NOW()</f></c>', '<c r="C3" t="inlineStr"><is><t> inline </t></is></c>']),
    ],
    "styled blanks": [(1, ['<c r="A1" s="5"/>', s("B1", "name")]), (2, [n("A2", 1), n("B2", 2)])],
    "no references": [(1, ['<c t="s"><v>0</v></c>', '<c t="s"><v>1</v></c>']), (2, ["<c><v>1</v></c>", "<c><v>2</v></c>"])],
}


def write_fixture(path):
    """Writes a workbook of hand-laid sheets, with what spreadsheet programs write.

    Args:
        path: The output path.
    """
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(
            "[Content_Types].xml",
            '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
            '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>',
        )
        names = list(SHEETS)
        sheets = "".join(f'<sheet name="{name}" sheetId="{i + 1}" r:id="rId{i + 1}"/>' for i, name in enumerate(names))
        z.writestr("xl/workbook.xml", f'<workbook xmlns="{MAIN}" xmlns:r="{RELS}"><sheets>{sheets}</sheets></workbook>')
        rels = "".join(f'<Relationship Id="rId{i + 1}" Type="{RELS}/worksheet" Target="worksheets/sheet{i + 1}.xml"/>' for i in range(len(names)))
        z.writestr("xl/_rels/workbook.xml.rels", f'<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>')
        z.writestr("xl/styles.xml", STYLES)
        escaped = [t.replace("&", "&amp;") for t in STRINGS]
        items = "".join(f'<si><t xml:space="preserve">{t}</t></si>' for t in escaped)
        z.writestr("xl/sharedStrings.xml", f'<sst xmlns="{MAIN}">{items}</sst>')
        for i, name in enumerate(names):
            rows = "".join(f'<row r="{r}">{"".join(cells)}</row>' for r, cells in SHEETS[name])
            z.writestr(f"xl/worksheets/sheet{i + 1}.xml", f'<worksheet xmlns="{MAIN}"><sheetData>{rows}</sheetData></worksheet>')


def gdal_sheets(path, options):
    """Reads every sheet with GDAL.

    Args:
        path: The workbook.
        options: Open options, as ``KEY=VALUE``.

    Returns:
        A dict of layer names to ``(fields, rows)``: field names, then values as text.
    """
    from osgeo import gdal, ogr

    gdal.UseExceptions()
    ds = gdal.OpenEx(path, gdal.OF_VECTOR, allowed_drivers=["XLSX"], open_options=options)
    out = {}
    for i in range(ds.GetLayerCount()):
        layer = ds.GetLayer(i)
        defn = layer.GetLayerDefn()
        count = defn.GetFieldCount()
        fields = [defn.GetFieldDefn(j).GetName() for j in range(count)]
        boolean = [defn.GetFieldDefn(j).GetSubType() == ogr.OFSTBoolean for j in range(count)]
        real = [defn.GetFieldDefn(j).GetType() == ogr.OFTReal for j in range(count)]
        rows = []
        for feature in layer:
            row = []
            for j in range(count):
                if not feature.IsFieldSetAndNotNull(j):
                    row.append(None)
                elif boolean[j]:
                    row.append("true" if feature.GetFieldAsInteger(j) else "false")
                elif real[j]:
                    row.append(repr(float(feature.GetFieldAsDouble(j))))
                else:
                    row.append(normalize_text(feature.GetFieldAsString(j)))
            rows.append(row)
        out[layer.GetName()] = (fields, rows)
    return out


def normalize_text(value):
    """Writes date-times one way: GDAL prints ``2024/01/02 12:00:00``, terrio ISO 8601.

    Args:
        value: A field's value as text.

    Returns:
        The value, with ISO 8601 dates in GDAL's slashes unchanged otherwise.
    """
    if len(value) >= 10 and value[4] == "-" and value[7] == "-" and value[:4].isdigit():
        return value[:10].replace("-", "/") + value[10:].replace("T", " ")
    return value


def terrio_sheets(terrio, path, options, tmp):
    """Reads every sheet with terrio, through its CLI and a GeoJSON file per layer.

    Args:
        terrio: The terrio executable.
        path: The workbook.
        options: Open options, as ``KEY=VALUE``.
        tmp: A directory for the outputs.

    Returns:
        A dict of layer names to ``(fields, rows)``, as :func:`gdal_sheets` makes.
    """
    info = subprocess.run([terrio, "info", "--json", path], capture_output=True, text=True, check=True)
    layers = json.loads(info.stdout)["layers"]
    flags = [a for o in options for a in ("--oo", o)]
    out = {}
    for i, layer in enumerate(layers):
        name = layer["name"]
        target = os.path.join(tmp, f"out{i}.geojson")
        if os.path.exists(target):
            os.remove(target)
        subprocess.run([terrio, "convert", path, target, "--layer", name, *flags], capture_output=True, text=True, check=True)
        with open(target, encoding="utf-8") as f:
            features = json.load(f)["features"]
        # The info above is without the open options: the features' properties name the fields.
        fields = list(features[0]["properties"]) if features else [field["name"] for field in layer["fields"]]
        rows = []
        for feature in features:
            properties = feature["properties"]
            rows.append([to_text(properties.get(name)) for name in fields])
        out[name] = (fields, rows)
    return out


def to_text(value):
    """A GeoJSON property as the text GDAL's values are compared in.

    Args:
        value: The property value.

    Returns:
        The text, or ``None`` for a null.
    """
    if value is None:
        return None
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, float):
        return repr(value)
    return normalize_text(str(value))


def same_value(a, b):
    """Whether two values agree, the types of a number column aside.

    Args:
        a: terrio's value as text.
        b: GDAL's.

    Returns:
        Whether they agree.
    """
    if a == b:
        return True
    if a is None or b is None:
        return False
    try:
        return float(a) == float(b)
    except ValueError:
        pass
    # Fractions of a second to the millisecond: GDAL prints a float's rounding.
    if len(a) < 19 or len(b) < 19 or a[:19] != b[:19]:
        return False
    try:
        return abs(float("0" + a[19:]) - float("0" + b[19:])) < 2e-3
    except ValueError:
        return False


def compare(ours, theirs):
    """Compares what two drivers read.

    Args:
        ours: terrio's sheets.
        theirs: GDAL's sheets.

    Returns:
        A dict of layer names to lists of differences.
    """
    results = {}
    for name, (fields, rows) in theirs.items():
        problems = []
        if name not in ours:
            results[name] = ["missing sheet"]
            continue
        our_fields, our_rows = ours[name]
        # terrio makes repeated headings unique; GDAL repeats them.
        unique = []
        for field in fields:
            candidate, k = field, 2
            while candidate in unique:
                candidate, k = f"{field}_{k}", k + 1
            unique.append(candidate)
        if our_fields != unique:
            problems.append(f"fields {our_fields} != {unique}")
        if len(our_rows) != len(rows):
            problems.append(f"{len(our_rows)} features != {len(rows)}")
        if not problems:
            for r, (a_row, b_row) in enumerate(zip(our_rows, rows)):
                for field, a, b in zip(fields, a_row, b_row):
                    if not same_value(a, b):
                        problems.append(f"row {r} {field}: {a!r} != {b!r}")
        results[name] = problems
    return results


def write_source(path):
    """Writes a CSV with every field type, with its ``.csvt`` for GDAL.

    Args:
        path: The output path.
    """
    with open(path, "w", encoding="utf-8") as f:
        f.write("id,big,r,s,d,dt,b\n")
        f.write('1,1099511627776,1.5,"hello, you",2024-01-02,2024-01-02T03:04:05,true\n')
        f.write("2,5,2.25,,1999-12-31,1999-01-01T00:00:00.5,false\n")
        f.write("3,,0.1,ünï,,,\n")
    with open(path + "t", "w", encoding="utf-8") as f:
        f.write("Integer,Integer64,Real,String,Date,DateTime,Integer(Boolean)\n")


def main():
    """Runs the comparison and prints or saves its cases.

    Returns:
        The process exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--terrio", required=True)
    parser.add_argument("--json")
    parser.add_argument("workbooks", nargs="*")
    args = parser.parse_args()
    cases = []

    def record(prefix, results):
        for name, problems in results.items():
            known = next((why for sheet, case, expected, why in KNOWN_DIFFERENCES if (sheet, case) == (name, prefix) and problems == expected), None)
            detail = "; ".join(problems[:5])
            if problems and known:
                detail = f"known: {known}: {detail}"
            cases.append({"name": f"{prefix}: {name}", "status": "differs" if problems else "identical", "detail": detail, "known": bool(problems and known)})

    with tempfile.TemporaryDirectory(prefix="terrio-xlsx-") as tmp:
        fixture = os.path.join(tmp, "fixture.xlsx")
        write_fixture(fixture)
        for options in ([], ["HEADERS=FORCE"], ["HEADERS=DISABLE"], ["FIELD_TYPES=STRING"]):
            label = options[0] if options else "defaults"
            record(f"read {label}", compare(terrio_sheets(args.terrio, fixture, options, tmp), gdal_sheets(fixture, options)))
        for workbook in args.workbooks:
            record(f"read {os.path.basename(workbook)}", compare(terrio_sheets(args.terrio, workbook, [], tmp), gdal_sheets(workbook, [])))

        source = os.path.join(tmp, "source.csv")
        write_source(source)
        by_gdal = os.path.join(tmp, "by_gdal.xlsx")
        subprocess.run(["ogr2ogr", "-f", "XLSX", by_gdal, source, "-nln", "layer"], check=True, capture_output=True)
        record("read ogr2ogr's", compare(terrio_sheets(args.terrio, by_gdal, [], tmp), gdal_sheets(by_gdal, [])))

        by_terrio = os.path.join(tmp, "by_terrio.xlsx")
        subprocess.run([args.terrio, "convert", by_gdal, by_terrio], check=True, capture_output=True)
        record("GDAL reading terrio's", compare(gdal_sheets(by_terrio, []), gdal_sheets(by_gdal, [])))
        by_terrio_fixture = os.path.join(tmp, "fixture_by_terrio.xlsx")
        subprocess.run([args.terrio, "convert", fixture, by_terrio_fixture], check=True, capture_output=True)
        record("GDAL reading terrio's fixture copy", compare(gdal_sheets(by_terrio_fixture, []), gdal_sheets(fixture, [])))

    if args.json:
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump({"cases": cases}, f, indent=2)
    failed = [c for c in cases if c["status"] != "identical" and not c["known"]]
    identical = sum(c["status"] == "identical" for c in cases)
    for case in cases:
        print(f"{'OK  ' if case['status'] == 'identical' else 'DIFF'}  {case['name']}" + (f": {case['detail']}" if case["detail"] else ""))
    print(f"{identical} of {len(cases)} identical, {len(failed)} unexpected differences")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
