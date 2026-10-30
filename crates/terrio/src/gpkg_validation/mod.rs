//! GeoPackage validation rules, built and tested before exposing the complete public validator.
//! Metadata requirements follow OGC GeoPackage 1.4 and GDAL 3.13.3 validate_gpkg.py.
mod constraints;
mod content;
mod extensions;
mod features;
mod geometry;
mod gridded;
mod integrity;
mod metadata;
mod relations;
mod rtree;
mod schema;
mod schema_extension;
mod sqlite_expr;
mod tiles;
mod views;

use crate::drivers::{
    sqlite::{SchemaEntry, Value},
    sqlite_repack,
};
use crate::{Error, Result};
use schema::Schema;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(untagged)]
enum Requirement {
    Core(u32),
    Extension(&'static str),
}
impl Requirement {
    fn number(self) -> Option<u32> {
        match self {
            Self::Core(number) => Some(number),
            Self::Extension(_) => None,
        }
    }
}
impl PartialEq<u32> for Requirement {
    fn eq(&self, other: &u32) -> bool {
        self.number() == Some(*other)
    }
}
#[derive(Debug)]
struct Violation {
    requirement: Requirement,
    message: String,
}
#[derive(Debug, Default)]
struct CoreReport {
    version: Option<u32>,
    errors: Vec<Violation>,
    warnings: Vec<String>,
}
impl CoreReport {
    fn require(&mut self, condition: bool, requirement: u32, message: impl Into<String>) {
        self.require_rule(condition, Requirement::Core(requirement), message);
    }
    fn require_rule(&mut self, condition: bool, requirement: Requirement, message: impl Into<String>) {
        if !condition {
            self.errors.push(Violation { requirement, message: message.into() });
        }
    }
}

struct Table {
    schema: Schema,
    rows: Vec<Vec<Value>>,
    evaluator: Option<sqlite_expr::RowEvaluator>,
}
impl Table {
    fn from_sql(name: &str, sql: &str) -> Result<Self> {
        let schema = Schema::parse(sql)?;
        let evaluator = if schema.virtual_module.is_none() { Some(sqlite_expr::RowEvaluator::new(name, &schema)?) } else { None };
        Ok(Self { schema, rows: Vec::new(), evaluator })
    }
    /// The logical SQLite row, including REAL read affinity and virtual generated
    /// values. All streaming
    /// validation passes share the plan compiled once while loading the schema.
    fn decode(&self, name: &str, record: &[u8], rowid: Option<i64>, encoding: u32) -> Result<Vec<Value>> {
        let mut values = self.decode_physical(record, rowid, encoding)?;
        for (value, column) in values.iter_mut().zip(&self.schema.columns) {
            if sqlite_expr::has_real_affinity(&column.data_type)
                && let Value::Int(n) = value
            {
                *value = Value::Real(*n as f64);
            }
        }
        match &self.evaluator {
            Some(evaluator) => evaluator.row(name, &self.schema, values, rowid, encoding),
            None => Ok(values),
        }
    }
    fn decode_physical(&self, record: &[u8], rowid: Option<i64>, encoding: u32) -> Result<Vec<Value>> {
        let stored = sqlite_repack::decode_record(record, encoding)?;
        let mut values = vec![Value::Null; self.schema.columns.len()];
        let mut order: Vec<_> =
            (0..values.len()).filter(|i| !self.schema.columns[*i].generated.as_ref().is_some_and(|g| !g.stored)).collect();
        if self.schema.without_rowid {
            order.sort_by_key(|i| {
                let pk = self.schema.columns[*i].primary_key;
                if pk == 0 { values.len() + *i } else { pk - 1 }
            });
        }
        if stored.len() > order.len() {
            return Err(Error::InvalidData("SQLite record has more fields than its table".into()));
        }
        for (field, column) in order.iter().enumerate() {
            values[*column] =
                stored.get(field).cloned().unwrap_or_else(|| literal_default(self.schema.columns[*column].default.as_deref()));
        }
        if let Some(column) = self.schema.rowid_alias {
            values[column] = rowid.map_or(Value::Null, Value::Int);
        }
        Ok(values)
    }
    fn get<'a>(&self, row: &'a [Value], name: &str) -> Option<&'a Value> {
        self.schema.columns.iter().position(|column| column.name == name).and_then(|i| row.get(i))
    }
}
struct Snapshot {
    objects: Vec<SchemaEntry>,
    tables: BTreeMap<String, Table>,
}
fn metadata_table(name: &str) -> bool {
    matches!(
        name,
        "gpkg_spatial_ref_sys"
            | "gpkg_contents"
            | "gpkg_geometry_columns"
            | "gpkg_tile_matrix_set"
            | "gpkg_tile_matrix"
            | "gpkg_extensions"
            | "gpkg_metadata"
            | "gpkg_metadata_reference"
            | "gpkg_data_columns"
            | "gpkg_data_column_constraints"
            | "gpkg_2d_gridded_coverage_ancillary"
            | "gpkg_2d_gridded_tile_ancillary"
            | "gpkgext_relations"
    )
}
fn literal_default(value: Option<&str>) -> Value {
    let Some(value) = value else { return Value::Null };
    if value.eq_ignore_ascii_case("null") {
        return Value::Null;
    }
    if value.eq_ignore_ascii_case("true") {
        return Value::Int(1);
    }
    if value.eq_ignore_ascii_case("false") {
        return Value::Int(0);
    }
    if let Some(text) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Value::Text(text.replace("''", "'"));
    }
    if let Some(text) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Value::Text(text.replace("\"\"", "\""));
    }
    if let Some(hex) = value.strip_prefix("X'").or_else(|| value.strip_prefix("x'")).and_then(|v| v.strip_suffix('\''))
        && hex.len().is_multiple_of(2)
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Value::Blob((0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("checked hex")).collect());
    }
    if let Ok(value) = value.parse::<i64>() {
        return Value::Int(value);
    }
    if let Ok(value) = value.parse::<f64>() {
        return Value::Real(value);
    }
    Value::Null
}
impl Snapshot {
    fn read(bytes: &[u8]) -> Result<Self> {
        let encoding = bytes
            .get(56..60)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes)
            .ok_or_else(|| Error::InvalidData("truncated SQLite header".into()))?;
        let mut tables: BTreeMap<String, Table> = BTreeMap::new();
        let objects = sqlite_repack::inspect(bytes, |entry, rowid, record| {
            if entry.0 != "table" {
                return Ok(());
            }
            if !tables.contains_key(&entry.1) {
                tables.insert(entry.1.clone(), Table::from_sql(&entry.1, &entry.4)?);
            }
            if metadata_table(&entry.1) {
                let table = tables.get_mut(&entry.1).expect("inserted table");
                let values = table.decode(&entry.1, record, rowid, encoding)?;
                table.rows.push(values);
            }
            Ok(())
        })?;
        for entry in &objects {
            if entry.0 == "table" && !tables.contains_key(&entry.1) {
                tables.insert(entry.1.clone(), Table::from_sql(&entry.1, &entry.4)?);
            }
        }
        Ok(Self { objects, tables })
    }
    fn object(&self, name: &str) -> bool {
        self.objects.iter().any(|entry| matches!(entry.0.as_str(), "table" | "view") && entry.1.eq_ignore_ascii_case(name))
    }
    fn srs(&self, id: i64) -> bool {
        self.tables
            .get("gpkg_spatial_ref_sys")
            .is_some_and(|table| table.rows.iter().any(|row| table.get(row, "srs_id") == Some(&Value::Int(id))))
    }
}

