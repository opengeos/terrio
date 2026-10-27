//! Updating GeoPackage files, as `ogr2ogr -append`, `-overwrite` and `-update` do: features added
//! to an existing layer (feature ids continuing its sequence, its R-tree index, extent in
//! `gpkg_contents` and feature count in `gpkg_ogr_contents` updated), a layer replaced, layers
//! added, and everything else in the file kept: other layers, rasters, metadata, extensions,
//! triggers and views.
//!
//! Files are changed in place (see [`super::gpkg_inplace`]). Those that cannot be (a write-ahead
//! log, auto-vacuum, triggers this module does not emulate) and GeoPackages held in memory are
//! read whole (every table's rows as stored, every index, trigger and view) and written anew;
//! a file is then renamed over the original. Files with tables or indexes that cannot be
//! rewritten (`WITHOUT ROWID` tables, indexes on expressions, UTF-16 text) or that a hot
//! rollback journal goes with are left untouched, with an error.

use super::geopackage::{self as gw, Builder};
use super::sqlite::{self as w, Index, SchemaObject, Table};
use crate::drivers::sqlite::{self as r, Db};
use crate::{Error, Result};
use wbvector::{FieldValue, Layer};

/// What to do with the layers of an existing file that have the name of a layer written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMode {
    /// Add the features to the layer.
    Append,
    /// Replace the layer.
    Overwrite,
}

fn unsupported(msg: impl std::fmt::Display) -> Error {
    Error::Unsupported(format!("GeoPackage update: {msg}"))
}

pub(super) fn to_w(v: r::Value) -> w::Value {
    match v {
        r::Value::Null => w::Value::Null,
        r::Value::Int(i) => w::Value::Int(i),
        r::Value::Real(f) => w::Value::Real(f),
        r::Value::Text(s) => w::Value::Text(s),
        r::Value::Blob(b) => w::Value::Blob(b),
    }
}

pub(super) fn text(v: &w::Value) -> Option<&str> {
    match v {
        w::Value::Text(s) => Some(s),
        _ => None,
    }
}

pub(super) fn int(v: &w::Value) -> Option<i64> {
    match v {
        w::Value::Int(i) => Some(*i),
        w::Value::Real(f) => Some(*f as i64),
        _ => None,
    }
}

fn real(v: &w::Value) -> Option<f64> {
    match v {
        w::Value::Int(i) => Some(*i as f64),
        w::Value::Real(f) => Some(*f),
        _ => None,
    }
}

/// Adds layers to a GeoPackage file, or features to its layers, as `mode` says for the layers
/// that exist. The file is changed in place, as SQLite changes it (only the pages that change
/// are written, in a transaction protected by a rollback journal); files that cannot be changed
/// that way are rewritten, the new file replacing the original once complete.
pub fn update(path: &str, layers: &[Layer], mode: UpdateMode) -> Result<()> {
    if super::gpkg_inplace::update_in_place(std::path::Path::new(path), layers, mode)? {
        return Ok(());
    }
    rewrite(path, layers, mode)
}

