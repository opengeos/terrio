// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/gpkg/ogrgeopackagetablelayer.cpp
// No code is translated from these sources.

//! GeoPackage reading, on terrio's SQLite reader: the feature and attribute tables (layers) and
//! the raster tables of a file, read as GDAL reads them.
//!
//! Layers are the tables `gpkg_contents` lists as `features` (in `gpkg_geometry_columns` order),
//! then those it lists as `attributes`. Fields have the declared column types, as GDAL maps them
//! (`MEDIUMINT`, `INTEGER`, `BOOLEAN`, `REAL`, `TEXT(n)`, `DATE`, `DATETIME`, `BLOB`); the
//! `INTEGER PRIMARY KEY` column gives the feature ids. CRSs come from `gpkg_spatial_ref_sys`:
//! EPSG codes when the organization is EPSG, otherwise the stored definition. The database is
//! backed by a consistent mapped file when possible, or by an owned byte input. Feature
//! ranges bound record decoding; spatially filtered ranges count matching features.

use super::wkb::from_wkb;
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

/// What reading layers needs of a database: its schema and its small tables, read whole.
pub(crate) trait Tables {
    /// The schema: `(type, name, table, root page, sql)` of every table, index, view and trigger.
    fn schema(&self) -> Result<Vec<sqlite::SchemaEntry>>;
    /// Reads a whole table by name.
    fn table(&self, name: &str) -> Result<sqlite::Table>;
    /// An optional table may be absent; errors reading a present table must propagate.
    fn optional_table(&self, name: &str) -> Result<Option<sqlite::Table>> {
        if self.schema()?.iter().any(|e| e.0 == "table" && e.1.eq_ignore_ascii_case(name)) { self.table(name).map(Some) } else { Ok(None) }
    }
    /// The payload of the row `rowid` of the table b-tree rooted at `root`.
    fn lookup(&self, root: u32, rowid: i64) -> Result<Option<Vec<u8>>>;
    /// The `CREATE TABLE` statement of a table.
    fn table_sql(&self, name: &str) -> Option<String> {
        self.schema().ok()?.into_iter().find(|(t, n, ..)| t == "table" && n.eq_ignore_ascii_case(name)).map(|r| r.4)
    }
}

impl Tables for Db {
    fn schema(&self) -> Result<Vec<sqlite::SchemaEntry>> {
        Db::schema(self)
    }
    fn table(&self, name: &str) -> Result<sqlite::Table> {
        Db::table(self, name)
    }
    fn lookup(&self, root: u32, rowid: i64) -> Result<Option<Vec<u8>>> {
        Db::lookup(self, root, rowid)
    }
    fn table_sql(&self, name: &str) -> Option<String> {
        Db::table_sql(self, name)
    }
}

/// The tables of a GeoPackage: vector layers (features, then attributes, in GDAL's order) and
/// rasters (tiles and gridded coverages, in `gpkg_contents` order).
pub(crate) fn contents(db: &impl Tables) -> Result<(Vec<Content>, Vec<Content>)> {
    let t = db.table("gpkg_contents")?;
    let all: Vec<Content> = t
        .rows
        .iter()
        .map(|r| {
            let text = |c: &str| t.get(r, c).and_then(Value::as_str).unwrap_or_default().to_string();
            Content { name: text("table_name"), data_type: text("data_type"), description: text("description") }
        })
        .collect();
    let geometry_columns = db.optional_table("gpkg_geometry_columns")?;
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
    let db = crate::drivers::sqlite_edit::open_consistent(std::path::Path::new(path))?;
    Ok(contents(&db)?.0.into_iter().map(|c| c.name).collect())
}

/// A rectangle `(min_x, min_y, max_x, max_y)`.
pub(crate) type Rect = (f64, f64, f64, f64);

/// Read the selected layers of a GeoPackage file, applying spatial filtering before paging.
pub(crate) fn read(path: &str, options: &crate::VectorOpenOptions) -> Result<Vec<Layer>> {
    read_db_named(&crate::drivers::sqlite_edit::open_consistent(std::path::Path::new(path))?, options, Some(path))
}

/// Reads the layers of a GeoPackage held in memory (`wanted` names them; all when empty).
pub(crate) fn parse(bytes: Vec<u8>, wanted: &[String]) -> Result<Vec<Layer>> {
    read_db(&Db::new(bytes)?, wanted, None)
}

/// Reads the layers of an open GeoPackage (`wanted` names them; all when empty). With `bbox`,
/// only the features whose bounding box intersects it, found through the layer's R-tree index
/// when it has one, as GDAL's spatial filters do.
pub(crate) fn read_db(db: &Db, wanted: &[String], bbox: Option<Rect>) -> Result<Vec<Layer>> {
    read_db_named(db, &crate::VectorOpenOptions { layers: wanted.to_vec(), spatial_filter: bbox, ..Default::default() }, None)
}

/// Reads selected tables, also accepting the single-table file-stem alias.
pub(crate) fn read_db_named(db: &Db, options: &crate::VectorOpenOptions, path: Option<&str>) -> Result<Vec<Layer>> {
    let (layers, _) = contents(db)?;
    let names: Vec<&str> = layers.iter().map(|c| c.name.as_str()).collect();
    let selected = crate::dataset::layer_selection(&options.layers, &names, path, true)?;
    layers.iter().zip(selected).filter_map(|(c, keep)| keep.then_some(c)).map(|c| read_layer(db, c, options)).collect()
}

/// The terrio field type of a declared column type.
pub(crate) fn declared_field_type(decl: &str) -> FieldType {
    field_type(decl).0
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
    // GDAL reads INTEGER columns as 64-bit integers, MEDIUMINT ones as 32-bit integers: the
    // former declare a width of 64-bit integer fields, which writers keep.
    let width = match (t, base) {
        (FieldType::Text, _) => width,
        (FieldType::Integer, "INTEGER" | "INT" | "BIGINT" | "INT8") => crate::ops::vector_schema::INTEGER64_WIDTH,
        _ => 0,
    };
    (t, width)
}

/// A column of a layer's table as declared: its name, declared type, `NOT NULL` and `UNIQUE`
/// constraints and default value.
#[derive(Debug, Clone)]
pub(crate) struct DeclaredColumn {
    pub name: String,
    pub decl: String,
    pub not_null: bool,
    pub unique: bool,
    pub default: Option<String>,
}

/// A layer as its GeoPackage tables declare it: what GDAL's `OGR_SCHEMA` reports.
#[derive(Debug, Clone)]
pub(crate) struct DeclaredLayer {
    /// The layer without features, with its CRS and geometry type.
    pub layer: Layer,
    /// The geometry column: its name, `geometry_type_name`, `z` and `m` flags and whether it is
    /// `NOT NULL`.
    pub geometry: Option<(String, String, i64, i64, bool)>,
    /// The `INTEGER PRIMARY KEY` column.
    pub fid: Option<String>,
    /// The other columns.
    pub columns: Vec<DeclaredColumn>,
}

