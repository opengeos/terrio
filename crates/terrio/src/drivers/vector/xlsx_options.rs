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
    /// (comma-separated) and `KEEP_GEOM_COLUMNS` (YES/NO). Returns `false` for other names, the
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
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Sets a layer creation option by name: `GEOMETRY` (AS_XY, AS_WKT, NONE). Returns `false` for
    /// other names, the open options of [`set`](Self::set) among them.
    ///
    /// ```
    /// let mut options = terrio::XlsxOptions::default();
    /// assert!(options.set_creation("GEOMETRY", "AS_XY")?);
    /// assert!(!options.set_creation("HEADERS", "FORCE")?);
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn set_creation(&mut self, key: &str, value: &str) -> Result<bool> {
        if !key.eq_ignore_ascii_case("GEOMETRY") {
            return Ok(false);
        }
        self.geometry = match value.to_ascii_uppercase().as_str() {
            "AS_WKT" => CsvGeometry::AsWkt,
            "AS_XY" => CsvGeometry::AsXy,
            "NONE" => CsvGeometry::None,
            _ => return Err(Error::InvalidArgument(format!("unknown XLSX GEOMETRY '{value}' (AS_WKT, AS_XY, NONE)"))),
        };
        Ok(true)
    }
}
