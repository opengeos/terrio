//! GeoPackage writing, on terrio's SQLite file writer.
//!
//! Tables are laid out as GDAL lays them out: an `fid INTEGER PRIMARY KEY AUTOINCREMENT` column, a
//! `geom` column declared with the type the features share (`GEOMETRY` when they differ), typed
//! columns (`MEDIUMINT`/`INTEGER`,
//! `REAL`, `TEXT`, `BOOLEAN`, `DATE`, `DATETIME`, `BLOB`) and the layer's CRS in
//! `gpkg_spatial_ref_sys`: its EPSG code when it is known for sure, otherwise its WKT under an
//! organization of `NONE`, and the undefined SRS (-1) without a CRS. No spatial index is written.

use super::sqlite::{self, Table, Value};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use wbvector::{FieldType, FieldValue, Geometry, Layer};

const SRS_SQL: &str = "CREATE TABLE gpkg_spatial_ref_sys (srs_name TEXT NOT NULL, srs_id INTEGER PRIMARY KEY, organization TEXT NOT NULL, organization_coordsys_id INTEGER NOT NULL, definition  TEXT NOT NULL, description TEXT)";
const CONTENTS_SQL: &str = "CREATE TABLE gpkg_contents (table_name TEXT NOT NULL PRIMARY KEY, data_type TEXT NOT NULL, identifier TEXT UNIQUE, description TEXT DEFAULT '', last_change DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE, srs_id INTEGER, CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id))";
const GEOMETRY_COLUMNS_SQL: &str = "CREATE TABLE gpkg_geometry_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL, geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, z TINYINT NOT NULL, m TINYINT NOT NULL, CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name), CONSTRAINT uk_gc_table_name UNIQUE (table_name), CONSTRAINT fk_gc_tn FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name), CONSTRAINT fk_gc_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id))";
const WGS84_WKT: &str = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563,AUTHORITY["EPSG","7030"]],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Latitude",NORTH],AXIS["Longitude",EAST],AUTHORITY["EPSG","4326"]]"#;

/// Writes layers as a GeoPackage.
pub fn write(layers: &[Layer], path: &str) -> Result<()> {
    std::fs::write(path, to_bytes(layers)?)?;
    Ok(())
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The current time in ISO 8601, or the Unix epoch where there is no clock.
fn now() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    #[cfg(target_arch = "wasm32")]
    let ms = 0;
    let s = super::geoparquet::format_datetime(ms - ms % 1000, true);
    s.replace('Z', ".000Z")
}

