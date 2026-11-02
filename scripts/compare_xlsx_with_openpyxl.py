#!/usr/bin/env python3
"""Compare terrio's XLSX layout options with openpyxl, in both directions.

GDAL has no behavior to compare these options with. Workbooks written by terrio are loaded with
openpyxl, and workbooks written by openpyxl are read by terrio.

Usage:
    compare_xlsx_with_openpyxl.py --terrio PATH [--json REPORT]
"""

import argparse
import datetime
import json
import os
import subprocess
import sys
import tempfile

import openpyxl

CSV = "id,name,day,when\n1,alpha,2024-01-02,2024-01-02T03:04:05\n2,a much longer name,2024-02-03,2024-02-03T04:05:06\n"
CSVT = "Integer,String,Date,DateTime\n"
LAYOUT = [
    "HEADER_STYLE=BOLD",
    "FREEZE_HEADER=YES",
    "AUTOFILTER=YES",
    "COLUMN_WIDTH=AUTO",
    "DATE_FORMAT=yyyy-mm-dd",
    "DATETIME_FORMAT=yyyy-mm-dd hh:mm:ss",
    "CREATION_TIME=2024-01-02T03:04:05Z",
]


def expect(cases, name, actual, wanted):
    """Records whether a value is what it should be.

    Args:
        cases: The list of cases to add to.
        name: The case's name.
        actual: The value found.
        wanted: The value wanted.
    """
    same = actual == wanted
    cases.append({"name": name, "status": "identical" if same else "differs", "detail": "" if same else f"{actual!r} != {wanted!r}"})


def terrio_read(terrio, path, tmp, options):
    """Reads a workbook's first sheet with terrio.

    Args:
        terrio: The terrio executable.
        path: The workbook.
        tmp: A directory for the output.
        options: Open options, as ``KEY=VALUE``.

    Returns:
        The features of the GeoJSON output.
    """
    target = os.path.join(tmp, "read.geojson")
    if os.path.exists(target):
        os.remove(target)
    flags = [a for o in options for a in ("--oo", o)]
    subprocess.run([terrio, "convert", path, target, *flags], check=True, capture_output=True, text=True)
    with open(target, encoding="utf-8") as f:
        return json.load(f)["features"]


def check_written(args, tmp, cases):
    """Checks a workbook written with every layout option, as openpyxl loads it.

    Args:
        args: The command-line arguments.
        tmp: A directory for the workbook.
        cases: The list of cases to add to.
    """
    source = os.path.join(tmp, "source.csv")
    with open(source, "w", encoding="utf-8") as f:
        f.write(CSV)
    with open(source + "t", "w", encoding="utf-8") as f:
        f.write(CSVT)
    path = os.path.join(tmp, "written.xlsx")
    flags = [a for o in LAYOUT for a in ("--lco", o)]
    subprocess.run([args.terrio, "convert", source, path, *flags], check=True, capture_output=True, text=True)
    sheet = openpyxl.load_workbook(path).worksheets[0]
    expect(cases, "written: header values", [c.value for c in sheet[1]], ["id", "name", "day", "when"])
    expect(cases, "written: bold header", [c.font.b for c in sheet[1]], [True] * 4)
    expect(cases, "written: body not bold", [bool(c.font.b) for c in sheet[2]], [False] * 4)
    expect(cases, "written: integer", sheet["A2"].value, 1)
    expect(cases, "written: text", sheet["B3"].value, "a much longer name")
    expect(cases, "written: date", sheet["C2"].value, datetime.datetime(2024, 1, 2))
    expect(cases, "written: date-time", sheet["D3"].value, datetime.datetime(2024, 2, 3, 4, 5, 6))
    expect(cases, "written: date format", sheet["C2"].number_format, "yyyy-mm-dd")
    expect(cases, "written: date-time format", sheet["D2"].number_format, "yyyy-mm-dd hh:mm:ss")
    expect(cases, "written: frozen pane", sheet.freeze_panes, "A2")
    expect(cases, "written: filter", sheet.auto_filter.ref, "A1:D3")
    expect(cases, "written: fitted width", sheet.column_dimensions["B"].width, 20.0)
    created = openpyxl.load_workbook(path).properties.created
    expect(cases, "written: creation time", created, datetime.datetime(2024, 1, 2, 3, 4, 5))


def check_read(args, tmp, cases):
    """Checks a report sheet written by openpyxl, as terrio reads it.

    Args:
        args: The command-line arguments.
        tmp: A directory for the workbook.
        cases: The list of cases to add to.
    """
    book = openpyxl.Workbook()
    sheet = book.active
    sheet["A1"] = "Report title"
    sheet["A3"] = "Name"
    sheet["B3"] = "Quantity"
    sheet["B4"] = "Phase Type"
    sheet["B5"] = "1PH"
    sheet["C5"] = "3PH"
    sheet["D3"] = "Location"
    sheet.merge_cells("A3:A5")
    sheet.merge_cells("B3:C3")
    sheet.merge_cells("B4:C4")
    sheet.merge_cells("D3:D5")
    for row in (["east", 9, 4, "x"], ["west", 1, 2, "y"], ["total", 10, 6, ""]):
        sheet.append(row)
    path = os.path.join(tmp, "report.xlsx")
    book.save(path)
    wanted = ["Name", "Quantity / Phase Type / 1PH", "Quantity / Phase Type / 3PH", "Location"]
    features = terrio_read(args.terrio, path, tmp, ["FIRST_ROW=3", "HEADER_ROWS=3"])
    expect(cases, "read: joined headings", list(features[0]["properties"]), wanted)
    expect(cases, "read: rows", len(features), 3)
    expect(cases, "read: value", features[1]["properties"][wanted[2]], 2)
    features = terrio_read(args.terrio, path, tmp, ["RANGE=B5:C7", "HEADER_ROWS=1"])
    expect(cases, "read: range fields", list(features[0]["properties"]), ["1PH", "3PH"])
    expect(cases, "read: range rows", [list(f["properties"].values()) for f in features], [[9, 4], [1, 2]])


def main():
    """Runs the comparison and prints or saves its cases.

    Returns:
        The process exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--terrio", required=True)
    parser.add_argument("--json")
    args = parser.parse_args()
    cases = []
    with tempfile.TemporaryDirectory(prefix="terrio-xlsx-layout-") as tmp:
        check_written(args, tmp, cases)
        check_read(args, tmp, cases)
    if args.json:
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump({"cases": cases}, f, indent=2)
    failed = [c for c in cases if c["status"] != "identical"]
    for case in cases:
        print(f"{'OK  ' if case['status'] == 'identical' else 'DIFF'}  {case['name']}" + (f": {case['detail']}" if case["detail"] else ""))
    print(f"{len(cases) - len(failed)} of {len(cases)} identical")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
