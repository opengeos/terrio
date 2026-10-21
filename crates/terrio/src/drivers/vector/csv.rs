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
    let mut stream = CsvStream::open(path, options)?;
    let mut layer = stream.layer.clone();
    while let Some(rows) = stream.next_rows(65536)? {
        layer.features.extend(rows);
    }
    Ok(layer)
}

/// The CSV options for a file: a `.tsv` file is tab-separated unless a separator is given.
fn file_options(path: &str, options: &CsvOptions) -> CsvOptions {
    let tsv = crate::formats::extension(path).as_deref() == Some("tsv");
    CsvOptions { separator: options.separator.or(tsv.then_some(b'\t')), ..options.clone() }
}

/// Parses CSV text into a layer.
pub fn parse(text: &str, name: &str, options: &CsvOptions) -> Result<Layer> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let sep = options.separator.unwrap_or_else(|| detect_separator(text.lines().next().unwrap_or_default()));
    let mut scan = CsvScan::default();
    let mut rows = Records::new(text.as_bytes(), sep);
    let header = rows.next_record()?.ok_or_else(|| Error::InvalidData("CSV: empty file".into()))?;
    let header: Vec<String> = header.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect();
    scan.start(&header, options);
    while let Some(r) = rows.next_record()? {
        scan.add(&r);
    }
    let decoder = scan.finish(header, name, options, true);
    let mut layer = decoder.layer.clone();
    let mut rows = Records::new(text.as_bytes(), sep);
    rows.next_record()?;
    let mut fid = 0;
    while let Some(r) = rows.next_record()? {
        if let Some(f) = decoder.feature(fid, &r) {
            layer.push(f);
            fid += 1;
        }
    }
    Ok(layer)
}

/// The rows of a CSV file, read as a stream: a first pass infers the field types, geometry
/// columns and CRS (as [`parse`] does over the whole text), the second decodes the rows.
pub(crate) struct CsvStream {
    /// The layer, without its features.
    pub layer: Layer,
    decoder: RowDecoder,
    rows: Records<std::io::BufReader<std::fs::File>>,
    fid: u64,
}

impl CsvStream {
    /// Opens a CSV file, scanning it once.
    pub(crate) fn open(path: &str, options: &CsvOptions) -> Result<Self> {
        let options = file_options(path, options);
        let name = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
        let open = || -> Result<std::io::BufReader<std::fs::File>> {
            let mut r = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
            // A UTF-8 byte order mark.
            if std::io::BufRead::fill_buf(&mut r)?.starts_with(b"\xEF\xBB\xBF") {
                std::io::BufRead::consume(&mut r, 3);
            }
            Ok(r)
        };
        let sep = match options.separator {
            Some(s) => s,
            None => {
                let mut line = Vec::new();
                std::io::BufRead::read_until(&mut open()?, b'\n', &mut line)?;
                detect_separator(&super::xml::decode(&line))
            }
        };
        let mut rows = Records::new(open()?, sep);
        let header = rows.next_record()?.ok_or_else(|| Error::InvalidData("CSV: empty file".into()))?;
        let mut scan = CsvScan::default();
        let mut utf8 = std::str::from_utf8(&header.concat()).is_ok();
        let header_bytes = header;
        // Decoded once the encoding is known.
        scan.start(&header_bytes.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect::<Vec<_>>(), &options);
        while let Some(r) = rows.next_record()? {
            utf8 &= r.iter().all(|f| std::str::from_utf8(f).is_ok());
            scan.add(&r);
        }
        let header: Vec<String> = header_bytes.iter().map(|f| decode_field(f, utf8)).collect();
        let decoder = scan.finish(header, name, &options, utf8);
        let mut layer = decoder.layer.clone();
        // As GDAL's CSV driver, the CRS of a `.prj` file next to it (which GDAL writes with
        // CREATE_CSVT=YES), for a layer with geometries.
        if options.crs.is_none() && decoder.has_geometry {
            let prj = std::path::Path::new(path).with_extension("prj");
            if let Some(crs) = std::fs::read(&prj).ok().and_then(|b| SpatialRef::parse(String::from_utf8_lossy(&b).trim()).ok()) {
                crs.apply_to_layer(&mut layer);
            }
        }
        let mut rows = Records::new(open()?, sep);
        rows.next_record()?;
        Ok(Self { layer, decoder, rows, fid: 0 })
    }

