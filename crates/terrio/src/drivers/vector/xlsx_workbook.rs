//! A workbook model for spreadsheets laid out for people: sheets, typed cells, merged ranges, column
//! and row settings, cell formats, sheet views, data tools, print setup and document properties.
//!
//! Rows and columns count from 1, as the sheet shows them, and cells are addressed as `B3`. Formulas
//! are never evaluated: a formula is written with its text and the cached value it is given.
//!
//! ```
//! use terrio::xlsx::{Align, CellValue, Format, Workbook};
//!
//! let mut workbook = Workbook::new();
//! let sheet = workbook.add_sheet("Summary")?;
//! let head = Format::new().bold().fill(0xD9E1F2).border_thin().align(Align::Center).wrap();
//! sheet.merge_write("A1:B1", "Quantity", &head)?;
//! sheet.write("A2", 9.0, &Format::new().num_format("#,##0"))?;
//! sheet.write("B2", CellValue::formula("A2*2", 18.0), &Format::new())?;
//! sheet.set_column_width("A", 15.7)?;
//! sheet.freeze_panes(1, 0)?;
//! assert_eq!(sheet.merged_ranges(), ["A1:B1"]);
//! let bytes = workbook.to_bytes()?;
//! assert!(bytes.starts_with(b"PK"));
//! # Ok::<(), terrio::Error>(())
//! ```

use super::datetime;
use super::xlsx::{
    MAIN_NS, MAX_CELL_TEXT_UTF16, MAX_COLUMNS, MAX_ROWS, OD_RELS, PACKAGE, StringTable, XML_HEADER, column_letters, excel_serial_days,
    wall_clock_millis, xml_text,
};
use crate::{Error, Result};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;

const MAX_SHEET_NAME_UTF16: usize = 31;

fn invalid(message: String) -> Error {
    Error::InvalidArgument(format!("XLSX: {message}"))
}

/// A cell's address as its row and column, from 1.
fn parse_cell(cell: &str) -> Result<(u32, u32)> {
    let bad = || invalid(format!("'{cell}' is not a cell such as B3"));
    let cleaned = cell.replace('$', "");
    let digits = cleaned.find(|c: char| c.is_ascii_digit()).ok_or_else(bad)?;
    let (letters, row) = cleaned.split_at(digits);
    let column = parse_column(letters).map_err(|_| bad())?;
    let row: u32 = row.parse().map_err(|_| bad())?;
    if row == 0 || row as usize > MAX_ROWS {
        return Err(bad());
    }
    Ok((row, column))
}

/// A column's letters as its number, from 1.
fn parse_column(letters: &str) -> Result<u32> {
    let bad = || invalid(format!("'{letters}' is not a column such as AB"));
    if letters.is_empty() || letters.len() > 3 || !letters.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(bad());
    }
    let column = letters.bytes().fold(0u32, |c, b| c * 26 + u32::from(b.to_ascii_uppercase() - b'A') + 1);
    if column as usize > MAX_COLUMNS {
        return Err(bad());
    }
    Ok(column)
}

fn letters(column: u32) -> String {
    column_letters(column as usize - 1)
}

fn cell_name(row: u32, column: u32) -> String {
    format!("{}{row}", letters(column))
}

/// A rectangle of cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    first_row: u32,
    first_column: u32,
    last_row: u32,
    last_column: u32,
}

impl Range {
    fn parse(range: &str) -> Result<Self> {
        let (start, end) = range.split_once(':').unwrap_or((range, range));
        let (first_row, first_column) = parse_cell(start)?;
        let (last_row, last_column) = parse_cell(end)?;
        if last_row < first_row || last_column < first_column {
            return Err(invalid(format!("range '{range}' ends before it starts")));
        }
        Ok(Self { first_row, first_column, last_row, last_column })
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.first_row <= other.last_row
            && other.first_row <= self.last_row
            && self.first_column <= other.last_column
            && other.first_column <= self.last_column
    }

    fn text(&self) -> String {
        if self.first_row == self.last_row && self.first_column == self.last_column {
            return cell_name(self.first_row, self.first_column);
        }
        format!("{}:{}", cell_name(self.first_row, self.first_column), cell_name(self.last_row, self.last_column))
    }

    /// The range as a defined name holds it: `$A$1:$C$3`.
    fn absolute(&self) -> String {
        format!("${}${}:${}${}", letters(self.first_column), self.first_row, letters(self.last_column), self.last_row)
    }
}

/// What a cell holds.
#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    /// A cell with only a format.
    Blank,
    /// Text, kept verbatim.
    Text(String),
    /// A number.
    Number(f64),
    /// A boolean.
    Boolean(bool),
    /// A date as `YYYY-MM-DD`. It needs a date number format to show as one, which it gets by default.
    Date(String),
    /// A date-time as ISO 8601, written without a UTC offset, which a sheet cannot hold.
    DateTime(String),
    /// An error such as `#N/A`.
    Error(String),
    /// A formula's text, without `=`, and the value it was last computed to.
    Formula {
        /// The formula's text.
        formula: String,
        /// Its cached value. Formulas are never evaluated here.
        cached: Option<Box<CellValue>>,
    },
}

impl CellValue {
    /// A formula with its cached value.
    pub fn formula(formula: impl Into<String>, cached: impl Into<CellValue>) -> Self {
        Self::Formula { formula: formula.into(), cached: Some(Box::new(cached.into())) }
    }
}

impl From<&str> for CellValue {
    fn from(v: &str) -> Self {
        Self::Text(v.to_string())
    }
}
impl From<String> for CellValue {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}
impl From<f64> for CellValue {
    fn from(v: f64) -> Self {
        Self::Number(v)
    }
}
impl From<i32> for CellValue {
    fn from(v: i32) -> Self {
        Self::Number(f64::from(v))
    }
}
impl From<bool> for CellValue {
    fn from(v: bool) -> Self {
        Self::Boolean(v)
    }
}

/// Horizontal alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Align {
    /// To the left.
    Left,
    /// Centered.
    Center,
    /// To the right.
    Right,
    /// Filled to the cell's width.
    Fill,
    /// Justified.
    Justify,
}

/// Vertical alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerticalAlign {
    /// To the top.
    Top,
    /// Centered.
    Center,
    /// To the bottom.
    Bottom,
}

/// A border's line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BorderStyle {
    /// Thin.
    Thin,
    /// Medium.
    Medium,
    /// Thick.
    Thick,
    /// Dashed.
    Dashed,
    /// Dotted.
    Dotted,
    /// Double.
    Double,
}

/// How a cell looks: font, fill, borders, alignment, number format and protection. What is not set
/// stays the workbook's default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Format {
    font_name: Option<String>,
    font_size_tenths: Option<u32>,
    bold: bool,
    italic: bool,
    underline: bool,
    font_color: Option<u32>,
    fill: Option<u32>,
    borders: [Option<(BorderStyle, Option<u32>)>; 4],
    align: Option<Align>,
    vertical: Option<VerticalAlign>,
    wrap: bool,
    rotation: Option<i16>,
    indent: Option<u8>,
    num_format: Option<String>,
    locked: Option<bool>,
    formula_hidden: bool,
}

impl Format {
    /// A format that sets nothing.
    pub fn new() -> Self {
        Self::default()
    }
    /// The font's name.
    pub fn font(mut self, name: impl Into<String>) -> Self {
        self.font_name = Some(name.into());
        self
    }
    /// The font's size in points, to a tenth.
    pub fn font_size(mut self, size: f64) -> Self {
        self.font_size_tenths = Some((size.clamp(1.0, 409.0) * 10.0).round() as u32);
        self
    }
    /// Bold text.
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    /// Italic text.
    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    /// Underlined text.
    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
    /// The text's color, as `0xRRGGBB`.
    pub fn font_color(mut self, rgb: u32) -> Self {
        self.font_color = Some(rgb & 0xFF_FFFF);
        self
    }
    /// A solid fill, as `0xRRGGBB`.
    pub fn fill(mut self, rgb: u32) -> Self {
        self.fill = Some(rgb & 0xFF_FFFF);
        self
    }
    /// A thin border on all four sides.
    pub fn border_thin(self) -> Self {
        self.border(BorderStyle::Thin, None)
    }
    /// A border on all four sides, in a color as `0xRRGGBB` if given.
    pub fn border(mut self, style: BorderStyle, rgb: Option<u32>) -> Self {
        self.borders = [Some((style, rgb.map(|c| c & 0xFF_FFFF))); 4];
        self
    }
    /// Horizontal alignment.
    pub fn align(mut self, align: Align) -> Self {
        self.align = Some(align);
        self
    }
    /// Vertical alignment.
    pub fn vertical_align(mut self, align: VerticalAlign) -> Self {
        self.vertical = Some(align);
        self
    }
    /// Wrapped text.
    pub fn wrap(mut self) -> Self {
        self.wrap = true;
        self
    }
    /// Rotated text, from -90 to 90 degrees.
    pub fn rotation(mut self, degrees: i16) -> Self {
        self.rotation = Some(degrees.clamp(-90, 90));
        self
    }
    /// An indent, in steps.
    pub fn indent(mut self, steps: u8) -> Self {
        self.indent = Some(steps);
        self
    }
    /// A number format such as `#,##0.00`.
    pub fn num_format(mut self, code: impl Into<String>) -> Self {
        self.num_format = Some(code.into());
        self
    }
    /// Whether sheet protection locks the cell. Cells are locked by default.
    pub fn locked(mut self, locked: bool) -> Self {
        self.locked = Some(locked);
        self
    }
    /// Hide the cell's formula under sheet protection.
    pub fn formula_hidden(mut self) -> Self {
        self.formula_hidden = true;
        self
    }
}

