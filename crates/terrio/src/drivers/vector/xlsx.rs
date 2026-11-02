// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/xlsx/ogrxlsxdatasource.cpp
// No code is translated from these sources.

//! Office Open XML spreadsheets (`.xlsx`, `.xlsm`): each worksheet is an attribute layer.
//!
//! Reading follows GDAL's XLSX driver: the `HEADERS` and `FIELD_TYPES` open options, its detection
//! of a header line, its field type widening, and feature ids that are row numbers. Rows without
//! cells stay empty features. Formulas are not evaluated: a formula cell reads as its cached value.
//!
//! Beyond GDAL, naming coordinate or WKT columns (`X_POSSIBLE_NAMES`, `Y_POSSIBLE_NAMES`,
//! `Z_POSSIBLE_NAMES`, `GEOM_POSSIBLE_NAMES`) reads geometries from them, and the `GEOMETRY`
//! creation option writes them. Without those options sheets hold no geometries, as in GDAL.
//!
//! Where this differs from GDAL on purpose: a `date1904` workbook's dates are honored (GDAL reads
//! them four years early), a boolean column that also holds other numbers becomes an integer field
//! (GDAL keeps it boolean and loses them), repeated headings get unique names, phonetic guides in
//! shared strings are left out of the text, column widths are written per column (GDAL writes
//! overlapping ranges), and sheet names are made valid for Excel. A time of day reads as `HH:MM:SS`
//! text, since layers have no time type, so it is written back as text.
//!
//! The first feature's null values are written as blank cells, which GDAL's driver and this one
//! count as non-text when they look for a header. A layer whose first feature is all text cannot be
//! told from a table without a header, so its names read back as data unless `HEADERS=FORCE` is set.