/// The layers of a GeoPackage file as their tables declare them (`wanted` names them; all when
/// empty).
pub(crate) fn declared_layers(path: &str, wanted: &[String]) -> Result<Vec<DeclaredLayer>> {
    let db = crate::drivers::sqlite_edit::open_consistent(std::path::Path::new(path))?;
    let (layers, _) = contents(&db)?;
    let names: Vec<&str> = layers.iter().map(|c| c.name.as_str()).collect();
    let selected = crate::dataset::layer_selection(wanted, &names, Some(path), true)?;
    let mut out = Vec::new();
    for c in layers.iter().zip(selected).filter_map(|(c, keep)| keep.then_some(c)) {
        let sql = db.table_sql(&c.name).ok_or_else(|| bad(format!("no table '{}'", c.name)))?;
        let types = sqlite::column_types(&sql);
        let constraints = sqlite::column_constraints(&sql);
        let defaults = sqlite::column_defaults(&sql);
        let mut layer = Layer::new(c.name.clone());
        let mut geometry = None;
        if let Some(g) = db.optional_table("gpkg_geometry_columns")?
            && let Some(r) =
                g.rows.iter().find(|r| g.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&c.name)))
        {
            let col = g.get(r, "column_name").and_then(Value::as_str).unwrap_or_default().to_string();
            let type_name = g.get(r, "geometry_type_name").and_then(Value::as_str).unwrap_or("GEOMETRY").to_string();
            layer.geom_type = geometry_type(&type_name);
            if let Some(srs_id) = g.get(r, "srs_id").and_then(Value::as_i64) {
                set_crs(&db, &mut layer, srs_id)?;
            }
            let z = g.get(r, "z").and_then(Value::as_i64).unwrap_or(0);
            let m = g.get(r, "m").and_then(Value::as_i64).unwrap_or(0);
            let not_null =
                types.iter().position(|(n, _)| n.eq_ignore_ascii_case(&col)).is_some_and(|i| constraints.get(i).is_some_and(|c| c.0));
            geometry = Some((col, type_name, z, m, not_null));
        }
        let fid_col = sqlite::rowid_column(&sql);
        let mut columns = Vec::new();
        for (i, (name, decl)) in types.iter().enumerate() {
            if Some(i) == fid_col || geometry.as_ref().is_some_and(|g| g.0.eq_ignore_ascii_case(name)) {
                continue;
            }
            let (not_null, unique) = constraints.get(i).copied().unwrap_or_default();
            let default = defaults.get(i).cloned().flatten().and_then(|v| match v {
                Value::Text(s) => Some(format!("'{}'", s.replace('\'', "''"))),
                Value::Int(i) => Some(i.to_string()),
                Value::Real(f) => Some(f.to_string()),
                _ => None,
            });
            let (t, width) = field_type(decl);
            let mut f = FieldDef::new(name.clone(), t);
            f.width = width;
            f.nullable = !not_null;
            layer.add_field(f);
            columns.push(DeclaredColumn { name: name.clone(), decl: decl.clone(), not_null, unique, default });
        }
        let fid = fid_col.and_then(|i| types.get(i)).map(|(n, _)| n.clone());
        out.push(DeclaredLayer { layer, geometry, fid, columns });
    }
    Ok(out)
}

/// Sets a layer's CRS from a `gpkg_spatial_ref_sys` entry: its EPSG code when the organization
/// is EPSG and the registry knows the code, otherwise its definition. The undefined SRSs (-1,
/// 0, GDAL's 99999, or a definition of `undefined`) give none.
fn set_crs(db: &impl Tables, layer: &mut Layer, srs_id: i64) -> Result<()> {
    let Some(t) = db.optional_table("gpkg_spatial_ref_sys")? else { return Ok(()) };
    let Some(row) = t.rows.iter().find(|r| t.get(r, "srs_id").and_then(Value::as_i64) == Some(srs_id)) else { return Ok(()) };
    let org = t.get(row, "organization").and_then(Value::as_str).unwrap_or_default();
    // GDAL's undefined SRS.
    if org.eq_ignore_ascii_case("GDAL") && srs_id == 99999 {
        return Ok(());
    }
    let def = t.get(row, "definition").and_then(Value::as_str).unwrap_or_default().trim();
    if org.eq_ignore_ascii_case("EPSG")
        && let Some(code) = t.get(row, "organization_coordsys_id").and_then(Value::as_i64).filter(|c| *c > 0)
        && wbprojection::to_ogc_wkt(code as u32).is_ok()
    {
        layer.set_crs_epsg(Some(code as u32));
        return Ok(());
    }
    if def.is_empty() || def.eq_ignore_ascii_case("undefined") {
        return Ok(());
    }
    let s = crate::crs::SpatialRef::from_wkt(def);
    layer.set_crs_wkt(Some(def.to_string()));
    layer.set_crs_epsg(s.epsg);
    Ok(())
}

