//! The options of the XLSX driver, whatever a build includes.

use crate::crs::SpatialRef;
use crate::{CsvGeometry, Error, Result};

/// Whether a sheet's first row names its fields (GDAL's `HEADERS` open option).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum XlsxHeaders {
    /// When the first row is all text and the second is not (`AUTO`).
    #[default]
    Auto,
    /// Always (`FORCE`).
    Force,
    /// Never (`DISABLE`): fields are named `Field1`, `Field2`...
    Disable,
}

/// How field types are chosen (GDAL's `FIELD_TYPES` open option).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum XlsxFieldTypes {
    /// From the cells' types and values (`AUTO`).
    #[default]
    Auto,
    /// Every field is text (`STRING`).
    String,
}

/// How wide the columns of a written sheet are.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum XlsxColumnWidth {
    /// GDAL's widths: 15, and 29 for date-times.
    #[default]
    Default,
    /// Wide enough for each column's longest value (`AUTO`).
    Auto,
    /// One width, in characters.
    Fixed(f64),
}

/// Options for reading and writing XLSX workbooks.
#[derive(Debug, Clone)]
pub struct XlsxOptions {
    /// Whether a sheet's first row names its fields.
    pub headers: XlsxHeaders,
    /// How field types are chosen.
    pub field_types: XlsxFieldTypes,
    /// Column names that hold X coordinates, case-insensitive; a trailing `*` matches any suffix.
    /// With `y_possible_names`, makes point geometries. Empty means no geometries.
    pub x_possible_names: Vec<String>,
    /// Column names that hold Y coordinates.
    pub y_possible_names: Vec<String>,
    /// Column names that hold Z coordinates.
    pub z_possible_names: Vec<String>,
    /// Column names that hold WKT geometries, used when no X/Y columns are found.
    pub geom_possible_names: Vec<String>,
    /// Keep the columns geometries were read from as attribute fields.
    pub keep_geom_columns: bool,
    /// The CRS of the coordinates. By default, EPSG:4326 when every coordinate is a valid
    /// longitude/latitude, and unknown otherwise.
    pub crs: Option<SpatialRef>,
    /// The number of rows that name the fields (`HEADER_ROWS`), whatever `headers` says. Above one,
    /// a merged heading names each column under it, and a column's headings are joined with ` / `.
    pub header_rows: Option<usize>,
    /// The sheet's first row to read, from 1 (`FIRST_ROW`, or the start of `RANGE`).
    pub first_row: usize,
    /// The last row to read, from 1, when not the sheet's last.
    pub last_row: Option<usize>,
    /// The first column to read, from 0 (the start of `RANGE`).
    pub first_column: usize,
    /// The last column to read, from 0, when not the row's last.
    pub last_column: Option<usize>,
    /// Write the header row in bold (`HEADER_STYLE=BOLD`).
    pub header_bold: bool,
    /// Keep the header row in view when scrolling (`FREEZE_HEADER`).
    pub freeze_header: bool,
    /// Add filter buttons to the header row (`AUTOFILTER`).
    pub autofilter: bool,
    /// How wide the columns are (`COLUMN_WIDTH`).
    pub column_width: XlsxColumnWidth,
    /// The number format of date cells (`DATE_FORMAT`), such as `yyyy-mm-dd`.
    pub date_format: Option<String>,
    /// The number format of date-time cells (`DATETIME_FORMAT`), such as `yyyy-mm-dd hh:mm:ss`.
    pub datetime_format: Option<String>,
    /// The creation time to record in the workbook's properties (`CREATION_TIME`), as ISO 8601.
    pub creation_time: Option<String>,
    /// How to write geometries. By default they are left out, as GDAL's driver does.
    pub geometry: CsvGeometry,
    /// Declare the workbook macro-enabled, as Excel requires of a `.xlsm` file. Writing to a path or
    /// name ending in `.xlsm` does this by itself.
    pub macro_enabled: bool,
}