#[derive(Debug, Clone)]
struct Cell {
    value: CellValue,
    format: Format,
}

/// A column's or row's settings.
#[derive(Debug, Clone, Default, PartialEq)]
struct Dimension {
    size: Option<f64>,
    hidden: bool,
    outline: u8,
    format: Option<Format>,
}

/// A page's orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Taller than wide.
    Portrait,
    /// Wider than tall.
    Landscape,
}

/// A sheet's print setup.
#[derive(Debug, Clone, Default, PartialEq)]
struct PrintSetup {
    orientation: Option<Orientation>,
    paper_size: Option<u16>,
    /// Left, right, top, bottom, header and footer, in inches.
    margins: Option<[f64; 6]>,
    fit_to: Option<(u16, u16)>,
    area: Option<Range>,
    title_rows: Option<(u32, u32)>,
    header: Option<String>,
    footer: Option<String>,
}

#[derive(Debug, Clone)]
struct Validation {
    range: Range,
    items: Vec<String>,
}

#[derive(Debug, Clone)]
struct Hyperlink {
    cell: (u32, u32),
    url: String,
}

#[derive(Debug, Clone)]
struct Comment {
    cell: (u32, u32),
    text: String,
    author: String,
}

/// A worksheet of a [`Workbook`].
#[derive(Debug, Clone)]
pub struct Worksheet {
    name: String,
    cells: BTreeMap<(u32, u32), Cell>,
    merges: Vec<Range>,
    columns: BTreeMap<u32, Dimension>,
    rows: BTreeMap<u32, Dimension>,
    hidden: bool,
    tab_color: Option<u32>,
    frozen: Option<(u32, u32)>,
    zoom: Option<u16>,
    show_gridlines: bool,
    autofilter: Option<Range>,
    validations: Vec<Validation>,
    hyperlinks: Vec<Hyperlink>,
    comments: Vec<Comment>,
    print: PrintSetup,
}

impl Worksheet {
    fn new(name: String) -> Self {
        Self {
            name,
            cells: BTreeMap::new(),
            merges: Vec::new(),
            columns: BTreeMap::new(),
            rows: BTreeMap::new(),
            hidden: false,
            tab_color: None,
            frozen: None,
            zoom: None,
            show_gridlines: true,
            autofilter: None,
            validations: Vec::new(),
            hyperlinks: Vec::new(),
            comments: Vec::new(),
            print: PrintSetup::default(),
        }
    }

    /// The sheet's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Writes a value to a cell such as `B3`, replacing what it held.
    pub fn write(&mut self, cell: &str, value: impl Into<CellValue>, format: &Format) -> Result<()> {
        let (row, column) = parse_cell(cell)?;
        self.write_at(row, column, value, format)
    }

    /// Writes a value to a cell by its row and column, from 1.
    pub fn write_at(&mut self, row: u32, column: u32, value: impl Into<CellValue>, format: &Format) -> Result<()> {
        if row == 0 || row as usize > MAX_ROWS || column == 0 || column as usize > MAX_COLUMNS {
            return Err(invalid(format!("row {row}, column {column} is outside a sheet")));
        }
        let value = value.into();
        check_value(&value, row, column)?;
        self.cells.insert((row, column), Cell { value, format: format.clone() });
        Ok(())
    }

    /// The value a cell holds, if any.
    pub fn value(&self, cell: &str) -> Result<Option<&CellValue>> {
        Ok(self.cells.get(&parse_cell(cell)?).map(|c| &c.value))
    }

    /// Merges a range such as `B1:E1`. Merged ranges may not overlap.
    pub fn merge(&mut self, range: &str) -> Result<()> {
        let range = Range::parse(range)?;
        if range.first_row == range.last_row && range.first_column == range.last_column {
            return Err(invalid(format!("merging the single cell {}", range.text())));
        }
        if let Some(other) = self.merges.iter().find(|m| m.overlaps(&range)) {
            return Err(invalid(format!("merged range {} overlaps {}", range.text(), other.text())));
        }
        self.merges.push(range);
        Ok(())
    }

    /// Merges a range and writes a value to it. The format applies to every cell, so that borders
    /// and fills cover the range.
    pub fn merge_write(&mut self, range: &str, value: impl Into<CellValue>, format: &Format) -> Result<()> {
        self.merge(range)?;
        let parsed = Range::parse(range)?;
        for row in parsed.first_row..=parsed.last_row {
            for column in parsed.first_column..=parsed.last_column {
                self.cells.insert((row, column), Cell { value: CellValue::Blank, format: format.clone() });
            }
        }
        self.write_at(parsed.first_row, parsed.first_column, value, format)
    }

    /// Unmerges a range. Returns whether it was merged.
    pub fn unmerge(&mut self, range: &str) -> Result<bool> {
        let range = Range::parse(range)?;
        let before = self.merges.len();
        self.merges.retain(|m| *m != range);
        Ok(self.merges.len() != before)
    }

    /// The sheet's merged ranges, in the order they were merged.
    pub fn merged_ranges(&self) -> Vec<String> {
        self.merges.iter().map(Range::text).collect()
    }

    fn column_entry(&mut self, column: &str) -> Result<&mut Dimension> {
        Ok(self.columns.entry(parse_column(column)?).or_default())
    }

    fn row_entry(&mut self, row: u32) -> Result<&mut Dimension> {
        if row == 0 || row as usize > MAX_ROWS {
            return Err(invalid(format!("row {row} is outside a sheet")));
        }
        Ok(self.rows.entry(row).or_default())
    }

    /// A column's width in characters, up to 255.
    pub fn set_column_width(&mut self, column: &str, width: f64) -> Result<()> {
        if !(0.0..=255.0).contains(&width) {
            return Err(invalid(format!("a column width of {width} is not from 0 to 255")));
        }
        self.column_entry(column)?.size = Some(width);
        Ok(())
    }
    /// Hides or shows a column.
    pub fn set_column_hidden(&mut self, column: &str, hidden: bool) -> Result<()> {
        self.column_entry(column)?.hidden = hidden;
        Ok(())
    }
    /// A column's outline level, up to 7.
    pub fn set_column_outline(&mut self, column: &str, level: u8) -> Result<()> {
        if level > 7 {
            return Err(invalid(format!("an outline level of {level} is above 7")));
        }
        self.column_entry(column)?.outline = level;
        Ok(())
    }
    /// The format of a column's empty cells.
    pub fn set_column_format(&mut self, column: &str, format: &Format) -> Result<()> {
        self.column_entry(column)?.format = Some(format.clone());
        Ok(())
    }
    /// A row's height in points, up to 409.
    pub fn set_row_height(&mut self, row: u32, height: f64) -> Result<()> {
        if !(0.0..=409.0).contains(&height) {
            return Err(invalid(format!("a row height of {height} is not from 0 to 409")));
        }
        self.row_entry(row)?.size = Some(height);
        Ok(())
    }
    /// Hides or shows a row.
    pub fn set_row_hidden(&mut self, row: u32, hidden: bool) -> Result<()> {
        self.row_entry(row)?.hidden = hidden;
        Ok(())
    }
    /// A row's outline level, up to 7.
    pub fn set_row_outline(&mut self, row: u32, level: u8) -> Result<()> {
        if level > 7 {
            return Err(invalid(format!("an outline level of {level} is above 7")));
        }
        self.row_entry(row)?.outline = level;
        Ok(())
    }
    /// The format of a row's empty cells.
    pub fn set_row_format(&mut self, row: u32, format: &Format) -> Result<()> {
        self.row_entry(row)?.format = Some(format.clone());
        Ok(())
    }