use super::xlsx_options::{XlsxFieldTypes, XlsxHeaders, XlsxOptions};
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
/// GDAL's limit on the column of a cell reference.
const MAX_REFERENCED_COLUMN: usize = 2000;
/// GDAL's limits on the empty rows a gap between two rows makes.
const MAX_ROW_GAP: usize = 10_000;
const MAX_GAP_CELLS: usize = 100_000;
/// The most a workbook's part may expand to, and the most cells (blank ones in the table included)
/// a workbook may hold: an untrusted file is refused rather than exhausting memory.
const MAX_PART_BYTES: u64 = 2 << 30;
const MAX_CELLS: usize = 100_000_000;
/// What a row costs against that limit, for the row entry and feature it holds even without a cell.
const ROW_CELLS: usize = 4;
/// The bytes of a repeated shared string that cost as much as another cell.
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
            has_header: false,
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }

    fn detect_header(&self, line: &[Cell]) -> bool {
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
        let line = self.line;
        self.line += 1;
        if line == 0 {
            self.first_line = cells;
            return;
        }
        if line == 1 {
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
                self.rows.push((1, std::mem::take(&mut self.first_line)));
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
        self.rows.push((line as u64 + 1, cells));
    }

    /// Ends the sheet: a single line is data, whatever `HEADERS` says.
    fn finish(&mut self) {
        if self.line == 1 && !self.first_line.is_empty() {
            self.columns_from_first_line();
            self.rows.push((1, std::mem::take(&mut self.first_line)));
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
    let offset = if date1904 { DATE1904_OFFSET_DAYS } else { 0 };
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
        // A layer's field names are unique, where GDAL's layers repeat a repeated heading.
        let mut name = c.name.clone();
        let mut n = 2;
        while layer.schema.field_index(&name).is_some() {
            name = format!("{}_{n}", c.name);
            n += 1;
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
    file.by_ref().take(MAX_PART_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PART_BYTES {
        return Err(too_large());
    }
    // Valid text keeps its bytes, so that a large part is not held twice.
    Ok(Some(match String::from_utf8(bytes) {
        Ok(text) if text.starts_with('\u{FEFF}') => text['\u{FEFF}'.len_utf8()..].to_string(),
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

/// The relationships of a part: id, type and target.
fn relationships(text: &str, part: &str) -> Result<Vec<(String, String, String)>> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut out = Vec::new();
    loop {
        match reader.read_event().map_err(|e| xml_error(part, e))? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "Relationship" => {
                if let (Some(id), Some(kind), Some(target)) = (attribute(&e, "Id"), attribute(&e, "Type"), attribute(&e, "Target")) {
                    out.push((id, kind, target));
                }
            }
            Event::Eof => return Ok(out),
            _ => {}
        }
    }
}

/// The name and part of each worksheet, in workbook order, and whether dates count from 1904.
fn workbook_sheets<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>) -> Result<(Vec<(String, String)>, bool)> {
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
    let targets: HashMap<String, String> = relationships(&rels, &rels_part)?
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
                    {
                        sheets.push((name, target.clone()));
                    }
                }
                _ => {}
            },
            Event::Eof => return Ok((sheets, date1904)),
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
                    take_cells(budget, 1)?;
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
        column = Some(next.min(MAX_REFERENCED_COLUMN));
    }
    column
}

/// The workbook's parts every sheet needs.
struct SharedParts {
    strings: Vec<String>,
    styles: Vec<CellKind>,
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
        let (target, line) = (row - 1, table.line);
        if target <= line {
            return Ok(());
        }
        let gap = target - line;
        let fields = table.first_line.len().max(table.columns.len());
        // As GDAL's driver: too big a gap is ignored rather than filled with empty rows.
        if gap > MAX_ROW_GAP || (fields > 0 && gap > MAX_GAP_CELLS / fields) {
            return Ok(());
        }
        if target >= MAX_ROWS {
            return Err(too_many_rows(part));
        }
        take_cells(budget, gap.saturating_mul(ROW_CELLS))?;
        for _ in 0..gap {
            table.end_line(Vec::new());
        }
        Ok(())
    };
    let start_cell = |e: &BytesStart<'_>, cells: &mut Vec<Cell>, budget: &mut usize| -> Result<(bool, CellKind)> {
        if let Some(column) = attribute(e, "r").as_deref().and_then(reference_column) {
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
            found.cloned().unwrap_or(value)
        } else {
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
    let (sheets, date1904) = workbook_sheets(&mut zip)?;
    let (mut budget, mut table_budget) = (MAX_CELLS, MAX_CELLS);
    let mut strings = Vec::new();
    for name in ["xl/sharedStrings.xml", "xl/SharedStrings.xml"] {
        if let Some(text) = read_part(&mut zip, name)? {
            strings = shared_strings(&text, &mut budget)?;
            break;
        }
    }
    let styles = read_part(&mut zip, "xl/styles.xml")?.map(|text| style_kinds(&text)).transpose()?.unwrap_or_default();
    let shared = SharedParts { strings, styles };
    let mut layers = Vec::with_capacity(sheets.len());
    // Naming no sheet here leaves the caller to report the names there are.
    let filter = sheets.iter().any(|(name, _)| only.contains(name));
    for (name, part) in sheets {
        if filter && !only.contains(&name) {
            continue;
        }
        let mut table = SheetTable::new(options);
        // A workbook naming a part it lacks still lists the sheet, empty, as GDAL's driver does.
        if let Some(text) = read_part(&mut zip, &part)? {
            read_sheet(&text, &part, &shared, &mut table, &mut budget)?;
        }
        layers.push(table_layer(table, &name, date1904, &mut table_budget)?);
    }
    Ok(layers)
}

/// Whether bytes look like a workbook: a ZIP archive named so, or whose first member is under `xl/`.
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
    if s.chars().all(xml_legal) { xml::escape(s) } else { xml::escape(&s.chars().filter(|&c| xml_legal(c)).collect::<String>()) }
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

/// A date-time's wall clock as milliseconds since 1970, without its UTC offset: spreadsheets have no
/// time zones, and the value reads back as it was written.
fn wall_clock_millis(s: &str) -> Option<i64> {
    let time = s.get(10..)?;
    let end = time.char_indices().skip(1).find(|(_, c)| matches!(c, 'Z' | '+' | '-')).map_or(time.len(), |(i, _)| i);
    datetime::parse_datetime(&s[..10 + end])
}

/// Appends a cell holding a text value.
fn text_cell(out: &mut String, reference: &str, text: &str, strings: &mut StringTable) -> Result<()> {
    if text.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
        return Err(Error::InvalidArgument(format!(
            "XLSX: cell {reference} holds more than the {MAX_CELL_TEXT_UTF16} characters a cell can"
        )));
    }
    let _ = write!(out, "<c r=\"{reference}\" t=\"s\"><v>{}</v></c>", strings.index(text));
    Ok(())
}

/// Appends a cell holding a number in a style.
fn number_cell(out: &mut String, reference: &str, style: u8, value: impl std::fmt::Display) {
    let _ = write!(out, "<c r=\"{reference}\" s=\"{style}\"><v>{value}</v></c>");
}

fn value_cell(out: &mut String, reference: &str, value: &FieldValue, strings: &mut StringTable) -> Result<()> {
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
        FieldValue::Date(s) => match datetime::parse_date(s) {
            Some(days) => number_cell(out, reference, STYLE_DATE, i64::from(days) + UNIX_EPOCH_SERIAL),
            None => text_cell(out, reference, s, strings)?,
        },
        FieldValue::DateTime(s) => match wall_clock_millis(s) {
            Some(ms) => {
                let style = if ms.rem_euclid(1000) == 0 { STYLE_DATETIME } else { STYLE_DATETIME_MS };
                number_cell(out, reference, style, ms as f64 / 86_400_000.0 + UNIX_EPOCH_SERIAL as f64);
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
fn sheet_xml(layer: &Layer, options: &XlsxOptions, strings: &mut StringTable) -> Result<String> {
    let extra = geometry_header(layer, options)?;
    let names: Vec<&str> = extra.iter().copied().chain(layer.schema.fields().iter().map(|f| f.name.as_str())).collect();
    if names.len() > MAX_COLUMNS {
        return Err(Error::InvalidArgument(format!("XLSX: '{}' has more than the {MAX_COLUMNS} columns a sheet can hold", layer.name)));
    }
    let has_header = names.iter().enumerate().any(|(i, n)| *n != format!("Field{}", i + 1)) && !layer.features.is_empty();
    if layer.features.len() + usize::from(has_header) > MAX_ROWS {
        return Err(Error::InvalidArgument(format!("XLSX: '{}' has more than the {MAX_ROWS} rows a sheet can hold", layer.name)));
    }
    let mut out = format!("{XML_HEADER}<worksheet xmlns=\"{MAIN_NS}\" xmlns:r=\"{OD_RELS}\">\n");
    if !names.is_empty() {
        out.push_str("<cols>\n");
        for i in 0..names.len() {
            let is_datetime = i >= extra.len() && layer.schema.fields()[i - extra.len()].field_type == FieldType::DateTime;
            let _ =
                writeln!(out, "<col min=\"{0}\" max=\"{0}\" width=\"{1}\" customWidth=\"1\"/>", i + 1, if is_datetime { 29 } else { 15 });
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
            text_cell(&mut out, &format!("{letter}{row}"), name, strings)?;
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
                value_cell(&mut out, &format!("{letter}{row}"), value, strings)?;
            }
        }
        out.push_str("</row>\n");
        row += 1;
    }
    out.push_str("</sheetData>\n</worksheet>\n");
    Ok(out)
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
            let mut name = truncate_utf16(&base, MAX_SHEET_NAME);
            let mut n = 2;
            while !seen.insert(name.to_lowercase()) {
                let suffix = format!("_{n}");
                name = truncate_utf16(&base, MAX_SHEET_NAME - suffix.len()) + suffix.as_str();
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
    let mut workbook = format!("{XML_HEADER}<workbook xmlns=\"{MAIN_NS}\" xmlns:r=\"{OD_RELS}\">\n<sheets>\n");
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
        sheets.push(sheet_xml(layer, options, &mut strings)?);
    }
    content_types.push_str("</Types>\n");
    workbook.push_str("</sheets>\n</workbook>\n");
    rels.push_str("</Relationships>\n");
    let package_rels = format!(
        "{XML_HEADER}<Relationships xmlns=\"{PACKAGE}/relationships\">\n\
         <Relationship Id=\"rId1\" Type=\"{OD_RELS}/officeDocument\" Target=\"xl/workbook.xml\"/>\n\
         <Relationship Id=\"rId2\" Type=\"{PACKAGE}/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\n\
         <Relationship Id=\"rId3\" Type=\"{OD_RELS}/extended-properties\" Target=\"docProps/app.xml\"/>\n\
         </Relationships>\n"
    );
    let core = format!(
        "{XML_HEADER}<cp:coreProperties xmlns:cp=\"{PACKAGE}/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:dcterms=\"http://purl.org/dc/terms/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n</cp:coreProperties>\n"
    );
    let app = format!(
        "{XML_HEADER}<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\">\n<Application>terrio</Application>\n</Properties>\n"
    );
    let styles = format!("{XML_HEADER}<styleSheet xmlns=\"{MAIN_NS}\">\n{STYLES}</styleSheet>\n");
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
            read_sheet(data, "s", &shared, &mut table, &mut MAX_CELLS.clone())
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
        assert_eq!(reference_column("ZZZ1"), Some(MAX_REFERENCED_COLUMN));
        assert_eq!(reference_column("7"), None);
    }
}