    /// The features of the next `max` rows at most; `None` at the end.
    pub(crate) fn next_rows(&mut self, max: usize) -> Result<Option<Vec<Feature>>> {
        let mut records = Vec::with_capacity(max.min(65536));
        while records.len() < max.max(1) {
            match self.rows.next_record()? {
                Some(r) if !(r.len() == 1 && r[0].is_empty()) => records.push(r),
                Some(_) => {}
                None => break,
            }
        }
        if records.is_empty() {
            return Ok(None);
        }
        let first = self.fid;
        let decoder = &self.decoder;
        let features = crate::par::map_range(records.len(), |i| decoder.feature(first + i as u64, &records[i]));
        self.fid += records.len() as u64;
        Ok(Some(features.into_iter().flatten().collect()))
    }
}

/// A field's bytes as text: UTF-8, or Latin-1 for files that are not UTF-8.
fn decode_field(b: &[u8], utf8: bool) -> String {
    if utf8 { String::from_utf8_lossy(b).into_owned() } else { b.iter().map(|&c| c as char).collect() }
}

/// The type GDAL's `AUTODETECT_TYPE` infers from a column's values, inferred value by value.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TypeInference {
    t: Option<FieldType>,
    text: bool,
}

impl TypeInference {
    /// Adds a value.
    pub(crate) fn add(&mut self, v: &str) {
        let v = v.trim();
        if self.text || v.is_empty() {
            return;
        }
        let vt = if is_integer(v) {
            FieldType::Integer
        } else if is_real(v) {
            FieldType::Float
        } else if is_datetime(v) {
            FieldType::DateTime
        } else if is_date(v) {
            FieldType::Date
        } else {
            self.text = true;
            return;
        };
        self.t = Some(match (self.t, vt) {
            (None, vt) => vt,
            (Some(a), b) if a == b => a,
            (Some(FieldType::Integer), FieldType::Float) | (Some(FieldType::Float), FieldType::Integer) => FieldType::Float,
            (Some(FieldType::Date), FieldType::DateTime) | (Some(FieldType::DateTime), FieldType::Date) => FieldType::DateTime,
            _ => {
                self.text = true;
                return;
            }
        });
    }

    /// The type inferred.
    pub(crate) fn field_type(&self) -> FieldType {
        if self.text { FieldType::Text } else { self.t.unwrap_or(FieldType::Text) }
    }
}

pub(crate) fn infer_type<'a>(values: impl Iterator<Item = &'a str>) -> FieldType {
    let mut t = TypeInference::default();
    values.for_each(|v| t.add(v));
    t.field_type()
}

/// A column that may hold WKT geometries, as the scan finds it.
#[derive(Debug, Clone, Default)]
struct WktCandidate {
    column: usize,
    /// Whether its first non-empty value is a geometry: `None` before any.
    is_wkt: Option<bool>,
    any: bool,
    lonlat: bool,
}

/// What a scan of a CSV file's rows finds: the geometry columns, the field types, whether the
/// coordinates are longitudes and latitudes.
#[derive(Debug, Clone, Default)]
struct CsvScan {
    x_col: Option<usize>,
    y_col: Option<usize>,
    z_col: Option<usize>,
    wkt: Vec<WktCandidate>,
    types: Vec<TypeInference>,
    any_xy: bool,
    lonlat_xy: bool,
}

fn in_lonlat(g: &Geometry) -> Option<bool> {
    g.bbox().map(|b| b.min_x >= -180.0 && b.max_x <= 180.0 && b.min_y >= -90.0 && b.max_y <= 90.0)
}

fn xy_point(get: impl Fn(usize) -> Option<String>, xc: usize, yc: usize, zc: Option<usize>) -> Option<Geometry> {
    match (get(xc).as_deref().and_then(parse_number), get(yc).as_deref().and_then(parse_number)) {
        (Some(x), Some(y)) => {
            let z = zc.and_then(&get).as_deref().and_then(parse_number);
            Some(Geometry::Point(Coord { x, y, z, m: None }))
        }
        _ => None,
    }
}