/// Decodes a GeoPackage geometry blob: the `GP` header, then WKB.
/// Empty non-point geometries retain their type; NaN empty points give `None`.
pub(crate) fn decode_geometry(b: &[u8]) -> Option<Geometry> {
    if b.len() < 8 || &b[..2] != b"GP" {
        // Some writers store plain WKB.
        return from_wkb(b);
    }
    let flags = b[3];
    let envelope = match (flags >> 1) & 0x07 {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        _ => return None,
    };
    let wkb = b.get(8 + envelope..)?;
    let g = from_wkb(wkb)?;
    // A flagged-empty blob with nonempty WKB is malformed. Match the envelope
    // reader so unfiltered reads and rebuilt spatial indexes agree.
    if flags & 0x10 != 0 && crate::vector::coord_count(&g) != 0 {
        return None;
    }
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

/// The feature ids the R-tree index `rtree` finds in `bbox`: the ids of the leaf entries whose
/// box intersects it, ascending. `None` when the index (its `_node` shadow table) is missing.
pub(crate) fn rtree_candidates(db: &impl Tables, rtree: &str, bbox: Rect) -> Result<Option<Vec<i64>>> {
    let node_table = format!("{rtree}_node");
    let Some(root) = db.schema()?.into_iter().find(|e| e.0 == "table" && e.1.eq_ignore_ascii_case(&node_table) && e.3 > 0).map(|e| e.3)
    else {
        return Ok(None);
    };
    let node = |n: i64| -> Result<Option<Vec<u8>>> {
        let Some(payload) = db.lookup(root, n)? else { return Ok(None) };
        // Node records hold an integer and a blob, never text: no text encoding applies.
        let mut values = crate::drivers::sqlite_repack::decode_record(&payload, 1)?;
        Ok(match values.get_mut(1) {
            Some(Value::Blob(b)) => Some(std::mem::take(b)),
            _ => return Err(bad(format!("R-tree '{rtree}' node is not a blob"))),
        })
    };
    let (x0, y0, x1, y1) = bbox;
    let mut out = Vec::new();
    // Node 1 is the root; its first two bytes give the depth of the tree.
    let Some(root_node) = node(1)?.filter(|n| n.len() >= 4) else { return Ok(None) };
    let depth = u16::from_be_bytes([root_node[0], root_node[1]]) as usize;
    let mut stack: Vec<(Vec<u8>, usize)> = vec![(root_node, 0)];
    while let Some((data, level)) = stack.pop() {
        let count = data.get(2..4).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(|| bad("truncated R-tree node"))?;
        for i in 0..count {
            let c = 4 + i * 24;
            let cell = data.get(c..c + 24).ok_or_else(|| bad("truncated R-tree node cells"))?;
            let id = i64::from_be_bytes(cell[..8].try_into().unwrap_or_default());
            let f = |k: usize| f64::from(f32::from_be_bytes(cell[8 + 4 * k..12 + 4 * k].try_into().unwrap_or_default()));
            let (minx, maxx, miny, maxy) = (f(0), f(1), f(2), f(3));
            if minx > x1 || maxx < x0 || miny > y1 || maxy < y0 {
                continue;
            }
            if level == depth {
                out.push(id);
            } else {
                let child = node(id)?.ok_or_else(|| bad("missing R-tree child node"))?;
                stack.push((child, level + 1));
            }
        }
        if stack.len() > 1_000_000 {
            return Err(bad(format!("R-tree '{rtree}' is malformed")));
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(Some(out))
}

/// Whether a geometry's bounding box intersects a rectangle.
pub(crate) fn bbox_intersects(g: &Geometry, (x0, y0, x1, y1): Rect) -> bool {
    g.bbox().is_some_and(|b| b.min_x <= x1 && b.max_x >= x0 && b.min_y <= y1 && b.max_y >= y0)
}

/// How the rows of a table make the features of a layer.
pub(crate) struct LayerDef {
    /// The layer, without its features.
    pub layer: Layer,
    /// The geometry column's name.
    pub geom_name: Option<String>,
    /// The index of the geometry column.
    pub geom_col: Option<usize>,
    /// The index of the rowid alias column (the feature ids).
    pub fid_col: Option<usize>,
    /// The columns that are fields, with their types.
    kept: Vec<(usize, FieldType)>,
}

/// How the rows of the table `c` make the features of a layer.
pub(crate) fn layer_def(db: &impl Tables, c: &Content) -> Result<LayerDef> {
    let sql = db.table_sql(&c.name).ok_or_else(|| bad(format!("no table '{}'", c.name)))?;
    let columns = sqlite::column_types(&sql);

    // The geometry column, its type and SRS.
    let mut geom_name: Option<String> = None;
    let mut layer = Layer::new(c.name.clone());
    if let Some(g) = db.optional_table("gpkg_geometry_columns")?
        && let Some(r) =
            g.rows.iter().find(|r| g.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&c.name)))
    {
        let col = g.get(r, "column_name").and_then(Value::as_str).unwrap_or_default();
        geom_name = Some(col.to_string());
        layer.geom_type = g.get(r, "geometry_type_name").and_then(Value::as_str).and_then(geometry_type);
        if let Some(srs_id) = g.get(r, "srs_id").and_then(Value::as_i64) {
            set_crs(db, &mut layer, srs_id)?;
        }
    }
    let fid_col = sqlite::rowid_column(&sql);
    let geom_col = geom_name.as_deref().and_then(|g| columns.iter().position(|(n, _)| n.eq_ignore_ascii_case(g)));
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
    Ok(LayerDef { layer, geom_name, geom_col, fid_col, kept })
}

impl LayerDef {
    /// The feature of a row (the `n`-th, from 0, numbering a table without a rowid alias), or
    /// `None` when `bbox` is given and the geometry's bounding box does not intersect it.
    pub(crate) fn feature(&self, row: &[Value], n: i64, bbox: Option<Rect>) -> Option<Feature> {
        let fid = self.fid_col.and_then(|i| row.get(i)?.as_i64()).unwrap_or(n + 1);
        let geometry = self.geom_col.and_then(|i| match row.get(i) {
            Some(Value::Blob(b)) => decode_geometry(b),
            _ => None,
        });
        // The exact test of the boxes the R-tree (in single precision) or the scan found.
        if let (Some(rect), Some(_)) = (bbox, self.geom_col)
            && !geometry.as_ref().is_some_and(|g| bbox_intersects(g, rect))
        {
            return None;
        }
        let attributes = self
            .kept
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
        Some(Feature { fid: fid as u64, geometry, attributes })
    }
}

fn read_layer(db: &Db, c: &Content, options: &crate::VectorOpenOptions) -> Result<Layer> {
    let def = layer_def(db, c)?;
    let bbox = options.spatial_filter;
    // The rows: with a filter on a spatial table, those its R-tree finds (by lookups of their
    // feature ids, the rowid alias), otherwise all of them.
    let candidates = match (&def.geom_name, bbox, def.fid_col) {
        _ if options.max_features == Some(0) => None,
        (Some(g), Some(rect), Some(_)) => rtree_candidates(db, &format!("rtree_{}_{g}", c.name), rect)?,
        _ => None,
    };
    if options.has_range() {
        let mut layer = def.layer.clone();
        let filtered = bbox.is_some() && def.geom_col.is_some();
        let skip = if filtered { 0 } else { options.skip_features };
        let limit = if filtered && options.max_features != Some(0) { None } else { options.max_features };
        let mut matching_skip = if filtered { options.skip_features } else { 0 };
        db.visit_table_rows(&c.name, candidates.as_deref(), skip, limit, &mut |ordinal, row| {
            if let (Some(rect), Some(column)) = (bbox, def.geom_col) {
                let geometry = row.blob(column)?.and_then(decode_geometry);
                if !geometry.as_ref().is_some_and(|g| bbox_intersects(g, rect)) {
                    return Ok(true);
                }
                if matching_skip > 0 {
                    matching_skip -= 1;
                    return Ok(true);
                }
            }
            if let Some(feature) = def.feature(&row.values()?, ordinal as i64, None) {
                layer.push(feature);
            }
            Ok(options.max_features.is_none_or(|m| layer.len() < m))
        })?;
        // Match keep_range: do not retain an extent describing the unpaged source.
        layer.extent = None;
        return Ok(layer);
    }
    let table = match &candidates {
        Some(ids) => db.table_rows(&c.name, ids)?,
        None => db.table(&c.name)?,
    };
    let mut layer = def.layer.clone();
    // Rows are decoded in parallel, and kept in order.
    let features = crate::par::map_range(table.rows.len(), |n| def.feature(&table.rows[n], n as i64, bbox));
    for f in features.into_iter().flatten() {
        layer.push(f);
    }
    Ok(layer)
}

/// A vector layer summarized without reading its attributes: the layer without its features
/// (name, declared geometry type, CRS and fields, as [`read`] gives them), its feature count, its
/// geometry type name and its extent.
pub(crate) struct LayerSummary {
    pub layer: Layer,
    pub feature_count: usize,
    pub geometry_type: String,
    pub extent: Option<(f64, f64, f64, f64)>,
}

/// Summarizes the vector layers of a GeoPackage file, as `terrio info` reports them, decoding only
/// the geometries (in parallel) of each table's rows. `None` when it has no vector layers.
/// Also gives the raster tables.
pub(crate) fn summarize(path: &str) -> Result<Option<(Vec<LayerSummary>, Vec<Content>)>> {
    crate::crs::prefetch();
    let db = crate::drivers::sqlite_edit::open_consistent(std::path::Path::new(path))?;
    let (layers, rasters) = contents(&db)?;
    if layers.is_empty() {
        return Ok(None);
    }
    let layers = layers.iter().map(|c| summarize_layer(&db, c, path)).collect::<Result<Vec<_>>>()?;
    Ok(Some((layers, rasters)))
}

/// The feature count GDAL keeps for a table in `gpkg_ogr_contents`, when the file has one.
fn stored_feature_count(db: &Db, table: &str) -> Result<Option<usize>> {
    let Some(t) = db.optional_table("gpkg_ogr_contents")? else { return Ok(None) };
    Ok(t.rows
        .iter()
        .find(|r| t.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(table)))
        .and_then(|r| usize::try_from(t.get(r, "feature_count")?.as_i64()?).ok()))
}

