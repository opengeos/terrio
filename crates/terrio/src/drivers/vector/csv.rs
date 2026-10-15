//! Comma-separated values with point geometries in X/Y columns or any geometry in a WKT column.
//!
//! Reading behaves like GDAL's CSV driver opened with `AUTODETECT_TYPE=YES` and the
//! `X_POSSIBLE_NAMES` / `Y_POSSIBLE_NAMES` / `GEOM_POSSIBLE_NAMES` open options set to common
//! names. Writing follows GDAL's `GEOMETRY=AS_WKT` (the default) or `GEOMETRY=AS_XY`.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// How [`CsvOptions`] writes geometries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CsvGeometry {
    /// A first `WKT` column with each geometry as well-known text (GDAL's `GEOMETRY=AS_WKT`).
    #[default]
    AsWkt,
    /// `X`, `Y` (and `Z`) first columns, for point layers (GDAL's `GEOMETRY=AS_XY`).
    AsXy,
    /// No geometry columns.
    None,
}

/// Options for reading and writing CSV.
#[derive(Debug, Clone)]
pub struct CsvOptions {
    /// Column names that may hold X (longitude) coordinates, case-insensitive; a trailing `*`
    /// matches any suffix. Empty means the defaults: `x`, `lon`, `lng`, `long`, `longitude`,
    /// `easting`, `xcoord`, `x_coord`, `point_x`.
    pub x_possible_names: Vec<String>,
    /// Column names that may hold Y (latitude) coordinates. Empty means the defaults: `y`, `lat`,
    /// `latitude`, `northing`, `ycoord`, `y_coord`, `point_y`.
    pub y_possible_names: Vec<String>,
    /// Column names that may hold Z coordinates. Empty means the default, `z`.
    pub z_possible_names: Vec<String>,
    /// Column names that may hold WKT geometries. Empty means the defaults: `wkt`, `geometry`,
    /// `geom`, `the_geom`.
    pub geom_possible_names: Vec<String>,
    /// Infer integer, real, date and date-time fields from the values (otherwise every field is a
    /// string, as GDAL does by default).
    pub autodetect_type: bool,
    /// Keep the columns geometries were read from as attribute fields (GDAL's default).
    pub keep_geom_columns: bool,
    /// The field separator. By default, detected from the header line (`,`, `;`, tab or `|`).
    pub separator: Option<u8>,
    /// The CRS of the coordinates. By default, EPSG:4326 when every coordinate is a valid
    /// longitude/latitude, and unknown otherwise.
    pub crs: Option<SpatialRef>,
    /// How to write geometries.
    pub geometry: CsvGeometry,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            x_possible_names: Vec::new(),
            y_possible_names: Vec::new(),
            z_possible_names: Vec::new(),
            geom_possible_names: Vec::new(),
            autodetect_type: true,
            keep_geom_columns: true,
            separator: None,
            crs: None,
            geometry: CsvGeometry::AsWkt,
        }
    }
}