    /// Hides or shows the sheet's tab.
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }
    /// The tab's color, as `0xRRGGBB`.
    pub fn set_tab_color(&mut self, rgb: u32) {
        self.tab_color = Some(rgb & 0xFF_FFFF);
    }
    /// Keeps this many rows and columns in view when scrolling. `freeze_panes(4, 1)` keeps rows 1-4
    /// and column A. Zeros for both remove the freeze.
    pub fn freeze_panes(&mut self, rows: u32, columns: u32) -> Result<()> {
        if rows as usize >= MAX_ROWS || columns as usize >= MAX_COLUMNS {
            return Err(invalid(format!("cannot freeze {rows} rows and {columns} columns")));
        }
        self.frozen = (rows > 0 || columns > 0).then_some((rows, columns));
        Ok(())
    }
    /// The view's zoom, from 10 to 400 percent.
    pub fn set_zoom(&mut self, percent: u16) -> Result<()> {
        if !(10..=400).contains(&percent) {
            return Err(invalid(format!("a zoom of {percent} is not from 10 to 400")));
        }
        self.zoom = Some(percent);
        Ok(())
    }
    /// Shows or hides the gridlines.
    pub fn set_show_gridlines(&mut self, show: bool) {
        self.show_gridlines = show;
    }
    /// Adds filter buttons to the top row of a range.
    pub fn autofilter(&mut self, range: &str) -> Result<()> {
        self.autofilter = Some(Range::parse(range)?);
        Ok(())
    }
    /// Limits a range's cells to a list of values.
    pub fn validate_list<S: AsRef<str>>(&mut self, range: &str, items: &[S]) -> Result<()> {
        let items: Vec<String> = items.iter().map(|s| s.as_ref().to_string()).collect();
        if items.is_empty() || items.iter().any(|item| item.contains(',')) {
            return Err(invalid("a list validation needs items without commas".into()));
        }
        // Excel's limit on a list written into the rule itself.
        if items.join(",").encode_utf16().count() > 255 {
            return Err(invalid("a list validation's items are more than 255 characters together".into()));
        }
        self.validations.push(Validation { range: Range::parse(range)?, items });
        Ok(())
    }
    /// Links a cell to a URL.
    pub fn hyperlink(&mut self, cell: &str, url: &str) -> Result<()> {
        let cell = parse_cell(cell)?;
        self.hyperlinks.retain(|h| h.cell != cell);
        self.hyperlinks.push(Hyperlink { cell, url: url.to_string() });
        Ok(())
    }
    /// Attaches a comment to a cell.
    pub fn comment(&mut self, cell: &str, text: &str, author: &str) -> Result<()> {
        let cell = parse_cell(cell)?;
        self.comments.retain(|c| c.cell != cell);
        self.comments.push(Comment { cell, text: text.to_string(), author: author.to_string() });
        Ok(())
    }

    /// The page's orientation.
    pub fn print_orientation(&mut self, orientation: Orientation) {
        self.print.orientation = Some(orientation);
    }
    /// The paper's size, by its SpreadsheetML code: 1 is Letter, 9 is A4.
    pub fn print_paper_size(&mut self, code: u16) {
        self.print.paper_size = Some(code);
    }
    /// The page's margins in inches: left, right, top, bottom, header, footer.
    pub fn print_margins(&mut self, left: f64, right: f64, top: f64, bottom: f64, header: f64, footer: f64) -> Result<()> {
        let margins = [left, right, top, bottom, header, footer];
        if margins.iter().any(|m| !m.is_finite() || *m < 0.0) {
            return Err(invalid("page margins must not be negative".into()));
        }
        self.print.margins = Some(margins);
        Ok(())
    }
    /// Fits the print to pages across and down; 0 leaves that direction free.
    pub fn print_fit_to(&mut self, width: u16, height: u16) {
        self.print.fit_to = Some((width, height));
    }
    /// Fits the print to a number of pages across.
    pub fn print_fit_to_width(&mut self, pages: u16) {
        self.print_fit_to(pages, 0);
    }
    /// The range to print.
    pub fn print_area(&mut self, range: &str) -> Result<()> {
        self.print.area = Some(Range::parse(range)?);
        Ok(())
    }
    /// Rows to repeat at the top of each page, from 1.
    pub fn print_title_rows(&mut self, first: u32, last: u32) -> Result<()> {
        if first == 0 || last < first || last as usize > MAX_ROWS {
            return Err(invalid(format!("rows {first} to {last} are not title rows")));
        }
        self.print.title_rows = Some((first, last));
        Ok(())
    }
    /// The page header, in SpreadsheetML's codes such as `&CTitle`.
    pub fn print_header(&mut self, text: &str) {
        self.print.header = Some(text.to_string());
    }
    /// The page footer, in SpreadsheetML's codes such as `&RPage &P`.
    pub fn print_footer(&mut self, text: &str) {
        self.print.footer = Some(text.to_string());
    }
}

/// Refuses what a cell cannot hold.
fn check_value(value: &CellValue, row: u32, column: u32) -> Result<()> {
    let at = || cell_name(row, column);
    match value {
        CellValue::Text(s) if s.encode_utf16().count() > MAX_CELL_TEXT_UTF16 => {
            Err(invalid(format!("cell {} holds more than the {MAX_CELL_TEXT_UTF16} characters a cell can", at())))
        }
        CellValue::Number(v) if !v.is_finite() => Err(invalid(format!("cell {} holds a number that is not finite", at()))),
        CellValue::Date(s) if datetime::parse_date(s).and_then(|d| excel_serial_days(i64::from(d))).is_none() => {
            Err(invalid(format!("cell {} holds '{s}', not a date from 1900 on", at())))
        }
        CellValue::DateTime(s) if wall_clock_millis(s).and_then(|ms| excel_serial_days(ms.div_euclid(86_400_000))).is_none() => {
            Err(invalid(format!("cell {} holds '{s}', not a date-time from 1900 on", at())))
        }
        CellValue::Formula { formula, .. } if formula.is_empty() => Err(invalid(format!("cell {} holds an empty formula", at()))),
        CellValue::Formula { cached: Some(cached), .. } => match **cached {
            CellValue::Formula { .. } => Err(invalid(format!("cell {} caches a formula", at()))),
            _ => check_value(cached, row, column),
        },
        _ => Ok(()),
    }
}

/// A workbook: its sheets and document properties.
#[derive(Debug, Clone, Default)]
pub struct Workbook {
    sheets: Vec<Worksheet>,
    creation_time: Option<String>,
    title: Option<String>,
    author: Option<String>,
    defined_names: Vec<(String, String)>,
}

impl Workbook {
    /// A workbook without sheets.
    pub fn new() -> Self {
        Self::default()
    }

    /// The sheets' names, in order.
    pub fn sheet_names(&self) -> Vec<&str> {
        self.sheets.iter().map(|s| s.name.as_str()).collect()
    }

    /// Adds a sheet at the end.
    pub fn add_sheet(&mut self, name: &str) -> Result<&mut Worksheet> {
        self.check_new_name(name)?;
        self.sheets.push(Worksheet::new(name.to_string()));
        Ok(self.sheets.last_mut().expect("a sheet was just added"))
    }

    /// A sheet by its name.
    pub fn sheet(&self, name: &str) -> Result<&Worksheet> {
        self.sheets.iter().find(|s| s.name == name).ok_or_else(|| invalid(format!("no sheet named '{name}'")))
    }

    /// A sheet by its name, to change.
    pub fn sheet_mut(&mut self, name: &str) -> Result<&mut Worksheet> {
        self.sheets.iter_mut().find(|s| s.name == name).ok_or_else(|| invalid(format!("no sheet named '{name}'")))
    }

    /// Renames a sheet. Formulas that name it are not rewritten.
    pub fn rename_sheet(&mut self, name: &str, new_name: &str) -> Result<()> {
        self.sheet(name)?;
        if !name.eq_ignore_ascii_case(new_name) {
            self.check_new_name(new_name)?;
        } else {
            check_sheet_name(new_name)?;
        }
        self.sheet_mut(name)?.name = new_name.to_string();
        Ok(())
    }

    /// Moves a sheet to a position, from 0.
    pub fn move_sheet(&mut self, name: &str, index: usize) -> Result<()> {
        let from = self.sheets.iter().position(|s| s.name == name).ok_or_else(|| invalid(format!("no sheet named '{name}'")))?;
        if index >= self.sheets.len() {
            return Err(invalid(format!("position {index} is past the {} sheets", self.sheets.len())));
        }
        let sheet = self.sheets.remove(from);
        self.sheets.insert(index, sheet);
        Ok(())
    }

