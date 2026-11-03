#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Check a workbook written by ``crates/terrio/examples/xlsx_workbook.rs`` as openpyxl loads it.

Usage:
    compare_xlsx_workbook_with_openpyxl.py WORKBOOK
"""

import datetime
import sys

import openpyxl


def main():
    """Compares each control with what the example writes.

    Returns:
        The process exit code.
    """
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    book = openpyxl.load_workbook(sys.argv[1])
    cached = openpyxl.load_workbook(sys.argv[1], data_only=True)
    sheet = book["LV Summary"]
    head = sheet["B1"]
    b3 = sheet["B3"]
    b5 = sheet["B5"]
    print_titles = sheet.print_title_rows
    checks = [
        ("sheets", book.sheetnames, ["LV Summary", "Hidden"]),
        ("hidden sheet", book["Hidden"].sheet_state, "hidden"),
        ("creation time", book.properties.created, datetime.datetime(2024, 1, 2, 3, 4, 5)),
        ("title", book.properties.title, "Report"),
        ("author", book.properties.creator, "terrio"),
        ("defined name", book.defined_names["rates"].attr_text, "'LV Summary'!$B$5"),
        ("merges", sorted(str(r) for r in sheet.merged_cells.ranges), ["B1:E1", "F1:G1"]),
        ("merged value", head.value, "Quantity"),
        ("bold", head.font.b, True),
        ("font", (head.font.name, head.font.sz, head.font.color.rgb), ("Arial", 12.0, "FF1F4E78")),
        ("fill", head.fill.fgColor.rgb, "FFD9E1F2"),
        ("border", (head.border.left.style, head.border.bottom.style), ("thin", "thin")),
        ("merged cell border", sheet["E1"].border.right.style, "thin"),
        ("alignment", (head.alignment.horizontal, head.alignment.wrap_text), ("center", True)),
        ("italic, underline", (b3.font.i, b3.font.u), (True, "single")),
        ("rotation, indent", (b3.alignment.textRotation, b3.alignment.indent), (45, 1)),
        ("vertical", b3.alignment.vertical, "top"),
        ("number", (b5.value, b5.number_format), (9, "#,##0")),
        ("border color", (b5.border.top.style, b5.border.top.color.rgb), ("double", "FFFF0000")),
        ("boolean", sheet["C5"].value, True),
        ("date", (sheet["D5"].value, sheet["D5"].number_format), (datetime.datetime(2024, 1, 2), "yyyy-mm-dd")),
        ("date-time", sheet["E5"].value, datetime.datetime(2024, 1, 2, 12)),
        ("formula", sheet["F5"].value, "=B5*2"),
        ("cached value", cached["LV Summary"]["F5"].value, 18),
        ("protection", sheet["F5"].protection.locked, False),
        ("verbatim text", sheet["G5"].value, " verbatim "),
        ("column width", sheet.column_dimensions["A"].width, 15.7),
        ("hidden column", sheet.column_dimensions["H"].hidden, True),
        ("column outline", sheet.column_dimensions["I"].outlineLevel, 2),
        ("row height", sheet.row_dimensions[3].height, 30),
        ("hidden row", sheet.row_dimensions[7].hidden, True),
        ("row outline", sheet.row_dimensions[8].outlineLevel, 1),
        ("tab color", sheet.sheet_properties.tabColor.rgb, "FF00B050"),
        ("frozen panes", sheet.freeze_panes, "B5"),
        ("zoom", sheet.sheet_view.zoomScale, 120),
        ("gridlines", sheet.sheet_view.showGridLines, False),
        ("filter", sheet.auto_filter.ref, "A3:K8"),
        ("validation", [(str(v.sqref), v.type, v.formula1) for v in sheet.data_validations.dataValidation], [("A5:A8", "list", '"yes,no"')]),
        ("hyperlink", sheet["A5"].hyperlink.target, "https://example.com/"),
        ("comment", (sheet["B5"].comment.text, sheet["B5"].comment.author), ("checked", "reviewer")),
        ("orientation", sheet.page_setup.orientation, "landscape"),
        ("paper", int(sheet.page_setup.paperSize), 9),
        ("margins", (sheet.page_margins.left, sheet.page_margins.footer), (0.5, 0.4)),
        ("fit to", (sheet.sheet_properties.pageSetUpPr.fitToPage, sheet.page_setup.fitToWidth, sheet.page_setup.fitToHeight), (True, 1, 0)),
        ("print area", sheet.print_area, "'LV Summary'!$A$1:$K$8"),
        ("title rows", print_titles, "$1:$4"),
        ("page header", sheet.oddHeader.center.text, "Report"),
        ("page footer", sheet.oddFooter.right.text, "Page &P"),
    ]
    failed = 0
    for name, actual, wanted in checks:
        same = actual == wanted
        failed += not same
        print(f"{'OK  ' if same else 'DIFF'}  {name}" + ("" if same else f": {actual!r} != {wanted!r}"))
    print(f"{len(checks) - failed} of {len(checks)} identical")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