/// Updates a GeoPackage file by writing it anew, the new file renamed over the original. A file
/// with a hot rollback journal is left untouched, with an error: without SQLite's locks (where
/// files are not changed in place) a transaction interrupted cannot be told from one under way,
/// and the journal, left next to the new file, would be rolled back over it.
fn rewrite(path: &str, layers: &[Layer], mode: UpdateMode) -> Result<()> {
    if crate::drivers::sqlite_edit::has_hot_journal(std::path::Path::new(path))? {
        return Err(Error::Unsupported(format!(
            "{path}: the database has a hot rollback journal (a transaction interrupted or under way); \
             open it with SQLite to recover it first"
        )));
    }
    let bytes = update_bytes(std::fs::read(path)?, layers, mode)?;
    let tmp = format!("{path}.terrio-update");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Updates a GeoPackage held in memory: see [`update`].
pub fn update_bytes(bytes: Vec<u8>, layers: &[Layer], mode: UpdateMode) -> Result<Vec<u8>> {
    let db = Db::new(bytes)?;
    let (existing, _) = super::gpkg_read::contents(&db)?;
    let mut d = Database::load(&db)?;
    drop(db);
    for layer in layers {
        match (existing.iter().find(|c| c.name.eq_ignore_ascii_case(&layer.name)), mode) {
            (Some(c), UpdateMode::Append) => d.append(&c.name, layer)?,
            (Some(c), UpdateMode::Overwrite) => {
                d.remove_layer(&c.name);
                d.add_layer(layer)?;
            }
            (None, _) => {
                if d.has(&layer.name) {
                    return Err(Error::InvalidArgument(format!(
                        "the GeoPackage has a table named '{}' that is not a vector layer",
                        layer.name
                    )));
                }
                d.add_layer(layer)?;
            }
        }
    }
    d.finish()
}

/// A database read whole: its tables with their rows as stored, indexes and schema objects.
pub(crate) struct Database {
    pub(super) tables: Vec<Table>,
    pub(super) indexes: Vec<Index>,
    pub(super) objects: Vec<SchemaObject>,
    pub(super) user_version: u32,
    pub(super) application_id: u32,
}

impl Database {
    pub(super) fn load(db: &Db) -> Result<Self> {
        if db.text_encoding() > 1 {
            return Err(unsupported("UTF-16 databases cannot be rewritten"));
        }
        let mut tables = Vec::new();
        let mut objects = Vec::new();
        let mut autos: Vec<(String, String)> = Vec::new();
        let mut explicit: Vec<(String, String, String)> = Vec::new();
        for (kind, name, table, root, sql) in db.schema()? {
            match kind.as_str() {
                "table" if root > 0 => {
                    if keywords_contain(&sql, "WITHOUT ROWID") {
                        return Err(unsupported(format!("'{name}' is a WITHOUT ROWID table")));
                    }
                    let rows = db.stored_rows(root)?.into_iter().map(|(id, v)| (id, v.into_iter().map(to_w).collect())).collect();
                    tables.push(Table { name, sql, rows, autoindexes: Vec::new() });
                }
                "table" => objects.push(SchemaObject { kind: "table", name, table, sql }),
                "trigger" => objects.push(SchemaObject { kind: "trigger", name, table, sql }),
                "view" => objects.push(SchemaObject { kind: "view", name, table, sql }),
                "index" if sql.is_empty() => autos.push((table, name)),
                "index" => explicit.push((name, table, sql)),
                other => return Err(unsupported(format!("schema entry '{name}' of type '{other}'"))),
            }
        }
        for t in &mut tables {
            let cols = r::autoindex_columns(&t.sql);
            let mine: Vec<&String> = autos.iter().filter(|(table, _)| table.eq_ignore_ascii_case(&t.name)).map(|(_, n)| n).collect();
            let expected: Vec<String> = (1..=cols.len()).map(|i| format!("sqlite_autoindex_{}_{i}", t.name)).collect();
            if mine.len() != cols.len() || !mine.iter().all(|n| expected.iter().any(|e| e.eq_ignore_ascii_case(n))) {
                return Err(unsupported(format!("cannot rebuild the indexes of table '{}'", t.name)));
            }
            t.autoindexes = cols;
        }
        let mut indexes = Vec::new();
        for (name, table, sql) in explicit {
            let t = tables
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(&table))
                .ok_or_else(|| unsupported(format!("index '{name}' of a missing table")))?;
            let columns = r::index_columns(&sql, &t.sql).ok_or_else(|| unsupported(format!("index '{name}' is not on plain columns")))?;
            indexes.push(Index { name, table, sql, columns });
        }
        Ok(Self { tables, indexes, objects, user_version: db.header_u32(60), application_id: db.header_u32(68) })
    }

    pub(super) fn has(&self, name: &str) -> bool {
        self.tables.iter().any(|t| t.name.eq_ignore_ascii_case(name)) || self.objects.iter().any(|o| o.name.eq_ignore_ascii_case(name))
    }

    pub(super) fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name))
    }

    pub(super) fn table_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.iter_mut().find(|t| t.name.eq_ignore_ascii_case(name))
    }

    /// The table, created (empty, with its automatic indexes) when the file lacks it.
    pub(super) fn ensure(&mut self, name: &str, sql: &str) -> &mut Table {
        if self.table(name).is_none() {
            self.tables.push(Table { name: name.into(), sql: sql.into(), rows: Vec::new(), autoindexes: r::autoindex_columns(sql) });
        }
        self.table_mut(name).expect("the table exists")
    }

    /// Removes the rows whose `column` is `value` (case-insensitively, as GDAL compares table
    /// names).
    pub(super) fn delete_rows(&mut self, table: &str, column: &str, value: &str) {
        let Some(t) = self.table_mut(table) else { return };
        let Some(c) = columns(&t.sql).iter().position(|n| n.eq_ignore_ascii_case(column)) else { return };
        t.rows.retain(|(_, v)| !v.get(c).and_then(text).is_some_and(|s| s.eq_ignore_ascii_case(value)));
    }

    /// Inserts a row given by column name into a table, the rowid alias column (when given)
    /// setting the rowid.
    pub(super) fn insert(t: &mut Table, values: &[(String, w::Value)]) {
        let names = columns(&t.sql);
        let ipk = r::rowid_column(&t.sql);
        let mut row = vec![w::Value::Null; names.len()];
        let mut rowid = None;
        for (name, v) in values {
            if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(name)) {
                if Some(i) == ipk {
                    rowid = int(v);
                } else {
                    row[i] = v.clone();
                }
            }
        }
        let rowid = rowid.unwrap_or_else(|| t.rows.iter().map(|(id, _)| *id).max().unwrap_or(0) + 1);
        t.rows.retain(|(id, _)| *id != rowid);
        t.rows.push((rowid, row));
    }

    /// The value of `column` in the row of `table` whose `key` column is `name`.
    pub(super) fn lookup(&self, table: &str, key: &str, name: &str, column: &str) -> Option<w::Value> {
        let t = self.table(table)?;
        let cols = columns(&t.sql);
        let k = cols.iter().position(|c| c.eq_ignore_ascii_case(key))?;
        let c = cols.iter().position(|c2| c2.eq_ignore_ascii_case(column))?;
        t.rows
            .iter()
            .find(|(_, v)| v.get(k).and_then(text).is_some_and(|s| s.eq_ignore_ascii_case(name)))
            .and_then(|(_, v)| v.get(c).cloned())
    }

    /// Sets `column` in the rows of `table` whose `key` column is `name`.
    pub(super) fn set(&mut self, table: &str, key: &str, name: &str, column: &str, value: w::Value) {
        let Some(t) = self.table_mut(table) else { return };
        let cols = columns(&t.sql);
        let (Some(k), Some(c)) =
            (cols.iter().position(|x| x.eq_ignore_ascii_case(key)), cols.iter().position(|x| x.eq_ignore_ascii_case(column)))
        else {
            return;
        };
        for (_, v) in t.rows.iter_mut().filter(|(_, v)| v.get(k).and_then(text).is_some_and(|s| s.eq_ignore_ascii_case(name))) {
            v[c] = value.clone();
        }
    }

    /// The geometry column of a layer: its name, SRS id and `z` flag.
    pub(super) fn geometry_column(&self, layer: &str) -> Option<(String, i64, i64)> {
        let column = self.lookup("gpkg_geometry_columns", "table_name", layer, "column_name")?;
        let srs = self.lookup("gpkg_geometry_columns", "table_name", layer, "srs_id").as_ref().and_then(int).unwrap_or(0);
        let z = self.lookup("gpkg_geometry_columns", "table_name", layer, "z").as_ref().and_then(int).unwrap_or(0);
        Some((text(&column)?.to_string(), srs, z))
    }

    /// Removes a layer: its table, R-tree, triggers, indexes and the rows describing it.
    pub(super) fn remove_layer(&mut self, name: &str) {
        let rtree = self.geometry_column(name).map(|(g, ..)| format!("rtree_{name}_{g}"));
        let gone = |n: &str| {
            n.eq_ignore_ascii_case(name)
                || rtree.as_ref().is_some_and(|rt| {
                    n.eq_ignore_ascii_case(rt) || ["_rowid", "_node", "_parent"].iter().any(|s| n.eq_ignore_ascii_case(&format!("{rt}{s}")))
                })
        };
        self.tables.retain(|t| !gone(&t.name));
        self.objects.retain(|o| !gone(&o.name) && !o.table.eq_ignore_ascii_case(name));
        self.indexes.retain(|i| !i.table.eq_ignore_ascii_case(name));
        for t in [
            "gpkg_contents",
            "gpkg_geometry_columns",
            "gpkg_ogr_contents",
            "gpkg_extensions",
            "gpkg_metadata_reference",
            "gpkg_data_columns",
        ] {
            self.delete_rows(t, "table_name", name);
        }
        self.delete_rows("sqlite_sequence", "name", name);
    }

    /// Adds a layer, as a new file would hold it, its CRS reusing an entry of
    /// `gpkg_spatial_ref_sys` that matches.
    pub(super) fn add_layer(&mut self, layer: &Layer) -> Result<()> {
        let mut b = Builder::new();
        b.add_layer(layer)?;
        let mut parts = b.into_layer_parts();

        let remap = self.merge_srs(&parts.srs)?;
        let remapped = |v: &mut w::Value| {
            if let Some((_, to)) = int(v).and_then(|x| remap.iter().find(|(from, _)| *from == x)) {
                *v = w::Value::Int(*to);
            }
        };
        for row in parts.contents.iter_mut() {
            remapped(&mut row[9]);
        }
        for row in parts.geometry_columns.iter_mut() {
            remapped(&mut row[3]);
        }
        // The SRS id in the geometry blobs of the new table.
        if !remap.is_empty()
            && let Some(t) = parts.tables.iter_mut().find(|t| t.name == layer.name)
        {
            for (_, v) in t.rows.iter_mut() {
                for value in v.iter_mut() {
                    if let w::Value::Blob(b) = value
                        && b.len() >= 8
                        && &b[..2] == b"GP"
                    {
                        let from = i64::from(i32::from_le_bytes([b[4], b[5], b[6], b[7]]));
                        if let Some((_, to)) = remap.iter().find(|(f, _)| *f == from) {
                            b[4..8].copy_from_slice(&(*to as i32).to_le_bytes());
                        }
                    }
                }
            }
        }

        let add_rows = |d: &mut Database, table: &str, sql: &str, rows: Vec<Vec<w::Value>>| {
            if rows.is_empty() {
                return;
            }
            let names = columns(sql);
            let t = d.ensure(table, sql);
            for row in rows {
                let values: Vec<(String, w::Value)> = names.iter().cloned().zip(row).collect();
                if table == "gpkg_extensions" {
                    // Unique (table_name, column_name, extension_name).
                    let key: Vec<Option<&str>> = values.iter().take(3).map(|(_, v)| text(v)).collect();
                    let cols = columns(&t.sql);
                    let pos: Vec<Option<usize>> = ["table_name", "column_name", "extension_name"]
                        .iter()
                        .map(|n| cols.iter().position(|c| c.eq_ignore_ascii_case(n)))
                        .collect();
                    let dup = t.rows.iter().any(|(_, v)| {
                        pos.iter().zip(&key).all(|(p, k)| {
                            p.and_then(|p| v.get(p)).and_then(text).map(str::to_ascii_lowercase) == k.map(str::to_ascii_lowercase)
                        })
                    });
                    if dup {
                        continue;
                    }
                }
                Self::insert(t, &values);
            }
        };
        add_rows(self, "gpkg_contents", gw::CONTENTS_SQL, parts.contents);
        add_rows(self, "gpkg_geometry_columns", gw::GEOMETRY_COLUMNS_SQL, parts.geometry_columns);
        add_rows(self, "gpkg_ogr_contents", gw::OGR_CONTENTS_SQL, parts.ogr_contents);
        add_rows(self, "gpkg_extensions", gw::EXTENSIONS_SQL, parts.extensions);
        for (name, seq) in parts.sequence {
            self.set_sequence(&name, seq);
        }
        self.tables.append(&mut parts.tables);
        self.objects.append(&mut parts.objects);
        Ok(())
    }

    /// Adds the `gpkg_spatial_ref_sys` rows a [`Builder`] made (numbered by their code) to the
    /// file, reusing its entries for the same SRSs: returns the ids that changed, `(builder's
    /// id, file's id)`.
    pub(super) fn merge_srs(&mut self, srs_rows: &[Vec<w::Value>]) -> Result<Vec<(i64, i64)>> {
        let mut remap: Vec<(i64, i64)> = Vec::new();
        let srs_names = columns(gw::SRS_SQL);
        let srs =
            self.table_mut("gpkg_spatial_ref_sys").ok_or_else(|| Error::InvalidData("not a GeoPackage: no gpkg_spatial_ref_sys".into()))?;
        let srs_cols = columns(&srs.sql);
        let col = |n: &str| srs_cols.iter().position(|c| c.eq_ignore_ascii_case(n));
        let (org_c, code_c, def_c) = (col("organization"), col("organization_coordsys_id"), col("definition"));
        for row in srs_rows {
            let assumed = int(&row[3]).unwrap_or_default();
            let org = text(&row[2]).unwrap_or_default();
            let def = text(&row[4]).unwrap_or_default();
            let same = |v: &[w::Value]| -> bool {
                let get = |c: Option<usize>| c.and_then(|c| v.get(c));
                let vorg = get(org_c).and_then(text).unwrap_or_default();
                let vcode = get(code_c).and_then(int);
                if org.eq_ignore_ascii_case("NONE") && assumed > 0 {
                    vorg.eq_ignore_ascii_case("NONE") && get(def_c).and_then(text) == Some(def)
                } else {
                    vorg.eq_ignore_ascii_case(org) && vcode == Some(assumed)
                }
            };
            let id = match srs.rows.iter().find(|(_, v)| same(v)) {
                Some((id, _)) => *id,
                None => {
                    let id = if srs.rows.iter().any(|(id, _)| *id == assumed) {
                        srs.rows.iter().map(|(id, _)| *id).max().unwrap_or(0).max(100000) + 1
                    } else {
                        assumed
                    };
                    let mut values: Vec<(String, w::Value)> = srs_names.iter().cloned().zip(row.iter().cloned()).collect();
                    values[1].1 = w::Value::Int(id);
                    Self::insert(srs, &values);
                    id
                }
            };
            if id != assumed {
                remap.push((assumed, id));
            }
        }
        Ok(remap)
    }

    pub(super) fn set_sequence(&mut self, name: &str, seq: i64) {
        let t = self.ensure("sqlite_sequence", "CREATE TABLE sqlite_sequence(name,seq)");
        match t.rows.iter_mut().find(|(_, v)| v.first().and_then(text).is_some_and(|s| s.eq_ignore_ascii_case(name))) {
            Some((_, v)) => v[1] = w::Value::Int(seq),
            None => Self::insert(t, &[("name".into(), w::Value::Text(name.into())), ("seq".into(), w::Value::Int(seq))]),
        }
    }

    /// Adds the features of `layer` to the existing layer `name`, as `ogr2ogr -append` does:
    /// fields matched by name (ignoring case; others dropped), new feature ids, geometries
    /// as they are (in the layer's SRS id).
    fn append(&mut self, name: &str, layer: &Layer) -> Result<()> {
        let geometry = self.geometry_column(name);
        let t = self.table(name).ok_or_else(|| Error::InvalidData(format!("GeoPackage: no table '{name}'")))?;
        let sql = t.sql.clone();
        let max_rowid = t.rows.iter().map(|(id, _)| *id).max().unwrap_or(0);
        let next = self.sequence(name).max(max_rowid) + 1;
        let enc = encode_append(&sql, geometry.as_ref(), layer, next);
        let geom_col = enc.geom_col;
        let total = {
            let t = self.table_mut(name).expect("the table exists");
            t.rows.extend(enc.rows.iter().cloned());
            t.rows.len()
        };

        if let (Some((g, ..)), Some(gi)) = (&geometry, geom_col) {
            self.rebuild_rtree(name, g, gi);
        }
        self.appended(name, &sql, &enc, total as i64);
        Ok(())
    }

    /// Rebuilds the R-tree of layer `name` (geometry column `g`, the `gi`-th column) over all
    /// its features, when it has one.
    pub(super) fn rebuild_rtree(&mut self, name: &str, g: &str, gi: usize) {
        let rt = format!("rtree_{name}_{g}");
        if self.table(&format!("{rt}_node")).is_none() {
            return;
        }
        let Some(t) = self.table(name) else { return };
        let entries: Vec<gw::RtreeEntry> = t
            .rows
            .iter()
            .filter_map(|(id, v)| match v.get(gi) {
                Some(w::Value::Blob(b)) => envelope(b).map(|e| (*id, rtree_box(e))),
                _ => None,
            })
            .collect();
        let (nodes, rowids, parents) = gw::build_rtree(entries);
        for (suffix, rows) in [("_node", nodes), ("_rowid", rowids), ("_parent", parents)] {
            if let Some(t) = self.table_mut(&format!("{rt}{suffix}")) {
                t.rows = rows;
            }
        }
    }

    /// The `sqlite_sequence` value of a table (0 when it has none).
    pub(super) fn sequence(&self, name: &str) -> i64 {
        self.table("sqlite_sequence")
            .and_then(|s| s.rows.iter().find(|(_, v)| v.first().and_then(text).is_some_and(|x| x.eq_ignore_ascii_case(name))))
            .and_then(|(_, v)| v.get(1).and_then(int))
            .unwrap_or(0)
    }

    /// Updates what describes a layer after features were appended to it: its sequence, `z`
    /// flag, extent, last change and feature count (`total`).
    pub(super) fn appended(&mut self, name: &str, sql: &str, enc: &Encoded, total: i64) {
        if keywords_contain(sql, "AUTOINCREMENT")
            && let Some((last, _)) = enc.rows.last()
        {
            let seq = self.sequence(name).max(*last);
            self.set_sequence(name, seq);
        }
        if let Some((_, _, z)) = self.geometry_column(name)
            && enc.has_z
            && z == 0
        {
            self.set("gpkg_geometry_columns", "table_name", name, "z", w::Value::Int(2));
        }

        // The extent and the feature count.
        if let Some(b) = enc.bounds {
            let get = |d: &Database, c: &str| d.lookup("gpkg_contents", "table_name", name, c).as_ref().and_then(real);
            let merged = [
                get(self, "min_x").map_or(b[0], |v| v.min(b[0])),
                get(self, "min_y").map_or(b[1], |v| v.min(b[1])),
                get(self, "max_x").map_or(b[2], |v| v.max(b[2])),
                get(self, "max_y").map_or(b[3], |v| v.max(b[3])),
            ];
            for (c, v) in ["min_x", "min_y", "max_x", "max_y"].iter().zip(merged) {
                self.set("gpkg_contents", "table_name", name, c, w::Value::Real(v));
            }
        }
        self.set("gpkg_contents", "table_name", name, "last_change", w::Value::Text(gw::now()));
        self.set("gpkg_ogr_contents", "table_name", name, "feature_count", w::Value::Int(total));
    }

    pub(super) fn finish(mut self) -> Result<Vec<u8>> {
        for t in &mut self.tables {
            t.rows.sort_by_key(|(id, _)| *id);
        }
        // `sqlite_sequence` last, as SQLite creates it after the first AUTOINCREMENT table.
        if let Some(i) = self.tables.iter().position(|t| t.name == "sqlite_sequence") {
            let t = self.tables.remove(i);
            self.tables.push(t);
        }
        w::write_database_with_indexes(&self.tables, &self.indexes, &self.objects, self.user_version, self.application_id)
    }
}