    /// Removes a sheet. Returns whether there was one of that name.
    pub fn remove_sheet(&mut self, name: &str) -> bool {
        let before = self.sheets.len();
        self.sheets.retain(|s| s.name != name);
        self.sheets.len() != before
    }

    fn check_new_name(&self, name: &str) -> Result<()> {
        check_sheet_name(name)?;
        let lower = name.to_lowercase();
        if self.sheets.iter().any(|s| s.name.to_lowercase() == lower) {
            return Err(invalid(format!("a sheet named '{name}' exists")));
        }
        Ok(())
    }

    /// The creation time recorded in the properties, as ISO 8601. The same workbook with a fixed time
    /// writes the same bytes. Without one, none is recorded.
    pub fn set_creation_time(&mut self, time: &str) -> Result<()> {
        if !super::xlsx_options::time_in_range(time) || datetime::parse_datetime(time).is_none() {
            return Err(invalid(format!("creation time '{time}' is not an ISO 8601 date-time")));
        }
        self.creation_time = Some(time.to_string());
        Ok(())
    }
    /// The document's title.
    pub fn set_title(&mut self, title: &str) {
        self.title = Some(title.to_string());
    }
    /// The document's author.
    pub fn set_author(&mut self, author: &str) {
        self.author = Some(author.to_string());
    }
    /// Defines a name for a reference such as `'Sheet 1'!$A$1:$B$2`.
    pub fn define_name(&mut self, name: &str, reference: &str) -> Result<()> {
        let valid = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
            && !name.to_ascii_lowercase().starts_with("_xlnm.");
        if !valid || reference.is_empty() {
            return Err(invalid(format!("'{name}' is not a name to define")));
        }
        self.defined_names.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.defined_names.push((name.to_string(), reference.to_string()));
        Ok(())
    }

    /// Writes the workbook to a file.
    pub fn save(&self, path: &str) -> Result<()> {
        let bytes = self.to_bytes()?;
        crate::vsi::write_local_atomic(std::path::Path::new(path), |out| {
            out.write_all(&bytes)?;
            Ok(())
        })
    }

    /// Encodes the workbook. The same workbook makes the same bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        if self.sheets.is_empty() {
            return Err(invalid("a workbook needs a sheet".into()));
        }
        if self.sheets.iter().all(|s| s.hidden) {
            return Err(invalid("a workbook needs a visible sheet".into()));
        }
        write_package(self)
    }
}

fn check_sheet_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.encode_utf16().count() > MAX_SHEET_NAME_UTF16
        || name.starts_with('\'')
        || name.ends_with('\'')
        || name.chars().any(|c| "[]:*?/\\".contains(c) || (c as u32) < 0x20 || matches!(c, '\u{FFFE}' | '\u{FFFF}'));
    if bad {
        return Err(invalid(format!("'{name}' is not a sheet name: up to 31 characters, none of []:*?/\\, no quote at either end")));
    }
    Ok(())
}

/// The fonts, fills, borders, number formats and cell formats a workbook uses.
struct StyleTable {
    fonts: Vec<String>,
    fills: Vec<String>,
    borders: Vec<String>,
    num_formats: Vec<String>,
    xfs: Vec<String>,
    by_format: HashMap<Format, usize>,
}

const DEFAULT_FONT: &str = "<font><sz val=\"11\"/><name val=\"Calibri\"/><family val=\"2\"/></font>";
const DEFAULT_BORDER: &str = "<border><left/><right/><top/><bottom/><diagonal/></border>";
const DATE_FORMAT: &str = "yyyy-mm-dd";
const DATETIME_FORMAT: &str = "yyyy-mm-dd hh:mm:ss";

fn index_of(list: &mut Vec<String>, item: String) -> usize {
    match list.iter().position(|x| *x == item) {
        Some(i) => i,
        None => {
            list.push(item);
            list.len() - 1
        }
    }
}

fn rgb(color: u32) -> String {
    format!("FF{color:06X}")
}

impl StyleTable {
    fn new() -> Self {
        Self {
            fonts: vec![DEFAULT_FONT.to_string()],
            fills: vec![
                "<fill><patternFill patternType=\"none\"/></fill>".into(),
                "<fill><patternFill patternType=\"gray125\"/></fill>".into(),
            ],
            borders: vec![DEFAULT_BORDER.to_string()],
            num_formats: Vec::new(),
            xfs: vec!["<xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>".into()],
            by_format: HashMap::from([(Format::default(), 0)]),
        }
    }

    /// The index of a format's cell style, adding it if new.
    fn index(&mut self, format: &Format) -> usize {
        if let Some(&i) = self.by_format.get(format) {
            return i;
        }
        let font = self.font_id(format);
        let fill = format.fill.map_or(0, |color| {
            let xml = format!(
                "<fill><patternFill patternType=\"solid\"><fgColor rgb=\"{}\"/><bgColor indexed=\"64\"/></patternFill></fill>",
                rgb(color)
            );
            index_of(&mut self.fills, xml)
        });
        let border = self.border_id(format);
        let num_format = format.num_format.as_ref().map_or(0, |code| 164 + index_of(&mut self.num_formats, code.clone()));
        let mut xf = format!("<xf numFmtId=\"{num_format}\" fontId=\"{font}\" fillId=\"{fill}\" borderId=\"{border}\" xfId=\"0\"");
        for (used, attribute) in
            [(num_format != 0, "applyNumberFormat"), (font != 0, "applyFont"), (fill != 0, "applyFill"), (border != 0, "applyBorder")]
        {
            if used {
                let _ = write!(xf, " {attribute}=\"1\"");
            }
        }
        let alignment = alignment_xml(format);
        let protection = (format.locked.is_some() || format.formula_hidden).then(|| {
            let mut p = String::from("<protection");
            if let Some(locked) = format.locked {
                let _ = write!(p, " locked=\"{}\"", u8::from(locked));
            }
            if format.formula_hidden {
                p.push_str(" hidden=\"1\"");
            }
            p + "/>"
        });
        if !alignment.is_empty() {
            xf.push_str(" applyAlignment=\"1\"");
        }
        if protection.is_some() {
            xf.push_str(" applyProtection=\"1\"");
        }
        if alignment.is_empty() && protection.is_none() {
            xf.push_str("/>");
        } else {
            let _ = write!(xf, ">{alignment}{}</xf>", protection.unwrap_or_default());
        }
        let i = index_of(&mut self.xfs, xf);
        self.by_format.insert(format.clone(), i);
        i
    }

    fn font_id(&mut self, f: &Format) -> usize {
        if f.font_name.is_none() && f.font_size_tenths.is_none() && !f.bold && !f.italic && !f.underline && f.font_color.is_none() {
            return 0;
        }
        let mut xml = String::from("<font>");
        if f.bold {
            xml.push_str("<b/>");
        }
        if f.italic {
            xml.push_str("<i/>");
        }
        if f.underline {
            xml.push_str("<u/>");
        }
        let size = f.font_size_tenths.unwrap_or(110);
        let _ = write!(xml, "<sz val=\"{}\"/>", f64::from(size) / 10.0);
        if let Some(color) = f.font_color {
            let _ = write!(xml, "<color rgb=\"{}\"/>", rgb(color));
        }
        let _ = write!(xml, "<name val=\"{}\"/><family val=\"2\"/></font>", xml_text(f.font_name.as_deref().unwrap_or("Calibri")));
        index_of(&mut self.fonts, xml)
    }

    fn border_id(&mut self, f: &Format) -> usize {
        if f.borders.iter().all(Option::is_none) {
            return 0;
        }
        let mut xml = String::from("<border>");
        for (side, border) in ["left", "right", "top", "bottom"].iter().zip(&f.borders) {
            match border {
                None => {
                    let _ = write!(xml, "<{side}/>");
                }
                Some((style, color)) => {
                    let name = match style {
                        BorderStyle::Thin => "thin",
                        BorderStyle::Medium => "medium",
                        BorderStyle::Thick => "thick",
                        BorderStyle::Dashed => "dashed",
                        BorderStyle::Dotted => "dotted",
                        BorderStyle::Double => "double",
                    };
                    let color = color.map_or_else(|| "<color auto=\"1\"/>".to_string(), |c| format!("<color rgb=\"{}\"/>", rgb(c)));
                    let _ = write!(xml, "<{side} style=\"{name}\">{color}</{side}>");
                }
            }
        }
        xml.push_str("<diagonal/></border>");
        index_of(&mut self.borders, xml)
    }

