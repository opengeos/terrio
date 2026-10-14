//! GeoPackage reading, on terrio's SQLite reader: the feature and attribute tables (layers) and
//! the raster tables of a file, read as GDAL reads them.
//!
//! Layers are the tables `gpkg_contents` lists as `features` (in `gpkg_geometry_columns` order),
//! then those it lists as `attributes`. Fields have the declared column types, as GDAL maps them
//! (`MEDIUMINT`, `INTEGER`, `BOOLEAN`, `REAL`, `TEXT(n)`, `DATE`, `DATETIME`, `BLOB`); the
//! `INTEGER PRIMARY KEY` column gives the feature ids. CRSs come from `gpkg_spatial_ref_sys`:
//! EPSG codes when the organization is EPSG, otherwise the stored definition. The whole file is
//! held in memory, so a GeoPackage reads from bytes as from a file.

use crate::drivers::sqlite::{self, Db, Value};
use crate::{Error, Result};
use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("GeoPackage: {msg}"))
}

/// A table `gpkg_contents` lists.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Content {
    /// The table name.
    pub name: String,
    /// `features`, `attributes`, `tiles` or `2d-gridded-coverage`.
    pub data_type: String,
    /// The description.
    pub description: String,
}

/// The tables of a GeoPackage: vector layers (features, then attributes, in GDAL's order) and
/// rasters (tiles and gridded coverages, in `gpkg_contents` order).
pub(crate) fn contents(db: &Db) -> Result<(Vec<Content>, Vec<Content>)> {
    let t = db.table("gpkg_contents")?;
    let all: Vec<Content> = t
        .rows
        .iter()
        .map(|r| {
            let text = |c: &str| t.get(r, c).and_then(Value::as_str).unwrap_or_default().to_string();
            Content { name: text("table_name"), data_type: text("data_type"), description: text("description") }
        })
        .collect();
    let geometry_columns = db.table("gpkg_geometry_columns").ok();
    let mut features: Vec<Content> = Vec::new();
    if let Some(g) = &geometry_columns {
        for r in &g.rows {
            let name = g.get(r, "table_name").and_then(Value::as_str).unwrap_or_default();
            if let Some(c) = all.iter().find(|c| c.data_type == "features" && c.name.eq_ignore_ascii_case(name))
                && !features.contains(c)
            {
                features.push(c.clone());
            }
        }
    }
    // Feature tables missing from gpkg_geometry_columns, then attribute tables.
    for c in &all {
        if (c.data_type == "features" || c.data_type == "attributes") && !features.contains(c) {
            features.push(c.clone());
        }
    }
    // Tables (not views) only: views need an SQL engine.
    let schema = db.schema()?;
    features.retain(|c| schema.iter().any(|(k, n, _, root, _)| k == "table" && *root > 0 && n.eq_ignore_ascii_case(&c.name)));
    let rasters = all.into_iter().filter(|c| c.data_type == "tiles" || c.data_type == "2d-gridded-coverage").collect();
    Ok((features, rasters))
}

/// The vector layer names of a GeoPackage file.
pub(crate) fn list_layers(path: &str) -> Result<Vec<String>> {
    let db = Db::new(std::fs::read(path)?)?;
    Ok(contents(&db)?.0.into_iter().map(|c| c.name).collect())
}

/// Reads the layers of a GeoPackage file (`wanted` names them; all when empty).
pub(crate) fn read(path: &str, wanted: &[String]) -> Result<Vec<Layer>> {
    parse(std::fs::read(path)?, wanted)
}

/// Reads the layers of a GeoPackage held in memory (`wanted` names them; all when empty).
pub(crate) fn parse(bytes: Vec<u8>, wanted: &[String]) -> Result<Vec<Layer>> {
    read_db(&Db::new(bytes)?, wanted)
}

/// Reads the layers of an open GeoPackage (`wanted` names them; all when empty).
pub(crate) fn read_db(db: &Db, wanted: &[String]) -> Result<Vec<Layer>> {
    let (layers, _) = contents(db)?;
    let names: Vec<&String> = layers.iter().map(|c| &c.name).collect();
    for w in wanted {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(w)) {
            let all: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            return Err(Error::InvalidArgument(format!("no layer named {w} (layers: {})", all.join(", "))));
        }
    }
    layers
        .iter()
        .filter(|c| wanted.is_empty() || wanted.iter().any(|w| w.eq_ignore_ascii_case(&c.name)))
        .map(|c| read_layer(db, c))
        .collect()
}