impl CsvOptions {
    /// Sets an option by its GDAL name: the open options `X_POSSIBLE_NAMES`, `Y_POSSIBLE_NAMES`,
    /// `Z_POSSIBLE_NAMES`, `GEOM_POSSIBLE_NAMES` (comma-separated), `AUTODETECT_TYPE`,
    /// `KEEP_GEOM_COLUMNS` (YES/NO) and `SEPARATOR` (COMMA, SEMICOLON, TAB, PIPE or a character),
    /// and the creation option `GEOMETRY` (AS_WKT, AS_XY, NONE). Returns `false` for other names.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        let list = || value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let yes = || matches!(value.to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1");
        match key.to_ascii_uppercase().as_str() {
            "X_POSSIBLE_NAMES" => self.x_possible_names = list(),
            "Y_POSSIBLE_NAMES" => self.y_possible_names = list(),
            "Z_POSSIBLE_NAMES" => self.z_possible_names = list(),
            "GEOM_POSSIBLE_NAMES" => self.geom_possible_names = list(),
            "AUTODETECT_TYPE" => self.autodetect_type = yes(),
            "KEEP_GEOM_COLUMNS" => self.keep_geom_columns = yes(),
            "SEPARATOR" => {
                self.separator = Some(match value.to_ascii_uppercase().as_str() {
                    "COMMA" => b',',
                    "SEMICOLON" => b';',
                    "TAB" => b'\t',
                    "PIPE" => b'|',
                    "SPACE" => b' ',
                    v if v.len() == 1 => v.as_bytes()[0],
                    _ => return Err(Error::InvalidArgument(format!("unknown CSV separator '{value}'"))),
                })
            }
            "GEOMETRY" => {
                self.geometry = match value.to_ascii_uppercase().as_str() {
                    "AS_WKT" => CsvGeometry::AsWkt,
                    "AS_XY" => CsvGeometry::AsXy,
                    "NONE" => CsvGeometry::None,
                    _ => return Err(Error::InvalidArgument(format!("unknown CSV GEOMETRY '{value}' (AS_WKT, AS_XY, NONE)"))),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

const X_NAMES: &[&str] = &["x", "lon", "lng", "long", "longitude", "easting", "xcoord", "x_coord", "point_x"];
const Y_NAMES: &[&str] = &["y", "lat", "latitude", "northing", "ycoord", "y_coord", "point_y"];
const Z_NAMES: &[&str] = &["z"];
const GEOM_NAMES: &[&str] = &["wkt", "geometry", "geom", "the_geom"];

fn matches_name(column: &str, patterns: &[String], defaults: &[&str]) -> bool {
    let column = column.trim().to_ascii_lowercase();
    let check = |p: &str| {
        let p = p.to_ascii_lowercase();
        match p.strip_suffix('*') {
            Some(prefix) => column.starts_with(prefix),
            None => column == p,
        }
    };
    if patterns.is_empty() { defaults.iter().any(|p| check(p)) } else { patterns.iter().any(|p| check(p)) }
}

/// Reads a CSV file.
pub fn read(path: &str, options: &CsvOptions) -> Result<Layer> {
    // UTF-8, or Latin-1 for files that are not UTF-8.
    let text = super::xml::decode(&std::fs::read(path)?);
    let name = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    let tsv = crate::formats::extension(path).as_deref() == Some("tsv");
    let mut layer = parse(&text, name, &CsvOptions { separator: options.separator.or(tsv.then_some(b'\t')), ..options.clone() })?;
    // As GDAL's CSV driver, the CRS of a `.prj` file next to it (which GDAL writes with
    // CREATE_CSVT=YES), for a layer with geometries.
    if options.crs.is_none() && layer.features.iter().any(|f| f.geometry.is_some()) {
        let prj = std::path::Path::new(path).with_extension("prj");
        if let Some(crs) = std::fs::read(&prj).ok().and_then(|b| SpatialRef::parse(String::from_utf8_lossy(&b).trim()).ok()) {
            crs.apply_to_layer(&mut layer);
        }
    }
    Ok(layer)
}

/// Parses CSV text into a layer.
pub fn parse(text: &str, name: &str, options: &CsvOptions) -> Result<Layer> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let sep = options.separator.unwrap_or_else(|| detect_separator(text.lines().next().unwrap_or_default()));
    let mut rows = Records { s: text.as_bytes(), i: 0, sep };
    let header = rows.next().ok_or_else(|| Error::InvalidData("CSV: empty file".into()))?;
    let records: Vec<Vec<String>> = rows.filter(|r| !(r.len() == 1 && r[0].is_empty())).collect();

    let find = |patterns: &[String], defaults: &[&str]| header.iter().position(|h| matches_name(h, patterns, defaults));
    let mut x_col = find(&options.x_possible_names, X_NAMES);
    let mut y_col = find(&options.y_possible_names, Y_NAMES);
    if x_col.is_none() || y_col.is_none() {
        (x_col, y_col) = (None, None);
    }
    let z_col = x_col.and_then(|_| find(&options.z_possible_names, Z_NAMES));
    let wkt_col = if x_col.is_some() {
        None
    } else {
        // A WKT column must hold geometries: a "geometry" column of other values is an attribute.
        header.iter().enumerate().position(|(i, h)| {
            matches_name(h, &options.geom_possible_names, GEOM_NAMES)
                && records.iter().filter_map(|r| r.get(i)).find(|v| !v.trim().is_empty()).is_some_and(|v| super::wkt::parse(v).is_some())
        })
    };
    let geom_cols: Vec<usize> = [x_col, y_col, z_col, wkt_col].into_iter().flatten().collect();

    let mut layer = Layer::new(name);
    let mut fields = Vec::new();
    for (i, h) in header.iter().enumerate() {
        if !options.keep_geom_columns && geom_cols.contains(&i) {
            continue;
        }
        let t = if !options.autodetect_type || Some(i) == wkt_col {
            FieldType::Text
        } else {
            infer_type(records.iter().filter_map(|r| r.get(i)).map(String::as_str))
        };
        let h = if h.is_empty() { format!("field_{}", i + 1) } else { h.clone() };
        layer.add_field(FieldDef::new(h, t));
        fields.push((i, t));
    }

    let mut lonlat = true;
    for (fid, r) in records.iter().enumerate() {
        let get = |i: usize| r.get(i).map(|s| s.trim()).filter(|s| !s.is_empty());
        let geometry = match (x_col, y_col, wkt_col) {
            (Some(xc), Some(yc), _) => match (get(xc).and_then(parse_number), get(yc).and_then(parse_number)) {
                (Some(x), Some(y)) => {
                    let z = z_col.and_then(get).and_then(parse_number);
                    Some(Geometry::Point(Coord { x, y, z, m: None }))
                }
                _ => None,
            },
            (_, _, Some(wc)) => get(wc).and_then(super::wkt::parse),
            _ => None,
        };
        if let Some(b) = geometry.as_ref().and_then(|g| g.bbox()) {
            lonlat &= b.min_x >= -180.0 && b.max_x <= 180.0 && b.min_y >= -90.0 && b.max_y <= 90.0;
        }
        let attributes = fields.iter().map(|&(i, t)| get(i).map_or(FieldValue::Null, |v| value(v, t))).collect();
        layer.push(Feature { fid: fid as u64, geometry, attributes });
    }
    if x_col.is_some() {
        layer.geom_type = Some(GeometryType::Point);
    }
    let has_geometry = layer.features.iter().any(|f| f.geometry.is_some());
    match &options.crs {
        Some(crs) => crs.apply_to_layer(&mut layer),
        None if has_geometry && lonlat => layer.set_crs_epsg(Some(4326)),
        None => {}
    }
    Ok(layer)
}

fn detect_separator(line: &str) -> u8 {
    let mut counts = [(b',', 0), (b';', 0), (b'\t', 0), (b'|', 0)];
    let mut quoted = false;
    for b in line.bytes() {
        if b == b'"' {
            quoted = !quoted;
        } else if !quoted && let Some(c) = counts.iter_mut().find(|c| c.0 == b) {
            c.1 += 1;
        }
    }
    counts.iter().max_by_key(|c| c.1).filter(|c| c.1 > 0).map_or(b',', |c| c.0)
}

/// An iterator over CSV records (RFC 4180: quoted fields may hold separators, quotes and newlines).
struct Records<'a> {
    s: &'a [u8],
    i: usize,
    sep: u8,
}

impl Iterator for Records<'_> {
    type Item = Vec<String>;

    fn next(&mut self) -> Option<Vec<String>> {
        if self.i >= self.s.len() {
            return None;
        }
        let mut fields = Vec::new();
        let mut field = Vec::new();
        let mut quoted = false;
        while self.i < self.s.len() {
            let b = self.s[self.i];
            self.i += 1;
            if quoted {
                if b == b'"' {
                    if self.s.get(self.i) == Some(&b'"') {
                        field.push(b'"');
                        self.i += 1;
                    } else {
                        quoted = false;
                    }
                } else {
                    field.push(b);
                }
            } else if b == b'"' && field.iter().all(u8::is_ascii_whitespace) {
                field.clear();
                quoted = true;
            } else if b == self.sep {
                fields.push(String::from_utf8_lossy(&field).into_owned());
                field.clear();
            } else if b == b'\n' || b == b'\r' {
                if b == b'\r' && self.s.get(self.i) == Some(&b'\n') {
                    self.i += 1;
                }
                break;
            } else {
                field.push(b);
            }
        }
        fields.push(String::from_utf8_lossy(&field).into_owned());
        Some(fields)
    }
}

fn parse_number(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn is_integer(s: &str) -> bool {
    let d = s.strip_prefix(['-', '+']).unwrap_or(s);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) && s.parse::<i64>().is_ok()
}

fn is_real(s: &str) -> bool {
    // Plain decimal notation only, as GDAL detects it: no "nan", "inf" or hex.
    !s.is_empty() && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E')) && s.parse::<f64>().is_ok()
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn is_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() >= 19
        && is_date(&s[..10])
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':'
        && [11, 12, 14, 15, 17, 18].iter().all(|&i| b[i].is_ascii_digit())
}

pub(crate) fn infer_type<'a>(values: impl Iterator<Item = &'a str>) -> FieldType {
    let mut t: Option<FieldType> = None;
    for v in values.map(str::trim).filter(|v| !v.is_empty()) {
        let vt = if is_integer(v) {
            FieldType::Integer
        } else if is_real(v) {
            FieldType::Float
        } else if is_datetime(v) {
            FieldType::DateTime
        } else if is_date(v) {
            FieldType::Date
        } else {
            return FieldType::Text;
        };
        t = Some(match (t, vt) {
            (None, vt) => vt,
            (Some(a), b) if a == b => a,
            (Some(FieldType::Integer), FieldType::Float) | (Some(FieldType::Float), FieldType::Integer) => FieldType::Float,
            (Some(FieldType::Date), FieldType::DateTime) | (Some(FieldType::DateTime), FieldType::Date) => FieldType::DateTime,
            _ => return FieldType::Text,
        });
    }
    t.unwrap_or(FieldType::Text)
}