    fn to_xml(&self) -> String {
        let mut out = format!("{XML_HEADER}<styleSheet xmlns=\"{MAIN_NS}\">\n");
        if !self.num_formats.is_empty() {
            let _ = write!(out, "<numFmts count=\"{}\">", self.num_formats.len());
            for (i, code) in self.num_formats.iter().enumerate() {
                let _ = write!(out, "<numFmt numFmtId=\"{}\" formatCode=\"{}\"/>", 164 + i, xml_text(code));
            }
            out.push_str("</numFmts>\n");
        }
        let _ = writeln!(out, "<fonts count=\"{}\">{}</fonts>", self.fonts.len(), self.fonts.concat());
        let _ = writeln!(out, "<fills count=\"{}\">{}</fills>", self.fills.len(), self.fills.concat());
        let _ = writeln!(out, "<borders count=\"{}\">{}</borders>", self.borders.len(), self.borders.concat());
        out.push_str("<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\n");
        let _ = writeln!(out, "<cellXfs count=\"{}\">{}</cellXfs>", self.xfs.len(), self.xfs.concat());
        out.push_str("<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>\n</styleSheet>\n");
        out
    }
}

fn alignment_xml(f: &Format) -> String {
    let mut xml = String::new();
    if let Some(align) = f.align {
        let name = match align {
            Align::Left => "left",
            Align::Center => "center",
            Align::Right => "right",
            Align::Fill => "fill",
            Align::Justify => "justify",
        };
        let _ = write!(xml, " horizontal=\"{name}\"");
    }
    if let Some(vertical) = f.vertical {
        let name = match vertical {
            VerticalAlign::Top => "top",
            VerticalAlign::Center => "center",
            VerticalAlign::Bottom => "bottom",
        };
        let _ = write!(xml, " vertical=\"{name}\"");
    }
    if let Some(degrees) = f.rotation {
        // SpreadsheetML: 0 to 90 upward, 91 to 180 for -1 to -90.
        let value = if degrees < 0 { 90 - i32::from(degrees) } else { i32::from(degrees) };
        let _ = write!(xml, " textRotation=\"{value}\"");
    }
    if f.wrap {
        xml.push_str(" wrapText=\"1\"");
    }
    if let Some(indent) = f.indent {
        let _ = write!(xml, " indent=\"{indent}\"");
    }
    if xml.is_empty() { xml } else { format!("<alignment{xml}/>") }
}

/// A date or date-time cell's serial number.
fn serial(value: &CellValue) -> Option<f64> {
    match value {
        CellValue::Date(s) => excel_serial_days(i64::from(datetime::parse_date(s)?)).map(|d| d as f64),
        CellValue::DateTime(s) => {
            let ms = wall_clock_millis(s)?;
            let days = excel_serial_days(ms.div_euclid(86_400_000))?;
            Some(days as f64 + ms.rem_euclid(86_400_000) as f64 / 86_400_000.0)
        }
        _ => None,
    }
}

/// The attributes and body of a value's cell.
fn value_xml(value: &CellValue, strings: &mut StringTable) -> (&'static str, String) {
    match value {
        CellValue::Blank => ("", String::new()),
        CellValue::Text(s) => (" t=\"s\"", format!("<v>{}</v>", strings.index(s))),
        CellValue::Number(v) => ("", format!("<v>{v}</v>")),
        CellValue::Boolean(b) => (" t=\"b\"", format!("<v>{}</v>", u8::from(*b))),
        CellValue::Date(_) | CellValue::DateTime(_) => ("", format!("<v>{}</v>", serial(value).unwrap_or_default())),
        CellValue::Error(e) => (" t=\"e\"", format!("<v>{}</v>", xml_text(e))),
        CellValue::Formula { formula, cached } => {
            let f = format!("<f>{}</f>", xml_text(formula.trim_start_matches('=')));
            match cached.as_deref() {
                None | Some(CellValue::Blank) | Some(CellValue::Formula { .. }) => ("", f),
                Some(CellValue::Text(s)) => (" t=\"str\"", format!("{f}<v>{}</v>", xml_text(s))),
                Some(CellValue::Boolean(b)) => (" t=\"b\"", format!("{f}<v>{}</v>", u8::from(*b))),
                Some(CellValue::Error(e)) => (" t=\"e\"", format!("{f}<v>{}</v>", xml_text(e))),
                Some(number) => {
                    let v = match number {
                        CellValue::Number(v) => *v,
                        other => serial(other).unwrap_or_default(),
                    };
                    ("", format!("{f}<v>{v}</v>"))
                }
            }
        }
    }
}

/// A cell's format, with the date format a date needs when it has no number format.
fn effective_format(cell: &Cell) -> std::borrow::Cow<'_, Format> {
    let date = |v: &CellValue| match v {
        CellValue::Date(_) => Some(DATE_FORMAT),
        CellValue::DateTime(_) => Some(DATETIME_FORMAT),
        _ => None,
    };
    let code = match &cell.value {
        CellValue::Formula { cached: Some(cached), .. } => date(cached),
        value => date(value),
    };
    match code {
        Some(code) if cell.format.num_format.is_none() => std::borrow::Cow::Owned(cell.format.clone().num_format(code)),
        _ => std::borrow::Cow::Borrowed(&cell.format),
    }
}