/// The field type GDAL gives a declared column type.
fn field_type(decl: &str) -> (FieldType, usize) {
    let upper = decl.trim().to_ascii_uppercase();
    let base = upper.split('(').next().unwrap_or_default().trim();
    let width = upper.split('(').nth(1).and_then(|w| w.trim_end_matches(')').trim().parse().ok()).unwrap_or(0);
    let t = match base {
        "INTEGER" | "INT" | "MEDIUMINT" | "SMALLINT" | "TINYINT" | "BIGINT" | "INT8" | "INT2" => FieldType::Integer,
        "BOOLEAN" => FieldType::Boolean,
        "REAL" | "DOUBLE" | "FLOAT" | "NUMERIC" | "DECIMAL" => FieldType::Float,
        "DATE" => FieldType::Date,
        "DATETIME" | "TIMESTAMP" => FieldType::DateTime,
        "BLOB" => FieldType::Blob,
        _ => FieldType::Text,
    };
    (t, if t == FieldType::Text { width } else { 0 })
}

/// Sets a layer's CRS from a `gpkg_spatial_ref_sys` entry: its EPSG code when the organization
/// is EPSG and the registry knows the code, otherwise its definition. The undefined SRSs (-1,
/// 0, GDAL's 99999, or a definition of `undefined`) give none.
fn set_crs(db: &Db, layer: &mut Layer, srs_id: i64) {
    let Ok(t) = db.table("gpkg_spatial_ref_sys") else { return };
    let Some(row) = t.rows.iter().find(|r| t.get(r, "srs_id").and_then(Value::as_i64) == Some(srs_id)) else { return };
    let org = t.get(row, "organization").and_then(Value::as_str).unwrap_or_default();
    // GDAL's undefined SRS.
    if org.eq_ignore_ascii_case("GDAL") && srs_id == 99999 {
        return;
    }
    let def = t.get(row, "definition").and_then(Value::as_str).unwrap_or_default().trim();
    if org.eq_ignore_ascii_case("EPSG")
        && let Some(code) = t.get(row, "organization_coordsys_id").and_then(Value::as_i64).filter(|c| *c > 0)
        && wbprojection::to_ogc_wkt(code as u32).is_ok()
    {
        layer.set_crs_epsg(Some(code as u32));
        return;
    }
    if def.is_empty() || def.eq_ignore_ascii_case("undefined") {
        return;
    }
    let s = crate::crs::SpatialRef::from_wkt(def);
    layer.set_crs_wkt(Some(def.to_string()));
    layer.set_crs_epsg(s.epsg);
}

/// Decodes a GeoPackage geometry blob: the `GP` header, then WKB. Empty geometries give `None`.
pub(crate) fn decode_geometry(b: &[u8]) -> Option<Geometry> {
    if b.len() < 8 || &b[..2] != b"GP" {
        // Some writers store plain WKB.
        return Geometry::from_wkb(b).ok();
    }
    let flags = b[3];
    if flags & 0x10 != 0 {
        return None;
    }
    let envelope = match (flags >> 1) & 0x07 {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        _ => return None,
    };
    let wkb = b.get(8 + envelope..)?;
    let g = Geometry::from_wkb(wkb).ok()?;
    // An empty point is written as NaN coordinates.
    match &g {
        Geometry::Point(c) if c.x.is_nan() && c.y.is_nan() => None,
        _ => Some(g),
    }
}

fn geometry_type(name: &str) -> Option<GeometryType> {
    Some(match name.trim().to_ascii_uppercase().as_str() {
        "POINT" => GeometryType::Point,
        "LINESTRING" => GeometryType::LineString,
        "POLYGON" => GeometryType::Polygon,
        "MULTIPOINT" => GeometryType::MultiPoint,
        "MULTILINESTRING" => GeometryType::MultiLineString,
        "MULTIPOLYGON" => GeometryType::MultiPolygon,
        "GEOMETRYCOLLECTION" => GeometryType::GeometryCollection,
        _ => return None,
    })
}