impl Default for XlsxOptions {
    fn default() -> Self {
        Self {
            headers: XlsxHeaders::Auto,
            field_types: XlsxFieldTypes::Auto,
            x_possible_names: Vec::new(),
            y_possible_names: Vec::new(),
            z_possible_names: Vec::new(),
            geom_possible_names: Vec::new(),
            keep_geom_columns: true,
            crs: None,
            header_rows: None,
            first_row: 1,
            last_row: None,
            first_column: 0,
            last_column: None,
            header_bold: false,
            freeze_header: false,
            autofilter: false,
            column_width: XlsxColumnWidth::Default,
            date_format: None,
            datetime_format: None,
            creation_time: None,
            geometry: CsvGeometry::None,
            macro_enabled: false,
        }
    }
}

impl XlsxOptions {
    /// These options for a file of this name: a `.xlsm` workbook declares itself macro-enabled.
    pub(crate) fn for_name(&self, name: &str) -> Self {
        Self { macro_enabled: self.macro_enabled || crate::formats::extension(name).as_deref() == Some("xlsm"), ..self.clone() }
    }

    /// Sets an open option by name: `HEADERS` (AUTO, FORCE, DISABLE), `FIELD_TYPES` (AUTO, STRING),
    /// `X_POSSIBLE_NAMES`, `Y_POSSIBLE_NAMES`, `Z_POSSIBLE_NAMES`, `GEOM_POSSIBLE_NAMES`
    /// (comma-separated), `KEEP_GEOM_COLUMNS` (YES/NO), `HEADER_ROWS` (a count), `FIRST_ROW` (from 1)
    /// and `RANGE` (`A5:K40`). Returns `false` for other names, the
    /// creation options of [`set_creation`](Self::set_creation) among them.
    ///
    /// ```
    /// let mut options = terrio::XlsxOptions::default();
    /// assert!(options.set("HEADERS", "FORCE")?);
    /// assert!(!options.set("GEOMETRY", "AS_XY")?);
    /// assert!(!options.set("NO_SUCH_OPTION", "1")?);
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        let list = || value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let upper = value.to_ascii_uppercase();
        match key.to_ascii_uppercase().as_str() {
            "HEADERS" => {
                self.headers = match upper.as_str() {
                    "AUTO" => XlsxHeaders::Auto,
                    "FORCE" => XlsxHeaders::Force,
                    "DISABLE" => XlsxHeaders::Disable,
                    _ => return Err(Error::InvalidArgument(format!("unknown XLSX HEADERS '{value}' (AUTO, FORCE, DISABLE)"))),
                }
            }
            "FIELD_TYPES" => {
                self.field_types = match upper.as_str() {
                    "AUTO" => XlsxFieldTypes::Auto,
                    "STRING" => XlsxFieldTypes::String,
                    _ => return Err(Error::InvalidArgument(format!("unknown XLSX FIELD_TYPES '{value}' (AUTO, STRING)"))),
                }
            }
            "X_POSSIBLE_NAMES" => self.x_possible_names = list(),
            "Y_POSSIBLE_NAMES" => self.y_possible_names = list(),
            "Z_POSSIBLE_NAMES" => self.z_possible_names = list(),
            "GEOM_POSSIBLE_NAMES" => self.geom_possible_names = list(),
            "KEEP_GEOM_COLUMNS" => self.keep_geom_columns = matches!(upper.as_str(), "YES" | "TRUE" | "ON" | "1"),
            "HEADER_ROWS" => self.header_rows = Some(parse_count("HEADER_ROWS", value, 0)?),
            "FIRST_ROW" => self.first_row = parse_count("FIRST_ROW", value, 1)?,
            "RANGE" => {
                let (first_column, first_row, last_column, last_row) = parse_range(value)?;
                (self.first_column, self.first_row) = (first_column, first_row);
                (self.last_column, self.last_row) = (Some(last_column), Some(last_row));
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Sets a layer creation option by name: `GEOMETRY` (AS_XY, AS_WKT, NONE), `HEADER_STYLE` (BOLD,
    /// NONE), `FREEZE_HEADER` and `AUTOFILTER` (YES/NO), `COLUMN_WIDTH` (AUTO or a width),
    /// `DATE_FORMAT`, `DATETIME_FORMAT` and `CREATION_TIME` (ISO 8601). Returns `false` for
    /// other names, the open options of [`set`](Self::set) among them.
    ///
    /// ```
    /// let mut options = terrio::XlsxOptions::default();
    /// assert!(options.set_creation("GEOMETRY", "AS_XY")?);
    /// assert!(!options.set_creation("HEADERS", "FORCE")?);
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn set_creation(&mut self, key: &str, value: &str) -> Result<bool> {
        let upper = value.to_ascii_uppercase();
        match key.to_ascii_uppercase().as_str() {
            "GEOMETRY" => {
                self.geometry = match upper.as_str() {
                    "AS_WKT" => CsvGeometry::AsWkt,
                    "AS_XY" => CsvGeometry::AsXy,
                    "NONE" => CsvGeometry::None,
                    _ => return Err(Error::InvalidArgument(format!("unknown XLSX GEOMETRY '{value}' (AS_WKT, AS_XY, NONE)"))),
                }
            }
            "HEADER_STYLE" => {
                self.header_bold = match upper.as_str() {
                    "BOLD" => true,
                    "NONE" => false,
                    _ => return Err(Error::InvalidArgument(format!("unknown XLSX HEADER_STYLE '{value}' (BOLD, NONE)"))),
                }
            }
            "FREEZE_HEADER" => self.freeze_header = crate::tiles::parse_bool(value)?,
            "AUTOFILTER" => self.autofilter = crate::tiles::parse_bool(value)?,
            "COLUMN_WIDTH" => {
                self.column_width = match value.trim().parse::<f64>() {
                    Ok(width) if width > 0.0 && width <= 255.0 => XlsxColumnWidth::Fixed(width),
                    _ if upper == "AUTO" => XlsxColumnWidth::Auto,
                    _ => return Err(Error::InvalidArgument(format!("unknown XLSX COLUMN_WIDTH '{value}' (AUTO, or a width up to 255)"))),
                }
            }
            "DATE_FORMAT" => self.date_format = Some(value.to_string()),
            "DATETIME_FORMAT" => self.datetime_format = Some(value.to_string()),
            "CREATION_TIME" => {
                if crate::drivers::vector::datetime::parse_datetime(value).is_none() {
                    return Err(Error::InvalidArgument(format!("XLSX CREATION_TIME '{value}' is not an ISO 8601 date-time")));
                }
                self.creation_time = Some(value.to_string());
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// A whole number of at least `min`.
fn parse_count(key: &str, value: &str, min: usize) -> Result<usize> {
    value
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|&n| n >= min)
        .ok_or_else(|| Error::InvalidArgument(format!("XLSX {key} '{value}' is not a whole number of at least {min}")))
}

/// A cell reference's column from 0, and its row from 1.
fn parse_cell(cell: &str) -> Option<(usize, usize)> {
    let digits = cell.find(|c: char| c.is_ascii_digit())?;
    let (letters, row) = cell.split_at(digits);
    if letters.is_empty() || !letters.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let column = letters.bytes().try_fold(0usize, |c, b| c.checked_mul(26)?.checked_add((b.to_ascii_uppercase() - b'A') as usize + 1))?;
    Some((column - 1, row.parse::<usize>().ok().filter(|&r| r > 0)?))
}

/// A range such as `A5:K40`: its first column and row, and its last.
fn parse_range(value: &str) -> Result<(usize, usize, usize, usize)> {
    let invalid = || Error::InvalidArgument(format!("XLSX RANGE '{value}' is not a range such as A5:K40"));
    let (start, end) = value.trim().split_once(':').ok_or_else(invalid)?;
    let (c0, r0) = parse_cell(start).ok_or_else(invalid)?;
    let (c1, r1) = parse_cell(end).ok_or_else(invalid)?;
    if c1 < c0 || r1 < r0 {
        return Err(invalid());
    }
    Ok((c0, r0, c1, r1))
}