struct ColumnRule {
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    default: Option<&'static str>,
    primary_key: usize,
}
fn column(name: &'static str, data_type: &'static str, not_null: bool, default: Option<&'static str>, primary_key: usize) -> ColumnRule {
    ColumnRule { name, data_type, not_null, default, primary_key }
}
fn same_default(actual: Option<&str>, expected: Option<&str>) -> bool {
    if actual == expected {
        return true;
    }
    if expected != Some("strftime('%Y-%m-%dT%H:%M:%fZ','now')") {
        return false;
    }
    actual.is_some_and(|value| {
        let value = value.to_ascii_lowercase().replace(' ', "");
        value == "strftime('%y-%m-%dt%h:%m:%fz','now')" || value == "strftime('%y-%m-%dt%h:%m:%fz',current_timestamp)"
    })
}
struct Checker {
    db: Snapshot,
    report: CoreReport,
}
impl Checker {
    fn structure(&mut self, name: &str, requirement: u32, expected: &[ColumnRule], legacy_wkt_defaults: bool) {
        self.structure_rule(name, Requirement::Core(requirement), expected, legacy_wkt_defaults);
    }
    fn structure_rule(&mut self, name: &str, requirement: Requirement, expected: &[ColumnRule], legacy_wkt_defaults: bool) {
        let Some(table) = self.db.tables.get(name) else {
            self.report.require_rule(false, requirement, format!("missing table {name}"));
            return;
        };
        self.report.require_rule(table.schema.columns.len() == expected.len(), requirement, format!("{name}: wrong column count"));
        for rule in expected {
            let Some(actual) = table.schema.columns.iter().find(|c| c.name == rule.name) else {
                self.report.require_rule(false, requirement, format!("{name}: missing column {}", rule.name));
                continue;
            };
            self.report.require_rule(
                actual.data_type == rule.data_type,
                requirement,
                format!("{name}.{}: expected {}, got {}", rule.name, rule.data_type, actual.data_type),
            );
            self.report.require_rule(
                actual.not_null == rule.not_null,
                requirement,
                format!("{name}.{}: wrong NOT NULL constraint", rule.name),
            );
            self.report.require_rule(
                actual.primary_key == rule.primary_key,
                requirement,
                format!("{name}.{}: wrong primary-key position", rule.name),
            );
            let default = if legacy_wkt_defaults
                && matches!(rule.name, "definition" | "definition_12_063")
                && actual.default.as_deref() == Some("''")
            {
                None
            } else {
                actual.default.as_deref()
            };
            self.report.require_rule(
                same_default(default, rule.default),
                requirement,
                format!("{name}.{}: wrong default expression", rule.name),
            );
        }
    }

    /// Attribute table declarations; view schemas need the later SQL/view layer.
    fn attribute_table(&mut self, name: &str) {
        let Some(table) = self.db.tables.get(name) else { return };
        self.report.require(table.schema.rowid_alias.is_some(), 119, format!("{name}: INTEGER primary key must act as a rowid alias"));
        for column in &table.schema.columns {
            self.report.require(
                content::valid_data_type(&column.data_type),
                5,
                format!("{name}.{}: invalid data type {}", column.name, column.data_type),
            );
        }
    }