/// A sheet's parts besides its XML, which refers to them.
struct SheetRelations {
    /// Relationship type (after `OD_RELS/`), target, and whether it is outside the package.
    relationships: Vec<(&'static str, String, bool)>,
}

fn sheet_xml(sheet: &Worksheet, number: usize, styles: &mut StyleTable, strings: &mut StringTable) -> (String, SheetRelations) {
    let mut out = format!("{XML_HEADER}<worksheet xmlns=\"{MAIN_NS}\" xmlns:r=\"{OD_RELS}\">\n");
    let mut relations = SheetRelations { relationships: Vec::new() };
    let fit = sheet.print.fit_to.is_some();
    if sheet.tab_color.is_some() || fit {
        out.push_str("<sheetPr>");
        if let Some(color) = sheet.tab_color {
            let _ = write!(out, "<tabColor rgb=\"{}\"/>", rgb(color));
        }
        if fit {
            out.push_str("<pageSetUpPr fitToPage=\"1\"/>");
        }
        out.push_str("</sheetPr>\n");
    }
    if sheet.frozen.is_some() || sheet.zoom.is_some() || !sheet.show_gridlines {
        out.push_str("<sheetViews><sheetView");
        if !sheet.show_gridlines {
            out.push_str(" showGridLines=\"0\"");
        }
        if let Some(zoom) = sheet.zoom {
            let _ = write!(out, " zoomScale=\"{zoom}\" zoomScaleNormal=\"{zoom}\"");
        }
        out.push_str(" workbookViewId=\"0\">");
        if let Some((rows, columns)) = sheet.frozen {
            let pane = match (rows > 0, columns > 0) {
                (true, true) => "bottomRight",
                (true, false) => "bottomLeft",
                _ => "topRight",
            };
            out.push_str("<pane");
            if columns > 0 {
                let _ = write!(out, " xSplit=\"{columns}\"");
            }
            if rows > 0 {
                let _ = write!(out, " ySplit=\"{rows}\"");
            }
            let _ = write!(
                out,
                " topLeftCell=\"{}\" activePane=\"{pane}\" state=\"frozen\"/><selection pane=\"{pane}\"/>",
                cell_name(rows + 1, columns + 1)
            );
        }
        out.push_str("</sheetView></sheetViews>\n");
    }
    let outline_rows = sheet.rows.values().map(|d| d.outline).max().unwrap_or(0);
    let outline_columns = sheet.columns.values().map(|d| d.outline).max().unwrap_or(0);
    out.push_str("<sheetFormatPr defaultRowHeight=\"15\"");
    if outline_rows > 0 {
        let _ = write!(out, " outlineLevelRow=\"{outline_rows}\"");
    }
    if outline_columns > 0 {
        let _ = write!(out, " outlineLevelCol=\"{outline_columns}\"");
    }
    out.push_str("/>\n");
    if !sheet.columns.is_empty() {
        out.push_str("<cols>");
        for (&column, d) in &sheet.columns {
            // A `col` needs a width; 8.43 is the sheet's default.
            let _ = write!(out, "<col min=\"{column}\" max=\"{column}\" width=\"{}\"", d.size.unwrap_or(8.43));
            if d.size.is_some() {
                out.push_str(" customWidth=\"1\"");
            }
            if let Some(format) = &d.format {
                let _ = write!(out, " style=\"{}\"", styles.index(format));
            }
            if d.hidden {
                out.push_str(" hidden=\"1\"");
            }
            if d.outline > 0 {
                let _ = write!(out, " outlineLevel=\"{}\"", d.outline);
            }
            out.push_str("/>");
        }
        out.push_str("</cols>\n");
    }
    out.push_str("<sheetData>\n");
    let mut rows: Vec<u32> = sheet.rows.keys().copied().chain(sheet.cells.keys().map(|&(row, _)| row)).collect();
    rows.sort_unstable();
    rows.dedup();
    let mut cells = sheet.cells.iter().peekable();
    for row in rows {
        let _ = write!(out, "<row r=\"{row}\"");
        if let Some(d) = sheet.rows.get(&row) {
            if let Some(height) = d.size {
                let _ = write!(out, " ht=\"{height}\" customHeight=\"1\"");
            }
            if let Some(format) = &d.format {
                let _ = write!(out, " s=\"{}\" customFormat=\"1\"", styles.index(format));
            }
            if d.hidden {
                out.push_str(" hidden=\"1\"");
            }
            if d.outline > 0 {
                let _ = write!(out, " outlineLevel=\"{}\"", d.outline);
            }
        }
        out.push('>');
        while let Some((&(cell_row, column), cell)) = cells.peek().copied() {
            if cell_row != row {
                break;
            }
            cells.next();
            let style = styles.index(&effective_format(cell));
            let style = if style == 0 { String::new() } else { format!(" s=\"{style}\"") };
            let (attributes, body) = value_xml(&cell.value, strings);
            let name = cell_name(row, column);
            if body.is_empty() {
                let _ = write!(out, "<c r=\"{name}\"{style}{attributes}/>");
            } else {
                let _ = write!(out, "<c r=\"{name}\"{style}{attributes}>{body}</c>");
            }
        }
        out.push_str("</row>\n");
    }
    out.push_str("</sheetData>\n");
    if let Some(range) = &sheet.autofilter {
        let _ = writeln!(out, "<autoFilter ref=\"{}\"/>", range.text());
    }
    if !sheet.merges.is_empty() {
        let _ = write!(out, "<mergeCells count=\"{}\">", sheet.merges.len());
        for merge in &sheet.merges {
            let _ = write!(out, "<mergeCell ref=\"{}\"/>", merge.text());
        }
        out.push_str("</mergeCells>\n");
    }
    if !sheet.validations.is_empty() {
        let _ = write!(out, "<dataValidations count=\"{}\">", sheet.validations.len());
        for v in &sheet.validations {
            let list = v.items.join(",").replace('"', "\"\"");
            let _ = write!(
                out,
                "<dataValidation type=\"list\" allowBlank=\"1\" showErrorMessage=\"1\" sqref=\"{}\"><formula1>{}</formula1></dataValidation>",
                v.range.text(),
                xml_text(&format!("\"{list}\""))
            );
        }
        out.push_str("</dataValidations>\n");
    }
    if !sheet.hyperlinks.is_empty() {
        out.push_str("<hyperlinks>");
        for link in &sheet.hyperlinks {
            relations.relationships.push(("hyperlink", link.url.clone(), true));
            let _ =
                write!(out, "<hyperlink ref=\"{}\" r:id=\"rId{}\"/>", cell_name(link.cell.0, link.cell.1), relations.relationships.len());
        }
        out.push_str("</hyperlinks>\n");
    }
    let p = &sheet.print;
    if let Some([left, right, top, bottom, header, footer]) = p.margins {
        let _ = writeln!(
            out,
            "<pageMargins left=\"{left}\" right=\"{right}\" top=\"{top}\" bottom=\"{bottom}\" header=\"{header}\" footer=\"{footer}\"/>"
        );
    }
    if p.orientation.is_some() || p.paper_size.is_some() || p.fit_to.is_some() {
        out.push_str("<pageSetup");
        if let Some(code) = p.paper_size {
            let _ = write!(out, " paperSize=\"{code}\"");
        }
        if let Some((width, height)) = p.fit_to {
            let _ = write!(out, " fitToWidth=\"{width}\" fitToHeight=\"{height}\"");
        }
        if let Some(orientation) = p.orientation {
            let _ = write!(out, " orientation=\"{}\"", if orientation == Orientation::Landscape { "landscape" } else { "portrait" });
        }
        out.push_str("/>\n");
    }
    if p.header.is_some() || p.footer.is_some() {
        out.push_str("<headerFooter>");
        if let Some(header) = &p.header {
            let _ = write!(out, "<oddHeader>{}</oddHeader>", xml_text(header));
        }
        if let Some(footer) = &p.footer {
            let _ = write!(out, "<oddFooter>{}</oddFooter>", xml_text(footer));
        }
        out.push_str("</headerFooter>\n");
    }
    if !sheet.comments.is_empty() {
        relations.relationships.push(("comments", format!("../comments{number}.xml"), false));
        relations.relationships.push(("vmlDrawing", format!("../drawings/vmlDrawing{number}.vml"), false));
        let _ = writeln!(out, "<legacyDrawing r:id=\"rId{}\"/>", relations.relationships.len());
    }
    out.push_str("</worksheet>\n");
    (out, relations)
}

/// The comments part of a sheet.
fn comments_xml(sheet: &Worksheet) -> String {
    let mut authors: Vec<&str> = Vec::new();
    let mut list = String::new();
    let mut comments: Vec<&Comment> = sheet.comments.iter().collect();
    comments.sort_by_key(|c| c.cell);
    for comment in comments {
        let author = authors.iter().position(|a| *a == comment.author).unwrap_or_else(|| {
            authors.push(&comment.author);
            authors.len() - 1
        });
        let _ = write!(
            list,
            "<comment ref=\"{}\" authorId=\"{author}\"><text><r><t xml:space=\"preserve\">{}</t></r></text></comment>",
            cell_name(comment.cell.0, comment.cell.1),
            xml_text(&comment.text)
        );
    }
    let authors: String = authors.iter().map(|a| format!("<author>{}</author>", xml_text(a))).collect();
    format!("{XML_HEADER}<comments xmlns=\"{MAIN_NS}\"><authors>{authors}</authors><commentList>{list}</commentList></comments>\n")
}

/// The drawing that shows a sheet's comments as notes, which Excel requires.
fn comments_vml(sheet: &Worksheet) -> String {
    let mut out = String::from(
        "<xml xmlns:v=\"urn:schemas-microsoft-com:vml\" xmlns:o=\"urn:schemas-microsoft-com:office:office\" xmlns:x=\"urn:schemas-microsoft-com:office:excel\">\
         <o:shapelayout v:ext=\"edit\"><o:idmap v:ext=\"edit\" data=\"1\"/></o:shapelayout>\
         <v:shapetype id=\"_x0000_t202\" coordsize=\"21600,21600\" o:spt=\"202\" path=\"m,l,21600r21600,l21600,xe\"><v:stroke joinstyle=\"miter\"/><v:path gradientshapeok=\"t\" o:connecttype=\"rect\"/></v:shapetype>\n",
    );
    let mut comments: Vec<&Comment> = sheet.comments.iter().collect();
    comments.sort_by_key(|c| c.cell);
    for (i, comment) in comments.iter().enumerate() {
        let (row, column) = comment.cell;
        let _ = writeln!(
            out,
            "<v:shape id=\"_x0000_s{}\" type=\"#_x0000_t202\" style=\"position:absolute;margin-left:60pt;margin-top:2pt;width:108pt;height:60pt;z-index:{};visibility:hidden\" fillcolor=\"#ffffe1\" o:insetmode=\"auto\"><v:fill color2=\"#ffffe1\"/><v:shadow on=\"t\" color=\"black\" obscured=\"t\"/><v:path o:connecttype=\"none\"/><x:ClientData ObjectType=\"Note\"><x:MoveWithCells/><x:SizeWithCells/><x:AutoFill>False</x:AutoFill><x:Row>{}</x:Row><x:Column>{}</x:Column></x:ClientData></v:shape>",
            1025 + i,
            i + 1,
            row - 1,
            column - 1
        );
    }
    out.push_str("</xml>\n");
    out
}

fn write_package(workbook: &Workbook) -> Result<Vec<u8>> {
    let mut content_types = format!(
        "{XML_HEADER}<Types xmlns=\"{PACKAGE}/content-types\">\n\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\n\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>\n\
         <Default Extension=\"vml\" ContentType=\"application/vnd.openxmlformats-officedocument.vmlDrawing\"/>\n\
         <Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\n\
         <Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>\n\
         <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\n\
         <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\n\
         <Override PartName=\"/xl/sharedStrings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml\"/>\n"
    );
    let mut sheets_xml = String::new();
    let mut names = String::new();
    let mut rels = format!(
        "{XML_HEADER}<Relationships xmlns=\"{PACKAGE}/relationships\">\n\
         <Relationship Id=\"rId1\" Type=\"{OD_RELS}/styles\" Target=\"styles.xml\"/>\n\
         <Relationship Id=\"rId2\" Type=\"{OD_RELS}/sharedStrings\" Target=\"sharedStrings.xml\"/>\n"
    );
    let mut styles = StyleTable::new();
    let mut strings = StringTable::default();
    let mut parts: Vec<(String, String)> = Vec::new();
    for (i, sheet) in workbook.sheets.iter().enumerate() {
        let number = i + 1;
        let _ = writeln!(
            content_types,
            "<Override PartName=\"/xl/worksheets/sheet{number}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        );
        let state = if sheet.hidden { " state=\"hidden\"" } else { "" };
        let _ = writeln!(sheets_xml, "<sheet name=\"{}\" sheetId=\"{number}\"{state} r:id=\"rId{}\"/>", xml_text(&sheet.name), i + 3);
        let _ = writeln!(rels, "<Relationship Id=\"rId{}\" Type=\"{OD_RELS}/worksheet\" Target=\"worksheets/sheet{number}.xml\"/>", i + 3);
        let quoted = format!("'{}'", sheet.name.replace('\'', "''"));
        let mut name = |defined: &str, hidden: bool, reference: String| {
            let hidden = if hidden { " hidden=\"1\"" } else { "" };
            let _ = writeln!(names, "<definedName name=\"{defined}\" localSheetId=\"{i}\"{hidden}>{}</definedName>", xml_text(&reference));
        };
        if let Some(range) = &sheet.autofilter {
            name("_xlnm._FilterDatabase", true, format!("{quoted}!{}", range.absolute()));
        }
        if let Some(range) = &sheet.print.area {
            name("_xlnm.Print_Area", false, format!("{quoted}!{}", range.absolute()));
        }
        if let Some((first, last)) = sheet.print.title_rows {
            name("_xlnm.Print_Titles", false, format!("{quoted}!${first}:${last}"));
        }
        let (xml, relations) = sheet_xml(sheet, number, &mut styles, &mut strings);
        parts.push((format!("xl/worksheets/sheet{number}.xml"), xml));
        if !relations.relationships.is_empty() {
            let mut sheet_rels = format!("{XML_HEADER}<Relationships xmlns=\"{PACKAGE}/relationships\">\n");
            for (j, (kind, target, external)) in relations.relationships.iter().enumerate() {
                let mode = if *external { " TargetMode=\"External\"" } else { "" };
                let _ = writeln!(
                    sheet_rels,
                    "<Relationship Id=\"rId{}\" Type=\"{OD_RELS}/{kind}\" Target=\"{}\"{mode}/>",
                    j + 1,
                    xml_text(target)
                );
            }
            sheet_rels.push_str("</Relationships>\n");
            parts.push((format!("xl/worksheets/_rels/sheet{number}.xml.rels"), sheet_rels));
        }
        if !sheet.comments.is_empty() {
            let _ = writeln!(
                content_types,
                "<Override PartName=\"/xl/comments{number}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml\"/>"
            );
            parts.push((format!("xl/comments{number}.xml"), comments_xml(sheet)));
            parts.push((format!("xl/drawings/vmlDrawing{number}.vml"), comments_vml(sheet)));
        }
    }
    for (name, reference) in &workbook.defined_names {
        let _ = writeln!(names, "<definedName name=\"{}\">{}</definedName>", xml_text(name), xml_text(reference));
    }
    content_types.push_str("</Types>\n");
    rels.push_str("</Relationships>\n");
    let names = if names.is_empty() { names } else { format!("<definedNames>\n{names}</definedNames>\n") };
    let first_visible = workbook.sheets.iter().position(|s| !s.hidden).unwrap_or(0);
    let book = format!(
        "{XML_HEADER}<workbook xmlns=\"{MAIN_NS}\" xmlns:r=\"{OD_RELS}\">\n<bookViews><workbookView activeTab=\"{first_visible}\"/></bookViews>\n<sheets>\n{sheets_xml}</sheets>\n{names}</workbook>\n"
    );
    let package_rels = format!(
        "{XML_HEADER}<Relationships xmlns=\"{PACKAGE}/relationships\">\n\
         <Relationship Id=\"rId1\" Type=\"{OD_RELS}/officeDocument\" Target=\"xl/workbook.xml\"/>\n\
         <Relationship Id=\"rId2\" Type=\"{PACKAGE}/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\n\
         <Relationship Id=\"rId3\" Type=\"{OD_RELS}/extended-properties\" Target=\"docProps/app.xml\"/>\n\
         </Relationships>\n"
    );
    let mut properties = String::new();
    if let Some(title) = &workbook.title {
        let _ = writeln!(properties, "<dc:title>{}</dc:title>", xml_text(title));
    }
    if let Some(author) = &workbook.author {
        let _ = writeln!(properties, "<dc:creator>{}</dc:creator>", xml_text(author));
    }
    if let Some(time) = &workbook.creation_time {
        let _ = writeln!(properties, "<dcterms:created xsi:type=\"dcterms:W3CDTF\">{}</dcterms:created>", xml_text(time));
    }
    let core = format!(
        "{XML_HEADER}<cp:coreProperties xmlns:cp=\"{PACKAGE}/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:dcterms=\"http://purl.org/dc/terms/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n{properties}</cp:coreProperties>\n"
    );
    let app = format!(
        "{XML_HEADER}<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\">\n<Application>terrio</Application>\n</Properties>\n"
    );
    let mut all: Vec<(String, String)> = vec![
        ("[Content_Types].xml".into(), content_types),
        ("_rels/.rels".into(), package_rels),
        ("docProps/app.xml".into(), app),
        ("docProps/core.xml".into(), core),
        ("xl/workbook.xml".into(), book),
        ("xl/_rels/workbook.xml.rels".into(), rels),
        ("xl/styles.xml".into(), styles.to_xml()),
        ("xl/sharedStrings.xml".into(), strings.to_xml()),
    ];
    all.extend(parts);
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    // A fixed time: the same workbook makes the same bytes.
    let member = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
    for (name, xml) in &all {
        zip.start_file(name.as_str(), member)?;
        zip.write_all(xml.as_bytes())?;
    }
    Ok(zip.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut text = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut text).unwrap();
        text
    }

    fn one_sheet(edit: impl FnOnce(&mut Worksheet)) -> Vec<u8> {
        let mut workbook = Workbook::new();
        edit(workbook.add_sheet("s").unwrap());
        workbook.to_bytes().unwrap()
    }

    #[test]
    fn cells_are_addressed_both_ways_and_keep_their_positions() {
        let plain = Format::new();
        let bytes = one_sheet(|s| {
            s.write("C5", " spaced ", &plain).unwrap();
            s.write_at(2, 1, 1.5, &plain).unwrap();
            assert_eq!(s.value("A2").unwrap(), Some(&CellValue::Number(1.5)));
            assert_eq!(s.value("B9").unwrap(), None);
        });
        let sheet = part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.find("<c r=\"A2\"><v>1.5</v></c>").unwrap() < sheet.find("<c r=\"C5\" t=\"s\">").unwrap());
        assert!(part(&bytes, "xl/sharedStrings.xml").contains("xml:space=\"preserve\"> spaced <"));
    }