fn value(v: &str, t: FieldType) -> FieldValue {
    match t {
        FieldType::Integer => v.parse().map_or_else(|_| FieldValue::Text(v.into()), FieldValue::Integer),
        FieldType::Float => v.parse().map_or_else(|_| FieldValue::Text(v.into()), FieldValue::Float),
        FieldType::Date => FieldValue::Date(v.into()),
        FieldType::DateTime => FieldValue::DateTime(v.into()),
        _ => FieldValue::Text(v.into()),
    }
}

/// Writes a layer as CSV.
pub fn write(layer: &Layer, path: &str, options: &CsvOptions) -> Result<()> {
    std::fs::write(path, to_string(layer, options)?)?;
    Ok(())
}

/// Encodes a layer as CSV text.
pub fn to_string(layer: &Layer, options: &CsvOptions) -> Result<String> {
    let sep = options.separator.unwrap_or(b',') as char;
    let has_geometry = layer.features.iter().any(|f| f.geometry.is_some());
    let mode = if has_geometry { options.geometry } else { CsvGeometry::None };
    let has_z = mode == CsvGeometry::AsXy && layer.features.iter().filter_map(|f| f.geometry.as_ref()).any(crate::vector::geometry_has_z);
    if mode == CsvGeometry::AsXy && layer.features.iter().any(|f| f.geometry.as_ref().is_some_and(|g| !matches!(g, Geometry::Point(_)))) {
        return Err(Error::InvalidArgument("CSV: writing geometries as X/Y columns needs a point layer".into()));
    }
    let mut out = String::new();
    let mut header: Vec<String> = match mode {
        CsvGeometry::AsWkt => vec!["WKT".into()],
        CsvGeometry::AsXy if has_z => vec!["X".into(), "Y".into(), "Z".into()],
        CsvGeometry::AsXy => vec!["X".into(), "Y".into()],
        CsvGeometry::None => vec![],
    };
    header.extend(layer.schema.fields().iter().map(|f| f.name.clone()));
    push_row(&mut out, header.iter().map(String::as_str), sep);
    let mut cells: Vec<String> = Vec::new();
    for f in &layer.features {
        cells.clear();
        match (mode, &f.geometry) {
            (CsvGeometry::AsWkt, g) => cells.push(g.as_ref().map(super::wkt::format).unwrap_or_default()),
            (CsvGeometry::AsXy, Some(Geometry::Point(c))) => {
                cells.push(fmt_f64(c.x));
                cells.push(fmt_f64(c.y));
                if has_z {
                    cells.push(c.z.map(fmt_f64).unwrap_or_default());
                }
            }
            (CsvGeometry::AsXy, _) => cells.extend(std::iter::repeat_n(String::new(), if has_z { 3 } else { 2 })),
            (CsvGeometry::None, _) => {}
        }
        for i in 0..layer.schema.len() {
            cells.push(match f.attributes.get(i).unwrap_or(&FieldValue::Null) {
                FieldValue::Null => String::new(),
                FieldValue::Integer(v) => v.to_string(),
                FieldValue::Float(v) => fmt_f64(*v),
                FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
                FieldValue::Boolean(b) => (if *b { "1" } else { "0" }).into(),
                FieldValue::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
            });
        }
        push_row(&mut out, cells.iter().map(String::as_str), sep);
    }
    Ok(out)
}

