// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Writes a report workbook with `terrio::xlsx`, for `scripts/compare_xlsx_workbook_with_openpyxl.py`.
//!
//! Usage: `cargo run -p terrio --example xlsx_workbook -- OUTPUT.xlsx`, or `-- edit INPUT.xlsx OUTPUT.xlsx`
//! to change one cell of a workbook, for `scripts/compare_xlsx_edit_with_openpyxl.py`.

use terrio::xlsx::{Align, BorderStyle, CellValue, Format, Orientation, VerticalAlign, Workbook};

fn main() -> terrio::Result<()> {
    let path = std::env::args().nth(1).expect("an output path");
    if path == "edit" && std::env::args().len() == 4 {
        let input = std::env::args().nth(2).expect("an input path");
        let output = std::env::args().nth(3).expect("an output path");
        let mut workbook = Workbook::open(&input)?;
        let Some(first) = workbook.sheet_names().first().map(|s| s.to_string()) else {
            return Err(terrio::Error::InvalidArgument("the workbook has no sheet to edit".into()));
        };
        workbook.sheet_mut(&first)?.write("A2", "edited", &Format::new().bold())?;
        return workbook.save(&output);
    }
    let mut workbook = Workbook::new();
    workbook.set_creation_time("2024-01-02T03:04:05Z")?;
    workbook.set_title("Report");
    workbook.set_author("terrio");
    let sheet = workbook.add_sheet("LV Summary")?;
    let head =
        Format::new().bold().font("Arial").font_size(12.0).font_color(0x1F4E78).fill(0xD9E1F2).border_thin().align(Align::Center).wrap();
    sheet.merge_write("B1:E1", "Quantity", &head)?;
    sheet.merge_write("F1:G1", "Location", &head)?;
    sheet.write("B3", "1PH", &Format::new().italic().underline().rotation(45).vertical_align(VerticalAlign::Top).indent(1))?;
    sheet.write("B5", 9.0, &Format::new().num_format("#,##0").border(BorderStyle::Double, Some(0xFF0000)))?;
    sheet.write("C5", true, &Format::new())?;
    sheet.write("D5", CellValue::Date("2024-01-02".into()), &Format::new())?;
    sheet.write("E5", CellValue::DateTime("2024-01-02T12:00:00".into()), &Format::new())?;
    sheet.write("F5", CellValue::formula("B5*2", 18.0), &Format::new().locked(false))?;
    sheet.write("G5", " verbatim ", &Format::new())?;
    sheet.set_column_width("A", 15.7)?;
    sheet.set_column_hidden("H", true)?;
    sheet.set_column_outline("I", 2)?;
    sheet.set_row_height(3, 30.0)?;
    sheet.set_row_hidden(7, true)?;
    sheet.set_row_outline(8, 1)?;
    sheet.set_tab_color(0x00B050);
    sheet.freeze_panes(4, 1)?;
    sheet.set_zoom(120)?;
    sheet.set_show_gridlines(false);
    sheet.autofilter("A3:K8")?;
    sheet.validate_list("A5:A8", &["yes", "no"])?;
    sheet.hyperlink("A5", "https://example.com/")?;
    sheet.comment("B5", "checked", "reviewer")?;
    sheet.print_orientation(Orientation::Landscape);
    sheet.print_paper_size(9);
    sheet.print_margins(0.5, 0.6, 0.7, 0.8, 0.3, 0.4)?;
    sheet.print_fit_to_width(1);
    sheet.print_area("A1:K8")?;
    sheet.print_title_rows(1, 4)?;
    sheet.print_header("&CReport")?;
    sheet.print_footer("&RPage &P")?;
    workbook.add_sheet("Hidden")?.set_hidden(true);
    workbook.define_name("rates", "'LV Summary'!$B$5")?;
    workbook.save(&path)
}
