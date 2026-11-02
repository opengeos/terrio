// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/xlsx/ogrxlsxdatasource.cpp
// No code is translated from these sources.

//! Office Open XML spreadsheets (`.xlsx`, `.xlsm`): each worksheet is an attribute layer.
//!
//! Reading follows GDAL's XLSX driver: the `HEADERS` and `FIELD_TYPES` open options, its detection
//! of a header line, its field type widening, and feature ids that are row numbers. Rows without
//! cells stay empty features. Past a gap too large to fill, GDAL's and this driver's ids count the
//! rows seen instead. Formulas are not evaluated: a formula cell reads as its cached value. Cells out
//! of column order are refused, where GDAL shifts the row's values.
//!
//! Beyond GDAL, naming coordinate or WKT columns (`X_POSSIBLE_NAMES`, `Y_POSSIBLE_NAMES`,
//! `Z_POSSIBLE_NAMES`, `GEOM_POSSIBLE_NAMES`) reads geometries from them, and the `GEOMETRY`
//! creation option writes them. Without those options sheets hold no geometries, as in GDAL.
//!
//! Where this differs from GDAL on purpose: a `date1904` workbook's dates are honored (GDAL reads
//! them four years early), a date format's quoted text does not make it a date format, a
//! date-styled cell that holds no number makes its column text, a boolean column that also holds
//! other numbers becomes an integer field (GDAL keeps it boolean and loses them), repeated headings
//! get unique names, phonetic guides in shared strings are left out of the text, column widths are
//! written per column (GDAL writes overlapping ranges), and sheet names are made valid for Excel. A
//! time of day reads as `HH:MM:SS` text, since layers have no time type, so it is written back as
//! text.
//!
//! The first feature's null values are written as blank cells, which GDAL's driver and this one
//! count as non-text when they look for a header. A layer whose first feature is all text cannot be
//! told from a table without a header, so its names read back as data unless `HEADERS=FORCE` is set.

use super::xlsx_options::{XlsxColumnWidth, XlsxFieldTypes, XlsxHeaders, XlsxOptions};
use super::{csv, datetime, wkt, xml};
use crate::{CsvGeometry, Error, Result};
use quick_xml::events::{BytesStart, Event};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Read, Write as _};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// Excel's limits on a worksheet.
const MAX_ROWS: usize = 1_048_576;
const MAX_COLUMNS: usize = 16_384;
const MAX_CELL_TEXT_UTF16: usize = 32_767;
const MAX_SHEET_NAME: usize = 31;
/// The most relationships or sheets a workbook part may declare: they hold memory before any cell.
const MAX_WORKBOOK_ENTRIES: usize = 65_536;
/// GDAL's limits on the empty rows a gap between two rows makes.
const MAX_ROW_GAP: usize = 10_000;
const MAX_GAP_CELLS: usize = 100_000;
/// The most a workbook's part may expand to, and the most cells (blank ones in the table included)
/// a workbook may hold: an untrusted file is refused rather than exhausting memory.
const MAX_PART_BYTES: u64 = 2 << 30;
/// The most a workbook's sheets may expand to together.
const MAX_SHEET_BYTES: u64 = 4 * MAX_PART_BYTES;
const MAX_CELLS: usize = 100_000_000;
/// What a row costs against that limit, for the row entry and feature it holds even without a cell.
const ROW_CELLS: usize = 4;
/// The bytes of a cell's text that cost as much as another cell.
const CELL_BYTES: usize = 32;
/// Excel's serial number of 1970-01-01, and the days between its 1900 and 1904 date systems.
const UNIX_EPOCH_SERIAL: i64 = 25_569;
const DATE1904_OFFSET_DAYS: i64 = 1462;
const SECONDS_PER_DAY: f64 = 86_400.0;
/// GDAL's limit on the numbers that are dates, in days.
const MAX_SERIAL: f64 = 3_650_000.0;

// Reading

/// What a cell holds, as GDAL's driver classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    /// A column skipped by a cell reference: no cell at all.
    Gap,
    Text,
    /// A number, or a formula's cached text or error, told apart by the value.
    General,
    Boolean,
    Date,
    Time,
    DateTime,
}

#[derive(Debug, Clone)]
struct Cell {
    kind: CellKind,
    value: String,
}

/// A field's type while the rows are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnType {
    Text,
    Integer,
    Integer64,
    Real,
    Date,
    Time,
    DateTime,
}

#[derive(Debug, Clone)]
struct Column {
    name: String,
    kind: ColumnType,
    boolean: bool,
    /// No value has told the type yet.
    unknown: bool,
}

/// The type of a cell's value, and whether it is a boolean.
fn cell_type(cell: &Cell, autodetect: bool) -> (ColumnType, bool) {
    if !autodetect {
        return (ColumnType::Text, false);
    }
    match cell.kind {
        CellKind::Gap | CellKind::Text => (ColumnType::Text, false),
        CellKind::Boolean => (ColumnType::Integer, true),
        // A formula's cached text or error, say, styled as a date.
        CellKind::Date | CellKind::Time | CellKind::DateTime if !cell.value.is_empty() && cell.value.trim().parse::<f64>().is_err() => {
            (ColumnType::Text, false)
        }
        CellKind::Date => (ColumnType::Date, false),
        CellKind::Time => (ColumnType::Time, false),
        CellKind::DateTime => (ColumnType::DateTime, false),
        CellKind::General => {
            let v = cell.value.trim();
            if csv::is_integer(v) {
                let fits = v.parse::<i32>().is_ok();
                (if fits { ColumnType::Integer } else { ColumnType::Integer64 }, false)
            } else if csv::is_real(v) {
                (ColumnType::Real, false)
            } else {
                (ColumnType::Text, false)
            }
        }
    }
}

impl Column {
    /// Widens the field's type to hold another value, as GDAL's driver does row by row.
    fn widen(&mut self, kind: ColumnType, boolean: bool) {
        use ColumnType as T;
        if self.unknown {
            (self.unknown, self.kind, self.boolean) = (false, kind, boolean);
            return;
        }
        match (self.kind, kind) {
            (T::DateTime, T::Date | T::Time) | (T::Real, T::Integer | T::Integer64) | (T::Integer64, T::Integer) | (T::Text, _) => {}
            (a, b) if a != b => {
                self.boolean = false;
                self.kind = match (a, b) {
                    (T::Date | T::Time, T::DateTime) => T::DateTime,
                    (T::Integer | T::Integer64, T::Real) => T::Real,
                    (T::Integer, T::Integer64) => T::Integer64,
                    _ => T::Text,
                };
            }
            (T::Integer, _) if self.boolean && !boolean => self.boolean = false,
            _ => {}
        }
    }
}

/// A sheet's rows, turned into fields and rows as they are read.
struct SheetTable<'a> {
    autodetect: bool,
    options: &'a XlsxOptions,
    first_line: Vec<Cell>,
    /// The lines seen: rows, with the empty rows of gaps.
    line: usize,
    /// The lines seen inside the rows asked for.
    seen: usize,
    /// The rows that name the fields, whole, while several do.
    header_lines: Vec<Vec<Cell>>,
    /// The sheet's merged ranges: first row and column, last row and column, from 0.
    merges: Vec<(usize, usize, usize, usize)>,
    has_header: bool,
    columns: Vec<Column>,
    rows: Vec<(u64, Vec<Cell>)>,
}

impl<'a> SheetTable<'a> {
    fn new(options: &'a XlsxOptions) -> Self {
        Self {
            autodetect: options.field_types == XlsxFieldTypes::Auto,
            options,
            first_line: Vec::new(),
            line: 0,
            seen: 0,
            header_lines: Vec::new(),
            merges: Vec::new(),
            has_header: false,
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// The rows that name the fields at most: whether they do is told after them unless asked for.
    fn header_count(&self) -> usize {
        self.options.header_rows.unwrap_or(1)
    }

    /// A line's cells in the columns asked for.
    fn crop(&self, mut cells: Vec<Cell>) -> Vec<Cell> {
        if let Some(last) = self.options.last_column {
            cells.truncate(last + 1);
        }
        cells.drain(..self.options.first_column.min(cells.len()));
        cells
    }

    /// The first line that several header rows make: each column's headings joined, a merged heading
    /// naming every column under it.
    fn joined_header(&self) -> Vec<Cell> {
        let first_row = self.options.first_row - 1;
        let first_column = self.options.first_column;
        let widest = self.header_lines.iter().map(Vec::len).max().unwrap_or(0);
        let end = self.options.last_column.map_or(widest, |last| widest.min(last + 1));
        let heading = |row: usize, column: usize| {
            let (mut r, mut c) = (row, column);
            if let Some(&(r0, c0, _, _)) =
                self.merges.iter().find(|&&(r0, c0, r1, c1)| (r0..=r1).contains(&(row + first_row)) && (c0..=c1).contains(&column))
            {
                (r, c) = (r0.saturating_sub(first_row), c0);
            }
            self.header_lines.get(r).and_then(|line| line.get(c)).map_or("", |cell| cell.value.trim())
        };
        (first_column..end)
            .map(|column| {
                let mut parts: Vec<&str> = Vec::new();
                for row in 0..self.header_lines.len() {
                    let text = heading(row, column);
                    if !text.is_empty() && parts.last() != Some(&text) {
                        parts.push(text);
                    }
                }
                Cell { kind: CellKind::Text, value: parts.join(" / ") }
            })
            .collect()
    }

    fn detect_header(&self, line: &[Cell]) -> bool {
        if self.options.header_rows.is_some() {
            return self.header_count() > 0;
        }
        match self.options.headers {
            XlsxHeaders::Force => return true,
            XlsxHeaders::Disable => return false,
            XlsxHeaders::Auto => {}
        }
        let first = &self.first_line;
        let text = line.iter().filter(|c| c.kind == CellKind::Text).count();
        let other = line.iter().filter(|c| !matches!(c.kind, CellKind::Text | CellKind::Gap)).count();
        !first.is_empty()
            && first.iter().all(|c| c.kind == CellKind::Text)
            && first.len() >= line.len()
            && text != first.len()
            && other != 0
    }

    /// The fields the first line makes when it holds values rather than names.
    fn columns_from_first_line(&mut self) {
        let autodetect = self.autodetect;
        self.columns = self
            .first_line
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let (kind, boolean) = cell_type(c, autodetect);
                Column { name: format!("Field{}", i + 1), kind, boolean, unknown: false }
            })
            .collect();
    }

    fn end_line(&mut self, cells: Vec<Cell>) {
        let row = self.line + 1;
        self.line += 1;
        if row < self.options.first_row || self.options.last_row.is_some_and(|last| row > last) {
            return;
        }
        let line = self.seen;
        self.seen += 1;
        let headers = self.header_count();
        if headers > 1 && line < headers {
            // Whole, for the merged ranges that start outside the columns asked for.
            self.header_lines.push(cells);
            if line + 1 == headers {
                self.first_line = self.joined_header();
            }
            return;
        }
        let cells = self.crop(cells);
        if headers <= 1 && line == 0 {
            self.first_line = cells;
            return;
        }
        if line == headers.max(1) {
            self.has_header = self.detect_header(&cells);
            if self.has_header {
                let autodetect = self.autodetect;
                self.columns = self
                    .first_line
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let name = if c.value.is_empty() { format!("Field{}", i + 1) } else { c.value.clone() };
                        match cells.get(i).filter(|v| !v.value.is_empty()) {
                            Some(v) => {
                                let (kind, boolean) = cell_type(v, autodetect);
                                Column { name, kind, boolean, unknown: false }
                            }
                            None => Column { name, kind: ColumnType::Text, boolean: false, unknown: true },
                        }
                    })
                    .collect();
            } else {
                self.columns_from_first_line();
                self.rows.push((self.options.first_row as u64, std::mem::take(&mut self.first_line)));
            }
        }
        for (i, cell) in cells.iter().enumerate().skip(self.columns.len()) {
            let (kind, boolean) = cell_type(cell, self.autodetect);
            self.columns.push(Column { name: format!("Field{}", i + 1), kind, boolean, unknown: false });
        }
        if self.autodetect {
            for (column, cell) in self.columns.iter_mut().zip(&cells) {
                if !cell.value.is_empty() {
                    let (kind, boolean) = cell_type(cell, true);
                    column.widen(kind, boolean);
                }
            }
        }
        self.rows.push((row as u64, cells));
    }

    /// Ends the sheet: a single line is data, whatever `HEADERS` says, unless header rows are asked for.
    fn finish(&mut self) {
        if self.seen > self.header_count().max(1) || self.first_line.is_empty() {
            return;
        }
        if self.options.header_rows.is_some_and(|rows| rows > 0) && self.seen == self.header_count() {
            self.columns = self
                .first_line
                .iter()
                .enumerate()
                .map(|(i, c)| Column {
                    name: if c.value.is_empty() { format!("Field{}", i + 1) } else { c.value.clone() },
                    kind: ColumnType::Text,
                    boolean: false,
                    unknown: true,
                })
                .collect();
        } else if self.seen == 1 {
            self.columns_from_first_line();
            self.rows.push((self.options.first_row as u64, std::mem::take(&mut self.first_line)));
        }
    }
}

/// The year-to-second parts of a serial date: its day since 1970, and its milliseconds in the day.
/// `None` for numbers that are no dates.
fn serial_parts(value: &str, date1904: bool) -> Option<(i64, i64)> {
    let serial: f64 = value.trim().parse().ok()?;
    if !serial.is_finite() || serial.abs() >= MAX_SERIAL {
        return None;
    }
    let mut seconds = serial * SECONDS_PER_DAY;
    if (seconds - seconds.round()).abs() < 1e-3 {
        seconds = seconds.round();
    }
    let whole = seconds.trunc() as i64;
    let millis = ((seconds - seconds.trunc()) * 1000.0).round() as i64;
    // Excel's phantom 1900-02-29 is serial 60: earlier serials are a day later than the epoch says.
    let offset = if date1904 {
        DATE1904_OFFSET_DAYS
    } else if (1.0..60.0).contains(&serial) {
        1
    } else {
        0
    };
    let total = (whole + (offset - UNIX_EPOCH_SERIAL) * SECONDS_PER_DAY as i64) * 1000 + millis;
    Some((total.div_euclid(86_400_000), total.rem_euclid(86_400_000)))
}