    #[test]
    fn invalid_addresses_and_values_are_refused() {
        let plain = Format::new();
        let mut workbook = Workbook::new();
        let s = workbook.add_sheet("s").unwrap();
        for cell in ["", "3", "A0", "A1048577", "XFE1", "A-1"] {
            assert!(s.write(cell, 1.0, &plain).is_err(), "{cell}");
        }
        assert!(s.write("A1", f64::NAN, &plain).is_err());
        assert!(s.write("A1", CellValue::Date("1899-12-31".into()), &plain).is_err());
        assert!(s.write("A1", CellValue::DateTime("soon".into()), &plain).is_err());
        assert!(s.write("A1", "x".repeat(MAX_CELL_TEXT_UTF16 + 1), &plain).is_err());
        assert!(s.write_at(0, 1, 1.0, &plain).is_err());
        assert!(s.set_column_width("A", f64::NAN).is_err());
        assert!(s.set_row_height(1, 500.0).is_err());
        assert!(s.set_row_outline(1, 8).is_err());
        assert!(s.set_zoom(5).is_err());
        assert!(s.print_title_rows(3, 1).is_err());
        assert!(s.validate_list("A1", &["a,b"]).is_err());
    }

    #[test]
    fn sheets_are_added_renamed_moved_and_removed() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("a").unwrap();
        workbook.add_sheet("b").unwrap();
        assert!(workbook.add_sheet("A").is_err());
        for name in ["", "a/b", "'q", "q'", &"x".repeat(32), &"\u{1F600}".repeat(16)] {
            assert!(workbook.add_sheet(name).is_err(), "{name}");
        }
        workbook.rename_sheet("a", "A").unwrap();
        workbook.rename_sheet("b", "c").unwrap();
        assert!(workbook.rename_sheet("c", "a").is_err());
        workbook.move_sheet("c", 0).unwrap();
        assert!(workbook.move_sheet("c", 2).is_err());
        assert_eq!(workbook.sheet_names(), ["c", "A"]);
        assert!(workbook.remove_sheet("A"));
        assert!(!workbook.remove_sheet("A"));
        workbook.sheet_mut("c").unwrap().set_hidden(true);
        assert!(workbook.to_bytes().is_err());
        assert!(Workbook::new().to_bytes().is_err());
    }

    #[test]
    fn merges_may_not_overlap() {
        let mut workbook = Workbook::new();
        let s = workbook.add_sheet("s").unwrap();
        s.merge("A1:B2").unwrap();
        assert!(s.merge("B2:C3").is_err());
        assert!(s.merge("D1").is_err());
        assert!(s.merge("C1:A1").is_err());
        s.merge_write("D1:E1", "t", &Format::new().bold()).unwrap();
        assert_eq!(s.merged_ranges(), ["A1:B2", "D1:E1"]);
        assert!(s.unmerge("A1:B2").unwrap());
        assert!(!s.unmerge("A1:B2").unwrap());
        // The second cell of a merge carries the format, for its borders and fills.
        assert_eq!(s.value("E1").unwrap(), Some(&CellValue::Blank));
    }

    #[test]
    fn formats_are_shared_and_the_default_has_no_style() {
        let head = Format::new().bold().fill(0xD9E1F2).border_thin().align(Align::Center).wrap();
        let bytes = one_sheet(|s| {
            s.write("A1", "a", &head).unwrap();
            s.write("A2", "b", &head.clone()).unwrap();
            s.write("A3", "c", &Format::new()).unwrap();
        });
        let styles = part(&bytes, "xl/styles.xml");
        assert!(styles.contains("<cellXfs count=\"2\">"));
        assert!(styles.contains("<b/>") && styles.contains("rgb=\"FFD9E1F2\"") && styles.contains("style=\"thin\""));
        assert!(styles.contains("horizontal=\"center\"") && styles.contains("wrapText=\"1\""));
        let sheet = part(&bytes, "xl/worksheets/sheet1.xml");
        assert_eq!(sheet.matches(" s=\"1\"").count(), 2);
        assert!(sheet.contains("<c r=\"A3\" t=\"s\">"));
    }

    #[test]
    fn dates_get_a_date_format_unless_they_have_one() {
        let bytes = one_sheet(|s| {
            s.write("A1", CellValue::Date("2024-01-02".into()), &Format::new()).unwrap();
            s.write("A2", CellValue::DateTime("2024-01-02T12:00:00".into()), &Format::new().num_format("d mmm")).unwrap();
        });
        let sheet = part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("<v>45293</v>") && sheet.contains("<v>45293.5</v>"));
        let styles = part(&bytes, "xl/styles.xml");
        assert!(styles.contains("formatCode=\"yyyy-mm-dd\"") && styles.contains("formatCode=\"d mmm\""));
    }

    #[test]
    fn formulas_keep_their_text_and_cached_values() {
        let bytes = one_sheet(|s| {
            s.write("A1", CellValue::formula("=1+1", 2.0), &Format::new()).unwrap();
            s.write("A2", CellValue::formula("\"a\"&\"<b\"", "a<b"), &Format::new()).unwrap();
            s.write("A3", CellValue::Formula { formula: "NOW()".into(), cached: None }, &Format::new()).unwrap();
            s.write("A4", CellValue::Error("#N/A".into()), &Format::new()).unwrap();
        });
        let sheet = part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("<c r=\"A1\"><f>1+1</f><v>2</v></c>"));
        assert!(sheet.contains("<c r=\"A2\" t=\"str\"><f>&quot;a&quot;&amp;&quot;&lt;b&quot;</f><v>a&lt;b</v></c>"), "{sheet}");
        assert!(sheet.contains("<c r=\"A3\"><f>NOW()</f></c>"));
        assert!(sheet.contains("<c r=\"A4\" t=\"e\"><v>#N/A</v></c>"));
    }

    #[test]
    fn sheet_elements_are_in_the_schemas_order() {
        let bytes = one_sheet(|s| {
            s.write("A1", 1.0, &Format::new()).unwrap();
            s.set_tab_color(0xFF0000);
            s.freeze_panes(4, 1).unwrap();
            s.set_zoom(120).unwrap();
            s.set_column_width("A", 15.7).unwrap();
            s.set_row_height(3, 30.0).unwrap();
            s.autofilter("A3:K8").unwrap();
            s.merge("B1:C1").unwrap();
            s.validate_list("A5:A9", &["yes", "no"]).unwrap();
            s.hyperlink("A1", "https://example.com/?a=1&b=2").unwrap();
            s.print_margins(0.5, 0.5, 1.0, 1.0, 0.3, 0.3).unwrap();
            s.print_fit_to_width(1);
            s.print_header("&CTitle");
            s.comment("A1", "note", "me").unwrap();
        });
        let sheet = part(&bytes, "xl/worksheets/sheet1.xml");
        let order = [
            "<sheetPr>",
            "<sheetViews>",
            "<sheetFormatPr",
            "<cols>",
            "<sheetData>",
            "<autoFilter",
            "<mergeCells",
            "<dataValidations",
            "<hyperlinks>",
            "<pageMargins",
            "<pageSetup",
            "<headerFooter>",
            "<legacyDrawing",
        ];
        let places: Vec<usize> = order.iter().map(|tag| sheet.find(tag).unwrap_or_else(|| panic!("{tag}"))).collect();
        assert!(places.windows(2).all(|w| w[0] < w[1]), "{sheet}");
        assert!(sheet.contains("xSplit=\"1\" ySplit=\"4\" topLeftCell=\"B5\" activePane=\"bottomRight\""));
        let rels = part(&bytes, "xl/worksheets/_rels/sheet1.xml.rels");
        assert!(rels.contains("Target=\"https://example.com/?a=1&amp;b=2\" TargetMode=\"External\""));
        assert!(part(&bytes, "xl/comments1.xml").contains("<author>me</author>"));
        assert!(part(&bytes, "xl/drawings/vmlDrawing1.vml").contains("<x:Row>0</x:Row>"));
        assert!(part(&bytes, "[Content_Types].xml").contains("/xl/comments1.xml"));
    }

    #[test]
    fn names_and_properties_are_written_and_bytes_repeat() {
        let mut workbook = Workbook::new();
        let s = workbook.add_sheet("My 'sheet").unwrap();
        s.autofilter("A1:B2").unwrap();
        s.print_area("A1:C3").unwrap();
        s.print_title_rows(1, 2).unwrap();
        workbook.define_name("rates", "'My ''sheet'!$A$1").unwrap();
        assert!(workbook.define_name("_xlnm.Print_Area", "x").is_err());
        assert!(workbook.define_name("1a", "x").is_err());
        workbook.set_creation_time("2024-01-02T03:04:05Z").unwrap();
        assert!(workbook.set_creation_time("2024-01-02T99:04:05Z").is_err());
        workbook.set_title("T & T");
        let bytes = workbook.to_bytes().unwrap();
        assert_eq!(bytes, workbook.to_bytes().unwrap());
        let book = part(&bytes, "xl/workbook.xml");
        assert!(book.contains("name=\"_xlnm.Print_Titles\" localSheetId=\"0\">&apos;My &apos;&apos;sheet&apos;!$1:$2<"), "{book}");
        assert!(book.contains("_xlnm._FilterDatabase") && book.contains("_xlnm.Print_Area") && book.contains("name=\"rates\""));
        let core = part(&bytes, "docProps/core.xml");
        assert!(core.contains("<dc:title>T &amp; T</dc:title>") && core.contains(">2024-01-02T03:04:05Z<"));
    }

    #[test]
    fn the_driver_reads_what_the_model_writes() {
        let bytes = one_sheet(|s| {
            s.write("A1", "name", &Format::new().bold()).unwrap();
            s.write("B1", "when", &Format::new().bold()).unwrap();
            s.write("A2", "a", &Format::new()).unwrap();
            s.write("B2", CellValue::Date("2024-01-02".into()), &Format::new()).unwrap();
        });
        let l = super::super::xlsx::parse(bytes, &[], &Default::default()).unwrap().remove(0);
        assert_eq!(l.schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["name", "when"]);
        assert_eq!(l.features[0].attributes[1], wbvector::FieldValue::Date("2024-01-02".into()));
    }
}