/// The GeoPackage geometry encoding: the `GP` header with the envelope, then ISO WKB.
fn gpkg_geometry(g: &Geometry, srs_id: i32) -> Vec<u8> {
    let bbox = g.bbox();
    let point = matches!(g, Geometry::Point(_));
    let envelope = if point { 0 } else { bbox.as_ref().map_or(0, |_| 1) };
    let empty = bbox.is_none();
    let mut out = vec![b'G', b'P', 0, 1 | (envelope << 1) | (u8::from(empty) << 4)];
    out.extend_from_slice(&srs_id.to_le_bytes());
    if let (1, Some(b)) = (envelope, &bbox) {
        for v in [b.min_x, b.max_x, b.min_y, b.max_y] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out.extend_from_slice(&g.to_wkb());
    out
}

fn encode(
    layers: &[Layer],
    srs: &mut Vec<Vec<Value>>,
    contents: &mut Vec<Vec<Value>>,
    columns: &mut Vec<Vec<Value>>,
) -> Result<Vec<Table>> {
    let mut tables = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut custom = 100000;
    for layer in layers {
        if names.iter().any(|n| n.eq_ignore_ascii_case(&layer.name)) {
            return Err(Error::InvalidArgument(format!("two layers are named '{}'", layer.name)));
        }
        names.push(layer.name.clone());

        // The CRS.
        let s = SpatialRef::from_layer(layer);
        let srs_id: i64 = match super::trusted_epsg(layer) {
            _ if s.is_unknown() => -1,
            Some(4326) => 4326,
            Some(code) => {
                if !srs.iter().any(|r| r[1] == Value::Null && r[3] == Value::Int(code as i64)) {
                    let def = s.to_wkt().unwrap_or_default();
                    srs.push(vec![
                        Value::Text(s.name().unwrap_or_else(|| format!("EPSG:{code}"))),
                        Value::Null,
                        Value::Text("EPSG".into()),
                        Value::Int(code as i64),
                        Value::Text(def),
                        Value::Null,
                    ]);
                }
                code as i64
            }
            None => {
                custom += 1;
                srs.push(vec![
                    Value::Text(s.name().unwrap_or_else(|| "Unknown".into())),
                    Value::Null,
                    Value::Text("NONE".into()),
                    Value::Int(custom),
                    Value::Text(s.to_wkt().unwrap_or_default()),
                    Value::Null,
                ]);
                custom
            }
        };

        // The geometry type the features share, as GDAL declares it (GEOMETRY when they differ).
        let all_none = layer.features.iter().all(|f| f.geometry.is_none());
        let declared = if all_none { layer.geom_type } else { super::uniform_type(layer) };
        let type_name = declared.map_or("GEOMETRY".to_string(), |t| t.as_str().to_ascii_uppercase());
        let has_z = layer.features.iter().filter_map(|f| f.geometry.as_ref()).any(crate::vector::geometry_has_z);

        // Columns: a field named fid holding unique integers gives the feature ids, as in GDAL.
        let fields = layer.schema.fields();
        let fid_field = fields.iter().position(|f| f.name.eq_ignore_ascii_case("fid") && f.field_type == FieldType::Integer).filter(|&i| {
            let mut ids: Vec<i64> = layer.features.iter().filter_map(|f| f.attributes.get(i)?.as_i64()).collect();
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            n == layer.len() && ids.len() == n
        });
        let mut col_names: Vec<String> = vec!["fid".into(), "geom".into()];
        let mut kept = Vec::new();
        let mut sql =
            format!("CREATE TABLE {} ( \"fid\" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, \"geom\" {type_name}", quote(&layer.name));
        for (i, f) in fields.iter().enumerate() {
            if Some(i) == fid_field {
                continue;
            }
            let mut name = f.name.clone();
            let mut n = 1;
            while col_names.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
                name = format!("{}_{n}", f.name);
                n += 1;
            }
            let t = match f.field_type {
                FieldType::Integer => {
                    let small = layer.features.iter().filter_map(|ft| ft.attributes.get(i)?.as_i64()).all(|v| i32::try_from(v).is_ok());
                    if small { "MEDIUMINT".to_string() } else { "INTEGER".to_string() }
                }
                FieldType::Float => "REAL".into(),
                FieldType::Boolean => "BOOLEAN".into(),
                FieldType::Date => "DATE".into(),
                FieldType::DateTime => "DATETIME".into(),
                FieldType::Blob => "BLOB".into(),
                FieldType::Text if f.width > 0 => format!("TEXT({})", f.width),
                _ => "TEXT".into(),
            };
            sql.push_str(&format!(", {} {t}", quote(&name)));
            col_names.push(name);
            kept.push(i);
        }
        sql.push(')');

        let mut rows = Vec::with_capacity(layer.len());
        let mut bounds: Option<[f64; 4]> = None;
        for (n, f) in layer.features.iter().enumerate() {
            let rowid = fid_field.and_then(|i| f.attributes.get(i)?.as_i64()).unwrap_or(n as i64 + 1);
            let geom = match &f.geometry {
                Some(g) => {
                    if let Some(b) = g.bbox() {
                        bounds = Some(match bounds {
                            None => [b.min_x, b.min_y, b.max_x, b.max_y],
                            Some(a) => [a[0].min(b.min_x), a[1].min(b.min_y), a[2].max(b.max_x), a[3].max(b.max_y)],
                        });
                    }
                    Value::Blob(gpkg_geometry(g, srs_id as i32))
                }
                None => Value::Null,
            };
            let mut values = vec![Value::Null, geom];
            for &i in &kept {
                let t = fields[i].field_type;
                values.push(match f.attributes.get(i).unwrap_or(&FieldValue::Null) {
                    FieldValue::Null => Value::Null,
                    FieldValue::Integer(x) => Value::Int(*x),
                    FieldValue::Float(x) => Value::Real(*x),
                    FieldValue::Boolean(b) => Value::Int(i64::from(*b)),
                    FieldValue::Blob(b) => Value::Blob(b.clone()),
                    FieldValue::DateTime(s) if t == FieldType::DateTime => Value::Text(s.replacen(' ', "T", 1)),
                    FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => Value::Text(s.clone()),
                });
            }
            rows.push((rowid, values));
        }
        rows.sort_by_key(|(id, _)| *id);
        let max_fid = rows.last().map_or(0, |(id, _)| *id);

        let b = bounds.map(|b| b.map(Value::Real)).unwrap_or([Value::Null, Value::Null, Value::Null, Value::Null]);
        let [min_x, min_y, max_x, max_y] = b;
        contents.push(vec![
            Value::Text(layer.name.clone()),
            Value::Text("features".into()),
            Value::Text(layer.name.clone()),
            Value::Text(String::new()),
            Value::Text(now()),
            min_x,
            min_y,
            max_x,
            max_y,
            Value::Int(srs_id),
        ]);
        columns.push(vec![
            Value::Text(layer.name.clone()),
            Value::Text("geom".into()),
            Value::Text(type_name),
            Value::Int(srs_id),
            Value::Int(i64::from(has_z)),
            Value::Int(0),
        ]);
        tables.push((Table { name: layer.name.clone(), sql, rows, autoindexes: vec![] }, max_fid));
    }
    let mut out: Vec<Table> = Vec::new();
    let seq: Vec<(i64, Vec<Value>)> =
        tables.iter().enumerate().map(|(i, (t, max))| (i as i64 + 1, vec![Value::Text(t.name.clone()), Value::Int(*max)])).collect();
    out.extend(tables.into_iter().map(|(t, _)| t));
    out.push(Table {
        name: "sqlite_sequence".into(),
        sql: "CREATE TABLE sqlite_sequence(name,seq)".into(),
        rows: seq,
        autoindexes: vec![],
    });
    Ok(out)
}