fn format_time(millis: i64) -> String {
    let (s, ms) = (millis / 1000, millis % 1000);
    let mut out = format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    if ms != 0 {
        let _ = write!(out, ".{ms:03}");
    }
    out
}

/// A date cell's value as text in a text field, as GDAL prints it.
fn serial_as_text(kind: CellKind, days: i64, millis: i64) -> String {
    let date = datetime::format_date(days).replace('-', "/");
    match kind {
        CellKind::Date => date,
        CellKind::Time => format_time(millis - millis % 1000),
        _ => format!("{date} {}", format_time(millis)),
    }
}

/// The value of a non-empty cell in a field of its final type.
fn cell_value(cell: Cell, column: &Column, date1904: bool) -> FieldValue {
    let v = cell.value;
    if matches!(cell.kind, CellKind::Date | CellKind::Time | CellKind::DateTime) {
        // Non-numeric values styled as dates (a formula's cached text, say) stay text.
        if v.trim().parse::<f64>().is_err() {
            return if column.kind == ColumnType::Text { FieldValue::Text(v) } else { FieldValue::Null };
        }
        let Some((days, millis)) = serial_parts(&v, date1904) else { return FieldValue::Null };
        return match column.kind {
            ColumnType::Date => FieldValue::Date(datetime::format_date(days)),
            ColumnType::DateTime => FieldValue::DateTime(format!("{}T{}", datetime::format_date(days), format_time(millis))),
            ColumnType::Time => FieldValue::Text(format_time(millis)),
            _ => FieldValue::Text(serial_as_text(cell.kind, days, millis)),
        };
    }
    match column.kind {
        ColumnType::Integer | ColumnType::Integer64 if column.boolean => FieldValue::Boolean(v.trim() != "0"),
        ColumnType::Integer | ColumnType::Integer64 => v.trim().parse().map_or(FieldValue::Null, FieldValue::Integer),
        ColumnType::Real => v.trim().parse().map_or(FieldValue::Null, FieldValue::Float),
        _ => FieldValue::Text(v),
    }
}

/// Where a sheet's geometries are.
#[derive(Debug, Clone, Copy, Default)]
struct GeometryColumns {
    x: Option<usize>,
    y: Option<usize>,
    z: Option<usize>,
    wkt: Option<usize>,
}

impl GeometryColumns {
    fn find(columns: &[Column], options: &XlsxOptions) -> Self {
        let find = |patterns: &[String]| {
            if patterns.is_empty() { None } else { columns.iter().position(|c| csv::matches_name(&c.name, patterns, &[])) }
        };
        let (x, y) = (find(&options.x_possible_names), find(&options.y_possible_names));
        if let (Some(x), Some(y)) = (x, y) {
            return Self { x: Some(x), y: Some(y), z: find(&options.z_possible_names), wkt: None };
        }
        Self { wkt: find(&options.geom_possible_names), ..Self::default() }
    }

    fn holds(&self, column: usize) -> bool {
        [self.x, self.y, self.z, self.wkt].contains(&Some(column))
    }

    fn geometry(&self, cells: &[Cell]) -> Option<Geometry> {
        let text = |c: usize| cells.get(c).filter(|c| matches!(c.kind, CellKind::Text | CellKind::General)).map(|c| c.value.trim());
        if let Some(c) = self.wkt {
            return wkt::parse(text(c)?);
        }
        let number = |c: usize| text(c).and_then(csv::parse_number);
        let (x, y) = (number(self.x?)?, number(self.y?)?);
        Some(Geometry::Point(Coord { x, y, z: self.z.and_then(number), m: None }))
    }
}

/// The layer a sheet's table makes.
fn table_layer(mut table: SheetTable<'_>, name: &str, date1904: bool, table_budget: &mut usize) -> Result<Layer> {
    table.finish();
    let SheetTable { columns, rows, options, .. } = table;
    // The table's blanks become values and its rows features: the sheets' tables share the limit too.
    take_cells(table_budget, rows.len().saturating_mul(columns.len().saturating_add(ROW_CELLS)))?;
    let geometry = GeometryColumns::find(&columns, options);
    let kept: Vec<usize> = (0..columns.len()).filter(|&i| options.keep_geom_columns || !geometry.holds(i)).collect();
    let mut layer = Layer::new(name);
    let mut next_suffix: HashMap<&str, usize> = HashMap::new();
    for &i in &kept {
        let c = &columns[i];
        let field_type = match c.kind {
            ColumnType::Integer | ColumnType::Integer64 if c.boolean => FieldType::Boolean,
            ColumnType::Integer | ColumnType::Integer64 => FieldType::Integer,
            ColumnType::Real => FieldType::Float,
            ColumnType::Date => FieldType::Date,
            ColumnType::DateTime => FieldType::DateTime,
            ColumnType::Text | ColumnType::Time => FieldType::Text,
        };
        // A layer's field names are unique, where GDAL's layers repeat a repeated heading. Each
        // heading resumes from its last suffix, so that a wide sheet of one heading stays linear.
        let mut name = c.name.clone();
        while layer.schema.field_index(&name).is_some() {
            let n = next_suffix.entry(c.name.as_str()).or_insert(2);
            name = format!("{}_{n}", c.name);
            *n += 1;
        }
        layer.add_field(FieldDef::new(name, field_type));
    }
    if geometry.x.is_some() {
        layer.geom_type = Some(GeometryType::Point);
    }
    let (mut any_geometry, mut lonlat) = (false, true);
    for (fid, mut cells) in rows {
        let g = geometry.geometry(&cells);
        if let Some(g) = &g {
            any_geometry = true;
            lonlat &= csv::in_lonlat(g).unwrap_or(true);
        }
        let attributes = kept
            .iter()
            .map(|&i| match cells.get_mut(i) {
                Some(cell) if !cell.value.is_empty() => {
                    let cell = Cell { kind: cell.kind, value: std::mem::take(&mut cell.value) };
                    cell_value(cell, &columns[i], date1904)
                }
                _ => FieldValue::Null,
            })
            .collect();
        layer.features.push(Feature { fid, geometry: g, attributes });
    }
    match &options.crs {
        Some(crs) => crs.apply_to_layer(&mut layer),
        None if any_geometry && lonlat => layer.set_crs_epsg(Some(4326)),
        None => {}
    }
    Ok(layer)
}

fn take_cells(budget: &mut usize, cells: usize) -> Result<()> {
    *budget =
        budget.checked_sub(cells).ok_or_else(|| Error::ResourceLimit(format!("XLSX: the workbook holds more than {MAX_CELLS} cells")))?;
    Ok(())
}

/// A sheet's merged ranges, when several header rows may need them.
fn merged_ranges(text: &str, part: &str, budget: &mut usize) -> Result<Vec<(usize, usize, usize, usize)>> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut merges = Vec::new();
    loop {
        match reader.read_event().map_err(|e| xml_error(part, e))? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "mergeCell" => {
                take_cells(budget, 1)?;
                let parsed = attribute(&e, "ref").and_then(|r| {
                    let (start, end) = r.split_once(':')?;
                    let row =
                        |cell: &str| cell.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<usize>().ok().filter(|&r| r > 0);
                    Some((row(start)? - 1, reference_column(start)?, row(end)? - 1, reference_column(end)?))
                });
                merges.extend(parsed);
            }
            Event::Eof => return Ok(merges),
            _ => {}
        }
    }
}

/// A workbook part as text, refused when it would expand past the limit.
fn read_part<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Option<String>> {
    let mut file = match zip.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let too_large = || Error::ResourceLimit(format!("XLSX: part '{name}' expands to more than {MAX_PART_BYTES} bytes"));
    if file.size() > MAX_PART_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    // Past what the platform can allocate, as in WebAssembly, the part is too large as well.
    file.by_ref()
        .take(MAX_PART_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| if e.kind() == std::io::ErrorKind::OutOfMemory { too_large() } else { e.into() })?;
    if bytes.len() as u64 > MAX_PART_BYTES {
        return Err(too_large());
    }
    // Valid text keeps its bytes, so that a large part is not held twice.
    Ok(Some(match String::from_utf8(bytes) {
        Ok(mut text) if text.starts_with('\u{FEFF}') => {
            text.drain(..'\u{FEFF}'.len_utf8());
            text
        }
        Ok(text) => text,
        Err(e) => xml::decode(e.as_bytes()),
    }))
}

fn attribute(e: &BytesStart<'_>, local_name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == local_name)
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

fn xml_error(part: &str, e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("XLSX: cannot parse '{part}': {e}"))
}

/// The text of a `&name;` or `&#n;` reference.
fn reference_text(r: &quick_xml::events::BytesRef<'_>) -> Option<char> {
    r.resolve_char_ref().ok().flatten().or(match &**r {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    })
}

/// The directory of a part, with its final `/`.
fn directory(part: &str) -> &str {
    part.rfind('/').map_or("", |i| &part[..=i])
}

/// A relationship's target as a part name, from the directory of the part it belongs to.
fn resolve_target(base: &str, target: &str) -> String {
    let path = if let Some(absolute) = target.strip_prefix('/') {
        absolute.trim_start_matches('/').to_string()
    } else {
        format!("{base}{target}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

fn too_many_entries(part: &str) -> Error {
    Error::ResourceLimit(format!("XLSX: '{part}' declares more than {MAX_WORKBOOK_ENTRIES} entries"))
}

/// The relationships of a part: id, type and target.
fn relationships(text: &str, part: &str) -> Result<Vec<(String, String, String)>> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut out = Vec::new();
    loop {
        match reader.read_event().map_err(|e| xml_error(part, e))? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "Relationship" => {
                if let (Some(id), Some(kind), Some(target)) = (attribute(&e, "Id"), attribute(&e, "Type"), attribute(&e, "Target")) {
                    if out.len() >= MAX_WORKBOOK_ENTRIES {
                        return Err(too_many_entries(part));
                    }
                    out.push((id, kind, target));
                }
            }
            Event::Eof => return Ok(out),
            _ => {}
        }
    }
}

/// What a workbook part tells of the others.
struct WorkbookParts {
    /// The name and part of each worksheet, in workbook order.
    sheets: Vec<(String, String)>,
    /// Whether dates count from 1904.
    date1904: bool,
    /// The shared strings and styles parts its relationships name.
    strings: Option<String>,
    styles: Option<String>,
}

/// Reads the workbook part and its relationships.
fn workbook_sheets<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>) -> Result<WorkbookParts> {
    let not_workbook = || Error::InvalidData("XLSX: the archive holds no workbook".into());
    let package = read_part(zip, "_rels/.rels")?.map(|text| relationships(&text, "_rels/.rels")).transpose()?.unwrap_or_default();
    let workbook = package
        .iter()
        .find(|(_, kind, _)| kind.ends_with("/officeDocument"))
        .map_or_else(|| "xl/workbook.xml".to_string(), |(_, _, target)| resolve_target("", target));
    let text = read_part(zip, &workbook)?.ok_or_else(not_workbook)?;
    let base = directory(&workbook);
    let (dir, file) = workbook.split_at(base.len());
    let rels_part = format!("{dir}_rels/{file}.rels");
    let rels = read_part(zip, &rels_part)?.ok_or_else(not_workbook)?;
    let rels = relationships(&rels, &rels_part)?;
    let part_of = |suffix: &str| rels.iter().find(|(_, kind, _)| kind.ends_with(suffix)).map(|(_, _, target)| resolve_target(base, target));
    let (strings, styles) = (part_of("/sharedStrings"), part_of("/styles"));
    let targets: HashMap<String, String> = rels
        .into_iter()
        .filter(|(_, kind, _)| kind.contains("/worksheet"))
        .map(|(id, _, target)| (id, resolve_target(base, &target)))
        .collect();
    let mut reader = quick_xml::Reader::from_str(&text);
    let (mut sheets, mut date1904) = (Vec::new(), false);
    let mut seen = std::collections::HashSet::new();
    loop {
        match reader.read_event().map_err(|e| xml_error(&workbook, e))? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "workbookPr" => date1904 = attribute(&e, "date1904").is_some_and(|v| matches!(v.as_str(), "1" | "true")),
                "sheet" => {
                    if let (Some(name), Some(id)) = (attribute(&e, "name"), attribute(&e, "id"))
                        && let Some(target) = targets.get(&id)
                        && seen.insert(id.clone())
                        // Sheets sharing a part would have it read and parsed again for each.
                        && seen.insert(target.clone())
                    {
                        if sheets.len() >= MAX_WORKBOOK_ENTRIES {
                            return Err(too_many_entries(&workbook));
                        }
                        sheets.push((name, target.clone()));
                    }
                }
                _ => {}
            },
            Event::Eof => return Ok(WorkbookParts { sheets, date1904, strings, styles }),
            _ => {}
        }
    }
}

fn shared_strings(text: &str, budget: &mut usize) -> Result<Vec<String>> {
    let part = "sharedStrings.xml";
    let mut reader = quick_xml::Reader::from_str(text);
    let mut strings = Vec::new();
    let (mut current, mut in_text, mut phonetic) = (String::new(), false, 0usize);
    loop {
        match reader.read_event().map_err(|e| xml_error(part, e))? {
            Event::Start(e) => match e.local_name().as_ref() {
                "si" => current.clear(),
                "rPh" => phonetic += 1,
                "t" => in_text = phonetic == 0,
                _ => {}
            },
            Event::Empty(e) if e.local_name().as_ref() == "si" => {
                take_cells(budget, 1)?;
                strings.push(String::new());
            }
            Event::End(e) => match e.local_name().as_ref() {
                "si" => {
                    take_cells(budget, 1 + current.len() / CELL_BYTES)?;
                    strings.push(std::mem::take(&mut current));
                }
                "rPh" => phonetic = phonetic.saturating_sub(1),
                "t" => in_text = false,
                _ => {}
            },
            Event::Text(t) if in_text => current.push_str(&t.xml10_content()),
            Event::CData(t) if in_text => current.push_str(&t.xml10_content()),
            Event::GeneralRef(r) if in_text => current.extend(reference_text(&r)),
            Event::Eof => return Ok(strings),
            _ => {}
        }
    }
}