fn fmt_f64(v: f64) -> String {
    format!("{v}")
}

fn push_row<'a>(out: &mut String, cells: impl Iterator<Item = &'a str>, sep: char) {
    for (i, c) in cells.enumerate() {
        if i > 0 {
            out.push(sep);
        }
        if c.contains([sep, '"', '\n', '\r']) || c.starts_with(' ') || c.ends_with(' ') {
            out.push('"');
            out.push_str(&c.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(c);
        }
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_points_from_lon_lat_columns() {
        let text =
            "id,Longitude,Latitude,name,when\n1,-122.5,45.25,\"A, quoted\",2024-01-02\n2,10,-20.5,\"say \"\"hi\"\"\",2024-01-03T10:00:00\n";
        let l = parse(text, "pts", &CsvOptions::default()).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l.crs_epsg(), Some(4326));
        let types: Vec<FieldType> = l.schema.fields().iter().map(|f| f.field_type).collect();
        assert_eq!(types, [FieldType::Integer, FieldType::Float, FieldType::Float, FieldType::Text, FieldType::DateTime]);
        assert_eq!(l.features[0].geometry, Some(Geometry::point(-122.5, 45.25)));
        assert_eq!(l.features[0].attributes[3], FieldValue::Text("A, quoted".into()));
        assert_eq!(l.features[1].attributes[3], FieldValue::Text("say \"hi\"".into()));
    }

    #[test]
    fn reads_wkt_and_projected_coordinates() {
        let text = "WKT;value\n\"POLYGON ((500000 4000000,500100 4000000,500100 4000100,500000 4000000))\";1.5\n;2\n";
        let l = parse(text, "polys", &CsvOptions::default()).unwrap();
        assert_eq!(l.crs_epsg(), None);
        assert!(matches!(l.features[0].geometry, Some(Geometry::Polygon { .. })));
        assert_eq!(l.features[1].geometry, None);
        assert_eq!(l.schema.fields()[1].field_type, FieldType::Float);
        // A "geometry" column of other values stays an attribute.
        let l = parse("geometry,x2\nround,1\n", "t", &CsvOptions::default()).unwrap();
        assert!(l.features[0].geometry.is_none());
    }

    #[test]
    fn writes_wkt_and_xy() {
        let l = parse("lon,lat,name\n1.5,2,a b\n3,4,\"c,d\"\n", "pts", &CsvOptions::default()).unwrap();
        let wkt = to_string(&l, &CsvOptions::default()).unwrap();
        assert_eq!(wkt, "WKT,lon,lat,name\nPOINT (1.5 2),1.5,2,a b\nPOINT (3 4),3,4,\"c,d\"\n");
        let xy = to_string(&l, &CsvOptions { geometry: CsvGeometry::AsXy, ..Default::default() }).unwrap();
        assert_eq!(xy, "X,Y,lon,lat,name\n1.5,2,1.5,2,a b\n3,4,3,4,\"c,d\"\n");
        let opts = CsvOptions { keep_geom_columns: false, ..Default::default() };
        let l = parse("WKT,name\nPOINT (1.5 2),a b\n", "pts", &opts).unwrap();
        assert_eq!(to_string(&l, &opts).unwrap(), "WKT,name\nPOINT (1.5 2),a b\n");
    }

    #[test]
    fn reads_the_crs_of_a_prj_file() {
        // GDAL's CSV driver reads a .prj next to the file (CREATE_CSVT=YES writes one).
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("utm.csv");
        std::fs::write(&csv, "WKT,id\nPOINT (500000 4000000),1\n").unwrap();
        let path = csv.to_str().unwrap();
        assert_eq!(read(path, &CsvOptions::default()).unwrap().crs_epsg(), None);
        std::fs::write(dir.path().join("utm.prj"), SpatialRef::from_epsg(32633).unwrap().to_wkt().unwrap()).unwrap();
        assert_eq!(read(path, &CsvOptions::default()).unwrap().crs_epsg(), Some(32633));
        // Without geometries, no CRS.
        std::fs::write(&csv, "id\n1\n").unwrap();
        assert_eq!(read(path, &CsvOptions::default()).unwrap().crs_epsg(), None);
    }
}