fn read_layer(db: &Db, c: &Content) -> Result<Layer> {
    let sql = db.table_sql(&c.name).ok_or_else(|| bad(format!("no table '{}'", c.name)))?;
    let columns = sqlite::column_types(&sql);
    let table = db.table(&c.name)?;

    // The geometry column, its type and SRS.
    let mut geom_col = None;
    let mut layer = Layer::new(c.name.clone());
    if let Ok(g) = db.table("gpkg_geometry_columns")
        && let Some(r) =
            g.rows.iter().find(|r| g.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&c.name)))
    {
        let col = g.get(r, "column_name").and_then(Value::as_str).unwrap_or_default();
        geom_col = table.column(col);
        layer.geom_type = g.get(r, "geometry_type_name").and_then(Value::as_str).and_then(geometry_type);
        if let Some(srs_id) = g.get(r, "srs_id").and_then(Value::as_i64) {
            set_crs(db, &mut layer, srs_id);
        }
    }
    // The feature id column: the rowid alias.
    let fid_col = sqlite::rowid_column(&sql);

    let mut kept = Vec::new();
    for (i, (name, decl)) in columns.iter().enumerate() {
        if Some(i) == geom_col || Some(i) == fid_col {
            continue;
        }
        let (t, width) = field_type(decl);
        let mut f = FieldDef::new(name.clone(), t);
        f.width = width;
        layer.add_field(f);
        kept.push((i, t));
    }

    for (n, row) in table.rows.iter().enumerate() {
        let fid = fid_col.and_then(|i| row.get(i)?.as_i64()).unwrap_or(n as i64 + 1);
        let geometry = geom_col.and_then(|i| match row.get(i) {
            Some(Value::Blob(b)) => decode_geometry(b),
            _ => None,
        });
        let attributes = kept
            .iter()
            .map(|&(i, t)| {
                let v = row.get(i).unwrap_or(&Value::Null);
                match (t, v) {
                    (_, Value::Null) => FieldValue::Null,
                    (FieldType::Integer, Value::Int(x)) => FieldValue::Integer(*x),
                    (FieldType::Integer, Value::Real(x)) => FieldValue::Integer(*x as i64),
                    (FieldType::Integer, Value::Text(s)) => s.trim().parse().map_or(FieldValue::Null, FieldValue::Integer),
                    (FieldType::Boolean, v) => v.as_i64().map_or(FieldValue::Null, |x| FieldValue::Boolean(x != 0)),
                    (FieldType::Float, v) => v.as_f64().map_or(FieldValue::Null, FieldValue::Float),
                    (FieldType::Date, Value::Text(s)) => FieldValue::Date(s.clone()),
                    (FieldType::DateTime, Value::Text(s)) => FieldValue::DateTime(s.clone()),
                    (FieldType::Blob, Value::Blob(b)) => FieldValue::Blob(b.clone()),
                    (FieldType::Blob, Value::Text(s)) => FieldValue::Blob(s.as_bytes().to_vec()),
                    (_, Value::Text(s)) => FieldValue::Text(s.clone()),
                    (_, Value::Int(x)) => FieldValue::Text(x.to_string()),
                    (_, Value::Real(x)) => FieldValue::Text(x.to_string()),
                    (_, Value::Blob(b)) => FieldValue::Text(String::from_utf8_lossy(b).into_owned()),
                }
            })
            .collect();
        layer.push(Feature { fid: fid as u64, geometry, attributes });
    }
    Ok(layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_column_types_as_gdal() {
        assert_eq!(field_type("MEDIUMINT"), (FieldType::Integer, 0));
        assert_eq!(field_type("TEXT(10)"), (FieldType::Text, 10));
        assert_eq!(field_type("BOOLEAN"), (FieldType::Boolean, 0));
        assert_eq!(field_type("DOUBLE"), (FieldType::Float, 0));
        assert_eq!(field_type("DATETIME"), (FieldType::DateTime, 0));
    }

    #[test]
    fn reads_what_terrio_writes() {
        use wbvector::Coord;
        let mut l = Layer::new("my \"layer\"").with_epsg(32633);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("n", FieldType::Integer));
        l.add_field(FieldDef::new("ok", FieldType::Boolean));
        for i in 0..3000u64 {
            let g = Geometry::polygon(vec![Coord::xy(i as f64, 0.0), Coord::xy(i as f64 + 1.0, 0.0), Coord::xy(i as f64, 1.0)], vec![]);
            l.push(Feature {
                fid: i,
                geometry: (i % 7 != 0).then_some(g),
                attributes: vec![FieldValue::Text(format!("feature {i}")), FieldValue::Integer(i as i64), FieldValue::Boolean(i % 2 == 0)],
            });
        }
        let bytes = super::super::geopackage::to_bytes(&[l.clone()]).unwrap();
        let back = parse(bytes, &[]).unwrap();
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!(b.name, l.name);
        assert_eq!(b.crs_epsg(), Some(32633));
        assert_eq!(b.len(), 3000);
        for (x, y) in b.features.iter().zip(&l.features) {
            assert_eq!(x.geometry, y.geometry);
            assert_eq!(x.attributes, y.attributes);
        }
        assert_eq!(b.features[10].fid, 11);
    }
}