/// A number format without its literal text: quoted, or escaped by a backslash.
fn unquoted_format(code: &str) -> String {
    let (mut out, mut quoted, mut chars) = (String::new(), false, code.chars());
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if !quoted => {
                chars.next();
            }
            c if !quoted => out.push(c),
            _ => {}
        }
    }
    out
}

/// The kind of value each cell style holds: from its number format, as GDAL's driver tells it.
fn style_kinds(text: &str) -> Result<Vec<CellKind>> {
    let part = "styles.xml";
    let mut reader = quick_xml::Reader::from_str(text);
    let mut formats: HashMap<i32, CellKind> = HashMap::new();
    let (mut kinds, mut in_cell_xfs) = (Vec::new(), false);
    loop {
        match reader.read_event().map_err(|e| xml_error(part, e))? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "numFmt" => {
                    if let (Some(code), Some(id)) =
                        (attribute(&e, "formatCode"), attribute(&e, "numFmtId").and_then(|v| v.parse::<i32>().ok()))
                        && id >= 164
                    {
                        let code = unquoted_format(&code);
                        let has = |upper: &str, lower: &str| code.contains(upper) || code.contains(lower);
                        let (date, time) = (has("DD", "dd") || has("YY", "yy"), has("HH", "hh"));
                        formats.insert(
                            id,
                            match (date, time) {
                                (true, true) => CellKind::DateTime,
                                (true, false) => CellKind::Date,
                                (false, true) => CellKind::Time,
                                (false, false) => CellKind::General,
                            },
                        );
                    }
                }
                "cellXfs" => in_cell_xfs = true,
                "xf" if in_cell_xfs => {
                    let id = attribute(&e, "numFmtId").and_then(|v| v.parse::<i32>().ok()).unwrap_or(-1);
                    kinds.push(match id {
                        14..=17 => CellKind::Date,
                        18..=21 => CellKind::Time,
                        22 => CellKind::DateTime,
                        id if id >= 164 => formats.get(&id).copied().unwrap_or(CellKind::General),
                        _ => CellKind::General,
                    });
                }
                _ => {}
            },
            Event::End(e) if e.local_name().as_ref() == "cellXfs" => in_cell_xfs = false,
            Event::Eof => return Ok(kinds),
            _ => {}
        }
    }
}

/// The column of a cell reference (`AB12`), from 0.
fn reference_column(reference: &str) -> Option<usize> {
    let letters = reference.bytes().take_while(u8::is_ascii_uppercase);
    let mut column = None;
    for b in letters {
        let next = column.map_or(0, |c: usize| (c + 1) * 26) + (b - b'A') as usize;
        column = Some(next.min(MAX_COLUMNS));
    }
    column
}

/// The workbook's parts every sheet needs.
struct SharedParts {
    strings: Vec<String>,
    styles: Vec<CellKind>,
}

fn too_many_columns(part: &str) -> Error {
    Error::ResourceLimit(format!("XLSX: '{part}' holds a row of more than the {MAX_COLUMNS} columns a sheet can"))
}

fn too_many_rows(part: &str) -> Error {
    Error::ResourceLimit(format!("XLSX: '{part}' holds more than the {MAX_ROWS} rows a sheet can"))
}

/// Reads a worksheet's rows into its table.
fn read_sheet(text: &str, part: &str, shared: &SharedParts, table: &mut SheetTable<'_>, budget: &mut usize) -> Result<()> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut in_data = false;
    let mut cells: Vec<Cell> = Vec::new();
    let mut in_row = false;
    // The cell being read: its kind, whether its value indexes the shared strings, its text.
    let (mut in_cell, mut lookup, mut kind) = (false, false, CellKind::General);
    let (mut value, mut in_value, mut value_seen, mut in_phonetic) = (String::new(), false, false, false);
    let start_row = |table: &mut SheetTable<'_>, e: &BytesStart<'_>, budget: &mut usize| -> Result<()> {
        // No sheet has more rows, and rows hold memory without any cell.
        if table.line >= MAX_ROWS {
            return Err(too_many_rows(part));
        }
        take_cells(budget, ROW_CELLS)?;
        let Some(row) = attribute(e, "r").and_then(|r| r.parse::<usize>().ok()).filter(|&r| r > 0) else { return Ok(()) };
        let (mut target, mut line) = (row - 1, table.line);
        if target <= line {
            return Ok(());
        }
        // Rows before those asked for hold nothing, however far they go.
        let skipped = table.options.first_row - 1;
        if line < skipped {
            line = target.min(skipped);
            table.line = line;
            target = row - 1;
            if target <= line {
                return Ok(());
            }
        }
        if target >= MAX_ROWS {
            return Err(too_many_rows(part));
        }
        let gap = target - line;
        let fields = table.first_line.len().max(table.columns.len());
        // As GDAL's driver: too big a gap is ignored rather than filled with empty rows.
        if gap > MAX_ROW_GAP || (fields > 0 && gap > MAX_GAP_CELLS / fields) {
            return Ok(());
        }
        take_cells(budget, gap.saturating_mul(ROW_CELLS))?;
        for _ in 0..gap {
            table.end_line(Vec::new());
        }
        Ok(())
    };
    let start_cell = |e: &BytesStart<'_>, cells: &mut Vec<Cell>, budget: &mut usize| -> Result<(bool, CellKind)> {
        if cells.len() >= MAX_COLUMNS {
            return Err(too_many_columns(part));
        }
        if let Some(column) = attribute(e, "r").as_deref().and_then(reference_column) {
            if column >= MAX_COLUMNS {
                return Err(too_many_columns(part));
            }
            // Placed by position, a cell behind the others would shift the row's values.
            if column < cells.len() {
                return Err(Error::InvalidData(format!("XLSX: '{part}' has a cell out of column order")));
            }
            take_cells(budget, column.saturating_sub(cells.len()))?;
            while cells.len() < column {
                cells.push(Cell { kind: CellKind::Gap, value: String::new() });
            }
        }
        let styled = attribute(e, "s")
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|s| shared.styles.get(s).copied())
            .unwrap_or(CellKind::General);
        Ok(match attribute(e, "t").map(|t| t.to_ascii_lowercase()).as_deref() {
            Some("s") => (true, CellKind::Text),
            Some("inlinestr") => (false, CellKind::Text),
            Some("b") => (false, CellKind::Boolean),
            _ => (false, styled),
        })
    };
    let end_cell = |cells: &mut Vec<Cell>, kind: CellKind, lookup: bool, value: String, budget: &mut usize| -> Result<()> {
        take_cells(budget, 1)?;
        let value = if lookup {
            let found = value.trim().parse::<usize>().ok().and_then(|i| shared.strings.get(i));
            // Each cell owns a copy, so one long string in many cells takes memory the XML does not.
            take_cells(budget, found.map_or(0, |s| s.len() / CELL_BYTES))?;
            match found {
                Some(text) => text.clone(),
                // A blank cell may be typed as a shared string; a lost index must not read as text.
                None if value.trim().is_empty() => value,
                None => return Err(Error::InvalidData(format!("XLSX: shared string '{value}' is not in the workbook"))),
            }
        } else {
            take_cells(budget, value.len() / CELL_BYTES)?;
            value
        };
        cells.push(Cell { kind, value });
        Ok(())
    };
    loop {
        match reader.read_event().map_err(|e| xml_error(part, e))? {
            Event::Start(e) => match e.local_name().as_ref() {
                "sheetData" => in_data = true,
                "row" if in_data => {
                    start_row(table, &e, budget)?;
                    (in_row, cells) = (true, Vec::new());
                }
                "c" if in_row => {
                    (lookup, kind) = start_cell(&e, &mut cells, budget)?;
                    (in_cell, value_seen) = (true, false);
                    value.clear();
                }
                "v" if in_cell && !value_seen => in_value = true,
                // Each run of an inline string; `rPh` runs hold a pronunciation, not text.
                "rPh" if in_cell => in_phonetic = true,
                "t" if in_cell && kind == CellKind::Text && !in_phonetic => in_value = true,
                _ => {}
            },
            Event::Empty(e) => match e.local_name().as_ref() {
                "row" if in_data => {
                    start_row(table, &e, budget)?;
                    table.end_line(Vec::new());
                }
                "c" if in_row => {
                    let (lookup, kind) = start_cell(&e, &mut cells, budget)?;
                    end_cell(&mut cells, kind, lookup, String::new(), budget)?;
                }
                _ => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                "sheetData" => in_data = false,
                "row" if in_row => {
                    in_row = false;
                    table.end_line(std::mem::take(&mut cells));
                }
                "c" if in_cell => {
                    in_cell = false;
                    end_cell(&mut cells, kind, lookup, std::mem::take(&mut value), budget)?;
                }
                "v" if in_value => {
                    (in_value, value_seen) = (false, true);
                }
                "t" => in_value = false,
                "rPh" => in_phonetic = false,
                _ => {}
            },
            Event::Text(t) if in_value => value.push_str(&t.xml10_content()),
            Event::CData(t) if in_value => value.push_str(&t.xml10_content()),
            Event::GeneralRef(r) if in_value => value.extend(reference_text(&r)),
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

/// Reads a workbook from a file.
pub fn read(path: &str, only: &[String], options: &XlsxOptions) -> Result<Vec<Layer>> {
    parse(std::fs::read(path)?, only, options)
}

/// Parses a workbook held in memory: a layer per worksheet in workbook order, or only the sheets
/// `only` names when it is not empty.
pub fn parse(bytes: Vec<u8>, only: &[String], options: &XlsxOptions) -> Result<Vec<Layer>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let WorkbookParts { sheets, date1904, strings: strings_part, styles: styles_part } = workbook_sheets(&mut zip)?;
    let (mut budget, mut table_budget) = (MAX_CELLS, MAX_CELLS);
    let mut strings = Vec::new();
    for name in strings_part.iter().map(String::as_str).chain(["xl/sharedStrings.xml", "xl/SharedStrings.xml"]) {
        if let Some(text) = read_part(&mut zip, name)? {
            strings = shared_strings(&text, &mut budget)?;
            break;
        }
    }
    let mut styles = Vec::new();
    for name in styles_part.iter().map(String::as_str).chain(["xl/styles.xml"]) {
        if let Some(text) = read_part(&mut zip, name)? {
            styles = style_kinds(&text)?;
            break;
        }
    }
    let shared = SharedParts { strings, styles };
    let mut layers = Vec::with_capacity(sheets.len());
    // Naming no sheet here leaves the caller to report the names there are.
    let filter = sheets.iter().any(|(name, _)| only.contains(name));
    let mut sheet_bytes = 0u64;
    for (name, part) in sheets {
        if filter && !only.contains(&name) {
            continue;
        }
        let mut table = SheetTable::new(options);
        // A workbook naming a part it lacks still lists the sheet, empty, as GDAL's driver does.
        if let Some(text) = read_part(&mut zip, &part)? {
            sheet_bytes += text.len() as u64;
            if sheet_bytes > MAX_SHEET_BYTES {
                return Err(Error::ResourceLimit(format!("XLSX: the sheets expand to more than {MAX_SHEET_BYTES} bytes")));
            }
            if options.header_rows.is_some_and(|rows| rows > 1) {
                table.merges = merged_ranges(&text, &part, &mut budget)?;
            }
            read_sheet(&text, &part, &shared, &mut table, &mut budget)?;
        }
        layers.push(table_layer(table, &name, date1904, &mut table_budget)?);
    }
    Ok(layers)
}

/// Whether bytes look like a workbook: a ZIP archive named so, or whose first member is under `xl/`.
/// Excel's and this driver's workbooks start with `[Content_Types].xml`, so they need the name.
pub(crate) fn looks_like(bytes: &[u8], name: &str) -> bool {
    if !bytes.starts_with(b"PK\x03\x04") {
        return false;
    }
    if matches!(crate::formats::extension(name).as_deref(), Some("xlsx" | "xlsm")) {
        return true;
    }
    let length = bytes.get(26..28).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as usize);
    bytes.get(30..30 + length).is_some_and(|member| member.starts_with(b"xl/"))
}

// Writing

const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OD_RELS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE: &str = "http://schemas.openxmlformats.org/package/2006";

/// Style indexes in [`STYLES`].
const STYLE_DATE: u8 = 1;
const STYLE_DATETIME: u8 = 2;
const STYLE_DATETIME_MS: u8 = 4;
const STYLE_BOOLEAN: u8 = 5;

/// The cell styles a workbook's options make: [`STYLES`]'s, and those added after them.
#[derive(Debug, Clone, Copy)]
struct CellStyles {
    header: Option<u8>,
    date: u8,
    datetime: u8,
    datetime_ms: u8,
}

/// The cell styles of these options, and the `styles.xml` body that holds them. Without options that
/// style anything, it is what GDAL's driver writes.
fn cell_styles(options: &XlsxOptions) -> (CellStyles, String) {
    let mut styles = CellStyles { header: None, date: STYLE_DATE, datetime: STYLE_DATETIME, datetime_ms: STYLE_DATETIME_MS };
    let (mut formats, mut xfs, mut next) = (String::new(), String::new(), 6u8);
    let mut add_format = |code: &str, id: u8| {
        let _ = writeln!(formats, "<numFmt numFmtId=\"{id}\" formatCode=\"{}\"/>", xml_text(code));
        let _ = writeln!(xfs, "<xf numFmtId=\"{id}\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>");
        next += 1;
        next - 1
    };
    let mut extra_formats = 0;
    if let Some(code) = &options.date_format {
        styles.date = add_format(code, 170);
        extra_formats += 1;
    }
    if let Some(code) = &options.datetime_format {
        styles.datetime = add_format(code, 171);
        styles.datetime_ms = styles.datetime;
        extra_formats += 1;
    }
    let mut body = STYLES.to_string();
    let mut extra_xfs = extra_formats;
    if options.header_bold {
        let _ = writeln!(xfs, "<xf numFmtId=\"164\" fontId=\"1\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyFont=\"1\"/>");
        styles.header = Some(next);
        extra_xfs += 1;
        body = body.replace(
            "<fonts count=\"1\"><font><sz val=\"10\"/><name val=\"Arial\"/><family val=\"2\"/></font></fonts>",
            "<fonts count=\"2\"><font><sz val=\"10\"/><name val=\"Arial\"/><family val=\"2\"/></font><font><b/><sz val=\"10\"/><name val=\"Arial\"/><family val=\"2\"/></font></fonts>",
        );
    }
    if extra_formats > 0 {
        body = body.replace("<numFmts count=\"6\">", &format!("<numFmts count=\"{}\">", 6 + extra_formats));
        body = body.replace("</numFmts>", &format!("{formats}</numFmts>"));
    }
    if extra_xfs > 0 {
        body = body.replace("<cellXfs count=\"6\">", &format!("<cellXfs count=\"{}\">", 6 + extra_xfs));
        body = body.replace("</cellXfs>", &format!("{xfs}</cellXfs>"));
    }
    (styles, body)
}

