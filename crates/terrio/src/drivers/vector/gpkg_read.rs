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
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

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
    let db = crate::drivers::sqlite_edit::open_consistent(std::path::Path::new(path))?;
    Ok(contents(&db)?.0.into_iter().map(|c| c.name).collect())
}

/// A rectangle `(min_x, min_y, max_x, max_y)`.
pub(crate) type Rect = (f64, f64, f64, f64);

/// Reads the layers of a GeoPackage file (`wanted` names them; all when empty), keeping only the
/// features whose bounding box intersects `bbox` when it is given.
pub(crate) fn read(path: &str, wanted: &[String], bbox: Option<Rect>) -> Result<Vec<Layer>> {
    read_db(&crate::drivers::sqlite_edit::open_consistent(std::path::Path::new(path))?, wanted, bbox)
}

/// Reads the layers of a GeoPackage held in memory (`wanted` names them; all when empty).
pub(crate) fn parse(bytes: Vec<u8>, wanted: &[String]) -> Result<Vec<Layer>> {
    read_db(&Db::new(bytes)?, wanted, None)
}

/// Reads the layers of an open GeoPackage (`wanted` names them; all when empty). With `bbox`,
/// only the features whose bounding box intersects it, found through the layer's R-tree index
/// when it has one, as GDAL's spatial filters do.
pub(crate) fn read_db(db: &Db, wanted: &[String], bbox: Option<Rect>) -> Result<Vec<Layer>> {
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
        .map(|c| read_layer(db, c, bbox))
        .collect()
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
    for w in wanted {
        if !layers.iter().any(|c| c.name.eq_ignore_ascii_case(w)) {
            let all: Vec<&str> = layers.iter().map(|c| c.name.as_str()).collect();
            return Err(Error::InvalidArgument(format!("no layer named {w} (layers: {})", all.join(", "))));
        }
    }
    let mut out = Vec::new();
    for c in layers.iter().filter(|c| wanted.is_empty() || wanted.iter().any(|w| w.eq_ignore_ascii_case(&c.name))) {
        let sql = db.table_sql(&c.name).ok_or_else(|| bad(format!("no table '{}'", c.name)))?;
        let types = sqlite::column_types(&sql);
        let constraints = sqlite::column_constraints(&sql);
        let defaults = sqlite::column_defaults(&sql);
        let mut layer = Layer::new(c.name.clone());
        let mut geometry = None;
        if let Ok(g) = db.table("gpkg_geometry_columns")
            && let Some(r) =
                g.rows.iter().find(|r| g.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&c.name)))
        {
            let col = g.get(r, "column_name").and_then(Value::as_str).unwrap_or_default().to_string();
            let type_name = g.get(r, "geometry_type_name").and_then(Value::as_str).unwrap_or("GEOMETRY").to_string();
            layer.geom_type = geometry_type(&type_name);
            if let Some(srs_id) = g.get(r, "srs_id").and_then(Value::as_i64) {
                set_crs(&db, &mut layer, srs_id);
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
fn set_crs(db: &impl Tables, layer: &mut Layer, srs_id: i64) {
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
        return from_wkb(b);
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
    let g = from_wkb(wkb)?;
    // An empty point is written as NaN coordinates.
    match &g {
        Geometry::Point(c) if c.x.is_nan() && c.y.is_nan() => None,
        _ => Some(g),
    }
}

/// Decodes WKB as `Geometry::from_wkb` does (the same geometries, and `None` where it fails),
/// reading from the slice directly and allocating coordinate vectors at their final size.
pub(crate) fn from_wkb(b: &[u8]) -> Option<Geometry> {
    Wkb { b, pos: 0 }.geometry()
}

/// A WKB reader.
struct Wkb<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Wkb<'_> {
    fn bytes<const N: usize>(&mut self) -> Option<[u8; N]> {
        let v = self.b.get(self.pos..self.pos + N)?.try_into().ok()?;
        self.pos += N;
        Some(v)
    }

    fn u32(&mut self, le: bool) -> Option<u32> {
        let b = self.bytes::<4>()?;
        Some(if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn f64(&mut self, le: bool) -> Option<f64> {
        let b = self.bytes::<8>()?;
        Some(if le { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) })
    }

    fn coord(&mut self, le: bool, z: bool, m: bool) -> Option<Coord> {
        let x = self.f64(le)?;
        let y = self.f64(le)?;
        let z = if z { Some(self.f64(le)?) } else { None };
        let m = if m { Some(self.f64(le)?) } else { None };
        Some(Coord { x, y, z, m })
    }

    fn coords(&mut self, le: bool, z: bool, m: bool) -> Option<Vec<Coord>> {
        let n = self.u32(le)? as u64;
        let size = 16 + 8 * (z as u64 + m as u64);
        // Fewer bytes than the coordinates need: reading them would fail.
        if n.checked_mul(size)? > (self.b.len() - self.pos) as u64 {
            return None;
        }
        let mut cs = Vec::with_capacity(n as usize);
        for _ in 0..n {
            cs.push(self.coord(le, z, m)?);
        }
        Some(cs)
    }

    fn ring(&mut self, le: bool, z: bool, m: bool) -> Option<Ring> {
        let mut cs = self.coords(le, z, m)?;
        // The closing point is dropped.
        if cs.len() > 1 && cs.first() == cs.last() {
            cs.pop();
        }
        Some(Ring::new(cs))
    }

    /// The members of a collection, each a WKB geometry, mapped by `f`.
    fn members<T>(&mut self, le: bool, f: impl Fn(Geometry) -> Option<T>) -> Option<Vec<T>> {
        let n = self.u32(le)? as usize;
        // Each member takes at least 5 bytes.
        let mut out = Vec::with_capacity(n.min((self.b.len() - self.pos) / 5));
        for _ in 0..n {
            out.push(f(self.geometry()?)?);
        }
        Some(out)
    }

    fn geometry(&mut self) -> Option<Geometry> {
        let le = self.bytes::<1>()?[0] == 1;
        let raw = self.u32(le)?;
        // ISO and EWKB type variants.
        let (base, z, m) = if raw & 0xFFFF_0000 != 0 {
            (raw & 0x0000_FFFF, raw & 0x8000_0000 != 0, raw & 0x4000_0000 != 0)
        } else if raw > 3000 {
            (raw - 3000, true, true)
        } else if raw > 2000 {
            (raw - 2000, false, true)
        } else if raw > 1000 {
            (raw - 1000, true, false)
        } else {
            (raw, false, false)
        };
        Some(match base {
            1 => Geometry::Point(self.coord(le, z, m)?),
            2 => Geometry::LineString(self.coords(le, z, m)?),
            3 => {
                let n = self.u32(le)? as usize;
                if n == 0 {
                    return Some(Geometry::Polygon { exterior: Ring::default(), interiors: vec![] });
                }
                let exterior = self.ring(le, z, m)?;
                let mut interiors = Vec::with_capacity((n - 1).min((self.b.len() - self.pos) / 4));
                for _ in 1..n {
                    interiors.push(self.ring(le, z, m)?);
                }
                Geometry::Polygon { exterior, interiors }
            }
            4 => Geometry::MultiPoint(self.members(le, |g| match g {
                Geometry::Point(c) => Some(c),
                _ => None,
            })?),
            5 => Geometry::MultiLineString(self.members(le, |g| match g {
                Geometry::LineString(cs) => Some(cs),
                _ => None,
            })?),
            6 => Geometry::MultiPolygon(self.members(le, |g| match g {
                Geometry::Polygon { exterior, interiors } => Some((exterior, interiors)),
                _ => None,
            })?),
            7 => Geometry::GeometryCollection(self.members(le, Some)?),
            _ => return None,
        })
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
        let mut values = Db::record_values(&payload);
        Ok(match values.get_mut(1) {
            Some(Value::Blob(b)) => Some(std::mem::take(b)),
            _ => None,
        })
    };
    let (x0, y0, x1, y1) = bbox;
    let mut out = Vec::new();
    // Node 1 is the root; its first two bytes give the depth of the tree.
    let Some(root_node) = node(1)?.filter(|n| n.len() >= 4) else { return Ok(None) };
    let depth = u16::from_be_bytes([root_node[0], root_node[1]]) as usize;
    let mut stack: Vec<(Vec<u8>, usize)> = vec![(root_node, 0)];
    while let Some((data, level)) = stack.pop() {
        let Some(count) = data.get(2..4).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else { continue };
        for i in 0..count {
            let c = 4 + i * 24;
            let Some(cell) = data.get(c..c + 24) else { break };
            let id = i64::from_be_bytes(cell[..8].try_into().unwrap_or_default());
            let f = |k: usize| f64::from(f32::from_be_bytes(cell[8 + 4 * k..12 + 4 * k].try_into().unwrap_or_default()));
            let (minx, maxx, miny, maxy) = (f(0), f(1), f(2), f(3));
            if minx > x1 || maxx < x0 || miny > y1 || maxy < y0 {
                continue;
            }
            if level == depth {
                out.push(id);
            } else if let Some(child) = node(id)? {
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
    if let Ok(g) = db.table("gpkg_geometry_columns")
        && let Some(r) =
            g.rows.iter().find(|r| g.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&c.name)))
    {
        let col = g.get(r, "column_name").and_then(Value::as_str).unwrap_or_default();
        geom_name = Some(col.to_string());
        layer.geom_type = g.get(r, "geometry_type_name").and_then(Value::as_str).and_then(geometry_type);
        if let Some(srs_id) = g.get(r, "srs_id").and_then(Value::as_i64) {
            set_crs(db, &mut layer, srs_id);
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

fn read_layer(db: &Db, c: &Content, bbox: Option<Rect>) -> Result<Layer> {
    let def = layer_def(db, c)?;
    // The rows: with a filter on a spatial table, those its R-tree finds (by lookups of their
    // feature ids, the rowid alias), otherwise all of them.
    let candidates = match (&def.geom_name, bbox, def.fid_col) {
        (Some(g), Some(rect), Some(_)) => rtree_candidates(db, &format!("rtree_{}_{g}", c.name), rect)?,
        _ => None,
    };
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
fn stored_feature_count(db: &Db, table: &str) -> Option<usize> {
    let t = db.table("gpkg_ogr_contents").ok()?;
    let r = t.rows.iter().find(|r| t.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(table)))?;
    usize::try_from(t.get(r, "feature_count")?.as_i64()?).ok()
}

/// A table's extent in `gpkg_contents`, when all four bounds are set.
fn stored_extent(db: &Db, table: &str) -> Option<(f64, f64, f64, f64)> {
    let t = db.table("gpkg_contents").ok()?;
    let r = t.rows.iter().find(|r| t.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(table)))?;
    let v = |name: &str| t.get(r, name).and_then(Value::as_f64).filter(|v| v.is_finite());
    Some((v("min_x")?, v("min_y")?, v("max_x")?, v("max_y")?))
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
    if let Ok(g) = db.table("gpkg_geometry_columns")
        && let Some(r) =
            g.rows.iter().find(|r| g.get(r, "table_name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&c.name)))
    {
        let col = g.get(r, "column_name").and_then(Value::as_str).unwrap_or_default();
        geom_col = columns.iter().position(|(n, _)| n.eq_ignore_ascii_case(col));
        layer.geom_type = g.get(r, "geometry_type_name").and_then(Value::as_str).and_then(geometry_type);
        z_flag = g.get(r, "z").and_then(Value::as_i64);
        if let Some(srs_id) = g.get(r, "srs_id").and_then(Value::as_i64) {
            set_crs(db, &mut layer, srs_id);
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
    let stored_count = stored_feature_count(db, &c.name);
    let stored_extent = stored_extent(db, &c.name);
    let declared = layer.geom_type.filter(|_| matches!(z_flag, Some(0 | 1)));
    if let (Some(feature_count), Some(t)) = (stored_count, declared)
        && (stored_extent.is_some() || feature_count == 0)
    {
        let geometry_type = crate::vector::geometry_type_name_of(Some(t), std::iter::empty(), z_flag == Some(1));
        return Ok(LayerSummary { layer, feature_count, geometry_type, extent: stored_extent });
    }
    let rows = db.raw_rows(&c.name)?;

    // Each geometry's type, Z flag and bounds.
    let geometries: Vec<Option<GeometrySummary>> = crate::par::map_range(rows.len(), |i| {
        let Value::Blob(b) = Db::record_column(&rows[i].1, geom_col?) else { return None };
        let g = decode_geometry(&b)?;
        let bbox = g.bbox().map(|b| (b.min_x, b.min_y, b.max_x, b.max_y));
        Some((g.geom_type(), crate::vector::geometry_has_z(&g), bbox))
    });
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
                    && let Some(feature) = self.feature(rowid, Db::record_values(&payload), bbox)
                {
                    layer.push(feature);
                }
            }
        } else {
            for (rowid, row) in self.db.stored_rows(self.root)? {
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

    #[test]
    fn decodes_wkb_as_wbvector() {
        use wbvector::Coord;
        let ring = |o: f64| vec![Coord::xy(o, 0.0), Coord::xy(o + 1.0, 0.0), Coord::xy(o, 1.0), Coord::xy(o, 0.0)];
        let poly = Geometry::polygon(ring(0.0), vec![ring(0.2)]);
        let geoms = vec![
            Geometry::point(1.5, -2.0),
            Geometry::Point(Coord { x: 1.0, y: 2.0, z: Some(3.0), m: Some(4.0) }),
            Geometry::line_string(ring(5.0)),
            poly.clone(),
            Geometry::MultiPolygon(vec![(Ring::new(ring(0.0)), vec![]), (Ring::new(ring(3.0)), vec![Ring::new(ring(3.1))])]),
            Geometry::MultiPoint(vec![Coord::xy(1.0, 1.0), Coord { x: 2.0, y: 2.0, z: Some(1.0), m: None }]),
            Geometry::MultiLineString(vec![ring(1.0), vec![]]),
            Geometry::GeometryCollection(vec![poly, Geometry::point(0.0, 0.0)]),
            Geometry::Polygon { exterior: Ring::default(), interiors: vec![] },
        ];
        for g in &geoms {
            let wkb = g.to_wkb();
            assert_eq!(from_wkb(&wkb), Geometry::from_wkb(&wkb).ok(), "{g:?}");
            // Truncated and corrupted encodings fail (or not) alike.
            for cut in 0..wkb.len() {
                assert_eq!(from_wkb(&wkb[..cut]), Geometry::from_wkb(&wkb[..cut]).ok(), "{g:?} cut at {cut}");
            }
            for at in 0..wkb.len().min(16) {
                let mut bad = wkb.clone();
                bad[at] ^= 0xff;
                assert_eq!(from_wkb(&bad), Geometry::from_wkb(&bad).ok(), "{g:?} corrupted at {at}");
            }
        }
        // Big-endian and EWKB/ISO type codes.
        let mut be = vec![0u8];
        be.extend(1001u32.to_be_bytes());
        for v in [1.0f64, 2.0, 3.0] {
            be.extend(v.to_be_bytes());
        }
        assert_eq!(from_wkb(&be), Geometry::from_wkb(&be).ok());
        let mut ewkb = vec![1u8];
        ewkb.extend((0x8000_0001u32).to_le_bytes());
        for v in [1.0f64, 2.0, 3.0] {
            ewkb.extend(v.to_le_bytes());
        }
        assert_eq!(from_wkb(&ewkb), Geometry::from_wkb(&ewkb).ok());
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
        let read = crate::dataset::finish_vector(read(path, &[], None).unwrap(), path, crate::formats::VectorDriver::GPKG);
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
}