/// Features encoded as rows of an existing feature table.
pub(super) struct Encoded {
    /// `(rowid, values)`, by increasing rowid; the rowid alias column holds NULL.
    pub rows: Vec<(i64, Vec<w::Value>)>,
    /// The geometry column's index.
    pub geom_col: Option<usize>,
    /// The extent of the geometries: `[min_x, min_y, max_x, max_y]`.
    pub bounds: Option<[f64; 4]>,
    /// Whether a geometry has z values.
    pub has_z: bool,
}

/// Encodes the features of `layer` as rows of the feature table created by `sql`, with ids
/// from `first`: fields matched by name (ignoring case; others dropped), column defaults for
/// the others, geometries in the layer's SRS id (`geometry` is the table's geometry column,
/// SRS id and `z` flag).
pub(super) fn encode_append(sql: &str, geometry: Option<&(String, i64, i64)>, layer: &Layer, first: i64) -> Encoded {
    let decls = r::column_types(sql);
    let defaults = r::column_defaults(sql);
    let ipk = r::rowid_column(sql);
    let geom_col = geometry.and_then(|(g, ..)| decls.iter().position(|(n, _)| n.eq_ignore_ascii_case(g)));
    let srs_id = geometry.map_or(0, |(_, s, _)| *s);
    let fields = layer.schema.fields();
    let map: Vec<Option<usize>> = decls
        .iter()
        .enumerate()
        .map(|(i, (n, _))| {
            if Some(i) == ipk || Some(i) == geom_col {
                return None;
            }
            fields.iter().position(|f| f.name.eq_ignore_ascii_case(n))
        })
        .collect();
    let encoded = crate::par::map_range(layer.len(), |k| {
        let f = &layer.features[k];
        let mut values: Vec<w::Value> = defaults.iter().map(|d| d.clone().map_or(w::Value::Null, to_w)).collect();
        values.resize(decls.len(), w::Value::Null);
        if let Some(ipk) = ipk {
            values[ipk] = w::Value::Null;
        }
        let mut bbox = None;
        let mut z = false;
        if let Some(gi) = geom_col {
            values[gi] = match &f.geometry {
                Some(g) => {
                    bbox = g.bbox().filter(|b| b.min_x.is_finite() && b.min_y.is_finite());
                    z = crate::vector::geometry_has_z(g);
                    w::Value::Blob(gw::gpkg_geometry(g, srs_id as i32))
                }
                None => w::Value::Null,
            };
        }
        for (i, m) in map.iter().enumerate() {
            if let Some(fi) = m
                && let Some(v) = f.attributes.get(*fi)
                && !v.is_null()
            {
                values[i] = convert(v, &decls[i].1);
            }
        }
        (values, bbox, z)
    });
    let mut bounds: Option<[f64; 4]> = None;
    let mut has_z = false;
    let mut rows = Vec::with_capacity(encoded.len());
    for ((values, bbox, z), id) in encoded.into_iter().zip(first..) {
        if let Some(b) = bbox {
            bounds = Some(match bounds {
                None => [b.min_x, b.min_y, b.max_x, b.max_y],
                Some(a) => [a[0].min(b.min_x), a[1].min(b.min_y), a[2].max(b.max_x), a[3].max(b.max_y)],
            });
        }
        has_z |= z;
        rows.push((id, values));
    }
    Encoded { rows, geom_col, bounds, has_z }
}