/// The number formats GDAL's driver writes, the only styling of its workbooks.
const STYLES: &str = "<numFmts count=\"6\">\n\
<numFmt numFmtId=\"164\" formatCode=\"GENERAL\"/>\n\
<numFmt numFmtId=\"165\" formatCode=\"DD/MM/YY\"/>\n\
<numFmt numFmtId=\"166\" formatCode=\"DD/MM/YYYY\\ HH:MM:SS\"/>\n\
<numFmt numFmtId=\"167\" formatCode=\"HH:MM:SS\"/>\n\
<numFmt numFmtId=\"168\" formatCode=\"DD/MM/YYYY\\ HH:MM:SS.000\"/>\n\
<numFmt numFmtId=\"169\" formatCode=\"&quot;TRUE&quot;;&quot;TRUE&quot;;&quot;FALSE&quot;\"/>\n\
</numFmts>\n\
<fonts count=\"1\"><font><sz val=\"10\"/><name val=\"Arial\"/><family val=\"2\"/></font></fonts>\n\
<fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>\n\
<borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>\n\
<cellStyleXfs count=\"1\"><xf numFmtId=\"164\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\n\
<cellXfs count=\"6\">\n\
<xf numFmtId=\"164\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>\n\
<xf numFmtId=\"165\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\n\
<xf numFmtId=\"166\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\n\
<xf numFmtId=\"167\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\n\
<xf numFmtId=\"168\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\n\
<xf numFmtId=\"169\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\n\
</cellXfs>\n\
<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>\n";

/// Whether XML 1.0 can hold a character.
fn xml_legal(c: char) -> bool {
    (c >= ' ' && !matches!(c, '\u{FFFE}' | '\u{FFFF}')) || matches!(c, '\t' | '\n' | '\r')
}

/// Text as XML: escaped, without the characters XML cannot hold.
fn xml_text(s: &str) -> String {
    let text =
        if s.chars().all(xml_legal) { xml::escape(s) } else { xml::escape(&s.chars().filter(|&c| xml_legal(c)).collect::<String>()) };
    // A parser turns a raw carriage return into a line feed.
    text.replace('\r', "&#13;")
}

/// The shared strings table being written.
#[derive(Default)]
struct StringTable {
    indexes: HashMap<String, usize>,
    strings: Vec<String>,
    count: usize,
}

impl StringTable {
    fn index(&mut self, s: &str) -> usize {
        self.count += 1;
        if let Some(&i) = self.indexes.get(s) {
            return i;
        }
        let i = self.strings.len();
        self.indexes.insert(s.to_string(), i);
        self.strings.push(s.to_string());
        i
    }

    fn to_xml(&self) -> String {
        let mut out = format!("{XML_HEADER}<sst xmlns=\"{MAIN_NS}\" count=\"{}\" uniqueCount=\"{}\">\n", self.count, self.strings.len());
        for s in &self.strings {
            let space = if s.trim() != s { " xml:space=\"preserve\"" } else { "" };
            let _ = writeln!(out, "<si><t{space}>{}</t></si>", xml_text(s));
        }
        out.push_str("</sst>\n");
        out
    }
}

/// A column's letters, from 0.
fn column_letters(mut column: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(b'A' + (column % 26) as u8);
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    letters.reverse();
    String::from_utf8(letters).expect("ASCII letters")
}

/// Excel's serial of a day since 1970: dates before its phantom 1900-02-29 are a serial lower, and
/// it has none for dates before 1900.
fn excel_serial_days(days: i64) -> Option<i64> {
    let serial = days + UNIX_EPOCH_SERIAL;
    match serial {
        ..2 => None,
        2..=60 => Some(serial - 1),
        _ => Some(serial),
    }
}

/// A date-time's wall clock as milliseconds since 1970, without its UTC offset: spreadsheets have no
/// time zones, and the value reads back as it was written.
fn wall_clock_millis(s: &str) -> Option<i64> {
    let time = s.get(10..)?;
    let end = time.char_indices().skip(1).find(|(_, c)| matches!(c, 'Z' | '+' | '-')).map_or(time.len(), |(i, _)| i);
    datetime::parse_datetime(&s[..10 + end])
}

/// Appends a cell holding a text value.
fn text_cell(out: &mut String, reference: &str, text: &str, strings: &mut StringTable) -> Result<()> {
    styled_text_cell(out, reference, text, None, strings)
}

/// Appends a cell holding a text value, in a style if any.
fn styled_text_cell(out: &mut String, reference: &str, text: &str, style: Option<u8>, strings: &mut StringTable) -> Result<()> {
    if text.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
        return Err(Error::InvalidArgument(format!(
            "XLSX: cell {reference} holds more than the {MAX_CELL_TEXT_UTF16} characters a cell can"
        )));
    }
    let style = style.map_or(String::new(), |s| format!(" s=\"{s}\""));
    let _ = write!(out, "<c r=\"{reference}\"{style} t=\"s\"><v>{}</v></c>", strings.index(text));
    Ok(())
}

/// How many characters a value takes on the sheet, roughly: for fitting a column to it.
fn display_width(value: &FieldValue, options: &XlsxOptions) -> usize {
    match value {
        FieldValue::Null | FieldValue::Blob(_) => 0,
        FieldValue::Integer(v) => v.to_string().len(),
        FieldValue::Float(v) => v.to_string().len(),
        FieldValue::Boolean(_) => 5,
        FieldValue::Date(_) => options.date_format.as_ref().map_or(10, |code| code.chars().count()),
        FieldValue::DateTime(_) => options.datetime_format.as_ref().map_or(19, |code| code.chars().count()),
        FieldValue::Text(s) => s.lines().map(|line| line.encode_utf16().count()).max().unwrap_or(0),
    }
}

/// Appends a cell holding a number in a style.
fn number_cell(out: &mut String, reference: &str, style: u8, value: impl std::fmt::Display) {
    let _ = write!(out, "<c r=\"{reference}\" s=\"{style}\"><v>{value}</v></c>");
}

fn value_cell(out: &mut String, reference: &str, value: &FieldValue, styles: &CellStyles, strings: &mut StringTable) -> Result<()> {
    match value {
        FieldValue::Null => {}
        FieldValue::Integer(v) => {
            let _ = write!(out, "<c r=\"{reference}\"><v>{v}</v></c>");
        }
        FieldValue::Float(v) if v.is_finite() => {
            let _ = write!(out, "<c r=\"{reference}\"><v>{v}</v></c>");
        }
        FieldValue::Float(_) => {}
        FieldValue::Boolean(b) => {
            let _ = write!(out, "<c r=\"{reference}\" t=\"b\" s=\"{STYLE_BOOLEAN}\"><v>{}</v></c>", u8::from(*b));
        }
        // Values Excel has no serial for stay text.
        FieldValue::Date(s) => match datetime::parse_date(s).and_then(|days| excel_serial_days(i64::from(days))) {
            Some(serial) => number_cell(out, reference, styles.date, serial),
            None => text_cell(out, reference, s, strings)?,
        },
        FieldValue::DateTime(s) => match wall_clock_millis(s).and_then(|ms| Some((ms, excel_serial_days(ms.div_euclid(86_400_000))?))) {
            Some((ms, serial)) => {
                let style = if ms.rem_euclid(1000) == 0 { styles.datetime } else { styles.datetime_ms };
                number_cell(out, reference, style, ms.rem_euclid(86_400_000) as f64 / 86_400_000.0 + serial as f64);
            }
            None => text_cell(out, reference, s, strings)?,
        },
        FieldValue::Text(s) => text_cell(out, reference, s, strings)?,
        FieldValue::Blob(b) => text_cell(
            out,
            reference,
            &b.iter().fold(String::new(), |mut hex, x| {
                let _ = write!(hex, "{x:02X}");
                hex
            }),
            strings,
        )?,
    }
    Ok(())
}

/// The columns written before a layer's fields for its geometries.
fn geometry_header(layer: &Layer, options: &XlsxOptions) -> Result<Vec<&'static str>> {
    let geometries = || layer.features.iter().filter_map(|f| f.geometry.as_ref());
    Ok(match options.geometry {
        CsvGeometry::None => vec![],
        _ if geometries().next().is_none() => vec![],
        CsvGeometry::AsWkt => vec!["WKT"],
        CsvGeometry::AsXy => {
            if geometries().any(|g| !matches!(g, Geometry::Point(_))) {
                return Err(Error::InvalidArgument("XLSX: writing geometries as X/Y columns needs a point layer".into()));
            }
            if geometries().any(|g| matches!(g, Geometry::Point(c) if c.z.is_some())) { vec!["X", "Y", "Z"] } else { vec!["X", "Y"] }
        }
    })
}

fn geometry_values(feature: &Feature, header: &[&str]) -> Vec<FieldValue> {
    match (header, &feature.geometry) {
        ([], _) => vec![],
        (["WKT"], Some(g)) => vec![FieldValue::Text(wkt::format(g))],
        (_, Some(Geometry::Point(c))) => {
            let mut values = vec![FieldValue::Float(c.x), FieldValue::Float(c.y)];
            if header.len() == 3 {
                values.push(c.z.map_or(FieldValue::Null, FieldValue::Float));
            }
            values
        }
        _ => vec![FieldValue::Null; header.len()],
    }
}

/// A worksheet's XML for a layer, as GDAL's driver lays it out: a column width of 15 (29 for
/// date-times) and a header row unless the fields are all named `FieldN`.
fn sheet_xml(layer: &Layer, options: &XlsxOptions, styles: &CellStyles, strings: &mut StringTable) -> Result<(String, Option<String>)> {
    let extra = geometry_header(layer, options)?;
    let names: Vec<&str> = extra.iter().copied().chain(layer.schema.fields().iter().map(|f| f.name.as_str())).collect();
    if let Some(name) = extra.iter().find(|name| layer.schema.field_index(name).is_some()) {
        return Err(Error::InvalidArgument(format!("XLSX: '{}' has a field named as the geometry column {name}", layer.name)));
    }
    if names.len() > MAX_COLUMNS {
        return Err(Error::InvalidArgument(format!("XLSX: '{}' has more than the {MAX_COLUMNS} columns a sheet can hold", layer.name)));
    }
    let has_header = names.iter().enumerate().any(|(i, n)| *n != format!("Field{}", i + 1)) && !layer.features.is_empty();
    if layer.features.len() + usize::from(has_header) > MAX_ROWS {
        return Err(Error::InvalidArgument(format!("XLSX: '{}' has more than the {MAX_ROWS} rows a sheet can hold", layer.name)));
    }
    let mut out = format!("{XML_HEADER}<worksheet xmlns=\"{MAIN_NS}\" xmlns:r=\"{OD_RELS}\">\n");
    if options.freeze_header && has_header {
        out.push_str(
            "<sheetViews><sheetView workbookViewId=\"0\"><pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/>\
             <selection pane=\"bottomLeft\"/></sheetView></sheetViews>\n",
        );
    }
    if !names.is_empty() {
        let mut widest: Vec<usize> = names.iter().map(|name| if has_header { name.encode_utf16().count() } else { 0 }).collect();
        if options.column_width == XlsxColumnWidth::Auto {
            let null = FieldValue::Null;
            for feature in &layer.features {
                let geometry = geometry_values(feature, &extra);
                let attributes = (0..layer.schema.len()).map(|i| feature.attributes.get(i).unwrap_or(&null));
                for (width, value) in widest.iter_mut().zip(geometry.iter().chain(attributes)) {
                    *width = (*width).max(display_width(value, options));
                }
            }
        }
        out.push_str("<cols>\n");
        for (i, longest) in widest.iter().enumerate() {
            let is_datetime = i >= extra.len() && layer.schema.fields()[i - extra.len()].field_type == FieldType::DateTime;
            let width = match options.column_width {
                XlsxColumnWidth::Default if is_datetime => 29.0,
                XlsxColumnWidth::Default => 15.0,
                XlsxColumnWidth::Fixed(width) => width,
                XlsxColumnWidth::Auto => ((longest + 2) as f64).clamp(8.0, 255.0),
            };
            let _ = writeln!(out, "<col min=\"{0}\" max=\"{0}\" width=\"{width}\" customWidth=\"1\"/>", i + 1);
        }
        out.push_str("</cols>\n");
    }
    out.push_str("<sheetData>\n");
    let letters: Vec<String> = (0..names.len()).map(column_letters).collect();
    let mut row = 1usize;
    // Blanks in the row after the header are cells that are not text, which tell it is a header.
    let blank_row = if has_header { 2 } else { 0 };
    if has_header {
        let _ = write!(out, "<row r=\"{row}\">");
        for (name, letter) in names.iter().zip(&letters) {
            styled_text_cell(&mut out, &format!("{letter}{row}"), name, styles.header, strings)?;
        }
        out.push_str("</row>\n");
        row += 1;
    }
    let fields = layer.schema.len();
    let null = FieldValue::Null;
    for feature in &layer.features {
        let _ = write!(out, "<row r=\"{row}\">");
        let geometry = geometry_values(feature, &extra);
        let attributes = (0..fields).map(|i| feature.attributes.get(i).unwrap_or(&null));
        for (value, letter) in geometry.iter().chain(attributes).zip(&letters) {
            let blank = matches!(value, FieldValue::Null) || matches!(value, FieldValue::Float(v) if !v.is_finite());
            if blank && row == blank_row {
                let _ = write!(out, "<c r=\"{letter}{row}\"/>");
            } else {
                value_cell(&mut out, &format!("{letter}{row}"), value, styles, strings)?;
            }
        }
        out.push_str("</row>\n");
        row += 1;
    }
    out.push_str("</sheetData>\n");
    let filter = (options.autofilter && has_header).then(|| {
        let last = &letters[letters.len() - 1];
        (format!("A1:{last}{}", row - 1), format!("$A$1:${last}${}", row - 1))
    });
    if let Some((reference, _)) = &filter {
        let _ = writeln!(out, "<autoFilter ref=\"{reference}\"/>");
    }
    out.push_str("</worksheet>\n");
    Ok((out, filter.map(|(_, absolute)| absolute)))
}