/// A field of a record, trimmed, as text (`None` when empty).
fn cell(r: &[Vec<u8>], i: usize, utf8: bool) -> Option<String> {
    let s = decode_field(r.get(i)?, utf8);
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

impl CsvScan {
    fn start(&mut self, header: &[String], options: &CsvOptions) {
        let find = |patterns: &[String], defaults: &[&str]| header.iter().position(|h| matches_name(h, patterns, defaults));
        let mut x_col = find(&options.x_possible_names, X_NAMES);
        let mut y_col = find(&options.y_possible_names, Y_NAMES);
        if x_col.is_none() || y_col.is_none() {
            (x_col, y_col) = (None, None);
        }
        self.z_col = x_col.and_then(|_| find(&options.z_possible_names, Z_NAMES));
        (self.x_col, self.y_col) = (x_col, y_col);
        if x_col.is_none() {
            self.wkt = header
                .iter()
                .enumerate()
                .filter(|(_, h)| matches_name(h, &options.geom_possible_names, GEOM_NAMES))
                .map(|(column, _)| WktCandidate { column, is_wkt: None, any: false, lonlat: true })
                .collect();
        }
        self.types = vec![TypeInference::default(); header.len()];
        self.lonlat_xy = true;
    }

    /// Adds a record (empty lines are skipped).
    fn add(&mut self, r: &[Vec<u8>]) {
        if r.len() == 1 && r[0].is_empty() {
            return;
        }
        // Field types, from the values as text: the separators and quotes are ASCII, so the
        // encoding does not change what they hold.
        for (t, f) in self.types.iter_mut().zip(r) {
            t.add(&String::from_utf8_lossy(f));
        }
        let get = |i: usize| cell(r, i, true);
        if let (Some(xc), Some(yc)) = (self.x_col, self.y_col) {
            if let Some(g) = xy_point(get, xc, yc, self.z_col) {
                self.any_xy = true;
                self.lonlat_xy &= in_lonlat(&g).unwrap_or(true);
            }
            return;
        }
        for c in &mut self.wkt {
            let Some(v) = r.get(c.column).map(|f| String::from_utf8_lossy(f)) else { continue };
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            let g = super::wkt::parse(v);
            if c.is_wkt.is_none() {
                c.is_wkt = Some(g.is_some());
            }
            if let Some(g) = g {
                c.any = true;
                c.lonlat &= in_lonlat(&g).unwrap_or(true);
            }
        }
    }

    /// The layer the rows make (without features), and their decoder.
    fn finish(self, header: Vec<String>, name: &str, options: &CsvOptions, utf8: bool) -> RowDecoder {
        // A WKT column must hold geometries: a "geometry" column of other values is an attribute.
        let wkt = self.wkt.iter().find(|c| c.is_wkt == Some(true));
        let wkt_col = wkt.map(|c| c.column);
        let geom_cols: Vec<usize> = [self.x_col, self.y_col, self.z_col, wkt_col].into_iter().flatten().collect();
        let mut layer = Layer::new(name);
        let mut fields = Vec::new();
        for (i, h) in header.iter().enumerate() {
            if !options.keep_geom_columns && geom_cols.contains(&i) {
                continue;
            }
            let t = if !options.autodetect_type || Some(i) == wkt_col { FieldType::Text } else { self.types[i].field_type() };
            let h = if h.is_empty() { format!("field_{}", i + 1) } else { h.clone() };
            layer.add_field(FieldDef::new(h, t));
            fields.push((i, t));
        }
        if self.x_col.is_some() {
            layer.geom_type = Some(GeometryType::Point);
        }
        let (has_geometry, lonlat) = match wkt {
            _ if self.x_col.is_some() => (self.any_xy, self.lonlat_xy),
            Some(c) => (c.any, c.lonlat),
            None => (false, true),
        };
        match &options.crs {
            Some(crs) => crs.apply_to_layer(&mut layer),
            None if has_geometry && lonlat => layer.set_crs_epsg(Some(4326)),
            None => {}
        }
        RowDecoder { layer, fields, x_col: self.x_col, y_col: self.y_col, z_col: self.z_col, wkt_col, utf8, has_geometry }
    }
}

/// Decodes CSV records into features.
struct RowDecoder {
    layer: Layer,
    fields: Vec<(usize, FieldType)>,
    x_col: Option<usize>,
    y_col: Option<usize>,
    z_col: Option<usize>,
    wkt_col: Option<usize>,
    utf8: bool,
    has_geometry: bool,
}

impl RowDecoder {
    /// The feature of a record; `None` for an empty line.
    fn feature(&self, fid: u64, r: &[Vec<u8>]) -> Option<Feature> {
        if r.len() == 1 && r[0].is_empty() {
            return None;
        }
        let get = |i: usize| cell(r, i, self.utf8);
        let geometry = match (self.x_col, self.y_col, self.wkt_col) {
            (Some(xc), Some(yc), _) => xy_point(get, xc, yc, self.z_col),
            (_, _, Some(wc)) => get(wc).as_deref().and_then(super::wkt::parse),
            _ => None,
        };
        let attributes = self.fields.iter().map(|&(i, t)| get(i).map_or(FieldValue::Null, |v| value(&v, t))).collect();
        Some(Feature { fid, geometry, attributes })
    }
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

/// CSV records (RFC 4180: quoted fields may hold separators, quotes and newlines), read from a
/// byte stream: each record's fields as bytes.
struct Records<R: std::io::Read> {
    r: R,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
    sep: u8,
}

impl<R: std::io::Read> Records<R> {
    fn new(r: R, sep: u8) -> Self {
        Self { r, buf: vec![0; 1 << 16], pos: 0, len: 0, eof: false, sep }
    }

    fn fill(&mut self) -> Result<bool> {
        if self.pos < self.len {
            return Ok(true);
        }
        if self.eof {
            return Ok(false);
        }
        self.len = loop {
            match self.r.read(&mut self.buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        };
        self.pos = 0;
        self.eof = self.len == 0;
        Ok(!self.eof)
    }

    fn next_byte(&mut self) -> Result<Option<u8>> {
        if !self.fill()? {
            return Ok(None);
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(b))
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        Ok(if self.fill()? { Some(self.buf[self.pos]) } else { None })
    }

    /// The next record; `None` at the end of the file.
    fn next_record(&mut self) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.fill()? {
            return Ok(None);
        }
        let mut fields = Vec::new();
        let mut field = Vec::new();
        let mut quoted = false;
        while let Some(b) = self.next_byte()? {
            if quoted {
                if b == b'"' {
                    if self.peek()? == Some(b'"') {
                        field.push(b'"');
                        self.pos += 1;
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
                fields.push(std::mem::take(&mut field));
            } else if b == b'\n' || b == b'\r' {
                if b == b'\r' && self.peek()? == Some(b'\n') {
                    self.pos += 1;
                }
                break;
            } else {
                field.push(b);
            }
        }
        fields.push(field);
        Ok(Some(fields))
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
    is_date_bytes(s.as_bytes())
}

fn is_date_bytes(b: &[u8]) -> bool {
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn is_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 19
        && is_date_bytes(&b[..10])
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':'
        && [11, 12, 14, 15, 17, 18].iter().all(|&i| b[i].is_ascii_digit())
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
    let enc = CsvEncoder::new(layer, &super::profile::LayerProfile::of(layer), options)?;
    let mut out = enc.header();
    enc.rows(&layer.features, &mut out);
    Ok(out)
}

/// Encodes the features of a layer as CSV rows, with the geometry columns its profile decides.
pub(crate) struct CsvEncoder {
    sep: char,
    mode: CsvGeometry,
    has_z: bool,
    header: Vec<String>,
    fields: usize,
}

impl CsvEncoder {
    /// An encoder for the features of `layer` (its fields), which `profile` describes.
    pub(crate) fn new(layer: &Layer, profile: &super::profile::LayerProfile, options: &CsvOptions) -> Result<Self> {
        let sep = options.separator.unwrap_or(b',') as char;
        let mode = if profile.no_geometry() { CsvGeometry::None } else { options.geometry };
        let has_z = mode == CsvGeometry::AsXy && profile.geometry.has_z;
        if mode == CsvGeometry::AsXy && profile.geometry.non_point {
            return Err(Error::InvalidArgument("CSV: writing geometries as X/Y columns needs a point layer".into()));
        }
        let mut header: Vec<String> = match mode {
            CsvGeometry::AsWkt => vec!["WKT".into()],
            CsvGeometry::AsXy if has_z => vec!["X".into(), "Y".into(), "Z".into()],
            CsvGeometry::AsXy => vec!["X".into(), "Y".into()],
            CsvGeometry::None => vec![],
        };
        header.extend(layer.schema.fields().iter().map(|f| f.name.clone()));
        Ok(Self { sep, mode, has_z, header, fields: layer.schema.len() })
    }

    /// The header line.
    pub(crate) fn header(&self) -> String {
        let mut out = String::new();
        push_row(&mut out, self.header.iter().map(String::as_str), self.sep);
        out
    }

    /// Appends the rows of features.
    pub(crate) fn rows(&self, features: &[Feature], out: &mut String) {
        let (mode, has_z, sep) = (self.mode, self.has_z, self.sep);
        let mut cells: Vec<String> = Vec::new();
        for f in features {
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
            for i in 0..self.fields {
                cells.push(match f.attributes.get(i).unwrap_or(&FieldValue::Null) {
                    FieldValue::Null => String::new(),
                    FieldValue::Integer(v) => v.to_string(),
                    FieldValue::Float(v) => fmt_f64(*v),
                    FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
                    FieldValue::Boolean(b) => (if *b { "1" } else { "0" }).into(),
                    FieldValue::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
                });
            }
            push_row(out, cells.iter().map(String::as_str), sep);
        }
    }
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
    fn reads_long_multibyte_text() {
        // Byte 10 of this name falls inside a character; the date check must not slice there (#159).
        let l = parse("id,name\n1,東京都千代田区\n", "t", &CsvOptions::default()).unwrap();
        assert_eq!(l.schema.fields()[1].field_type, FieldType::Text);
        assert_eq!(l.features[0].attributes[1], FieldValue::Text("東京都千代田区".into()));
    }

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