/// Encodes layers as a GeoPackage file.
pub fn to_bytes(layers: &[Layer]) -> Result<Vec<u8>> {
    let mut srs = vec![
        vec![
            Value::Text("Undefined Cartesian SRS".into()),
            Value::Null,
            Value::Text("NONE".into()),
            Value::Int(-1),
            Value::Text("undefined".into()),
            Value::Text("undefined Cartesian coordinate reference system".into()),
        ],
        vec![
            Value::Text("Undefined geographic SRS".into()),
            Value::Null,
            Value::Text("NONE".into()),
            Value::Int(0),
            Value::Text("undefined".into()),
            Value::Text("undefined geographic coordinate reference system".into()),
        ],
        vec![
            Value::Text("WGS 84 geodetic".into()),
            Value::Null,
            Value::Text("EPSG".into()),
            Value::Int(4326),
            Value::Text(WGS84_WKT.into()),
            Value::Text("longitude/latitude coordinates in decimal degrees on the WGS 84 spheroid".into()),
        ],
    ];
    let (mut contents, mut columns) = (Vec::new(), Vec::new());
    let layer_tables = encode(layers, &mut srs, &mut contents, &mut columns)?;
    // srs_id is the rowid of gpkg_spatial_ref_sys.
    let mut srs_rows: Vec<(i64, Vec<Value>)> = srs
        .into_iter()
        .map(|r| {
            let id = match &r[3] {
                Value::Int(code) => *code,
                _ => 0,
            };
            (id, r)
        })
        .collect();
    srs_rows.sort_by_key(|(id, _)| *id);
    let mut tables = vec![
        Table { name: "gpkg_spatial_ref_sys".into(), sql: SRS_SQL.into(), rows: srs_rows, autoindexes: vec![] },
        Table {
            name: "gpkg_contents".into(),
            sql: CONTENTS_SQL.into(),
            rows: contents.into_iter().enumerate().map(|(i, r)| (i as i64 + 1, r)).collect(),
            autoindexes: vec![vec![0], vec![2]],
        },
        Table {
            name: "gpkg_geometry_columns".into(),
            sql: GEOMETRY_COLUMNS_SQL.into(),
            rows: columns.into_iter().enumerate().map(|(i, r)| (i as i64 + 1, r)).collect(),
            autoindexes: vec![vec![0, 1], vec![0]],
        },
    ];
    tables.extend(layer_tables);
    // GeoPackage 1.4: application id "GPKG", user version 10400.
    sqlite::write_database(&tables, 10400, 0x4750_4B47)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Coord, Feature, FieldDef};

    #[test]
    fn reads_back_with_wbvector() {
        let mut l = Layer::new("my layer").with_epsg(32633);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("n", FieldType::Integer));
        for i in 0..3000 {
            let g = Geometry::polygon(vec![Coord::xy(i as f64, 0.0), Coord::xy(i as f64 + 1.0, 0.0), Coord::xy(i as f64, 1.0)], vec![]);
            l.push(Feature {
                fid: i,
                geometry: Some(g),
                attributes: vec![FieldValue::Text(format!("feature {i}")), FieldValue::Integer(i as i64)],
            });
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.gpkg");
        let path = path.to_str().unwrap();
        write(&[l.clone()], path).unwrap();
        assert_eq!(wbvector::geopackage::list_layers(path).unwrap(), ["my layer"]);
        let back = wbvector::geopackage::read_layer(path, "my layer").unwrap();
        assert_eq!(back.len(), 3000);
        assert_eq!(back.crs_epsg(), Some(32633));
        assert_eq!(back.features[2999].geometry, l.features[2999].geometry);
    }
}