/// An envelope `[min_x, max_x, min_y, max_y]` as SQLite's R-tree stores it, rounded outwards to
/// floats.
pub(super) fn rtree_box(e: [f64; 4]) -> [f32; 4] {
    [gw::f32_down(e[0]), gw::f32_up(e[1]), gw::f32_down(e[2]), gw::f32_up(e[3])]
}

/// The column names of a `CREATE TABLE` statement.
pub(super) fn columns(sql: &str) -> Vec<String> {
    r::column_types(sql).into_iter().map(|(n, _)| n).collect()
}

/// Whether a statement contains a keyword sequence outside quotes and parentheses.
pub(super) fn keywords_contain(sql: &str, words: &str) -> bool {
    sql.to_ascii_uppercase().split_whitespace().collect::<Vec<_>>().join(" ").contains(words)
}

/// An attribute value stored in a column of the declared type, with SQLite's type affinity and
/// GDAL's conversions (date-times with a `T`).
pub(super) fn convert(v: &FieldValue, decl: &str) -> w::Value {
    let upper = decl.to_ascii_uppercase();
    let base = upper.split('(').next().unwrap_or_default().trim().to_string();
    let integer =
        matches!(base.as_str(), "INTEGER" | "INT" | "MEDIUMINT" | "SMALLINT" | "TINYINT" | "BIGINT" | "INT8" | "INT2" | "BOOLEAN");
    let real = matches!(base.as_str(), "REAL" | "DOUBLE" | "FLOAT" | "NUMERIC" | "DECIMAL");
    match v {
        FieldValue::Null => w::Value::Null,
        FieldValue::Blob(b) => w::Value::Blob(b.clone()),
        FieldValue::Boolean(b) if integer || real => w::Value::Int(i64::from(*b)),
        FieldValue::Boolean(b) => w::Value::Text(if *b { "1" } else { "0" }.into()),
        FieldValue::Integer(i) if real => w::Value::Real(*i as f64),
        FieldValue::Integer(i) if integer => w::Value::Int(*i),
        FieldValue::Float(f) if integer => w::Value::Int(*f as i64),
        FieldValue::Float(f) if real => w::Value::Real(*f),
        FieldValue::Integer(i) => w::Value::Text(i.to_string()),
        FieldValue::Float(f) => w::Value::Text(f.to_string()),
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => {
            if integer && let Ok(i) = s.trim().parse::<i64>() {
                return w::Value::Int(i);
            }
            if (integer || real)
                && let Ok(f) = s.trim().parse::<f64>()
            {
                return w::Value::Real(f);
            }
            if base == "DATETIME" && s.len() > 10 && s.as_bytes()[10] == b' ' {
                return w::Value::Text(s.replacen(' ', "T", 1));
            }
            w::Value::Text(s.clone())
        }
    }
}