/// A table's extent in `gpkg_contents`, when all four bounds are set.
fn stored_extent(db: &Db, table: &str) -> Result<Option<(f64, f64, f64, f64)>> {
    let Some(t) = db.optional_table("gpkg_contents")? else { return Ok(None) };
    let extent = || {
        let r = t.rows.iter().find(|r| t.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(table)))?;
        let v = |name: &str| t.get(r, name).and_then(Value::as_f64).filter(|v| v.is_finite());
        Some((v("min_x")?, v("min_y")?, v("max_x")?, v("max_y")?))
    };
    Ok(extent())
}

/// What a layer summary needs of a geometry: its type, whether it has Z coordinates, its bounds.
type GeometrySummary = (GeometryType, bool, Option<(f64, f64, f64, f64)>);

fn summarize_layer(db: &Db, c: &Content, path: &str) -> Result<LayerSummary> {
    let sql = db.table_sql(&c.name).ok_or_else(|| bad(format!("no table '{}'", c.name)))?;
    let columns = sqlite::column_types(&sql);

    // The geometry column, its type and SRS, and the fields, as `read_layer` reads them.
    let mut geom_col = None;
    // The `z` flag of the geometry column: 0 prohibited, 1 mandatory, 2 optional.
    let mut z_flag = None;
    let mut layer = Layer::new(c.name.clone());
    if let Some(g) = db.optional_table("gpkg_geometry_columns")?
        && let Some(r) =
            g.rows.iter().find(|r| g.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&c.name)))
    {
        let col = g.get(r, "column_name").and_then(Value::as_str).unwrap_or_default();
        geom_col = columns.iter().position(|(n, _)| n.eq_ignore_ascii_case(col));
        layer.geom_type = g.get(r, "geometry_type_name").and_then(Value::as_str).and_then(geometry_type);
        z_flag = g.get(r, "z").and_then(Value::as_i64);
        if let Some(srs_id) = g.get(r, "srs_id").and_then(Value::as_i64) {
            set_crs(db, &mut layer, srs_id)?;
        }
    }
    let fid_col = sqlite::rowid_column(&sql);
    for (i, (name, decl)) in columns.iter().enumerate() {
        if Some(i) == geom_col || Some(i) == fid_col {
            continue;
        }
        let (t, width) = field_type(decl);
        let mut f = FieldDef::new(name.clone(), t);
        f.width = width;
        layer.add_field(f);
    }
    let layer = crate::dataset::fix_layer(layer, path, crate::formats::VectorDriver::GPKG);

    // As `ogrinfo -so` does: the feature count GDAL keeps in `gpkg_ogr_contents`, the extent of
    // `gpkg_contents` and the declared geometry type, without reading the features. Each falls
    // back to the features when the file doesn't have it.
    let stored_count = stored_feature_count(db, &c.name)?;
    let stored_extent = stored_extent(db, &c.name)?;
    let declared = layer.geom_type.filter(|_| matches!(z_flag, Some(0 | 1)));
    if let (Some(feature_count), Some(t)) = (stored_count, declared)
        && (stored_extent.is_some() || feature_count == 0)
    {
        let geometry_type = crate::vector::geometry_type_name_of(Some(t), std::iter::empty(), z_flag == Some(1));
        return Ok(LayerSummary { layer, feature_count, geometry_type, extent: stored_extent });
    }
    let rows = db.raw_rows(&c.name)?;

    // Each geometry's type, Z flag and bounds.
    // A record whose header is incomplete or whose values run past its end is an error: every
    // record's layout is checked, with or without a geometry column. Bytes after the last value
    // are not checked here, though reading the layer reports them.
    let geometries = crate::par::map_range(rows.len(), |i| -> Result<Option<GeometrySummary>> {
        let Some(geom_col) = geom_col else { return Db::check_record(&rows[i].1).map(|()| None) };
        let Value::Blob(b) = Db::record_column(&rows[i].1, geom_col)? else { return Ok(None) };
        Ok(decode_geometry(&b).map(|g| {
            let bbox = g.bbox().map(|b| (b.min_x, b.min_y, b.max_x, b.max_y));
            (g.geom_type(), crate::vector::geometry_has_z(&g), bbox)
        }))
    });
    let geometries = geometries.into_iter().collect::<Result<Vec<_>>>()?;
    let present = || geometries.iter().flatten();
    let geometry_type = crate::vector::geometry_type_name_of(layer.geom_type, present().map(|g| g.0), present().take(100).any(|g| g.1));
    let extent = crate::vector::extent_of(present().filter_map(|g| g.2));
    Ok(LayerSummary { layer, feature_count: rows.len(), geometry_type, extent })
}

