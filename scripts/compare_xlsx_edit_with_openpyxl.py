#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Edit one cell of an openpyxl workbook with terrio, then check what openpyxl and the archive hold.

Usage:
    compare_xlsx_edit_with_openpyxl.py EXAMPLE

EXAMPLE is the built ``xlsx_workbook`` example of ``crates/terrio/examples``.
"""

import argparse
import json
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

import openpyxl
from openpyxl.chart import BarChart, Reference
from openpyxl.formatting.rule import CellIsRule
from openpyxl.styles import Font, PatternFill
from openpyxl.worksheet.table import Table


def source_workbook(path):
    """Writes a workbook with what terrio's model does not hold.

    Args:
        path: The output path.
    """
    book = openpyxl.Workbook()
    sheet = book.active
    sheet.title = "Data"
    sheet.append(["name", "count"])
    sheet.append(["a", 1])
    sheet.append(["b", 2])
    sheet["A1"].font = Font(bold=True, color="FF0000")
    sheet["B3"].fill = PatternFill("solid", fgColor="00FF00")
    sheet["D1"] = "=SUM(B2:B3)"
    sheet.merge_cells("F1:G1")
    sheet.add_table(Table(displayName="T1", ref="A1:B3"))
    sheet.conditional_formatting.add("B2:B3", CellIsRule(operator="greaterThan", formula=["1"], font=Font(italic=True)))
    chart = BarChart()
    chart.add_data(Reference(sheet, min_col=2, min_row=1, max_row=3), titles_from_data=True)
    sheet.add_chart(chart, "H2")
    book.create_sheet("Second")["A1"] = "kept"
    book.save(path)


def main():
    """Runs the edit and compares what it kept.

    Returns:
        The process exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("example")
    parser.add_argument("--json", type=Path, help="Write the comparison cases as JSON")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="terrio-xlsx-edit-") as tmp:
        before, after = Path(tmp, "before.xlsx"), Path(tmp, "after.xlsx")
        source_workbook(before)
        subprocess.run([args.example, "edit", str(before), str(after)], check=True, timeout=120)
        with zipfile.ZipFile(before) as a, zipfile.ZipFile(after) as b:
            kept = [n for n in a.namelist() if "chart" in n or "drawing" in n or "table" in n or "theme" in n]
            parts = {n: n in b.namelist() and a.read(n) == b.read(n) for n in kept}
        book = openpyxl.load_workbook(after)
        sheet = book["Data"]
        checks = [
            ("the edit", (sheet["A2"].value, sheet["A2"].font.b), ("edited", True)),
            ("other cells", (sheet["A1"].value, sheet["A3"].value, sheet["B2"].value), ("name", "b", 1)),
            ("kept font", (sheet["A1"].font.b, sheet["A1"].font.color.rgb), (True, "00FF0000")),
            ("kept fill", sheet["B3"].fill.fgColor.rgb, "0000FF00"),
            ("kept formula", sheet["D1"].value, "=SUM(B2:B3)"),
            ("kept merge", [str(r) for r in sheet.merged_cells.ranges], ["F1:G1"]),
            ("kept table", list(sheet.tables), ["T1"]),
            ("kept chart", len(sheet._charts), 1),
            ("kept conditional format", [str(r.sqref) for r in sheet.conditional_formatting], ["B2:B3"]),
            ("kept sheet", book["Second"]["A1"].value, "kept"),
            ("kept parts", (bool(parts), all(parts.values())), (True, True)),
        ]
    failed = 0
    cases = []
    for name, actual, wanted in checks:
        same = actual == wanted
        failed += not same
        cases.append({"name": name, "status": "identical" if same else "differs",
                      "detail": "" if same else f"{actual!r} != {wanted!r}"})
        print(f"{'OK  ' if same else 'DIFF'}  {name}" + ("" if same else f": {actual!r} != {wanted!r}"))
    print(f"{len(checks) - failed} of {len(checks)} identical")
    if args.json:
        args.json.write_text(json.dumps({"cases": cases}, indent=2) + "\n", encoding="utf-8")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