/// The envelope `[min_x, max_x, min_y, max_y]` of a GeoPackage geometry blob: from its header,
/// or else its geometry. `None` for empty geometries.
pub(super) fn envelope(b: &[u8]) -> Option<[f64; 4]> {
    if b.len() >= 8 && &b[..2] == b"GP" {
        let flags = b[3];
        if flags & 0x10 != 0 {
            return None;
        }
        let little = flags & 1 == 1;
        if (flags >> 1) & 0x07 != 0 {
            let f = |i: usize| -> Option<f64> {
                let bytes: [u8; 8] = b.get(8 + 8 * i..16 + 8 * i)?.try_into().ok()?;
                Some(if little { f64::from_le_bytes(bytes) } else { f64::from_be_bytes(bytes) })
            };
            let e = [f(0)?, f(1)?, f(2)?, f(3)?];
            return e.iter().all(|v| v.is_finite()).then_some(e);
        }
    }
    let g = super::gpkg_read::decode_geometry(b)?;
    let bb = g.bbox()?;
    let e = [bb.min_x, bb.max_x, bb.min_y, bb.max_y];
    e.iter().all(|v| v.is_finite()).then_some(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Coord, Feature, FieldDef, FieldType, Geometry};

    fn layer(name: &str, n: u64, x0: f64) -> Layer {
        let mut l = Layer::new(name).with_epsg(32633);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("v", FieldType::Integer));
        for i in 0..n {
            let x = x0 + i as f64;
            let g = Geometry::polygon(vec![Coord::xy(x, 0.0), Coord::xy(x + 1.0, 0.0), Coord::xy(x, 1.0)], vec![]);
            l.push(Feature {
                fid: i,
                geometry: Some(g),
                attributes: vec![FieldValue::Text(format!("f{i}")), FieldValue::Integer(i as i64)],
            });
        }
        l
    }

    #[test]
    fn appends_overwrites_and_adds() {
        let base = super::super::geopackage::to_bytes(&[layer("a", 300, 0.0), layer("b", 5, 0.0)]).unwrap();
        // Append: fields matched ignoring case, extra fields dropped.
        let mut extra = layer("A", 200, 1000.0);
        extra.add_field(FieldDef::new("other", FieldType::Text));
        for f in &mut extra.features {
            f.attributes.push(FieldValue::Text("x".into()));
        }
        let out = update_bytes(base.clone(), &[extra], UpdateMode::Append).unwrap();
        let db = Db::new(out.clone()).unwrap();
        let layers = super::super::gpkg_read::read_db(&db, &[], None).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].len(), 500);
        assert_eq!(layers[0].features[499].fid, 500);
        assert_eq!(layers[0].features[499].attributes, [FieldValue::Text("f199".into()), FieldValue::Integer(199)]);
        assert_eq!(layers[1].len(), 5);
        // The extent, count and sequence follow.
        let contents = db.table("gpkg_contents").unwrap();
        let row = &contents.rows[0];
        assert_eq!(contents.get(row, "max_x").and_then(r::Value::as_f64), Some(1200.0));
        let counts = db.table("gpkg_ogr_contents").unwrap();
        assert_eq!(counts.get(&counts.rows[0], "feature_count").and_then(r::Value::as_i64), Some(500));
        let seq = db.table("sqlite_sequence").unwrap();
        assert!(seq.rows.iter().any(|r| r[0] == r::Value::Text("a".into()) && r[1] == r::Value::Int(500)));
        // The R-tree finds the new features.
        let hits = super::super::gpkg_read::read_db(&db, &["a".into()], Some((1100.0, 0.1, 1100.4, 0.2))).unwrap();
        assert_eq!(hits[0].features.iter().map(|f| f.fid).collect::<Vec<_>>(), [400, 401]);

        // Overwrite and add.
        let out = update_bytes(out, &[layer("b", 2, 50.0), layer("c", 3, 0.0).with_epsg(4326)], UpdateMode::Overwrite).unwrap();
        let db = Db::new(out).unwrap();
        let layers = super::super::gpkg_read::read_db(&db, &[], None).unwrap();
        assert_eq!(layers.iter().map(|l| (l.name.as_str(), l.len())).collect::<Vec<_>>(), [("a", 500), ("b", 2), ("c", 3)]);
        assert_eq!(layers[1].features[0].fid, 1);
        assert_eq!(layers[2].crs_epsg(), Some(4326));
    }

    #[test]
    fn rewrite_refuses_a_hot_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.gpkg");
        let base = super::super::geopackage::to_bytes(&[layer("a", 3, 0.0)]).unwrap();
        std::fs::write(&path, &base).unwrap();
        // A journal header (magic, no records, 1 page originally, 512-byte sectors and pages).
        let mut journal = vec![0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7, 0, 0, 0, 0, 0, 0, 0, 0];
        journal.extend([1u32, 512, 512].iter().flat_map(|v| v.to_be_bytes()));
        journal.resize(512, 0);
        let journal_path = dir.path().join("a.gpkg-journal");
        std::fs::write(&journal_path, &journal).unwrap();
        let p = path.to_str().unwrap();
        assert!(rewrite(p, &[layer("a", 2, 10.0)], UpdateMode::Append).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), base);
        assert!(journal_path.exists());
        // An empty journal is not hot.
        std::fs::write(&journal_path, []).unwrap();
        rewrite(p, &[layer("a", 2, 10.0)], UpdateMode::Append).unwrap();
        let db = Db::new(std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(super::super::gpkg_read::read_db(&db, &[], None).unwrap()[0].len(), 5);
    }

    #[test]
    fn refuses_to_replace_other_tables() {
        let base = super::super::geopackage::to_bytes(&[layer("a", 3, 0.0)]).unwrap();
        assert!(update_bytes(base, &[layer("gpkg_contents", 1, 0.0)], UpdateMode::Overwrite).is_err());
    }
}