    /// Dispatch registered physical attribute tables. Attribute views still need
    /// the separate view evaluator before the complete validator is exposed.
    fn attributes(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(contents) = self.db.tables.get("gpkg_contents") else { return Ok(()) };
        let names: Vec<_> = contents
            .rows
            .iter()
            .filter(|row| contents.get(row, "data_type").and_then(Value::as_str) == Some("attributes"))
            .filter_map(|row| contents.get(row, "table_name").and_then(Value::as_str))
            .filter_map(|name| self.db.tables.keys().find(|key| key.eq_ignore_ascii_case(name)).cloned())
            .collect();
        if names.is_empty() {
            return Ok(());
        }
        for name in &names {
            self.attribute_table(name);
        }
        self.user_content(bytes, &names.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// A second streaming inspection examines only requested user tables. Retain
    /// at most one warning per column, never the table's records or geometry blobs.
    fn user_content(&mut self, bytes: &[u8], names: &[&str]) -> Result<()> {
        let encoding = bytes
            .get(56..60)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes)
            .ok_or_else(|| Error::InvalidData("truncated SQLite header".into()))?;
        let mut warned: BTreeMap<&str, Vec<bool>> = names
            .iter()
            .filter_map(|&name| self.db.tables.get(name).map(|table| (name, vec![false; table.schema.columns.len()])))
            .collect();
        sqlite_repack::inspect(bytes, |entry, rowid, record| {
            if entry.0 != "table" {
                return Ok(());
            }
            let Some(warned) = warned.get_mut(entry.1.as_str()) else { return Ok(()) };
            let table = &self.db.tables[&entry.1];
            let values = table.decode(&entry.1, record, rowid, encoding)?;
            for ((column, value), warned) in table.schema.columns.iter().zip(&values).zip(warned) {
                if !*warned && let Some(warning) = content::warning(column, value) {
                    self.report.warnings.push(format!("{}: {warning}", entry.1));
                    *warned = true;
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    fn spatial_references(&mut self) {
        let Some(table) = self.db.tables.get("gpkg_spatial_ref_sys") else {
            self.report.require(false, 10, "missing table gpkg_spatial_ref_sys");
            return;
        };
        let wkt = table.schema.columns.iter().any(|c| c.name == "definition_12_063");
        let epoch = table.schema.columns.iter().any(|c| c.name == "epoch");
        let extension = self.db.tables.get("gpkg_extensions").and_then(|table| {
            table.rows.iter().find_map(|row| {
                let name = table.get(row, "extension_name").and_then(Value::as_str)?;
                matches!(name, "gpkg_crs_wkt" | "gpkg_crs_wkt_1_1")
                    .then(|| (name.to_owned(), table.get(row, "scope").and_then(Value::as_str).unwrap_or("").to_owned()))
            })
        });
        if let Some((name, scope)) = extension {
            self.report.require(scope == "read-write", 145, format!("{name}: scope must be read-write"));
            self.report.require(wkt, 145, "declared CRS WKT extension needs definition_12_063");
            self.report.require(!epoch || name == "gpkg_crs_wkt_1_1", 145, "epoch requires gpkg_crs_wkt_1_1");
        } else {
            self.report.require(!wkt && !epoch, 145, "extended CRS columns require a declared CRS WKT extension");
        }
        let mut expected = vec![
            column("srs_name", "TEXT", true, None, 0),
            column("srs_id", "INTEGER", true, None, 1),
            column("organization", "TEXT", true, None, 0),
            column("organization_coordsys_id", "INTEGER", true, None, 0),
            column("definition", "TEXT", true, None, 0),
            column("description", "TEXT", false, None, 0),
        ];
        if wkt {
            expected.push(column("definition_12_063", "TEXT", true, None, 0));
        }
        if epoch {
            expected.push(column("epoch", "DOUBLE", false, None, 0));
        }
        self.structure("gpkg_spatial_ref_sys", 10, &expected, wkt);
        let table = &self.db.tables["gpkg_spatial_ref_sys"];
        let required_count = table.rows.iter().filter(|row| matches!(table.get(row, "srs_id"), Some(Value::Int(-1 | 0 | 4326)))).count();
        self.report.require(required_count == 3, 11, "expected one record for each required SRS -1, 0 and 4326");
        if required_count != 3 {
            return;
        }
        for id in [-1, 0, 4326] {
            let Some(row) = table.rows.iter().find(|row| table.get(row, "srs_id") == Some(&Value::Int(id))) else {
                self.report.require(false, 11, format!("missing required SRS {id}"));
                continue;
            };
            let org = table.get(row, "organization").and_then(Value::as_str);
            self.report.require(
                if id == 4326 { org.is_some_and(|v| v.eq_ignore_ascii_case("EPSG")) } else { org == Some("NONE") },
                11,
                format!("SRS {id}: wrong organization"),
            );
            self.report.require(
                table.get(row, "organization_coordsys_id") == Some(&Value::Int(id)),
                11,
                format!("SRS {id}: wrong organization coordinate system ID"),
            );
            for (field, requirement) in [("definition", 11), ("definition_12_063", 116)] {
                if field == "definition_12_063" && !wkt {
                    continue;
                }
                let definition = table.get(row, field).and_then(Value::as_str);
                self.report.require(
                    if id == 4326 { definition.is_some_and(|v| v != "undefined") } else { definition == Some("undefined") },
                    requirement,
                    format!("SRS {id}: wrong {field}"),
                );
            }
        }
        if wkt {
            for row in &table.rows {
                if let Some(Value::Int(id)) = table.get(row, "srs_id") {
                    self.report.require(
                        matches!(*id, -1 | 0)
                            || table.get(row, "definition").and_then(Value::as_str) != Some("undefined")
                            || table.get(row, "definition_12_063").and_then(Value::as_str) != Some("undefined"),
                        117,
                        format!("SRS {id}: both definitions are undefined"),
                    );
                }
            }
        }
    }

    fn contents(&mut self) {
        let expected = [
            column("table_name", "TEXT", true, None, 1),
            column("data_type", "TEXT", true, None, 0),
            column("identifier", "TEXT", false, None, 0),
            column("description", "TEXT", false, Some("''"), 0),
            column("last_change", "DATETIME", true, Some("strftime('%Y-%m-%dT%H:%M:%fZ','now')"), 0),
            column("min_x", "DOUBLE", false, None, 0),
            column("min_y", "DOUBLE", false, None, 0),
            column("max_x", "DOUBLE", false, None, 0),
            column("max_y", "DOUBLE", false, None, 0),
            column("srs_id", "INTEGER", false, None, 0),
        ];
        self.structure("gpkg_contents", 13, &expected, false);
        let Some(table) = self.db.tables.get("gpkg_contents") else { return };
        for row in &table.rows {
            let name = table.get(row, "table_name").and_then(Value::as_str);
            self.report.require(name.is_some_and(|name| self.db.object(name)), 14, "contents references a missing table or view");
            self.report.require(
                table
                    .get(row, "data_type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "features" | "tiles" | "attributes" | "2d-gridded-coverage")),
                17,
                "contents has an unknown data_type",
            );
            self.report.require(
                table.get(row, "last_change").and_then(Value::as_str).is_some_and(valid_timestamp),
                15,
                "contents has an invalid last_change timestamp",
            );
            match table.get(row, "srs_id") {
                Some(Value::Null) => (),
                Some(Value::Int(id)) => self.report.require(self.db.srs(*id), 14, format!("contents references missing SRS {id}")),
                _ => self.report.require(false, 14, "contents srs_id is not an integer or null"),
            }
        }
    }
}
fn valid_timestamp(value: &str) -> bool {
    let Some((date, fraction)) = value.strip_suffix('Z').and_then(|v| v.rsplit_once('.')) else { return false };
    if date.len() != 19 || !date.is_ascii() || !(1..=6).contains(&fraction.len()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let bytes = date.as_bytes();
    if [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')].iter().any(|(i, c)| bytes[*i] != *c) {
        return false;
    }
    if bytes.iter().enumerate().any(|(i, b)| !matches!(i, 4 | 7 | 10 | 13 | 16) && !b.is_ascii_digit()) {
        return false;
    }
    let number = |start, end| date[start..end].parse::<u32>().unwrap_or(0);
    let (year, month, day) = (number(0, 4), number(5, 7), number(8, 10));
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    year > 0
        && (1..=12).contains(&month)
        && day > 0
        && day <= days[month.saturating_sub(1) as usize]
        && number(11, 13) < 24
        && number(14, 16) < 60
        && number(17, 19) < 60
}
fn check_core(bytes: &[u8]) -> CoreReport {
    let mut report = CoreReport::default();
    if bytes.len() < 100 || !bytes.starts_with(b"SQLite format 3\0") {
        report.require(false, 1, "not an SQLite database");
        return report;
    }
    let integer = |at| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("checked header"));
    let id = integer(68);
    report.version = match id {
        0x4750_3130 => Some(10000),
        0x4750_3131 => Some(10100),
        0x4750_4b47 => {
            let version = integer(60);
            report.require(version >= 10200, 2, "GPKG application ID requires user_version >= 10200");
            if version >= 10500 {
                report.warnings.push(format!("GeoPackage version {version} is newer than the reference validator"));
            }
            Some(version)
        }
        _ => {
            report.require(false, 2, "wrong GeoPackage application ID");
            None
        }
    };
    let db = match Snapshot::read(bytes) {
        Ok(db) => db,
        Err(error) => {
            report.require(false, 6, error.to_string());
            return report;
        }
    };
    let mut checker = Checker { db, report };
    if let Err(error) = checker.constraints(bytes) {
        checker.report.require(false, 6, error.to_string());
    }
    if let Err(error) = checker.foreign_keys(bytes) {
        checker.report.require(false, 6, error.to_string());
    }
    checker.spatial_references();
    checker.contents();
    checker.extensions();
    checker.features();
    if let Err(error) = checker.views(bytes) {
        checker.report.require(false, 6, error.to_string());
    }
    checker.spatial_indexes();
    checker.relations();
    checker.schema_extension();
    if let Err(error) = checker.gridded(bytes) {
        checker.report.require(false, 6, error.to_string());
    }
    if let Err(error) = checker.metadata(bytes) {
        checker.report.require(false, 6, error.to_string());
    }
    if let Err(error) = checker.geometries(bytes) {
        checker.report.require(false, 6, error.to_string());
    }
    if let Err(error) = checker.attributes(bytes) {
        checker.report.require(false, 6, error.to_string());
    }
    if let Err(error) = checker.tiles(bytes) {
        checker.report.require(false, 6, error.to_string());
    }
    checker.report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn logical_real_reads_honor_text_and_integer_affinity_precedence() {
        use crate::drivers::vector::sqlite::{self as writer, Table as OutputTable, Value as OutputValue};
        let sql = "CREATE TABLE sample(a REAL,b DOUBLE,c FLOAT,d TEXTREAL,e CHARREAL,f CLOBREAL,g FLOATING POINT)";
        let tables =
            [OutputTable { name: "sample".into(), sql: sql.into(), rows: vec![(1, vec![OutputValue::Int(1); 7])], autoindexes: vec![] }];
        let bytes = writer::write_database(&tables, &[], 10400, 0x4750_4b47).unwrap();
        let db = Snapshot::read(&bytes).unwrap();
        sqlite_repack::inspect(&bytes, |entry, rowid, record| {
            if entry.1 == "sample" {
                let row = db.tables["sample"].decode("sample", record, rowid, 1)?;
                assert_eq!(
                    row,
                    [Value::Real(1.), Value::Real(1.), Value::Real(1.), Value::Int(1), Value::Int(1), Value::Int(1), Value::Int(1)]
                );
            }
            Ok(())
        })
        .unwrap();
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn generated_rows_reach_metadata_content_and_geometry_checks() {
        use crate::drivers::vector::sqlite::{self as writer, Table as OutputTable, Value as OutputValue};
        let mut point = b"GP\0\x01".to_vec();
        point.extend(4326i32.to_le_bytes());
        point.push(1);
        point.extend(1u32.to_le_bytes());
        point.extend(1f64.to_le_bytes());
        point.extend(2f64.to_le_bytes());
        for (blob, valid) in [(point, true), (vec![0, 1, 2], false)] {
            let text = |value: &str| OutputValue::Text(value.into());
            let tables = [
                OutputTable {
                    name: "sample".into(),
                    sql: "CREATE TABLE sample(id INTEGER PRIMARY KEY,raw BLOB,geom POINT AS(raw),count SMALLINT AS(id+40000))".into(),
                    rows: vec![(1, vec![OutputValue::Null, OutputValue::Blob(blob)])],
                    autoindexes: vec![],
                },
                OutputTable {
                    name: "gpkg_geometry_columns".into(),
                    sql: "CREATE TABLE gpkg_geometry_columns(table_name TEXT,column_name TEXT,geometry_type_name TEXT,srs_id INTEGER AS(4326),z INTEGER,m INTEGER)".into(),
                    rows: vec![(1, vec![text("sample"), text("geom"), text("POINT"), OutputValue::Int(0), OutputValue::Int(0)])],
                    autoindexes: vec![],
                },
            ];
            let bytes = writer::write_database(&tables, &[], 10400, 0x4750_4b47).unwrap();
            let db = Snapshot::read(&bytes).unwrap();
            assert!(db.tables["sample"].rows.is_empty());
            let registry = &db.tables["gpkg_geometry_columns"];
            assert_eq!(registry.get(&registry.rows[0], "srs_id"), Some(&Value::Int(4326)));
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.user_content(&bytes, &["sample"]).unwrap();
            assert_eq!(checker.report.warnings.len(), 1, "{:?}", checker.report);
            assert!(checker.report.warnings[0].contains("count"));
            checker.geometries(&bytes).unwrap();
            assert_eq!(checker.report.errors.is_empty(), valid, "{:?}", checker.report);
            assert!(checker.report.errors.iter().all(|e| e.requirement == 19));
        }
    }
    #[test]
    fn extension_requirements_keep_their_names_in_reports() {
        let core = Requirement::Core(19);
        let extension = Requirement::Extension("gpkg_2d_gridded_coverage#13");
        assert_eq!(serde_json::to_value(core).unwrap(), serde_json::json!(19));
        assert_eq!(serde_json::to_value(extension).unwrap(), serde_json::json!("gpkg_2d_gridded_coverage#13"));
        assert_eq!(core.number(), Some(19));
        assert_eq!(extension.number(), None);
        assert_ne!(extension, 0);
    }
    #[test]
    fn timestamps_match_reference_calendar_and_fraction_rules() {
        for value in ["2024-02-29T23:59:59.1Z", "2026-10-29T00:00:00.123456Z"] {
            assert!(valid_timestamp(value), "{value}");
        }
        for value in [
            "0000-01-01T00:00:00.000Z",
            "1900-02-29T00:00:00.000Z",
            "2026-01-01T24:00:00.000Z",
            "2026-01-01T00:00:60.000Z",
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00.1234567Z",
            "2026-13-01T00:00:00.000Z",
            "2026-01-01T00:00:00.000+00:00",
        ] {
            assert!(!valid_timestamp(value), "{value}");
        }
        assert!(same_default(Some("strftime('%Y-%m-%dT%H:%M:%fZ', CURRENT_TIMESTAMP)"), Some("strftime('%Y-%m-%dT%H:%M:%fZ','now')")));
        assert!(!same_default(Some("CURRENT_TIMESTAMP"), Some("strftime('%Y-%m-%dT%H:%M:%fZ','now')")));
    }
    #[test]
    fn column_rules_check_names_types_nullability_defaults_and_keys() {
        let rules = [column("id", "INTEGER", true, None, 1), column("note", "TEXT", false, Some("''"), 0)];
        for (sql, valid) in [
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT DEFAULT '')", true),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, other TEXT DEFAULT '')", false),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note BLOB DEFAULT '')", false),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT NOT NULL DEFAULT '')", false),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT DEFAULT 'changed')", false),
            ("CREATE TABLE t(id INTEGER, note TEXT DEFAULT '')", false),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT DEFAULT '', extra TEXT)", false),
        ] {
            let db = Snapshot {
                objects: vec![],
                tables: BTreeMap::from([("t".into(), Table { schema: Schema::parse(sql).unwrap(), rows: vec![], evaluator: None })]),
            };
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.structure("t", 13, &rules, false);
            assert_eq!(checker.report.errors.is_empty(), valid, "{sql}: {:?}", checker.report);
            assert!(checker.report.errors.iter().all(|e| e.requirement == 13));
        }
    }

    #[test]
    fn attribute_table_requires_an_integer_rowid_alias_and_standard_types() {
        for (sql, valid) in [
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT)", true),
            ("CREATE TABLE t(id INT PRIMARY KEY, note TEXT)", false),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY DESC, note TEXT)", false),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT) WITHOUT ROWID", false),
            ("CREATE TABLE t(id INTEGER, other INTEGER, PRIMARY KEY(id,other))", false),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note VARCHAR)", false),
            ("CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT(8), bytes BLOB(2))", true),
        ] {
            let db = Snapshot {
                objects: vec![],
                tables: BTreeMap::from([("t".into(), Table { schema: Schema::parse(sql).unwrap(), rows: vec![], evaluator: None })]),
            };
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.attribute_table("t");
            assert_eq!(checker.report.errors.is_empty(), valid, "{sql}: {:?}", checker.report);
        }
    }

    #[test]
    fn required_srs_records_cannot_be_duplicated() {
        let schema = Schema::parse("CREATE TABLE gpkg_spatial_ref_sys(srs_name TEXT NOT NULL,srs_id INTEGER PRIMARY KEY,organization TEXT NOT NULL,organization_coordsys_id INTEGER NOT NULL,definition TEXT NOT NULL,description TEXT)").unwrap();
        let rows = [-1, 0, 4326]
            .into_iter()
            .map(|id| {
                vec![
                    Value::Text("name".into()),
                    Value::Int(id),
                    Value::Text(if id == 4326 { "EPSG" } else { "NONE" }.into()),
                    Value::Int(id),
                    Value::Text(if id == 4326 { "WGS84" } else { "undefined" }.into()),
                    Value::Null,
                ]
            })
            .collect();
        let mut checker = Checker {
            db: Snapshot {
                objects: vec![],
                tables: BTreeMap::from([("gpkg_spatial_ref_sys".into(), Table { schema, rows, evaluator: None })]),
            },
            report: CoreReport::default(),
        };
        checker.spatial_references();
        assert!(checker.report.errors.is_empty(), "{:?}", checker.report);
        let table = checker.db.tables.get_mut("gpkg_spatial_ref_sys").unwrap();
        table.rows.push(table.rows[0].clone());
        checker.spatial_references();
        assert!(checker.report.errors.iter().any(|e| e.requirement == 11));
    }

    #[test]
    fn rowid_alias_respects_inline_desc_and_without_rowid() {
        assert_eq!(Schema::parse("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)").unwrap().rowid_alias, Some(0));
        assert_eq!(Schema::parse("CREATE TABLE t(id INTEGER PRIMARY KEY DESC, v TEXT)").unwrap().rowid_alias, None);
        assert_eq!(Schema::parse("CREATE TABLE t(id INTEGER, v TEXT, PRIMARY KEY(id DESC))").unwrap().rowid_alias, Some(0));
        assert_eq!(Schema::parse("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT) WITHOUT ROWID").unwrap().rowid_alias, None);
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn core_metadata_matches_gdal_on_real_derivatives() {
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import sys, pathlib, json, sqlite3, struct, shutil
sys.path.insert(0,'scripts')
from compare_gpkg_validation_with_gdal import fixtures, sql
from osgeo_utils.samples.validate_gpkg import GPKGChecker
root=pathlib.Path(sys.argv[1]); data=pathlib.Path(sys.argv[2]); cases=list(fixtures(data,root))
for name, statement in [
 ('legacy GP10 header','PRAGMA application_id=0x47503130; PRAGMA user_version=0'),
 ('legacy GP11 header','PRAGMA application_id=0x47503131; PRAGMA user_version=0'),
 ('required Cartesian organization',"UPDATE gpkg_spatial_ref_sys SET organization='EPSG' WHERE srs_id=-1"),
 ('required geographic definition',"UPDATE gpkg_spatial_ref_sys SET definition='bad' WHERE srs_id=0"),
 ('required WGS84 definition',"UPDATE gpkg_spatial_ref_sys SET definition='undefined' WHERE srs_id=4326"),
]:
 path=root/('core-'+str(len(cases))+'.gpkg');shutil.copyfile(root/'vector.gpkg',path);sql(path,statement);cases.append((name,path,None))
from osgeo import gdal
wkt=root/'wkt.gpkg'
gdal.VectorTranslate(str(wkt),str(root/'vector.gpkg'),format='GPKG',datasetCreationOptions=['CRS_WKT_EXTENSION=YES']).Close()
cases.append(('declared CRS WKT extension',wkt,None))
for name, statement in [
 ('missing WKT declaration', "DELETE FROM gpkg_extensions WHERE extension_name IN ('gpkg_crs_wkt','gpkg_crs_wkt_1_1')"),
 ('wrong WKT scope', "UPDATE gpkg_extensions SET scope='read-only' WHERE extension_name IN ('gpkg_crs_wkt','gpkg_crs_wkt_1_1')"),
 ('epoch with old WKT declaration', "UPDATE gpkg_extensions SET extension_name='gpkg_crs_wkt' WHERE extension_name IN ('gpkg_crs_wkt','gpkg_crs_wkt_1_1')"),
 ('epoch with WKT 1.1 declaration', "UPDATE gpkg_extensions SET extension_name='gpkg_crs_wkt_1_1' WHERE extension_name IN ('gpkg_crs_wkt','gpkg_crs_wkt_1_1')"),
 ('custom SRS with undefined definitions', "INSERT INTO gpkg_spatial_ref_sys(srs_name,srs_id,organization,organization_coordsys_id,definition,definition_12_063) VALUES('custom',99999,'NONE',99999,'undefined','undefined')"),
]:
 path=root/('wkt-'+str(len(cases))+'.gpkg');shutil.copyfile(wkt,path)
 if name.startswith('epoch'):
  with sqlite3.connect(path) as db:
   if not any(row[1]=='epoch' for row in db.execute('PRAGMA table_info(gpkg_spatial_ref_sys)')):
    db.execute('ALTER TABLE gpkg_spatial_ref_sys ADD COLUMN epoch DOUBLE')
 sql(path,statement);cases.append((name,path,None))
path=root/'legacy-defaults.gpkg';shutil.copyfile(wkt,path)
with sqlite3.connect(path) as db:
 columns=list(db.execute('PRAGMA table_info(gpkg_spatial_ref_sys)'));parts=[]
 for _,name,kind,notnull,default,pk in columns:
  default="''" if name in ('definition','definition_12_063') else default
  parts.append('"'+name+'" '+kind+(' NOT NULL' if notnull else '')+(' PRIMARY KEY' if pk else '')+(' DEFAULT '+default if default is not None else ''))
 declaration='CREATE TABLE gpkg_spatial_ref_sys('+','.join(parts)+')'
 db.execute('PRAGMA writable_schema=ON')
 db.execute("UPDATE sqlite_master SET sql=? WHERE name='gpkg_spatial_ref_sys'",(declaration,));db.commit()
cases.append(('legacy empty WKT defaults',path,None))
out=[]
for name,path,_ in cases:
 raw=path.read_bytes(); app=raw[68:72]; version=int.from_bytes(raw[60:64],'big')
 header=app in (b'GP10',b'GP11') or app==b'GPKG' and version>=10200
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.extended_pragma_info=True
 checker.version=(1,0,0) if app==b'GP10' else (1,1,0) if app==b'GP11' else (version//10000,(version//100)%100,version%100)
 failed=False
 try:
  with sqlite3.connect(path) as db:
   cursor=db.cursor()
   failed=db.execute('PRAGMA integrity_check').fetchone()[0]!='ok'
   checker._check_gpkg_spatial_ref_sys(cursor);checker._check_gpkg_contents(cursor);checker._check_gpkg_extensions(cursor);checker._check_tiles(cursor);checker._check_features(cursor)
 except (sqlite3.Error,TypeError,ZeroDivisionError,IndexError):
  # GDAL can raise on missing/empty tile metadata instead of returning diagnostics.
  failed=True
 out.append({'name':name,'path':str(path),'valid':header and not failed and not checker.errors})
print(json.dumps(out))
"#;
        let output = std::process::Command::new("python3")
            .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .arg("-c")
            .arg(script)
            .arg(dir.path())
            .arg(data)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(cases.as_array().unwrap().len(), 37);
        for case in cases.as_array().unwrap() {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let report = check_core(&bytes);
            assert_eq!(report.errors.is_empty(), case["valid"].as_bool().unwrap(), "{}: {report:?}", case["name"]);
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn user_content_matches_gdal_with_documented_spec_corrections() {
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import sys,pathlib,json,sqlite3
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
root=pathlib.Path(sys.argv[1]); data=pathlib.Path(sys.argv[2]); path=root/'attributes.gpkg'
gdal.UseExceptions()
gdal.VectorTranslate(str(path),str(data/'countries.gpkg'),format='GPKG',layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
cases=[]
with sqlite3.connect(path) as db:
 def add(kind,values,*,schema=None,warning_delta=0,error_delta=0,post=None,without_rowid=False):
  name='case_'+str(len(cases)); schema=schema or ('id INTEGER PRIMARY KEY,v '+kind)
  db.execute('CREATE TABLE '+name+'('+schema+')'+(' WITHOUT ROWID' if without_rowid else ''))
  for rowid,value in enumerate(values): db.execute('INSERT INTO '+name+'(id,v) VALUES(?,?)',(rowid,value))
  if post: db.execute('ALTER TABLE '+name+' ADD COLUMN '+post)
  registered=name.upper() if name=='case_0' else name
  db.execute("INSERT INTO gpkg_contents(table_name,data_type,identifier) VALUES(?,'attributes',?)",(registered,name))
  db.commit()
  checker=GPKGChecker(str(path),abort_at_first_error=False,extra_checks=True,warning_msg=False)
  checker._check_attribute_user_table(db.cursor(),name)
  cases.append(dict(name=name,warnings=len(checker.warnings),errors=len(checker.errors),warning_delta=warning_delta,error_delta=error_delta))
 for kind,lo,hi in [('BOOLEAN',0,1),('TINYINT',-128,127),('SMALLINT',-32768,32767),('MEDIUMINT',-2147483648,2147483647)]:
  for value in [lo-1,lo,hi,hi+1,None,'bad',1.5]: add(kind,[value])
 for kind in ['INT','INTEGER','REAL','FLOAT','DOUBLE','TEXT','BLOB']:
  for value in [None,1,1.5,'text',b'blob']: add(kind,[value])
 for kind,values in [('TEXT(3)',['', 'é中🙂','four',1,b'x']),('BLOB(2)',[b'',b'ab',b'abc','text',1]),('DATE',['2024-02-29','bad',None]),('DATETIME',['2024-02-29T23:59Z','2024-02-29T23:59:59Z','2024-02-29T23:59:59.123Z','bad',None])]:
  for value in values: add(kind,[value])
 # Explicit standard corrections to GDAL's permissive prefix regex/SQLite length.
 for kind,value in [('DATE','2024-02-31'),('DATE','2024-01-01tail'),('DATETIME','2024-02-29T24:00Z'),('DATETIME','2024-02-29T23:59:59X123Z'),('DATETIME','2024-02-29T23:59:59.123Ztail'),('TEXT(3)','a\0bc')]:
  add(kind,[value],warning_delta=1)
 # One warning per column regardless of the number of offending rows.
 add('TINYINT',[0,128,129,130])
 # Missing fields in old records acquire the ALTER TABLE constant default.
 add('TEXT',['old'],post="extra BLOB(1) DEFAULT X'0102'")
 add('TEXT',['old'],post="extra TEXT(1) DEFAULT 'long'")
 add('TEXT',['old'],post="extra REAL DEFAULT 1")
 # The standard requires an actual rowid alias; GDAL checks only pk ordinal 1.
 add('TEXT',[],schema='id INTEGER PRIMARY KEY DESC,v TEXT',error_delta=1)
 add('TEXT',[],schema='id INTEGER PRIMARY KEY,v TEXT',without_rowid=True,error_delta=1)
 add('TEXT',[],schema='id INTEGER,v TEXT,PRIMARY KEY(id,v)',error_delta=1)
 add('VARCHAR',['text'])
 for value in [1.1,1e40,-1e40,1e-300]: add('FLOAT',[value],warning_delta=1)
 checker=GPKGChecker(str(path),abort_at_first_error=False,extra_checks=True,warning_msg=False)
 checker._check_user_table_content(db.cursor(),'countries',list(db.execute('PRAGMA table_info(countries)')))
 cases.append(dict(name='countries',warnings=len(checker.warnings),errors=0,warning_delta=0,error_delta=0,features=True))
 db.commit()
 # Independent SQLite rewrites exercise actual UTF-16 physical records, including
 # all table values and the real Natural Earth geometry/attribute payloads.
 objects=list(db.execute('SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL'))
 def quote(name): return '\"'+name.replace('\"','\"\"')+'\"'
 for encoding in ['UTF-16le','UTF-16be']:
  with sqlite3.connect(root/(encoding+'.gpkg')) as other:
   other.execute('PRAGMA encoding=\"'+encoding+'\"')
   for kind,name,sql in objects:
    if kind=='table' and not name.startswith('sqlite_'): other.execute(sql)
   for kind,name,sql in objects:
    if kind!='table': continue
    columns=list(db.execute('PRAGMA table_info('+quote(name)+')'))
    other.execute('DELETE FROM '+quote(name))
    # Bind values rather than SQL text dumps, which truncate TEXT at embedded NUL.
    other.executemany('INSERT INTO '+quote(name)+' VALUES('+','.join('?' for _ in columns)+')',db.execute('SELECT * FROM '+quote(name)))
    assert list(other.execute('SELECT * FROM '+quote(name)))==list(db.execute('SELECT * FROM '+quote(name)))
   for kind,name,sql in objects:
    if kind!='table': other.execute(sql)
print(json.dumps(dict(path=str(path),cases=cases,encodings=[str(root/(encoding+'.gpkg')) for encoding in ['UTF-16le','UTF-16be']])))
"#;
        let output = std::process::Command::new("python3")
            .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .arg("-c")
            .arg(script)
            .arg(dir.path())
            .arg(data)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let oracle: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let cases = oracle["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 100);
        let paths = std::iter::once(oracle["path"].as_str().unwrap())
            .chain(oracle["encodings"].as_array().unwrap().iter().map(|value| value.as_str().unwrap()));
        for path in paths {
            let bytes = std::fs::read(path).unwrap();
            let mut checker = Checker { db: Snapshot::read(&bytes).unwrap(), report: CoreReport::default() };
            for case in cases {
                let name = case["name"].as_str().unwrap();
                assert!(checker.db.tables[name].rows.is_empty(), "user records must not be retained");
                checker.report = CoreReport::default();
                if case["features"] != true {
                    checker.attribute_table(name);
                }
                checker.user_content(&bytes, &[name]).unwrap();
                assert_eq!(
                    checker.report.errors.len() as u64,
                    case["errors"].as_u64().unwrap() + case["error_delta"].as_u64().unwrap(),
                    "{case}: {:?}",
                    checker.report
                );
                assert_eq!(
                    checker.report.warnings.len() as u64,
                    case["warnings"].as_u64().unwrap() + case["warning_delta"].as_u64().unwrap(),
                    "{case}: {:?}",
                    checker.report
                );
            }
            let report = check_core(&bytes);
            let attributes: Vec<_> = cases.iter().filter(|case| case["features"] != true).collect();
            let expected_errors: u64 =
                attributes.iter().map(|case| case["errors"].as_u64().unwrap() + case["error_delta"].as_u64().unwrap()).sum();
            let expected_warnings: u64 =
                attributes.iter().map(|case| case["warnings"].as_u64().unwrap() + case["warning_delta"].as_u64().unwrap()).sum();
            assert_eq!(
                report.errors.iter().filter(|error| matches!(error.requirement.number(), Some(5 | 119))).count() as u64,
                expected_errors
            );
            assert_eq!(report.warnings.len() as u64, expected_warnings);
        }
    }

    #[test]
    fn invalid_header_is_reported_without_parsing() {
        let report = check_core(b"not a database");
        assert_eq!(report.errors[0].requirement, 1);
        assert!(report.errors[0].message.contains("SQLite"));
        assert!(report.version.is_none());
        assert!(report.warnings.is_empty());
    }
}