/// A read-only index layer with an open database, parsed schema and feature definition.
/// Repeated spatial queries visit only R-tree nodes and matching feature rows.
pub(crate) struct IndexedLayer {
    db: Db,
    schema: Vec<sqlite::SchemaEntry>,
    def: LayerDef,
    root: u32,
    columns: usize,
    rtree: Option<String>,
}
impl IndexedLayer {
    pub(crate) fn new(db: Db, content: &Content) -> Result<Self> {
        let schema = db.schema()?;
        let entry = schema
            .iter()
            .find(|e| e.0 == "table" && e.1.eq_ignore_ascii_case(&content.name))
            .ok_or_else(|| bad("index table is missing"))?;
        let root = entry.3;
        let columns = sqlite::column_types(&entry.4).len();
        let def = layer_def(&db, content)?;
        let rtree = def.geom_name.as_ref().map(|g| format!("rtree_{}_{g}", content.name));
        Ok(Self { db, schema, def, root, columns, rtree })
    }
    pub(crate) fn layer(&self) -> &Layer {
        &self.def.layer
    }
    fn feature(&self, rowid: i64, mut row: Vec<Value>, bbox: Option<Rect>) -> Option<Feature> {
        row.resize(self.columns.max(row.len()), Value::Null);
        if let Some(column) = self.def.fid_col
            && row[column] == Value::Null
        {
            row[column] = Value::Int(rowid);
        }
        self.def.feature(&row, rowid.saturating_sub(1), bbox)
    }
    pub(crate) fn first(&self) -> Result<Layer> {
        let mut layer = self.def.layer.clone();
        if let Some((rowid, row)) = self.db.first_row(self.root)?
            && let Some(feature) = self.feature(rowid, row, None)
        {
            layer.push(feature);
        }
        Ok(layer)
    }
    pub(crate) fn query(&self, bbox: Option<Rect>) -> Result<Layer> {
        let mut layer = self.def.layer.clone();
        let candidates = if let (Some(rtree), Some(bbox), Some(_)) = (&self.rtree, bbox, self.def.fid_col) {
            rtree_candidates(self, rtree, bbox)?
        } else {
            None
        };
        if let Some(ids) = candidates {
            for rowid in ids {
                if let Some(payload) = self.db.lookup(self.root, rowid)?
                    && let Some(feature) = self.feature(rowid, self.db.decode_record(&payload)?, bbox)
                {
                    layer.push(feature);
                }
            }
        } else {
            for (rowid, row) in self.db.rows(self.root)? {
                if let Some(feature) = self.feature(rowid, row, bbox) {
                    layer.push(feature);
                }
            }
        }
        Ok(layer)
    }
}
impl Tables for IndexedLayer {
    fn schema(&self) -> Result<Vec<sqlite::SchemaEntry>> {
        Ok(self.schema.clone())
    }
    fn table(&self, name: &str) -> Result<sqlite::Table> {
        self.db.table(name)
    }
    fn lookup(&self, root: u32, rowid: i64) -> Result<Option<Vec<u8>>> {
        self.db.lookup(root, rowid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::Ring;

    /// GDAL 3.13.3 feature order, schema and CRS remain unchanged by native paging.
    #[test]
    fn feature_pages_keep_layer_metadata_and_filtered_order() {
        let mut primary = Layer::new("Primary").with_epsg(32633);
        primary.add_field(FieldDef::new("number", FieldType::Integer));
        primary.add_field(FieldDef::new("name", FieldType::Text));
        for i in 0..1000 {
            primary.push(Feature {
                fid: i,
                geometry: (i % 11 != 0).then(|| Geometry::point(i as f64, 0.0)),
                attributes: vec![FieldValue::Integer(i as i64), FieldValue::Text(format!("row {i} {}", "x".repeat(120)))],
            });
        }
        let mut secondary = primary.clone();
        secondary.name = "secondary".into();
        let bytes = super::super::geopackage::to_bytes(&[primary, secondary]).unwrap();
        let db = Db::new(bytes.clone()).unwrap();
        assert!(rtree_candidates(&db, "rtree_Primary_geom", (100., -1., 200., 1.)).unwrap().is_some());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pages.gpkg");
        std::fs::write(&path, &bytes).unwrap();
        let all = crate::open_vector(path.to_str().unwrap(), &Default::default()).unwrap();
        for layers in [Vec::new(), vec!["PRIMARY".into()], vec!["secondary".into()]] {
            for (skip, maximum, bbox) in [
                (0, Some(0), None),
                (3, Some(4), None),
                (998, None, None),
                (usize::MAX, Some(3), None),
                (3, Some(4), Some((100., -1., 200., 1.))),
            ] {
                let options = crate::VectorOpenOptions {
                    layers: layers.clone(),
                    skip_features: skip,
                    max_features: maximum,
                    spatial_filter: bbox,
                    ..Default::default()
                };
                let mut expected = all.layers.clone();
                expected.retain(|l| layers.is_empty() || layers.iter().any(|n| n.eq_ignore_ascii_case(&l.name)));
                for layer in &mut expected {
                    if let Some(rect) = bbox {
                        layer.features.retain(|f| f.geometry.as_ref().is_some_and(|g| bbox_intersects(g, rect)));
                    }
                    options.keep_range(layer);
                }
                let direct = match crate::drivers::gpkg::open_bytes(bytes.clone(), "pages.gpkg", &options).unwrap() {
                    crate::Dataset::Vector(v) => v,
                    _ => panic!("expected vector dataset"),
                };
                for actual in [
                    crate::open_vector(path.to_str().unwrap(), &options).unwrap(),
                    crate::memory::open_vector_bytes(bytes.clone(), "pages.gpkg", &options).unwrap(),
                    direct,
                ] {
                    assert_eq!(actual.layers.len(), expected.len());
                    for (a, e) in actual.layers.iter().zip(&expected) {
                        assert_eq!(a.name, e.name);
                        assert_eq!(a.geom_type, e.geom_type);
                        assert_eq!(a.crs_epsg(), e.crs_epsg());
                        assert_eq!(a.crs_wkt(), e.crs_wkt());
                        assert_eq!(format!("{:?}", a.schema.fields()), format!("{:?}", e.schema.fields()));
                        assert_eq!(a.features.len(), e.features.len());
                        for (a, e) in a.features.iter().zip(&e.features) {
                            assert_eq!(a.fid, e.fid);
                            assert_eq!(a.geometry, e.geometry);
                            assert_eq!(a.attributes, e.attributes);
                        }
                        assert!(a.extent.is_none());
                    }
                }
            }
        }
    }

    /// Without an R-tree, count only matching geometries before skipping features.
    #[test]
    fn spatial_pages_decode_only_selected_attributes_with_or_without_an_index() {
        use super::super::sqlite as writer;
        let table = |name: &str, sql: &str, rows| writer::Table { name: name.into(), sql: sql.into(), rows, autoindexes: vec![] };
        let contents = table(
            "gpkg_contents",
            "CREATE TABLE gpkg_contents(table_name TEXT,data_type TEXT,description TEXT)",
            vec![(
                1,
                vec![writer::Value::Text("points".into()), writer::Value::Text("features".into()), writer::Value::Text(String::new())],
            )],
        );
        let geometry = table(
            "gpkg_geometry_columns",
            "CREATE TABLE gpkg_geometry_columns(table_name TEXT,column_name TEXT,geometry_type_name TEXT)",
            vec![(1, vec![writer::Value::Text("points".into()), writer::Value::Text("geom".into()), writer::Value::Text("POINT".into())])],
        );
        let rows = (0..16)
            .map(|i| {
                (
                    i * 3 + 10,
                    vec![
                        writer::Value::Null,
                        if [5, 8].contains(&i) {
                            writer::Value::Null
                        } else {
                            writer::Value::Blob(super::super::geopackage::gpkg_geometry(&Geometry::point(i as f64, 0.), 4326))
                        },
                        writer::Value::Text(format!("p{i}")),
                    ],
                )
            })
            .collect();
        let points = table("points", "CREATE TABLE points(fid INTEGER PRIMARY KEY,geom POINT,name TEXT)", rows);
        let mut tables = vec![contents, geometry, points];
        for indexed in [false, true] {
            if indexed {
                // Deliberately loose boxes include null and nonmatching candidate geometries.
                let entries = (0..16).map(|i| (i * 3 + 10, [-100., 100., -100., 100.])).collect();
                let (nodes, _, _) = super::super::geopackage::build_rtree(entries);
                tables.push(table(
                    "rtree_points_geom_node",
                    "CREATE TABLE rtree_points_geom_node(nodeno INTEGER PRIMARY KEY,data BLOB)",
                    nodes,
                ));
            }
            let db = Db::new(writer::write_database(&tables, &[], 0, 0).unwrap()).unwrap();
            let options = crate::VectorOpenOptions {
                skip_features: 2,
                max_features: Some(3),
                spatial_filter: Some((3.5, -1., 10.5, 1.)),
                ..Default::default()
            };
            let before = db.decoded_record_count();
            read_db_named(&db, &crate::VectorOpenOptions { max_features: Some(0), ..options.clone() }, None).unwrap();
            // A nonzero spatial request also probes the R-tree schema before visiting records.
            let candidates = rtree_candidates(&db, "rtree_points_geom", options.spatial_filter.unwrap()).unwrap();
            assert_eq!(candidates.is_some(), indexed);
            let metadata = db.decoded_record_count() - before;
            let before = db.decoded_record_count();
            let read = read_db_named(&db, &options, None).unwrap();
            assert_eq!(db.decoded_record_count() - before - metadata, 3, "skipped spatial matches decoded their attributes");
            assert_eq!(
                read[0].features.iter().map(|f| f.attributes.clone()).collect::<Vec<_>>(),
                [vec![FieldValue::Text("p7".into())], vec![FieldValue::Text("p9".into())], vec![FieldValue::Text("p10".into())]]
            );
            assert_eq!(read[0].features.iter().map(|f| f.fid).collect::<Vec<_>>(), [31, 37, 40]);
        }
    }

    /// Decode counts at the native SQLite boundary on an optional real GeoPackage.
    #[test]
    #[ignore = "requires TERRIO_GPKG_PAGING_DATA pointing to a real GeoPackage"]
    fn real_feature_pages_decode_only_requested_rows() {
        let path = std::env::var("TERRIO_GPKG_PAGING_DATA").expect("set TERRIO_GPKG_PAGING_DATA to a real GeoPackage");
        let db = crate::drivers::sqlite_edit::open_consistent(std::path::Path::new(&path)).unwrap();
        let name = contents(&db).unwrap().0.remove(0).name;
        let base = crate::VectorOpenOptions { layers: vec![name.clone()], ..Default::default() };
        let measure = |options: &crate::VectorOpenOptions| {
            let before = db.decoded_record_count();
            let layers = read_db_named(&db, options, Some(&path)).unwrap();
            (layers, db.decoded_record_count() - before)
        };
        // The zero-feature request performs the same metadata reads without decoding rows.
        let (empty, metadata) = measure(&crate::VectorOpenOptions { max_features: Some(0), ..base.clone() });
        assert_eq!(empty[0].len(), 0);
        let (all, full_decodes) = measure(&base);
        assert!(all[0].len() > 10);
        let skip = all[0].len() / 2;
        let (result, page_decodes) = measure(&crate::VectorOpenOptions { skip_features: skip, max_features: Some(3), ..base });
        let full = full_decodes.checked_sub(metadata).unwrap();
        let page = page_decodes.checked_sub(metadata).unwrap();
        assert_eq!(full, all[0].len());
        assert_eq!(page, 3);
        assert_eq!(result[0].len(), page);
        for (actual, expected) in result[0].features.iter().zip(&all[0].features[skip..skip + 3]) {
            assert_eq!(actual.fid, expected.fid);
            assert_eq!(actual.geometry, expected.geometry);
            assert_eq!(actual.attributes, expected.attributes);
        }
        eprintln!("native table {name}: full decoded rows={full}, page decoded rows={page}, skipped={skip}, metadata decodes={metadata}");
    }

    #[test]
    fn malformed_feature_records_fail_native_eager_reads_even_with_gdal_enabled() {
        mod fixtures {
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/sqlite_encodings/fixtures.rs"));
        }
        for (name, bytes) in fixtures::DATABASES.iter().filter(|(name, _)| name.ends_with("-truncated")) {
            let db = Db::new(bytes.to_vec()).unwrap();
            for bbox in [None, Some((5., 5., 15., 15.))] {
                assert!(read_db(&db, &[], bbox).is_err(), "{name}");
            }
        }
    }

    #[test]
    fn malformed_rtree_child_records_fail_eager_and_streamed_queries() {
        use super::super::sqlite as writer;
        let node = |depth: u16, id: i64| {
            let mut bytes = depth.to_be_bytes().to_vec();
            bytes.extend_from_slice(&1u16.to_be_bytes());
            bytes.extend_from_slice(&id.to_be_bytes());
            for bound in [0.0f32, 20.0, 0.0, 20.0] {
                bytes.extend_from_slice(&bound.to_be_bytes());
            }
            bytes
        };
        for corruption in [0, 1, 2] {
            let child = if corruption == 2 { vec![0, 0, 0, 1] } else { node(0, 2) };
            let values = vec![writer::Value::Null, writer::Value::Blob(child)];
            let child_record = writer::record(&values);
            let table = writer::Table {
                name: "rtree_test_node".into(),
                sql: "CREATE TABLE rtree_test_node(nodeno INTEGER PRIMARY KEY,data BLOB)".into(),
                rows: vec![(1, vec![writer::Value::Null, writer::Value::Blob(node(1, 2))]), (2, values)],
                autoindexes: vec![],
            };
            let mut bytes = writer::write_database(&[table], &[], 0, 0).unwrap();
            if corruption == 1 {
                let at = bytes.windows(child_record.len()).position(|b| b == child_record).unwrap();
                // The blob now claims a byte outside the SQLite record, not outside the page.
                bytes[at + child_record[0] as usize - 1] += 2;
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nodes.gpkg");
            std::fs::write(&path, &bytes).unwrap();
            let eager = Db::new(bytes).unwrap();
            let streamed = crate::stream::sqlite::FileDb::open(path.to_str().unwrap()).unwrap();
            let bbox = (5.0, 5.0, 15.0, 15.0);
            let a = rtree_candidates(&eager, "rtree_test", bbox);
            let b = rtree_candidates(&streamed, "rtree_test", bbox);
            if corruption == 0 {
                assert_eq!(a.unwrap(), Some(vec![2]));
                assert_eq!(b.unwrap(), Some(vec![2]));
            } else {
                assert!(a.is_err());
                assert!(b.is_err());
            }
        }
    }

    #[test]
    fn malformed_optional_metadata_is_not_treated_as_absent() {
        use super::super::sqlite as writer;
        for (name, sql, values) in [
            (
                "gpkg_spatial_ref_sys",
                "CREATE TABLE gpkg_spatial_ref_sys(srs_id INTEGER,organization TEXT,organization_coordsys_id INTEGER,definition TEXT)",
                vec![
                    writer::Value::Int(4326),
                    writer::Value::Text("EPSG".into()),
                    writer::Value::Int(4326),
                    writer::Value::Text("broken".into()),
                ],
            ),
            (
                "gpkg_contents",
                "CREATE TABLE gpkg_contents(table_name TEXT,description TEXT)",
                vec![writer::Value::Text("features".into()), writer::Value::Text("broken".into())],
            ),
            (
                "gpkg_geometry_columns",
                "CREATE TABLE gpkg_geometry_columns(table_name TEXT,column_name TEXT,geometry_type_name TEXT,srs_id INTEGER,z INTEGER,m INTEGER,description TEXT)",
                vec![
                    writer::Value::Text("unrelated".into()),
                    writer::Value::Text("geom".into()),
                    writer::Value::Text("POINT".into()),
                    writer::Value::Int(4326),
                    writer::Value::Int(0),
                    writer::Value::Int(0),
                    writer::Value::Text("broken".into()),
                ],
            ),
            (
                "gpkg_ogr_contents",
                "CREATE TABLE gpkg_ogr_contents(table_name TEXT,feature_count INTEGER,description TEXT)",
                vec![writer::Value::Text("features".into()), writer::Value::Int(3), writer::Value::Text("broken".into())],
            ),
        ] {
            let record = writer::record(&values);
            let table = writer::Table { name: name.into(), sql: sql.into(), rows: vec![(1, values)], autoindexes: vec![] };
            let mut tables = vec![table];
            if name == "gpkg_geometry_columns" {
                tables.push(writer::Table {
                    name: "gpkg_contents".into(),
                    sql: "CREATE TABLE gpkg_contents(table_name TEXT,data_type TEXT,description TEXT)".into(),
                    rows: vec![(
                        1,
                        vec![
                            writer::Value::Text("features".into()),
                            writer::Value::Text("features".into()),
                            writer::Value::Text(String::new()),
                        ],
                    )],
                    autoindexes: vec![],
                });
                tables.push(writer::Table {
                    name: "features".into(),
                    sql: "CREATE TABLE features(fid INTEGER PRIMARY KEY,geom POINT)".into(),
                    rows: vec![],
                    autoindexes: vec![],
                });
            }
            let mut bytes = writer::write_database(&tables, &[], 0, 0).unwrap();
            let at = bytes.windows(record.len()).position(|b| b == record).unwrap();
            bytes[at + record[0] as usize - 1] += 2;
            let db = Db::new(bytes.clone()).unwrap();
            assert!(db.optional_table(name).is_err(), "{name}");
            match name {
                "gpkg_spatial_ref_sys" => assert!(set_crs(&db, &mut Layer::new("features"), 4326).is_err()),
                "gpkg_geometry_columns" => {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("malformed.gpkg");
                    std::fs::write(&path, bytes).unwrap();
                    let path = path.to_str().unwrap();
                    let content = Content { name: "features".into(), data_type: "features".into(), description: String::new() };
                    for error in [
                        contents(&db).err().unwrap(),
                        declared_layers(path, &[]).err().unwrap(),
                        layer_def(&db, &content).err().unwrap(),
                        summarize_layer(&db, &content, path).err().unwrap(),
                    ] {
                        assert!(error.to_string().contains("truncated record"), "{error}");
                    }
                }
                "gpkg_contents" => assert!(stored_extent(&db, "features").is_err()),
                _ => assert!(stored_feature_count(&db, "features").is_err()),
            }
        }
        let absent = Db::new(writer::write_database(&[], &[], 0, 0).unwrap()).unwrap();
        assert!(stored_extent(&absent, "features").unwrap().is_none());
        assert!(stored_feature_count(&absent, "features").unwrap().is_none());
        assert!(set_crs(&absent, &mut Layer::new("features"), 4326).is_ok());
    }

    #[test]
    fn empty_flag_with_nonempty_wkb_is_rejected_consistently_with_index_envelopes() {
        let mut blob = super::super::geopackage::gpkg_geometry(&Geometry::point(1., 2.), 4326);
        blob[3] |= 0x10;
        assert!(decode_geometry(&blob).is_none());
        assert!(super::super::gpkg_update::envelope(&blob).is_none());
    }

    #[test]
    fn empty_nonpoint_geometries_keep_their_type_in_geopackage_blobs() {
        for geometry in [
            Geometry::Polygon { exterior: Ring::default(), interiors: vec![] },
            Geometry::MultiPolygon(vec![]),
            Geometry::LineString(vec![]),
            Geometry::MultiLineString(vec![]),
            Geometry::MultiPoint(vec![]),
            Geometry::GeometryCollection(vec![]),
        ] {
            let blob = super::super::geopackage::gpkg_geometry(&geometry, 4326);
            assert_ne!(blob[3] & 0x10, 0, "GeoPackage empty flag");
            assert_eq!(decode_geometry(&blob), Some(geometry));
        }
        let point = Geometry::point(f64::NAN, f64::NAN);
        let blob = super::super::geopackage::gpkg_geometry(&point, 4326);
        assert!(decode_geometry(&blob).is_none());
    }

    #[test]
    fn empty_geometry_summaries_and_geojson_remain_distinct_from_null() {
        let mut layer = Layer::new("empty").with_epsg(4326);
        for (i, geometry) in
            [Some(Geometry::Polygon { exterior: Ring::default(), interiors: vec![] }), Some(Geometry::MultiPolygon(vec![])), None]
                .into_iter()
                .enumerate()
        {
            layer.features.push(Feature { fid: i as u64 + 1, geometry, attributes: vec![] });
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.gpkg");
        std::fs::write(&path, super::super::geopackage::to_bytes(&[layer]).unwrap()).unwrap();
        let path = path.to_str().unwrap();
        let layers = read(path, &Default::default()).unwrap();
        assert!(layers[0].geom_type.is_none(), "mixed polygon types declare GEOMETRY");
        assert!(matches!(&layers[0].features[0].geometry,Some(Geometry::Polygon{exterior,..}) if exterior.0.is_empty()));
        assert!(matches!(&layers[0].features[1].geometry,Some(Geometry::MultiPolygon(parts)) if parts.is_empty()));
        assert!(layers[0].features[2].geometry.is_none());
        let projected = crate::vector::reproject_layer(&layers[0], &crate::crs::SpatialRef::from_epsg(3857).unwrap()).unwrap();
        assert_eq!(projected.crs_epsg(), Some(3857));
        assert_eq!(projected.features.len(), 3);
        assert_eq!(projected.features[0].geometry, layers[0].features[0].geometry);
        assert_eq!(projected.features[1].geometry, layers[0].features[1].geometry);
        assert!(projected.features[2].geometry.is_none());
        let buffered =
            crate::ops::topology::buffer(&layers[0], &crate::ops::topology::BufferOptions { distance: 1.0, ..Default::default() }).unwrap();
        assert_eq!(buffered.features.len(), 3);
        for feature in &buffered.features[..2] {
            assert!(matches!(&feature.geometry, Some(Geometry::MultiPolygon(parts)) if parts.is_empty()));
        }
        assert!(buffered.features[2].geometry.is_none());
        use crate::ops::topology;
        let dissolved = topology::dissolve(&layers[0], &[]).unwrap();
        assert_eq!(dissolved.features.len(), 1);
        assert!(matches!(&dissolved.features[0].geometry, Some(Geometry::MultiPolygon(parts)) if parts.is_empty()));
        let overlay = topology::overlay(&layers[0], &layers[0], topology::OverlayOperation::Intersection).unwrap();
        assert!(overlay.features.is_empty());
        let repaired = topology::make_valid(&layers[0]).unwrap();
        assert_eq!(repaired.features.len(), 3);
        for (a, b) in repaired.features.iter().zip(&layers[0].features) {
            assert_eq!(a.geometry, b.geometry);
        }
        let cleaned = topology::clean(&layers[0], &Default::default()).unwrap();
        assert_eq!(cleaned.features.len(), 3);
        assert!(cleaned.features.iter().all(|f| f.geometry.is_none()));
        let (summaries, _) = summarize(path).unwrap().unwrap();
        assert_eq!(summaries[0].feature_count, 3);
        assert_eq!(summaries[0].geometry_type, "MultiPolygon");
        assert_eq!(summaries[0].geometry_type, crate::vector::geometry_type_name(&layers[0]));
        assert!(summaries[0].extent.is_none());
        let bytes = crate::memory::write_vector_bytes(&layers, "empty.geojson", &Default::default()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["features"][0]["geometry"]["type"], "Polygon");
        assert!(json["features"][0]["geometry"]["coordinates"].as_array().unwrap().iter().all(|ring| ring.as_array().unwrap().is_empty()));
        assert_eq!(json["features"][1]["geometry"]["type"], "MultiPolygon");
        assert!(json["features"][2]["geometry"].is_null());
    }

    #[test]
    fn summarizes_as_reading_does() {
        use wbvector::Coord;
        let mut l = Layer::new("mixed").with_epsg(4326);
        l.add_field(FieldDef::new("name", FieldType::Text));
        for i in 0..500u64 {
            let x = i as f64 * 0.1 - 20.0;
            let g = match i % 4 {
                0 => None,
                1 => Some(Geometry::point(x, -x)),
                2 => Some(Geometry::Point(Coord { x, y: 1.0, z: Some(3.0), m: None })),
                _ => Some(Geometry::line_string(vec![Coord::xy(x, 0.0), Coord::xy(x + 1.0, 2.0)])),
            };
            l.push(Feature { fid: i, geometry: g, attributes: vec![FieldValue::Text(format!("f{i}"))] });
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.gpkg");
        std::fs::write(&path, super::super::geopackage::to_bytes(&[l]).unwrap()).unwrap();
        let path = path.to_str().unwrap();
        let read = crate::dataset::finish_vector(read(path, &Default::default()).unwrap(), path, crate::formats::VectorDriver::GPKG);
        let (summaries, rasters) = summarize(path).unwrap().unwrap();
        assert!(rasters.is_empty());
        assert_eq!(summaries.len(), read.layers.len());
        for (s, l) in summaries.iter().zip(&read.layers) {
            assert_eq!(s.feature_count, l.len());
            assert_eq!(s.geometry_type, crate::vector::geometry_type_name(l));
            assert_eq!(s.extent, crate::vector::layer_extent(l));
            assert_eq!(s.layer.name, l.name);
            assert_eq!(s.layer.geom_type, l.geom_type);
            assert_eq!(s.layer.crs_epsg(), l.crs_epsg());
            assert_eq!(s.layer.crs_wkt(), l.crs_wkt());
            assert_eq!(format!("{:?}", s.layer.schema.fields()), format!("{:?}", l.schema.fields()));
        }
    }

    /// A feature record whose geometry runs past the record is an error for the summary, as
    /// for reading the layer, not a feature without a geometry. The layer's mixed geometry
    /// types send the summary to its feature scan.
    #[test]
    fn summaries_of_truncated_geometry_records_are_errors() {
        let mut l = Layer::new("mixed").with_epsg(4326);
        l.add_field(FieldDef::new("name", FieldType::Text));
        let marker = "MARKER-TRUNCATED";
        l.push(Feature { fid: 1, geometry: Some(Geometry::point(1.0, 2.0)), attributes: vec![FieldValue::Text(marker.into())] });
        l.push(Feature {
            fid: 2,
            geometry: Some(Geometry::line_string(vec![wbvector::Coord::xy(0.0, 0.0), wbvector::Coord::xy(1.0, 1.0)])),
            attributes: vec![FieldValue::Text("b".into())],
        });
        let mut bytes = super::super::geopackage::to_bytes(&[l]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.gpkg");
        std::fs::write(&path, &bytes).unwrap();
        assert!(summarize(path.to_str().unwrap()).is_ok());
        // The record: its header (length, NULL fid, BLOB geometry, TEXT name), the geometry,
        // then the name. The geometry's declared length becomes 57 bytes, past the record.
        let at = bytes.windows(marker.len()).position(|w| w == marker.as_bytes()).unwrap();
        let text = 13 + 2 * marker.len() as u8;
        let header = (at.saturating_sub(200)..at).rev().find(|&h| bytes[h] == 4 && bytes[h + 1] == 0 && bytes[h + 3] == text).unwrap();
        let geometry = (bytes[header + 2] as usize - 12) / 2;
        assert_eq!(header + 4 + geometry, at);
        bytes[header + 2] = 12 + 2 * 57;
        std::fs::write(&path, &bytes).unwrap();
        let error = summarize(path.to_str().unwrap()).err().expect("an error");
        assert!(error.to_string().contains("truncated record"), "{error}");
    }

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
        let db = Db::new(bytes.clone()).unwrap();
        let content = contents(&db).unwrap().0.remove(0);
        let indexed = IndexedLayer::new(db, &content).unwrap();
        let first = indexed.first().unwrap();
        assert_eq!(first.features.len(), 1);
        assert_eq!(first.features[0].fid, 1);
        assert_eq!(first.features[0].attributes, l.features[0].attributes);
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

    #[test]
    fn indexed_layer_reads_utf16_text_through_the_rtree() {
        mod fixtures {
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/sqlite_encodings/fixtures.rs"));
        }
        for (encoding, bytes) in fixtures::DATABASES.iter().filter(|(name, _)| ["UTF-8", "UTF-16le", "UTF-16be"].contains(name)) {
            let db = Db::new(bytes.to_vec()).unwrap();
            let content = contents(&db).unwrap().0.remove(0);
            assert_eq!(content.name, "couches_é水", "{encoding}");
            let indexed = IndexedLayer::new(db, &content).unwrap();
            let text = |layer: &Layer| layer.features.iter().map(|f| (f.fid, f.attributes.clone())).collect::<Vec<_>>();
            let name = |fid| vec![FieldValue::Text(if fid == 2 { "TRUNCATE_ME" } else { "é水😀" }.into())];
            assert_eq!(text(&indexed.first().unwrap()), [(1, name(1))]);
            assert_eq!(text(&indexed.query(Some((5.0, 5.0, 15.0, 15.0))).unwrap()), [(2, name(2))]);
            assert_eq!(text(&indexed.query(None).unwrap()), [(1, name(1)), (2, name(2)), (3, name(3))]);
            let db = Db::new(bytes.to_vec()).unwrap();
            for bbox in [None, Some((5., 5., 15., 15.))] {
                let options = crate::VectorOpenOptions {
                    skip_features: usize::from(bbox.is_none()),
                    max_features: Some(1),
                    spatial_filter: bbox,
                    ..Default::default()
                };
                let page = read_db_named(&db, &options, None).unwrap();
                assert_eq!(text(&page[0]), [(2, name(2))], "paged {encoding}");
            }
        }
    }
}