/// The start of a text, of `max` UTF-16 units at most, the units Excel counts.
fn truncate_utf16(s: &str, max: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= max
        })
        .collect()
}

/// Unique sheet names Excel accepts: without `[]:*?/\`, without quotes at their ends, of 31
/// UTF-16 units at most. Names that differ only in case are the same name to Excel.
fn sheet_names(layers: &[Layer]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    layers
        .iter()
        .enumerate()
        .map(|(i, layer)| {
            let cleaned: String =
                layer.name.chars().map(|c| if "[]:*?/\\".contains(c) || !xml_legal(c) || c < ' ' { '_' } else { c }).collect();
            let cleaned = cleaned.trim_matches('\'');
            let base = if cleaned.is_empty() { format!("Sheet{}", i + 1) } else { cleaned.to_string() };
            let mut name = truncate_utf16(&base, MAX_SHEET_NAME).trim_end_matches('\'').to_string();
            let mut n = 2;
            while !seen.insert(name.to_lowercase()) {
                let suffix = format!("_{n}");
                name = truncate_utf16(&base, MAX_SHEET_NAME - suffix.len()).trim_end_matches('\'').to_string() + suffix.as_str();
                n += 1;
            }
            name
        })
        .collect()
}

/// Encodes layers as a workbook, a sheet per layer.
pub fn to_bytes(layers: &[Layer], options: &XlsxOptions) -> Result<Vec<u8>> {
    if layers.is_empty() {
        return Err(Error::InvalidArgument("no layers to write: a workbook needs a sheet".into()));
    }
    // With the styles and shared strings, what the reader's limit on a part's entries allows.
    if layers.len() > MAX_WORKBOOK_ENTRIES - 2 {
        return Err(Error::InvalidArgument(format!("XLSX: more than {} sheets to write", MAX_WORKBOOK_ENTRIES - 2)));
    }
    let main_type = if options.macro_enabled {
        "application/vnd.ms-excel.sheet.macroEnabled.main+xml"
    } else {
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"
    };
    let mut content_types = format!(
        "{XML_HEADER}<Types xmlns=\"{PACKAGE}/content-types\">\n\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\n\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>\n\
         <Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\n\
         <Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>\n\
         <Override PartName=\"/xl/workbook.xml\" ContentType=\"{main_type}\"/>\n\
         <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\n\
         <Override PartName=\"/xl/sharedStrings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml\"/>\n"
    );
    // A view is what a frozen pane's sheet view refers to.
    let views = if options.freeze_header { "<bookViews><workbookView/></bookViews>\n" } else { "" };
    let mut workbook = format!("{XML_HEADER}<workbook xmlns=\"{MAIN_NS}\" xmlns:r=\"{OD_RELS}\">\n{views}<sheets>\n");
    let (styles_used, styles_body) = cell_styles(options);
    let mut filters = String::new();
    let mut rels = format!(
        "{XML_HEADER}<Relationships xmlns=\"{PACKAGE}/relationships\">\n\
         <Relationship Id=\"rId1\" Type=\"{OD_RELS}/styles\" Target=\"styles.xml\"/>\n\
         <Relationship Id=\"rId2\" Type=\"{OD_RELS}/sharedStrings\" Target=\"sharedStrings.xml\"/>\n"
    );
    let mut strings = StringTable::default();
    let mut sheets = Vec::with_capacity(layers.len());
    for (i, (layer, name)) in layers.iter().zip(sheet_names(layers)).enumerate() {
        let number = i + 1;
        let _ = writeln!(
            content_types,
            "<Override PartName=\"/xl/worksheets/sheet{number}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        );
        let _ = writeln!(workbook, "<sheet name=\"{}\" sheetId=\"{number}\" r:id=\"rId{}\"/>", xml_text(&name), i + 3);
        let _ = writeln!(rels, "<Relationship Id=\"rId{}\" Type=\"{OD_RELS}/worksheet\" Target=\"worksheets/sheet{number}.xml\"/>", i + 3);
        let (sheet, filter) = sheet_xml(layer, options, &styles_used, &mut strings)?;
        sheets.push(sheet);
        if let Some(absolute) = filter {
            let quoted = name.replace('\'', "''");
            let _ = writeln!(
                filters,
                "<definedName name=\"_xlnm._FilterDatabase\" localSheetId=\"{i}\" hidden=\"1\">{}</definedName>",
                xml_text(&format!("'{quoted}'!{absolute}"))
            );
        }
    }
    content_types.push_str("</Types>\n");
    workbook.push_str("</sheets>\n");
    if !filters.is_empty() {
        let _ = writeln!(workbook, "<definedNames>\n{filters}</definedNames>");
    }
    workbook.push_str("</workbook>\n");
    rels.push_str("</Relationships>\n");
    let package_rels = format!(
        "{XML_HEADER}<Relationships xmlns=\"{PACKAGE}/relationships\">\n\
         <Relationship Id=\"rId1\" Type=\"{OD_RELS}/officeDocument\" Target=\"xl/workbook.xml\"/>\n\
         <Relationship Id=\"rId2\" Type=\"{PACKAGE}/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\n\
         <Relationship Id=\"rId3\" Type=\"{OD_RELS}/extended-properties\" Target=\"docProps/app.xml\"/>\n\
         </Relationships>\n"
    );
    let created = options
        .creation_time
        .as_ref()
        .map_or(String::new(), |time| format!("<dcterms:created xsi:type=\"dcterms:W3CDTF\">{}</dcterms:created>\n", xml_text(time)));
    let core = format!(
        "{XML_HEADER}<cp:coreProperties xmlns:cp=\"{PACKAGE}/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:dcterms=\"http://purl.org/dc/terms/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n{created}</cp:coreProperties>\n"
    );
    let app = format!(
        "{XML_HEADER}<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\">\n<Application>terrio</Application>\n</Properties>\n"
    );
    let styles = format!("{XML_HEADER}<styleSheet xmlns=\"{MAIN_NS}\">\n{styles_body}</styleSheet>\n");
    let mut parts: Vec<(String, String)> = vec![
        ("[Content_Types].xml".into(), content_types),
        ("_rels/.rels".into(), package_rels),
        ("docProps/app.xml".into(), app),
        ("docProps/core.xml".into(), core),
        ("xl/workbook.xml".into(), workbook),
        ("xl/_rels/workbook.xml.rels".into(), rels),
        ("xl/styles.xml".into(), styles),
        ("xl/sharedStrings.xml".into(), strings.to_xml()),
    ];
    parts.extend(sheets.into_iter().enumerate().map(|(i, xml)| (format!("xl/worksheets/sheet{}.xml", i + 1), xml)));
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    // A fixed time: the same layers make the same bytes.
    let member = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
    for (name, xml) in &parts {
        zip.start_file(name.as_str(), member)?;
        zip.write_all(xml.as_bytes())?;
    }
    Ok(zip.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    trait AddRows {
        fn add_rows(self, count: usize) -> String;
    }

    impl AddRows for String {
        fn add_rows(mut self, count: usize) -> String {
            self.push_str(&"<row/>".repeat(count));
            self
        }
    }

    /// A workbook of raw parts: sheets by name and XML body of `sheetData`.
    fn workbook(sheets: &[(&str, &str)], strings: &[&str], styles: &str, pr: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let member = zip::write::SimpleFileOptions::default();
        let mut add = |name: String, body: String| {
            zip.start_file(name.as_str(), member).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        let list: String =
            sheets.iter().enumerate().map(|(i, (n, _))| format!("<sheet name=\"{n}\" sheetId=\"{0}\" r:id=\"rId{0}\"/>", i + 1)).collect();
        add(
            "xl/workbook.xml".into(),
            format!("<workbook xmlns=\"{MAIN_NS}\" xmlns:r=\"{OD_RELS}\">{pr}<sheets>{list}</sheets></workbook>"),
        );
        let rels: String = (0..sheets.len())
            .map(|i| format!("<Relationship Id=\"rId{0}\" Type=\"{OD_RELS}/worksheet\" Target=\"worksheets/sheet{0}.xml\"/>", i + 1))
            .collect();
        add("xl/_rels/workbook.xml.rels".into(), format!("<Relationships xmlns=\"{PACKAGE}/relationships\">{rels}</Relationships>"));
        let si: String = strings.iter().map(|s| format!("<si>{s}</si>")).collect();
        add("xl/sharedStrings.xml".into(), format!("<sst xmlns=\"{MAIN_NS}\">{si}</sst>"));
        add("xl/styles.xml".into(), format!("<styleSheet xmlns=\"{MAIN_NS}\">{styles}</styleSheet>"));
        for (i, (_, data)) in sheets.iter().enumerate() {
            add(
                format!("xl/worksheets/sheet{}.xml", i + 1),
                format!("<worksheet xmlns=\"{MAIN_NS}\"><sheetData>{data}</sheetData></worksheet>"),
            );
        }
        zip.finish().unwrap().into_inner()
    }

    fn one_sheet(data: &str, options: &XlsxOptions) -> Layer {
        parse(workbook(&[("s", data)], &["<t>a</t>", "<t>b</t>", "<t>c</t>"], "", ""), &[], options).unwrap().remove(0)
    }

    fn field_types(layer: &Layer) -> Vec<FieldType> {
        layer.schema.fields().iter().map(|f| f.field_type).collect()
    }

    fn field_names(layer: &Layer) -> Vec<&str> {
        layer.schema.fields().iter().map(|f| f.name.as_str()).collect()
    }

    const TEXT_THEN_NUMBERS: &str = "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c></row>\
        <row r=\"2\"><c r=\"A2\"><v>1</v></c><c r=\"B2\" t=\"s\"><v>2</v></c></row>";

    #[test]
    fn header_detection_follows_the_headers_option() {
        let auto = one_sheet(TEXT_THEN_NUMBERS, &XlsxOptions::default());
        assert_eq!(field_names(&auto), ["a", "b"]);
        assert_eq!(field_types(&auto), [FieldType::Integer, FieldType::Text]);
        // Row numbers: the header is row 1.
        assert_eq!(auto.features[0].fid, 2);
        assert_eq!(auto.features[0].attributes[0], FieldValue::Integer(1));

        let mut disable = XlsxOptions::default();
        disable.set("HEADERS", "DISABLE").unwrap();
        let l = one_sheet(TEXT_THEN_NUMBERS, &disable);
        assert_eq!(field_names(&l), ["Field1", "Field2"]);
        assert_eq!(l.features[0].fid, 1);
        assert_eq!(l.len(), 2);

        // Two rows of text, or a single row: no way to tell, so no header.
        let text = "<row r=\"1\"><c t=\"s\"><v>0</v></c></row><row r=\"2\"><c t=\"s\"><v>1</v></c></row>";
        assert_eq!(field_names(&one_sheet(text, &XlsxOptions::default())), ["Field1"]);
        let mut force = XlsxOptions::default();
        force.set("HEADERS", "FORCE").unwrap();
        assert_eq!(field_names(&one_sheet(text, &force)), ["a"]);
        let single = one_sheet("<row r=\"1\"><c t=\"s\"><v>0</v></c></row>", &force);
        assert_eq!(field_names(&single), ["Field1"]);
        assert_eq!(single.features[0].attributes[0], FieldValue::Text("a".into()));
    }

    #[test]
    fn a_blank_first_cell_is_not_a_header_but_keeps_the_column() {
        let data = "<row r=\"1\"><c r=\"B1\" t=\"s\"><v>0</v></c></row><row r=\"2\"><c r=\"A2\"><v>1</v></c><c r=\"B2\"><v>2</v></c></row>";
        let l = one_sheet(data, &XlsxOptions::default());
        assert_eq!(field_names(&l), ["Field1", "Field2"]);
        // A column with no cell in the first row is text, as in GDAL.
        assert_eq!(field_types(&l), [FieldType::Text, FieldType::Text]);
        assert_eq!(l.features[1].attributes[0], FieldValue::Text("1".into()));
        assert_eq!(l.features[1].attributes[1], FieldValue::Text("2".into()));
    }

    #[test]
    fn field_types_widen_as_gdal_widens_them() {
        let row = |n: usize, cells: &str| format!("<row r=\"{n}\">{cells}</row>");
        let data = [
            row(1, "<c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c><c t=\"s\"><v>2</v></c><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c>"),
            row(2, "<c><v>1</v></c><c><v>2</v></c><c t=\"b\"><v>1</v></c><c><v>7</v></c><c t=\"b\"><v>0</v></c>"),
            row(3, "<c><v>1.5</v></c><c><v>4294967296</v></c><c t=\"b\"><v>0</v></c><c t=\"s\"><v>2</v></c><c><v>3</v></c>"),
        ]
        .concat();
        let l = one_sheet(&data, &XlsxOptions::default());
        // Repeated headings stay separate columns.
        assert_eq!(field_names(&l), ["a", "b", "c", "a_2", "b_2"]);
        assert_eq!(field_types(&l), [FieldType::Float, FieldType::Integer, FieldType::Boolean, FieldType::Text, FieldType::Integer]);
        assert_eq!(l.features[0].attributes[0], FieldValue::Float(1.0));
        assert_eq!(l.features[1].attributes[1], FieldValue::Integer(4_294_967_296));
        assert_eq!(l.features[1].attributes[2], FieldValue::Boolean(false));
        assert_eq!(l.features[0].attributes[3], FieldValue::Text("7".into()));
        assert_eq!(l.features[0].attributes[4], FieldValue::Integer(0));
    }

    #[test]
    fn gaps_make_empty_rows_and_ids_are_row_numbers() {
        let data = "<row r=\"2\"><c r=\"B2\" t=\"s\"><v>0</v></c></row><row r=\"3\"><c r=\"B3\"><v>1</v></c></row><row r=\"6\"><c r=\"B6\"><v>3</v></c><c r=\"F6\"><v>9</v></c></row>";
        let l = one_sheet(data, &XlsxOptions::default());
        assert_eq!(l.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(l.schema.len(), 6);
        assert_eq!(l.features[3].attributes.iter().filter(|v| **v != FieldValue::Null).count(), 0);
        assert_eq!(l.features[5].attributes[5], FieldValue::Integer(9));
    }

    #[test]
    fn dates_times_and_their_text_forms() {
        let styles = "<numFmts><numFmt numFmtId=\"170\" formatCode=\"yyyy-mm-dd hh:mm:ss\"/></numFmts><cellXfs><xf numFmtId=\"0\"/><xf numFmtId=\"14\"/><xf numFmtId=\"170\"/><xf numFmtId=\"18\"/></cellXfs>";
        let data = "<row r=\"1\"><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c><c t=\"s\"><v>2</v></c></row>\
            <row r=\"2\"><c s=\"1\"><v>45293</v></c><c s=\"2\"><v>45293.5</v></c><c s=\"3\"><v>0.25</v></c></row>";
        let parsed = |pr: &str, options: &XlsxOptions| {
            parse(workbook(&[("s", data)], &["<t>d</t>", "<t>dt</t>", "<t>t</t>"], styles, pr), &[], options).unwrap().remove(0)
        };
        let l = parsed("", &XlsxOptions::default());
        assert_eq!(field_types(&l), [FieldType::Date, FieldType::DateTime, FieldType::Text]);
        assert_eq!(l.features[0].attributes[0], FieldValue::Date("2024-01-02".into()));
        assert_eq!(l.features[0].attributes[1], FieldValue::DateTime("2024-01-02T12:00:00".into()));
        assert_eq!(l.features[0].attributes[2], FieldValue::Text("06:00:00".into()));
        // As strings, the way GDAL prints them.
        let mut strings = XlsxOptions::default();
        strings.set("FIELD_TYPES", "STRING").unwrap();
        let l = parsed("", &strings);
        assert_eq!(field_types(&l), [FieldType::Text; 3]);
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("2024/01/02".into()));
        assert_eq!(l.features[0].attributes[1], FieldValue::Text("2024/01/02 12:00:00".into()));
        // The 1904 date system.
        let l = parsed("<workbookPr date1904=\"1\"/>", &XlsxOptions::default());
        assert_eq!(l.features[0].attributes[0], FieldValue::Date("2028-01-03".into()));
    }

    #[test]
    fn strings_keep_their_text() {
        let data = "<row r=\"1\"><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c><c t=\"inlineStr\"><is><t> in &amp; line </t><rPh sb=\"0\" eb=\"1\"><t>ignored</t></rPh></is></c></row>";
        let strings = [
            "<t xml:space=\"preserve\"> a &lt;b&gt; </t>",
            "<r><t>rich</t></r><r><t> text</t></r><rPh sb=\"0\" eb=\"1\"><t>ignored</t></rPh>",
        ];
        let l = parse(workbook(&[("s", data)], &strings, "", ""), &[], &XlsxOptions::default()).unwrap().remove(0);
        let values: Vec<_> = l.features[0].attributes.iter().map(|v| if let FieldValue::Text(t) = v { t.as_str() } else { "" }).collect();
        assert_eq!(values, [" a <b> ", "rich text", " in & line "]);
    }

    #[test]
    fn formulas_read_as_their_cached_values() {
        let data = "<row r=\"1\"><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c></row>\
            <row r=\"2\"><c><f>1+1</f><v>2</v></c><c t=\"str\"><f>\"x\"</f><v>x</v></c></row>\
            <row r=\"3\"><c t=\"e\"><f>1/0</f><v>#DIV/0!</v></c><c><f>NOW()</f></c></row>";
        let l = one_sheet(data, &XlsxOptions::default());
        assert_eq!(field_types(&l), [FieldType::Text, FieldType::Text]);
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("2".into()));
        assert_eq!(l.features[1].attributes[0], FieldValue::Text("#DIV/0!".into()));
        assert_eq!(l.features[1].attributes[1], FieldValue::Null);
    }

    #[test]
    fn geometries_come_only_from_named_columns() {
        let data = "<row r=\"1\"><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c><c t=\"s\"><v>2</v></c></row>\
            <row r=\"2\"><c><v>-122.5</v></c><c><v>45.25</v></c><c><v>1</v></c></row><row r=\"3\"><c/><c><v>1</v></c><c><v>2</v></c></row>";
        let strings = ["<t>lon</t>", "<t>lat</t>", "<t>id</t>"];
        let bytes = workbook(&[("s", data)], &strings, "", "");
        let plain = parse(bytes.clone(), &[], &XlsxOptions::default()).unwrap().remove(0);
        assert!(plain.features[0].geometry.is_none());
        assert_eq!(plain.crs_epsg(), None);

        let mut options = XlsxOptions::default();
        options.set("X_POSSIBLE_NAMES", "lon*").unwrap();
        options.set("Y_POSSIBLE_NAMES", "LAT").unwrap();
        let l = parse(bytes.clone(), &[], &options).unwrap().remove(0);
        assert_eq!(l.features[0].geometry, Some(Geometry::point(-122.5, 45.25)));
        assert_eq!(l.features[1].geometry, None);
        assert_eq!(l.crs_epsg(), Some(4326));
        assert_eq!(l.schema.len(), 3);
        options.set("KEEP_GEOM_COLUMNS", "NO").unwrap();
        let l = parse(bytes, &[], &options).unwrap().remove(0);
        assert_eq!(field_names(&l), ["id"]);
        assert_eq!(l.features[0].attributes, [FieldValue::Integer(1)]);
    }

    #[test]
    fn only_named_sheets_are_read() {
        let bytes = workbook(&[("one", "<row r=\"1\"><c><v>1</v></c></row>"), ("two", "")], &[], "", "");
        let names =
            |only: &[String]| parse(bytes.clone(), only, &XlsxOptions::default()).unwrap().into_iter().map(|l| l.name).collect::<Vec<_>>();
        assert_eq!(names(&[]), ["one", "two"]);
        assert_eq!(names(&["two".to_string()]), ["two"]);
    }

    #[test]
    fn in_memory_and_file_paths_agree() {
        let layers = [sample_layer()];
        let options = crate::VectorWriteOptions::default();
        let bytes = crate::memory::write_vector_bytes(&layers, "o.xlsx", &options).unwrap();
        let read = |name: &str| crate::memory::open_vector_bytes(bytes.clone(), name, &Default::default()).unwrap();
        assert_eq!(read("o.xlsx").driver, crate::formats::VectorDriver::XLSX);
        assert_eq!(read("o.xlsm").layers[0].name, "my layer");
        let raw = workbook(&[("layer", "<row r=\"1\"><c><v>1</v></c></row>")], &[], "", "");
        // Without a telling name, a first member under xl/ shows what it is.
        assert!(looks_like(&raw, "download.bin"));
        assert!(!looks_like(&to_bytes(&layers, &XlsxOptions::default()).unwrap(), "download.bin"));
        assert!(looks_like(&to_bytes(&layers, &XlsxOptions::default()).unwrap(), "o.xlsm"));
        let found = crate::memory::open_vector_bytes(raw, "named.xlsx", &Default::default()).unwrap();
        // A sheet may really be named "layer".
        assert_eq!(found.layers[0].name, "layer");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.xlsm");
        let path = path.to_str().unwrap();
        crate::write_vector(&layers, path, &options).unwrap();
        let on_disk = crate::open_vector(path, &Default::default()).unwrap();
        assert_eq!(on_disk.layers[0].len(), 2);
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        assert!(read_part(&mut zip, "[Content_Types].xml").unwrap().unwrap().contains("macroEnabled"));

        let mut open = crate::VectorOpenOptions { layers: vec!["nope".into()], ..Default::default() };
        open.set("HEADERS", "DISABLE").unwrap();
        assert!(matches!(crate::open_vector(path, &open), Err(Error::InvalidArgument(m)) if m.contains("my layer")));
    }

    #[test]
    fn too_many_cells_are_refused() {
        let mut budget = 1;
        assert!(take_cells(&mut budget, 1).is_ok());
        assert!(matches!(take_cells(&mut budget, 1), Err(Error::ResourceLimit(m)) if m.contains("cells")));
    }

    #[test]
    fn rows_and_padding_count_against_the_cell_limit() {
        let table_options = XlsxOptions::default();
        let run = |data: &str, mut budget: usize| {
            let mut table = SheetTable::new(&table_options);
            let shared = SharedParts { strings: Vec::new(), styles: Vec::new() };
            read_sheet(data, "s", &shared, &mut table, &mut budget).map(|()| budget)
        };
        // An empty row, gap rows and gap cells all count, each once.
        assert_eq!(run("<sheetData><row r=\"1\"/></sheetData>", 20).unwrap(), 20 - ROW_CELLS);
        assert_eq!(run("<sheetData><row r=\"4\"/></sheetData>", 20).unwrap(), 20 - 4 * ROW_CELLS);
        assert_eq!(run("<sheetData><row r=\"1\"><c r=\"C1\"><v>1</v></c></row></sheetData>", 20).unwrap(), 20 - ROW_CELLS - 3);
        assert!(matches!(run("<sheetData><row r=\"1\"><c r=\"ZZZ1\"/></row></sheetData>", 20), Err(Error::ResourceLimit(_))));
        assert!(matches!(run(&"<sheetData>".to_string().add_rows(20), 20), Err(Error::ResourceLimit(_))));
    }

    #[test]
    fn rows_past_a_sheets_limit_are_refused() {
        let options = XlsxOptions::default();
        let shared = SharedParts { strings: Vec::new(), styles: Vec::new() };
        let run = |data: &str| {
            let mut table = SheetTable::new(&options);
            read_sheet(data, "s", &shared, &mut table, &mut { MAX_CELLS })
        };
        assert!(run(&"<sheetData>".to_string().add_rows(MAX_ROWS)).is_ok());
        let too_many = run(&"<sheetData>".to_string().add_rows(MAX_ROWS + 1));
        assert!(matches!(too_many, Err(Error::ResourceLimit(m)) if m.contains("rows")));
    }

    #[test]
    fn rows_without_columns_count_as_features() {
        let options = XlsxOptions::default();
        let shared = SharedParts { strings: Vec::new(), styles: Vec::new() };
        let table = || {
            let mut table = SheetTable::new(&options);
            read_sheet(&"<sheetData>".to_string().add_rows(3), "s", &shared, &mut table, &mut 100).unwrap();
            table
        };
        assert!(table_layer(table(), "s", false, &mut (3 * ROW_CELLS)).is_ok());
        assert!(matches!(table_layer(table(), "s", false, &mut (3 * ROW_CELLS - 1)), Err(Error::ResourceLimit(_))));
    }

    #[test]
    fn shared_strings_count_against_the_cell_limit() {
        let mut budget = 3;
        assert_eq!(shared_strings("<si/><si><t>a</t></si><si/>", &mut budget).unwrap(), ["", "a", ""]);
        assert_eq!(budget, 0);
        assert!(matches!(shared_strings("<si/>", &mut budget), Err(Error::ResourceLimit(_))));
    }

    #[test]
    fn a_long_shared_string_costs_each_cell_that_copies_it() {
        let options = XlsxOptions::default();
        let shared = SharedParts { strings: vec!["x".repeat(10 * CELL_BYTES)], styles: Vec::new() };
        let mut table = SheetTable::new(&options);
        let cell = "<c t=\"s\"><v>0</v></c>";
        let data = format!("<sheetData><row>{cell}{cell}</row></sheetData>");
        let mut budget = 100;
        read_sheet(&data, "s", &shared, &mut table, &mut budget).unwrap();
        assert_eq!(budget, 100 - ROW_CELLS - 2 * (1 + 10));
        let mut table = SheetTable::new(&options);
        assert!(matches!(read_sheet(&data, "s", &shared, &mut table, &mut 20), Err(Error::ResourceLimit(_))));
    }

    #[test]
    fn columns_past_a_sheets_limit_are_refused() {
        let options = XlsxOptions::default();
        let shared = SharedParts { strings: Vec::new(), styles: Vec::new() };
        let run = |row: String| {
            let data = format!("<sheetData><row>{row}</row></sheetData>");
            read_sheet(&data, "s", &shared, &mut SheetTable::new(&options), &mut { MAX_CELLS })
        };
        assert!(run("<c/>".repeat(MAX_COLUMNS)).is_ok());
        assert!(matches!(run("<c/>".repeat(MAX_COLUMNS + 1)), Err(Error::ResourceLimit(m)) if m.contains("columns")));
        assert!(matches!(run("<c r=\"XFE1\"/>".into()), Err(Error::ResourceLimit(m)) if m.contains("columns")));
    }

    #[test]
    fn cells_past_gdals_field_limit_keep_their_columns() {
        let data = "<row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"CAA1\"><v>2</v></c><c r=\"CAC1\"><v>3</v></c></row>";
        let l = one_sheet(data, &XlsxOptions::default());
        let column = reference_column("CAC1").unwrap();
        assert_eq!(l.schema.len(), column + 1);
        assert_eq!(l.features[0].attributes[column], FieldValue::Integer(3));
    }

    #[test]
    fn keep_geom_columns_reads_as_csv_reads_it() {
        // The open options reach both drivers, so CSV's lenient values must keep working.
        let mut options = crate::VectorOpenOptions::default();
        assert!(options.set("KEEP_GEOM_COLUMNS", "y").unwrap());
        assert!(!options.xlsx.keep_geom_columns);
        assert!(options.set("KEEP_GEOM_COLUMNS", "YES").unwrap());
        assert!(options.xlsx.keep_geom_columns);
    }

    #[test]
    fn dates_before_excels_phantom_leap_day_keep_their_serials() {
        let layer = |d: &str| {
            let mut l = Layer::new("t");
            l.add_field(FieldDef::new("d", FieldType::Date));
            l.add_field(FieldDef::new("n", FieldType::Integer));
            l.features.push(Feature { fid: 0, geometry: None, attributes: vec![FieldValue::Date(d.into()), FieldValue::Integer(1)] });
            l
        };
        for (date, serial) in [("1900-01-01", 1), ("1900-02-28", 59), ("1900-03-01", 61), ("2024-01-02", 45293)] {
            let bytes = to_bytes(&[layer(date)], &XlsxOptions::default()).unwrap();
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
            let sheet = read_part(&mut zip, "xl/worksheets/sheet1.xml").unwrap().unwrap();
            assert!(sheet.contains(&format!("<v>{serial}</v>")), "{date}: {sheet}");
            let l = parse(bytes, &[], &XlsxOptions::default()).unwrap().remove(0);
            assert_eq!(l.features[0].attributes[0], FieldValue::Date(date.into()));
        }
    }

    #[test]
    fn dates_before_1900_stay_text() {
        let mut l =
            name_and_id([FieldValue::Text("a".into()), FieldValue::Integer(1)], [FieldValue::Text("b".into()), FieldValue::Integer(2)]);
        l.add_field(FieldDef::new("d", FieldType::Date));
        l.features[0].attributes.push(FieldValue::Date("1899-12-31".into()));
        l.features[1].attributes.push(FieldValue::Date("1800-01-01".into()));
        let read = parse(to_bytes(&[l], &XlsxOptions::default()).unwrap(), &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(read.features[0].attributes[2], FieldValue::Text("1899-12-31".into()));
        assert_eq!(read.features[1].attributes[2], FieldValue::Text("1800-01-01".into()));
    }

    #[test]
    fn a_field_named_as_a_geometry_column_is_refused() {
        let mut l = Layer::new("t");
        l.add_field(FieldDef::new("WKT", FieldType::Text));
        l.features.push(Feature { fid: 0, geometry: Some(Geometry::point(1.0, 2.0)), attributes: vec![FieldValue::Null] });
        let options = XlsxOptions { geometry: CsvGeometry::AsWkt, ..Default::default() };
        assert!(matches!(to_bytes(&[l], &options), Err(Error::InvalidArgument(m)) if m.contains("WKT")));
    }

    const REPORT_STRINGS: [&str; 7] =
        ["<t>Quantity</t>", "<t>Phase Type</t>", "<t>1PH</t>", "<t>3PH</t>", "<t>Name</t>", "<t>title</t>", "<t>west</t>"];

    fn report_sheet(extra: &str) -> Vec<u8> {
        let s = |r: &str, i: usize| format!("<c r=\"{r}\" t=\"s\"><v>{i}</v></c>");
        let n = |r: &str, v: i32| format!("<c r=\"{r}\"><v>{v}</v></c>");
        let data = [
            format!("<row r=\"1\">{}</row>", s("A1", 5)),
            format!("<row r=\"3\">{}{}</row>", s("A3", 4), s("B3", 0)),
            format!("<row r=\"4\">{}</row>", s("B4", 1)),
            format!("<row r=\"5\">{}{}</row>", s("B5", 2), s("C5", 3)),
            format!("<row r=\"6\">{}{}{}</row>", s("A6", 6), n("B6", 9), n("C6", 4)),
            format!("<row r=\"7\">{}{}{}</row>", s("A7", 6), n("B7", 1), n("C7", 2)),
        ]
        .concat();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(workbook(&[("s", &data)], &REPORT_STRINGS, "", ""))).unwrap();
        let sheet =
            read_part(&mut zip, "xl/worksheets/sheet1.xml").unwrap().unwrap().replace("</sheetData>", &format!("</sheetData>{extra}"));
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for i in 0..zip.len() {
            let name = zip.by_index(i).unwrap().name().unwrap().to_string();
            let body = if name.ends_with("sheet1.xml") { sheet.clone() } else { read_part(&mut zip, &name).unwrap().unwrap() };
            out.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            out.write_all(body.as_bytes()).unwrap();
        }
        out.finish().unwrap().into_inner()
    }

    #[test]
    fn several_header_rows_join_merged_headings() {
        let bytes = report_sheet(
            "<mergeCells count=\"3\"><mergeCell ref=\"B3:C3\"/><mergeCell ref=\"B4:C4\"/><mergeCell ref=\"A3:A5\"/></mergeCells>",
        );
        let mut options = XlsxOptions::default();
        options.set("FIRST_ROW", "3").unwrap();
        options.set("HEADER_ROWS", "3").unwrap();
        let l = parse(bytes, &[], &options).unwrap().remove(0);
        assert_eq!(field_names(&l), ["Name", "Quantity / Phase Type / 1PH", "Quantity / Phase Type / 3PH"]);
        assert_eq!(field_types(&l), [FieldType::Text, FieldType::Integer, FieldType::Integer]);
        assert_eq!(l.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [6, 7]);
        assert_eq!(l.features[0].attributes[2], FieldValue::Integer(4));
    }

    #[test]
    fn a_range_reads_only_its_cells() {
        let mut options = XlsxOptions::default();
        options.set("RANGE", "B5:C6").unwrap();
        options.set("HEADER_ROWS", "1").unwrap();
        let l = parse(report_sheet(""), &[], &options).unwrap().remove(0);
        assert_eq!(field_names(&l), ["1PH", "3PH"]);
        assert_eq!(l.features.len(), 1);
        assert_eq!(l.features[0].fid, 6);
        assert_eq!(l.features[0].attributes, [FieldValue::Integer(9), FieldValue::Integer(4)]);
    }

    #[test]
    fn first_row_reaches_past_a_gap_too_large_to_fill() {
        let data = "<row r=\"30000\"><c r=\"A30000\" t=\"s\"><v>0</v></c></row><row r=\"30001\"><c r=\"A30001\"><v>1</v></c></row>";
        let mut options = XlsxOptions::default();
        options.set("FIRST_ROW", "30000").unwrap();
        let l = one_sheet(data, &options);
        assert_eq!(field_names(&l), ["a"]);
        assert_eq!(l.features[0].fid, 30001);
    }

    #[test]
    fn a_sheet_of_only_header_rows_has_their_fields() {
        let mut options = XlsxOptions::default();
        options.set("HEADER_ROWS", "1").unwrap();
        let l = one_sheet("<row r=\"1\"><c t=\"s\"><v>0</v></c></row>", &options);
        assert_eq!(field_names(&l), ["a"]);
        assert_eq!(l.features.len(), 0);
        options.set("HEADER_ROWS", "0").unwrap();
        let l = one_sheet(TEXT_THEN_NUMBERS, &options);
        assert_eq!(field_names(&l), ["Field1", "Field2"]);
        assert_eq!(l.features.len(), 2);
    }

    #[test]
    fn sheets_sharing_a_part_read_it_once() {
        let (sheets, _) = {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let member = zip::write::SimpleFileOptions::default();
            let mut add = |name: &str, body: String| {
                zip.start_file(name, member).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            };
            add(
                "xl/workbook.xml",
                format!(
                    "<workbook xmlns:r=\"{OD_RELS}\"><sheets><sheet name=\"a\" r:id=\"r1\"/><sheet name=\"b\" r:id=\"r2\"/></sheets></workbook>"
                ),
            );
            add(
                "xl/_rels/workbook.xml.rels",
                format!(
                    "<Relationships><Relationship Id=\"r1\" Type=\"{OD_RELS}/worksheet\" Target=\"s.xml\"/><Relationship Id=\"r2\" Type=\"{OD_RELS}/worksheet\" Target=\"s.xml\"/></Relationships>"
                ),
            );
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip.finish().unwrap().into_inner())).unwrap();
            let parts = workbook_sheets(&mut archive).unwrap();
            (parts.sheets, parts.date1904)
        };
        assert_eq!(sheets.len(), 1);
    }

    #[test]
    fn layout_options_are_written_and_read_back() {
        let mut options = XlsxOptions::default();
        for (key, value) in [
            ("HEADER_STYLE", "BOLD"),
            ("FREEZE_HEADER", "YES"),
            ("AUTOFILTER", "YES"),
            ("COLUMN_WIDTH", "AUTO"),
            ("DATE_FORMAT", "yyyy-mm-dd"),
            ("DATETIME_FORMAT", "yyyy-mm-dd hh:mm:ss"),
            ("CREATION_TIME", "2024-01-02T03:04:05Z"),
        ] {
            assert!(options.set_creation(key, value).unwrap(), "{key}");
        }
        let bytes = to_bytes(&[sample_layer()], &options).unwrap();
        assert_eq!(bytes, to_bytes(&[sample_layer()], &options).unwrap());
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let part = |zip: &mut zip::ZipArchive<_>, name: &str| read_part(zip, name).unwrap().unwrap();
        let sheet = part(&mut zip, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("state=\"frozen\""));
        assert!(sheet.contains("<autoFilter ref=\"A1:F3\"/>"));
        assert!(sheet.find("<sheetViews>").unwrap() < sheet.find("<cols>").unwrap());
        assert!(sheet.find("</sheetData>").unwrap() < sheet.find("<autoFilter").unwrap());
        assert!(sheet.contains("<c r=\"A1\" s=\"8\" t=\"s\">"));
        let styles = part(&mut zip, "xl/styles.xml");
        assert!(styles.contains("<fonts count=\"2\">") && styles.contains("<b/>"));
        assert!(styles.contains("<numFmts count=\"8\">") && styles.contains("<cellXfs count=\"9\">"));
        assert!(part(&mut zip, "docProps/core.xml").contains(">2024-01-02T03:04:05Z</dcterms:created>"));
        let book = part(&mut zip, "xl/workbook.xml");
        assert!(book.contains("_xlnm._FilterDatabase") && book.contains("my layer&apos;!$A$1:$F$3") && book.contains("<bookViews>"));
        // The styled cells read as the same values and types.
        let l = parse(bytes, &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(field_names(&l), ["i", "r", "s", "b", "d", "dt"]);
        assert_eq!(l.features[0].attributes[4], FieldValue::Date("2024-01-02".into()));
        assert_eq!(l.features[0].attributes[5], FieldValue::DateTime("2024-01-02T03:04:05.500".into()));
    }

    #[test]
    fn default_options_keep_gdals_layout() {
        let bytes = to_bytes(&[sample_layer()], &XlsxOptions::default()).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let sheet = read_part(&mut zip, "xl/worksheets/sheet1.xml").unwrap().unwrap();
        assert!(sheet.contains("width=\"15\"") && sheet.contains("width=\"29\""));
        assert!(!sheet.contains("sheetViews") && !sheet.contains("autoFilter"));
        assert!(read_part(&mut zip, "xl/styles.xml").unwrap().unwrap().contains("<cellXfs count=\"6\">"));
    }

    #[test]
    fn layout_option_values_are_checked() {
        let mut options = XlsxOptions::default();
        for (key, value) in
            [("COLUMN_WIDTH", "0"), ("COLUMN_WIDTH", "wide"), ("CREATION_TIME", "soon"), ("HEADER_STYLE", "ITALIC"), ("AUTOFILTER", "y")]
        {
            assert!(options.set_creation(key, value).is_err(), "{key}={value}");
        }
        for (key, value) in [("RANGE", "A5"), ("RANGE", "C5:A1"), ("FIRST_ROW", "0"), ("HEADER_ROWS", "x")] {
            assert!(options.set(key, value).is_err(), "{key}={value}");
        }
        assert!(!options.set("HEADER_STYLE", "BOLD").unwrap());
        assert!(!options.set_creation("RANGE", "A1:B2").unwrap());
    }

    #[test]
    fn workbook_entries_are_capped() {
        let entry = "<Relationship Id=\"a\" Type=\"b\" Target=\"c\"/>";
        assert!(relationships(&entry.repeat(MAX_WORKBOOK_ENTRIES), "p").is_ok());
        assert!(matches!(relationships(&entry.repeat(MAX_WORKBOOK_ENTRIES + 1), "p"), Err(Error::ResourceLimit(_))));
    }

    #[test]
    fn xlsx_write_options_are_refused_on_other_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.geojson");
        let options =
            crate::VectorWriteOptions { xlsx: XlsxOptions { geometry: CsvGeometry::AsWkt, ..Default::default() }, ..Default::default() };
        let result = crate::write_vector(&[Layer::new("a")], path.to_str().unwrap(), &options);
        assert!(matches!(result, Err(Error::InvalidArgument(m)) if m.contains("XLSX")));
    }

    #[test]
    fn cells_out_of_column_order_are_refused() {
        let options = XlsxOptions::default();
        let shared = SharedParts { strings: Vec::new(), styles: Vec::new() };
        for row in ["<c r=\"B1\"/><c r=\"A1\"/>", "<c r=\"A1\"/><c r=\"A1\"/>"] {
            let data = format!("<sheetData><row>{row}</row></sheetData>");
            let result = read_sheet(&data, "s", &shared, &mut SheetTable::new(&options), &mut 100);
            assert!(matches!(result, Err(Error::InvalidData(m)) if m.contains("column order")));
        }
    }

    #[test]
    fn a_row_past_the_limit_is_refused_whatever_its_gap() {
        let options = XlsxOptions::default();
        let shared = SharedParts { strings: Vec::new(), styles: Vec::new() };
        let data = format!("<sheetData><row r=\"{}\"/></sheetData>", MAX_ROWS + 1);
        let result = read_sheet(&data, "s", &shared, &mut SheetTable::new(&options), &mut 100);
        assert!(matches!(result, Err(Error::ResourceLimit(m)) if m.contains("rows")));
    }

    #[test]
    fn text_in_a_date_format_or_style_stays_what_it_is() {
        assert_eq!(unquoted_format(r#"#,##0.00 "odd" \d yy"#), "#,##0.00   yy");
        let styles = "<numFmts><numFmt numFmtId=\"170\" formatCode=\"0.0 &quot;odd&quot;\"/></numFmts><cellXfs><xf numFmtId=\"170\"/><xf numFmtId=\"14\"/></cellXfs>";
        let data = "<row r=\"1\"><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c></row>\
            <row r=\"2\"><c s=\"0\"><v>1.5</v></c><c s=\"1\" t=\"e\"><v>#N/A</v></c></row>";
        let l = parse(workbook(&[("s", data)], &["<t>a</t>", "<t>b</t>"], styles, ""), &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(field_types(&l), [FieldType::Float, FieldType::Text]);
        assert_eq!(l.features[0].attributes, [FieldValue::Float(1.5), FieldValue::Text("#N/A".into())]);
    }

    #[test]
    fn carriage_returns_read_back() {
        let layer = name_and_id(
            [FieldValue::Text("a\r\nb".into()), FieldValue::Integer(1)],
            [FieldValue::Text("c".into()), FieldValue::Integer(2)],
        );
        let bytes = to_bytes(&[layer], &XlsxOptions::default()).unwrap();
        let l = parse(bytes, &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("a\r\nb".into()));
    }

    #[test]
    fn the_in_memory_codec_reads_a_workbook_rather_than_its_archive() {
        let bytes = to_bytes(&[sample_layer()], &XlsxOptions::default()).unwrap();
        let opened = crate::memory::codec::open(bytes, "o.xlsx", &Default::default()).unwrap();
        let crate::Dataset::Vector(v) = opened else { panic!("a workbook is a vector dataset") };
        assert_eq!(v.driver, crate::formats::VectorDriver::XLSX);
        assert_eq!(v.layers[0].name, "my layer");
    }

    #[test]
    fn a_long_cell_value_costs_more_than_one_cell() {
        let options = XlsxOptions::default();
        let shared = SharedParts { strings: Vec::new(), styles: Vec::new() };
        let data = format!("<sheetData><row><c><v>{}</v></c></row></sheetData>", "1".repeat(10 * CELL_BYTES));
        let mut budget = 100;
        read_sheet(&data, "s", &shared, &mut SheetTable::new(&options), &mut budget).unwrap();
        assert_eq!(budget, 100 - ROW_CELLS - 1 - 10);
    }

    #[test]
    fn many_repeated_headings_get_their_suffixes_in_order() {
        let head = "<c t=\"s\"><v>0</v></c>".repeat(4);
        let data = format!("<row r=\"1\">{head}</row><row r=\"2\"><c><v>1</v></c></row>");
        let l = one_sheet(&data, &XlsxOptions::default());
        assert_eq!(field_names(&l), ["a", "a_2", "a_3", "a_4"]);
    }

    #[test]
    fn a_shared_string_index_must_resolve() {
        let data = "<row r=\"1\"><c t=\"s\"><v>9</v></c></row>";
        let bytes = workbook(&[("s", data)], &["<t>a</t>"], "", "");
        assert!(matches!(parse(bytes, &[], &XlsxOptions::default()), Err(Error::InvalidData(m)) if m.contains("shared string")));
        // A blank cell typed as a shared string stays blank.
        let l = one_sheet("<row r=\"1\"><c t=\"s\"/><c t=\"s\"><v>0</v></c></row>", &XlsxOptions::default());
        assert_eq!(l.features[0].attributes, [FieldValue::Null, FieldValue::Text("a".into())]);
    }

    #[test]
    fn shared_strings_and_styles_are_found_through_the_relationships() {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let member = zip::write::SimpleFileOptions::default();
        let mut add = |name: &str, body: String| {
            zip.start_file(name, member).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        add("xl/workbook.xml", format!("<workbook xmlns:r=\"{OD_RELS}\"><sheets><sheet name=\"s\" r:id=\"rId1\"/></sheets></workbook>"));
        add(
            "xl/_rels/workbook.xml.rels",
            format!(
                "<Relationships><Relationship Id=\"rId1\" Type=\"{OD_RELS}/worksheet\" Target=\"sheet.xml\"/>\
                 <Relationship Id=\"rId2\" Type=\"{OD_RELS}/sharedStrings\" Target=\"text/strings.xml\"/>\
                 <Relationship Id=\"rId3\" Type=\"{OD_RELS}/styles\" Target=\"text/looks.xml\"/></Relationships>"
            ),
        );
        add("xl/text/strings.xml", "<sst><si><t>a</t></si></sst>".into());
        add("xl/text/looks.xml", "<styleSheet><cellXfs><xf numFmtId=\"14\"/></cellXfs></styleSheet>".into());
        add(
            "xl/sheet.xml",
            "<worksheet><sheetData><row r=\"1\"><c t=\"s\"><v>0</v></c><c s=\"0\"><v>45293</v></c></row></sheetData></worksheet>".into(),
        );
        let bytes = zip.finish().unwrap().into_inner();
        let l = parse(bytes, &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(l.features[0].attributes, [FieldValue::Text("a".into()), FieldValue::Date("2024-01-02".into())]);
    }

    #[test]
    fn a_byte_order_mark_is_left_out_of_a_part() {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("a.xml", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"\xEF\xBB\xBF<a/>").unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zip.finish().unwrap().into_inner())).unwrap();
        assert_eq!(read_part(&mut zip, "a.xml").unwrap().unwrap(), "<a/>");
    }

    fn sample_layer() -> Layer {
        let mut l = Layer::new("my layer");
        for (name, t) in [
            ("i", FieldType::Integer),
            ("r", FieldType::Float),
            ("s", FieldType::Text),
            ("b", FieldType::Boolean),
            ("d", FieldType::Date),
            ("dt", FieldType::DateTime),
        ] {
            l.add_field(FieldDef::new(name, t));
        }
        l.features.push(Feature {
            fid: 0,
            geometry: Some(Geometry::point(1.5, 2.5)),
            attributes: vec![
                FieldValue::Integer(1_099_511_627_776),
                FieldValue::Float(0.1),
                FieldValue::Text(" <a&b> \u{1}".into()),
                FieldValue::Boolean(true),
                FieldValue::Date("2024-01-02".into()),
                FieldValue::DateTime("2024-01-02T03:04:05.500+02:00".into()),
            ],
        });
        l.features.push(Feature {
            fid: 1,
            geometry: None,
            attributes: vec![
                FieldValue::Integer(2),
                FieldValue::Null,
                FieldValue::Text("x".into()),
                FieldValue::Boolean(false),
                FieldValue::Null,
                FieldValue::Null,
            ],
        });
        l
    }

    #[test]
    fn writes_what_it_reads() {
        let bytes = to_bytes(&[sample_layer()], &XlsxOptions::default()).unwrap();
        let l = parse(bytes, &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(l.name, "my layer");
        assert_eq!(field_names(&l), ["i", "r", "s", "b", "d", "dt"]);
        assert_eq!(
            field_types(&l),
            [FieldType::Integer, FieldType::Float, FieldType::Text, FieldType::Boolean, FieldType::Date, FieldType::DateTime]
        );
        let a = &l.features[0].attributes;
        assert_eq!(a[0], FieldValue::Integer(1_099_511_627_776));
        assert_eq!(a[1], FieldValue::Float(0.1));
        assert_eq!(a[2], FieldValue::Text(" <a&b> ".into()));
        assert_eq!(a[3], FieldValue::Boolean(true));
        assert_eq!(a[4], FieldValue::Date("2024-01-02".into()));
        // The wall clock, without the offset a spreadsheet cannot hold.
        assert_eq!(a[5], FieldValue::DateTime("2024-01-02T03:04:05.500".into()));
        assert_eq!(l.features[1].attributes[1], FieldValue::Null);
        assert!(l.features[0].geometry.is_none());
    }

    /// A layer of a text and an integer field, with these first and second features.
    fn name_and_id(first: [FieldValue; 2], second: [FieldValue; 2]) -> Layer {
        let mut l = Layer::new("t");
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("id", FieldType::Integer));
        for (fid, attributes) in [first, second].into_iter().enumerate() {
            l.features.push(Feature { fid: fid as u64, geometry: None, attributes: attributes.to_vec() });
        }
        l
    }

    #[test]
    fn nulls_in_the_first_feature_keep_the_field_names() {
        let text = |s: &str| FieldValue::Text(s.into());
        for first in [[text("a"), FieldValue::Null], [FieldValue::Null, FieldValue::Null]] {
            let layer = name_and_id(first.clone(), [text("b"), FieldValue::Integer(2)]);
            let bytes = to_bytes(&[layer], &XlsxOptions::default()).unwrap();
            let l = parse(bytes, &[], &XlsxOptions::default()).unwrap().remove(0);
            assert_eq!(field_names(&l), ["name", "id"]);
            assert_eq!(field_types(&l), [FieldType::Text, FieldType::Integer]);
            assert_eq!(l.features.len(), 2);
            assert_eq!(l.features[0].attributes, first);
            assert_eq!(l.features[1].attributes[1], FieldValue::Integer(2));
        }
    }

    #[test]
    fn an_all_text_layer_needs_the_header_forced() {
        let mut layer = Layer::new("t");
        layer.add_field(FieldDef::new("name", FieldType::Text));
        layer.features.push(Feature { fid: 0, geometry: None, attributes: vec![FieldValue::Text("a".into())] });
        let bytes = to_bytes(&[layer], &XlsxOptions::default()).unwrap();
        let auto = parse(bytes.clone(), &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(field_names(&auto), ["Field1"]);
        assert_eq!(auto.features.len(), 2);
        let mut force = XlsxOptions::default();
        force.set("HEADERS", "FORCE").unwrap();
        let forced = parse(bytes, &[], &force).unwrap().remove(0);
        assert_eq!(field_names(&forced), ["name"]);
        assert_eq!(forced.features.len(), 1);
    }

    #[test]
    fn characters_xml_forbids_are_left_out() {
        let mut layer = name_and_id(
            [FieldValue::Text("a\u{FFFE}b\u{FFFF}\u{1}c\td".into()), FieldValue::Integer(1)],
            [FieldValue::Text("e".into()), FieldValue::Integer(2)],
        );
        layer.name = "s\u{FFFF}".into();
        let bytes = to_bytes(&[layer], &XlsxOptions::default()).unwrap();
        let l = parse(bytes.clone(), &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(l.name, "s_");
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("abc\td".into()));
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let strings = read_part(&mut zip, "xl/sharedStrings.xml").unwrap().unwrap();
        assert!(!strings.contains(['\u{FFFE}', '\u{FFFF}']));
    }

    #[test]
    fn the_same_layers_make_the_same_bytes() {
        let layers = [sample_layer()];
        assert_eq!(to_bytes(&layers, &XlsxOptions::default()).unwrap(), to_bytes(&layers, &XlsxOptions::default()).unwrap());
    }

    #[test]
    fn writes_geometry_columns_on_request() {
        let mut options = XlsxOptions::default();
        options.set_creation("GEOMETRY", "AS_XY").unwrap();
        let bytes = to_bytes(&[sample_layer()], &options).unwrap();
        let l = parse(bytes, &[], &XlsxOptions::default()).unwrap().remove(0);
        assert_eq!(&field_names(&l)[..2], ["X", "Y"]);
        assert_eq!(l.features[0].attributes[0], FieldValue::Float(1.5));
        options.set_creation("GEOMETRY", "AS_WKT").unwrap();
        let bytes = to_bytes(&[sample_layer()], &options).unwrap();
        let mut read = XlsxOptions::default();
        read.set("GEOM_POSSIBLE_NAMES", "WKT").unwrap();
        let l = parse(bytes, &[], &read).unwrap().remove(0);
        assert_eq!(l.features[0].geometry, Some(Geometry::point(1.5, 2.5)));
    }

    #[test]
    fn sheet_names_are_made_valid_and_unique() {
        let layers = ["a/b", "A_B", &"x".repeat(40), &"x".repeat(35), "", "'q'"].map(Layer::new);
        let names = sheet_names(&layers);
        assert_eq!(names[0], "a_b");
        assert_eq!(names[1], "A_B_2");
        assert_eq!(names[2], "x".repeat(31));
        assert_eq!(names[3], format!("{}_2", "x".repeat(29)));
        assert_eq!(names[4], "Sheet5");
        assert_eq!(names[5], "q");
        // Truncating must not leave a quote at the end.
        let names = sheet_names(&[format!("{}'b", "a".repeat(30))].map(Layer::new));
        assert_eq!(names[0], "a".repeat(30));
        // Excel counts UTF-16 units: each of these characters is two.
        let names = sheet_names(&["\u{1F600}".repeat(20), "\u{1F600}".repeat(16)].map(Layer::new));
        assert_eq!(names[0], "\u{1F600}".repeat(15));
        assert_eq!(names[1], format!("{}_2", "\u{1F600}".repeat(14)));
    }

    #[test]
    fn refuses_what_a_sheet_cannot_hold() {
        let mut l = Layer::new("t");
        l.add_field(FieldDef::new("s", FieldType::Text));
        l.features.push(Feature { fid: 0, geometry: None, attributes: vec![FieldValue::Text("x".repeat(MAX_CELL_TEXT_UTF16 + 1))] });
        assert!(matches!(to_bytes(&[l], &XlsxOptions::default()), Err(Error::InvalidArgument(m)) if m.contains("characters")));
    }

    #[test]
    fn macro_enabled_workbooks_declare_it() {
        let options = XlsxOptions { macro_enabled: true, ..Default::default() };
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(to_bytes(&[sample_layer()], &options).unwrap())).unwrap();
        let types = read_part(&mut zip, "[Content_Types].xml").unwrap().unwrap();
        assert!(types.contains("macroEnabled"));
    }

    #[test]
    fn column_letters_and_references_agree() {
        for (column, letters) in [(0, "A"), (25, "Z"), (26, "AA"), (51, "AZ"), (52, "BA"), (701, "ZZ"), (702, "AAA")] {
            assert_eq!(column_letters(column), letters);
            assert_eq!(reference_column(&format!("{letters}7")), Some(column));
        }
        assert_eq!(reference_column("ZZZ1"), Some(MAX_COLUMNS));
        assert_eq!(reference_column("7"), None);
    }
}
