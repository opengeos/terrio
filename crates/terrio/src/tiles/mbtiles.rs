// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/mbtiles/mbtilesdataset.cpp
// No code is translated from these sources.

//! MBTiles: tiles in an SQLite database, read and written with terrio's own SQLite reader and
//! writer (the ones GeoPackage uses).
//!
//! Tiles are in the `tiles` table, or in the `tiles` view over `map` and `images` that some
//! producers write; rows are numbered from the bottom (TMS), and are turned into XYZ rows here.
//! The `metadata` table holds `name`/`value` pairs (`format`, `bounds`, `minzoom`, `maxzoom`, and
//! for vector tiles the `json` item with `vector_layers`).

use crate::drivers::sqlite::{self as reader, Db, SchemaEntry, Value};
use crate::drivers::vector::sqlite::{self as writer, Table};
use crate::gpkg_validation::integrity::real_text;
use crate::{Error, Result};
use std::collections::HashMap;

/// An open MBTiles file: its metadata, and the means of finding a tile's row. Tiles are read
/// from the file (mapped into memory on native builds) when they are asked for, by a lookup in
/// the index on `(zoom_level, tile_column, tile_row)` that MBTiles writers create; an archive
/// without one has the rowids of its tiles listed on opening, from their key columns only.
pub(crate) struct MbTiles {
    /// The `metadata` table, in file order.
    pub metadata: Vec<(String, String)>,
    db: Db,
    /// The rows that hold the tiles: `tiles`, or `map` under a `tiles` view.
    rows: Rows,
    /// Where a `map` row's `tile_id` finds its image, under a `tiles` view.
    images: Option<Images>,
    /// The zoom levels, found on opening.
    zooms: Option<(u8, u8)>,
}

impl std::fmt::Debug for MbTiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let indexed = matches!(self.rows.keys, Keys::Index(_));
        f.debug_struct("MBTiles").field("metadata", &self.metadata).field("indexed", &indexed).field("zooms", &self.zooms).finish()
    }
}

/// The table whose rows are tiles.
struct Rows {
    root: u32,
    /// The column of the tile's bytes (`tile_data`), or of its image's id (`tile_id`).
    value: usize,
    /// The column that is the rowid (stored as NULL), if any.
    alias: Option<usize>,
    keys: Keys,
}

/// How a tile's row is found from its `(z, x, y)`.
enum Keys {
    /// The root of an index whose leading columns are `zoom_level, tile_column, tile_row`.
    Index(u32),
    /// The rowids by `(z, x, y)`, rows counted from the top, read on opening.
    Scanned(HashMap<(u8, u32, u32), i64>),
}

/// The `images` table of a `map` and `images` layout.
struct Images {
    root: u32,
    /// The `tile_data` column.
    data: usize,
    ids: Ids,
    /// Whether `tile_id` has numeric affinity, so that a text id is compared as the number it
    /// spells; otherwise text ids are compared as text.
    numeric: bool,
}

/// How an image's row is found from its `tile_id`.
enum Ids {
    /// `tile_id` is the rowid.
    Rowid,
    /// The root of an index whose leading column is `tile_id`.
    Index(u32),
    /// The rowids by `tile_id` (as [`id_key`] spells it), read on opening. When `tile_id` follows
    /// `tile_data` in the table (mbutil's column order), reaching it reads each image's overflow
    /// pages, so opening such an archive without an index on `tile_id` reads its image bytes.
    Scanned(HashMap<IdKey, i64>),
}

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("MBTiles: {msg}"))
}

/// A key column's integer: an integer, or a real with no fraction (as SQLite compares them).
fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Int(i) => Some(*i),
        Value::Real(f) => whole(*f),
        _ => None,
    }
}

/// The integer equal to `f`, if there is one.
fn whole(f: f64) -> Option<i64> {
    // -2^63 <= f < 2^63, both exactly representable.
    (f.fract() == 0.0 && (i64::MIN as f64..-(i64::MIN as f64)).contains(&f)).then_some(f as i64)
}

/// The `(z, x, y)` of a row's key columns, rows counted from the top; `None` for a key outside
/// the tile grid.
fn xyz(z: Option<i64>, x: Option<i64>, row: Option<i64>) -> Option<(u8, u32, u32)> {
    let (z, x, row) = (z?, x?, row?);
    ((0..=30).contains(&z) && (0..1i64 << z).contains(&x) && (0..1i64 << z).contains(&row))
        .then(|| (z as u8, x as u32, ((1i64 << z) - 1 - row) as u32))
}

/// The root of an index on `table` whose leading columns are `want`, as written (an automatic
/// index of a `UNIQUE` constraint, or a `CREATE INDEX` on plain columns).
fn find_index(schema: &[SchemaEntry], table: &str, table_sql: &str, want: &[usize]) -> Option<u32> {
    // An index ordered otherwise than the search compares (a column's own collation) is not used.
    if want.iter().any(|&c| reader::column_has_collation(table_sql, c)) {
        return None;
    }
    // Nor an automatic index of a constraint with a descending column or a collation (such as
    // `UNIQUE (a, b COLLATE NOCASE)`), which `autoindex_columns` does not report.
    let unusual = without_quoted(table_sql)
        .to_ascii_uppercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| w == "DESC" || w == "COLLATE");
    let automatic = if unusual { Vec::new() } else { reader::autoindex_columns(table_sql) };
    schema.iter().filter(|e| e.0 == "index" && e.2.eq_ignore_ascii_case(table) && e.3 != 0).find_map(|(_, name, _, root, sql)| {
        let columns = if sql.trim().is_empty() {
            let n: usize = name.rsplit('_').next()?.parse().ok()?;
            automatic.get(n.checked_sub(1)?)?.clone()
        } else {
            reader::index_columns(sql, table_sql)?
        };
        columns.starts_with(want).then_some(*root)
    })
}

/// `sql` without its quoted strings and identifiers, so that a word in them (`DEFAULT 'desc'`)
/// is not taken for a keyword.
fn without_quoted(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut quote = None;
    for c in sql.chars() {
        match quote {
            // A doubled quote ends one quoted span and starts the next, dropping both.
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
            None if c == '[' => quote = Some(']'),
            None => out.push(c),
        }
    }
    out
}

/// The position of each of `names` among a table's columns.
fn column_positions(table: &str, sql: &str, names: &[&str]) -> Result<Vec<usize>> {
    let columns: Vec<String> = reader::column_types(sql).into_iter().map(|(n, _)| n).collect();
    names
        .iter()
        .map(|name| columns.iter().position(|c| c.eq_ignore_ascii_case(name)).ok_or_else(|| bad(format!("no {name} column in {table}"))))
        .collect()
}

impl Rows {
    /// The rows of `table`, whose tiles' bytes or ids are in the column `value`.
    fn new(db: &Db, schema: &[SchemaEntry], table: &str, value: &str) -> Result<Self> {
        let (root, sql) = db.table_root(table).map_err(|_| bad(format!("no {table} table")))?;
        let columns = column_positions(table, &sql, &["zoom_level", "tile_column", "tile_row", value])?;
        let alias = reader::rowid_column(&sql);
        let keys = match find_index(schema, table, &sql, &columns[..3]) {
            Some(index) => Keys::Index(index),
            None => {
                let mut rowids = HashMap::new();
                db.scan_columns(root, &columns[..3], &mut |rowid, mut v| {
                    for (i, &c) in columns[..3].iter().enumerate() {
                        if alias == Some(c) {
                            v[i] = Value::Int(rowid);
                        }
                    }
                    // The last row of a key, as a lookup in a non-unique index finds it, even if
                    // it holds no tile.
                    if let Some(key) = xyz(int(v.first()), int(v.get(1)), int(v.get(2))) {
                        rowids.insert(key, rowid);
                    }
                })?;
                Keys::Scanned(rowids)
            }
        };
        Ok(Self { root, value: columns[3], alias, keys })
    }
}

impl Images {
    /// The `images` table of a `map` and `images` layout.
    fn new(db: &Db, schema: &[SchemaEntry]) -> Result<Self> {
        let (root, sql) = db.table_root("images").map_err(|_| bad("no images table"))?;
        let columns = column_positions("images", &sql, &["tile_data", "tile_id"])?;
        let id = columns[1];
        let ids = if reader::rowid_column(&sql) == Some(id) {
            Ids::Rowid
        } else if let Some(index) = find_index(schema, "images", &sql, &[id]) {
            Ids::Index(index)
        } else {
            let mut rowids = HashMap::new();
            db.scan_columns(root, &[id], &mut |rowid, v| {
                // A NULL id matches nothing, as in the view's join.
                if v[0] != Value::Null {
                    rowids.insert(id_key(&v[0]), rowid);
                }
            })?;
            Ids::Scanned(rowids)
        };
        let kind = reader::column_types(&sql).get(id).map(|(_, t)| t.to_ascii_uppercase()).unwrap_or_default();
        let textual = ["CHAR", "CLOB", "TEXT"].iter().any(|s| kind.contains(s)) || kind.is_empty() || kind.contains("BLOB");
        let numeric = kind.contains("INT") || !textual;
        Ok(Self { root, data: columns[0], ids, numeric })
    }

    /// The rowid of the image `id`, matching a number and the text spelling it.
    fn rowid(&self, db: &Db, id: &Value) -> Result<Option<i64>> {
        // `images.tile_id = map.tile_id` is never true of a NULL id.
        if *id == Value::Null {
            return Ok(None);
        }
        match &self.ids {
            Ids::Rowid => Ok(match id {
                Value::Int(i) => Some(*i),
                // Text compared with the integer rowid is taken as a number, as SQLite does.
                Value::Text(s) => s.trim().parse().ok().or_else(|| s.trim().parse::<f64>().ok().and_then(whole)),
                Value::Real(f) => whole(*f),
                _ => None,
            }),
            Ids::Scanned(rowids) => Ok(spellings(id, self.numeric).iter().find_map(|id| rowids.get(&id_key(id)).copied())),
            Ids::Index(root) => {
                for id in spellings(id, self.numeric) {
                    if let Some(rowid) = last_rowid(db, *root, &[id])? {
                        return Ok(Some(rowid));
                    }
                }
                Ok(None)
            }
        }
    }
}

/// The rowid of the last record of an index whose leading values are `prefix`: of rows sharing
/// a key, the last, as the archive was read before (rows read in order, the last one kept).
fn last_rowid(db: &Db, root: u32, prefix: &[Value]) -> Result<Option<i64>> {
    let mut found: Option<i64> = None;
    db.index_prefix(root, prefix, &mut |record| {
        // The greatest rowid, as the scanned ids keep it, whatever the index's trailing columns.
        if let Some(rowid) = record.last().and_then(Value::as_i64) {
            found = Some(found.map_or(rowid, |f| f.max(rowid)));
        }
        true
    })?;
    Ok(found)
}

/// Column `c` of the row `rowid` of the table b-tree rooted at `root`; `alias` is the column that
/// is the rowid, stored as NULL.
fn row_value(db: &Db, root: u32, rowid: i64, c: usize, alias: Option<usize>) -> Result<Option<Value>> {
    let Some(payload) = db.lookup(root, rowid)? else { return Ok(None) };
    if alias == Some(c) {
        return Ok(Some(Value::Int(rowid)));
    }
    Ok(db.decode_record(&payload)?.into_iter().nth(c))
}

impl MbTiles {
    /// Opens an MBTiles file in place: only its schema, metadata and (for archives without an
    /// index on their tile keys) the key columns of its tiles are read now. The exception is an
    /// `images` table without an index on `tile_id`: see [`Ids::Scanned`].
    pub(crate) fn open(path: &std::path::Path) -> Result<Self> {
        Self::from_db(Db::open(path)?)
    }

    /// Reads an MBTiles file from its bytes.
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::from_db(Db::new(bytes)?)
    }

    fn from_db(db: Db) -> Result<Self> {
        let md = db.table("metadata").map_err(|_| bad("no metadata table"))?;
        let metadata = md
            .rows
            .iter()
            .filter_map(|r| {
                let name = md.get(r, "name").and_then(Value::as_str)?;
                let value = match md.get(r, "value") {
                    Some(Value::Text(s)) => s.clone(),
                    Some(Value::Int(i)) => i.to_string(),
                    Some(Value::Real(f)) => f.to_string(),
                    _ => String::new(),
                };
                Some((name.to_string(), value))
            })
            .collect();
        let schema = db.schema()?;
        let is_view = schema.iter().any(|(kind, name, ..)| kind == "view" && name.eq_ignore_ascii_case("tiles"));
        // The usual deduplicating layout: `map` rows point to `images` by tile_id.
        let (rows, images) = if is_view {
            (Rows::new(&db, &schema, "map", "tile_id")?, Some(Images::new(&db, &schema)?))
        } else {
            (Rows::new(&db, &schema, "tiles", "tile_data")?, None)
        };
        let mut m = Self { metadata, db, rows, images, zooms: None };
        m.zooms = m.find_zoom_range()?;
        Ok(m)
    }

    /// A metadata item.
    pub(crate) fn item(&self, name: &str) -> Option<&str> {
        self.metadata.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The zoom levels: from `minzoom` and `maxzoom` when the metadata has them, as GDAL reads
    /// them, otherwise from the tiles.
    pub(crate) fn zoom_range(&self) -> Option<(u8, u8)> {
        self.zooms
    }

    fn find_zoom_range(&self) -> Result<Option<(u8, u8)>> {
        let md = |n: &str| self.item(n).and_then(|v| v.trim().parse::<u8>().ok());
        if let (Some(a), Some(b)) = (md("minzoom"), md("maxzoom")) {
            return Ok(Some((a, b)));
        }
        let zooms: Vec<u8> = match &self.rows.keys {
            Keys::Scanned(rowids) => rowids.keys().map(|k| k.0).collect(),
            Keys::Index(_) => {
                let mut zooms = Vec::new();
                for z in 0..=30 {
                    if self.first_key(z)?.is_some() {
                        zooms.push(z);
                    }
                }
                zooms
            }
        };
        Ok(zooms.iter().min().zip(zooms.iter().max()).map(|(&a, &b)| (a, b)))
    }

    /// The rowid of the tile `(z, x, y)`.
    fn rowid(&self, z: u8, x: u32, y: u32) -> Result<Option<i64>> {
        if z > 30 || u64::from(x) >= 1 << z || u64::from(y) >= 1 << z {
            return Ok(None);
        }
        match &self.rows.keys {
            Keys::Scanned(rowids) => Ok(rowids.get(&(z, x, y)).copied()),
            Keys::Index(root) => {
                let row = (1i64 << z) - 1 - i64::from(y);
                last_rowid(&self.db, *root, &[Value::Int(z.into()), Value::Int(x.into()), Value::Int(row)])
            }
        }
    }

    /// A tile's bytes (as stored: vector tiles are usually gzipped).
    pub(crate) fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        let Some(rowid) = self.rowid(z, x, y)? else { return Ok(None) };
        let Some(value) = row_value(&self.db, self.rows.root, rowid, self.rows.value, self.rows.alias)? else { return Ok(None) };
        let data = match &self.images {
            None => Some(value),
            Some(images) => match images.rowid(&self.db, &value)? {
                Some(rowid) => row_value(&self.db, images.root, rowid, images.data, None)?,
                None => None,
            },
        };
        Ok(match data {
            Some(Value::Blob(b)) => Some(b),
            _ => None,
        })
    }

    /// Visits the keys of the tiles at zoom `z`, in the index's order, until `visit` returns
    /// `false`.
    fn keys_at_zoom(&self, root: u32, z: u8, visit: &mut dyn FnMut((u32, u32)) -> bool) -> Result<()> {
        self.db.index_prefix(root, &[Value::Int(z.into())], &mut |r| match xyz(Some(z.into()), int(r.get(1)), int(r.get(2))) {
            Some((_, x, y)) => visit((x, y)),
            None => true,
        })
    }

    /// The first key at zoom `z`, in the index's order.
    fn first_key(&self, z: u8) -> Result<Option<(u32, u32)>> {
        let Keys::Index(root) = self.rows.keys else { return Ok(None) };
        let mut first = None;
        self.keys_at_zoom(root, z, &mut |k| {
            first = Some(k);
            false
        })?;
        Ok(first)
    }

    /// The tiles `(x, y)` at zoom `z` (the rows of the `tiles` table, or of `map`, whether or not
    /// they have an image), by row then column.
    pub(crate) fn tiles_at_zoom(&self, z: u8) -> Result<Vec<(u32, u32)>> {
        let mut v: Vec<(u32, u32)> = match &self.rows.keys {
            Keys::Scanned(rowids) => rowids.keys().filter(|k| k.0 == z).map(|k| (k.1, k.2)).collect(),
            Keys::Index(root) => {
                let mut v = Vec::new();
                self.keys_at_zoom(*root, z, &mut |k| {
                    v.push(k);
                    true
                })?;
                v
            }
        };
        v.sort_by_key(|&(x, y)| (y, x));
        v.dedup();
        Ok(v)
    }

    /// A tile of the archive at zoom `z`: the first that has bytes of its first few, in the
    /// order of the index on tile keys (by column, then stored (TMS) row).
    pub(crate) fn any_tile(&self, z: u8) -> Option<Vec<u8>> {
        const TRIED: usize = 16;
        let keys: Vec<(u32, u32)> = match &self.rows.keys {
            Keys::Scanned(rowids) => {
                let mut keys: Vec<(u32, u32)> = rowids.keys().filter(|k| k.0 == z).map(|k| (k.1, k.2)).collect();
                keys.sort_by_key(|&(x, y)| (x, std::cmp::Reverse(y)));
                keys.truncate(TRIED);
                keys
            }
            Keys::Index(root) => {
                let mut keys = Vec::new();
                self.keys_at_zoom(*root, z, &mut |k| {
                    keys.push(k);
                    keys.len() < TRIED
                })
                .ok()?;
                keys
            }
        };
        // Errors are passed over on purpose: this only finds a sample tile to sniff the format
        // from, and `tile` reports a damaged tile when it is read.
        keys.into_iter().find_map(|(x, y)| self.tile(z, x, y).ok().flatten())
    }
}

/// A `tile_id` as the scanned ids are keyed: its text spelling, and whether it is a blob (which
/// equals no text).
type IdKey = (bool, String);

fn id_key(v: &Value) -> IdKey {
    match v {
        Value::Text(s) => (false, s.clone()),
        Value::Int(i) => (false, i.to_string()),
        Value::Real(f) => (false, real_text(*f)),
        Value::Blob(b) => (true, b.iter().map(|x| format!("{x:02x}")).collect()),
        Value::Null => (false, String::new()),
    }
}

/// The values an `images.tile_id` equal to `id` may be stored as: `id`, the texts spelling a
/// number, and, when `numeric` (the column's affinity), the number a text spells. A text id
/// compared with a text column is compared as text, so `'01'` is not `'1'`.
fn spellings(id: &Value, numeric: bool) -> Vec<Value> {
    let mut out = vec![id.clone()];
    match id {
        Value::Int(i) => out.extend([Value::Text(i.to_string()), Value::Text(format!("{i}.0"))]),
        Value::Real(f) => {
            out.push(Value::Text(real_text(*f)));
            if f.to_string() != real_text(*f) {
                out.push(Value::Text(f.to_string()));
            }
        }
        Value::Text(s) if numeric => match s.parse::<i64>() {
            Ok(i) => out.extend([Value::Int(i), Value::Real(i as f64)]),
            Err(_) => out.extend(s.parse::<f64>().ok().filter(|f| f.is_finite()).map(Value::Real)),
        },
        _ => {}
    }
    out
}

/// Writes an MBTiles file as GDAL lays it out: a `metadata (name, value)` table and a `tiles`
/// table with a unique index on `(zoom_level, tile_column, tile_row)`. Tiles are `(z, x, y,
/// bytes)` with rows counted from the top; they are stored counted from the bottom.
pub(crate) fn write(metadata: &[(String, String)], mut tiles: Vec<(u8, u32, u32, Vec<u8>)>) -> Result<Vec<u8>> {
    use writer::Value as W;
    tiles.sort_by_key(|t| (t.0, t.1, t.2));
    let rows = tiles
        .into_iter()
        .enumerate()
        .map(|(i, (z, x, y, data))| {
            let row = (1i64 << z) - 1 - y as i64;
            (i as i64 + 1, vec![W::Int(z as i64), W::Int(x as i64), W::Int(row), W::Blob(data)])
        })
        .collect();
    let md = metadata.iter().enumerate().map(|(i, (k, v))| (i as i64 + 1, vec![W::Text(k.clone()), W::Text(v.clone())])).collect();
    let tables = [
        Table { name: "metadata".into(), sql: "CREATE TABLE metadata (name TEXT, value TEXT)".into(), rows: md, autoindexes: vec![] },
        Table {
            name: "tiles".into(),
            sql: "CREATE TABLE tiles (zoom_level INTEGER NOT NULL,tile_column INTEGER NOT NULL,tile_row INTEGER NOT NULL,tile_data BLOB NOT NULL,UNIQUE (zoom_level, tile_column, tile_row) )".into(),
            rows,
            autoindexes: vec![vec![0, 1, 2]],
        },
    ];
    writer::write_database(&tables, &[], 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use writer::Value as W;

    fn md() -> Vec<(String, String)> {
        vec![("name".to_string(), "t".to_string()), ("format".to_string(), "png".to_string())]
    }

    /// `tiles` rows `(z, x, tms_row, bytes)` with the given table SQL and automatic indexes.
    fn tiles_table(sql: &str, autoindexes: Vec<Vec<usize>>, tiles: &[(i64, i64, i64, Vec<u8>)]) -> Table {
        let rows = tiles
            .iter()
            .enumerate()
            .map(|(i, t)| (i as i64 + 1, vec![W::Int(t.0), W::Int(t.1), W::Int(t.2), W::Blob(t.3.clone())]))
            .collect();
        Table { name: "tiles".into(), sql: sql.into(), rows, autoindexes }
    }

    fn metadata_table() -> Table {
        let rows = md().into_iter().enumerate().map(|(i, (k, v))| (i as i64 + 1, vec![W::Text(k), W::Text(v)])).collect();
        Table { name: "metadata".into(), sql: "CREATE TABLE metadata (name TEXT, value TEXT)".into(), rows, autoindexes: vec![] }
    }

    #[test]
    fn round_trips() {
        let tiles: Vec<_> = (0..300u32).map(|i| (9u8, i, 511 - i, vec![i as u8; 1000])).collect();
        let bytes = write(&md(), tiles).unwrap();
        let m = MbTiles::from_bytes(bytes).unwrap();
        assert!(matches!(m.rows.keys, Keys::Index(_)));
        assert_eq!(m.item("format"), Some("png"));
        assert_eq!(m.tile(9, 5, 506).unwrap(), Some(vec![5u8; 1000]));
        assert_eq!(m.tile(9, 5, 505).unwrap(), None);
        assert_eq!(m.tile(8, 5, 506).unwrap(), None);
        assert_eq!(m.tiles_at_zoom(9).unwrap().len(), 300);
        assert_eq!(m.zoom_range(), Some((9, 9)));
    }

    /// An index of many levels finds each tile and lists each zoom level, as a scan of the
    /// whole table does.
    #[test]
    fn index_lookups_match_a_scan() {
        let mut tiles = Vec::new();
        for z in 0..=7u8 {
            for x in 0..(1u32 << z) {
                for y in 0..(1u32 << z) {
                    tiles.push((z, x, y, format!("{z}/{x}/{y}").into_bytes()));
                }
            }
        }
        let indexed = MbTiles::from_bytes(write(&[], tiles.clone()).unwrap()).unwrap();
        assert!(matches!(indexed.rows.keys, Keys::Index(_)));
        let rows: Vec<_> = tiles.iter().map(|t| (t.0 as i64, t.1 as i64, (1i64 << t.0) - 1 - t.2 as i64, t.3.clone())).collect();
        let plain =
            tiles_table("CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB)", vec![], &rows);
        let scanned = MbTiles::from_bytes(writer::write_database(&[metadata_table(), plain], &[], 0, 0).unwrap()).unwrap();
        assert!(matches!(scanned.rows.keys, Keys::Scanned(_)));
        for m in [&indexed, &scanned] {
            assert_eq!(m.zoom_range(), Some((0, 7)));
            for (z, x, y, data) in &tiles {
                assert_eq!(m.tile(*z, *x, *y).unwrap().as_ref(), Some(data));
            }
            for z in 0..=7u8 {
                let n = 1u32 << z;
                let mut want: Vec<(u32, u32)> = (0..n).flat_map(|y| (0..n).map(move |x| (x, y))).collect();
                want.sort_by_key(|&(x, y)| (y, x));
                assert_eq!(m.tiles_at_zoom(z).unwrap(), want);
            }
            assert_eq!(m.tiles_at_zoom(8).unwrap(), vec![]);
            assert_eq!(m.tile(8, 0, 0).unwrap(), None);
            assert_eq!(m.tile(2, 4, 0).unwrap(), None);
            assert_eq!(m.any_tile(3), Some(b"3/0/7".to_vec()));
        }
    }

    /// Opening an archive reads none of its tiles' bytes: with an index nothing of the tiles
    /// table, and without one only the key columns kept on its leaf pages, not overflow pages.
    #[test]
    fn opening_reads_no_tile_bytes() {
        let rows: Vec<_> = (0..16i64).map(|i| (4, i, 0, vec![0x40 + i as u8; 20_000])).collect();
        for (sql, auto) in [
            ("CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB)", vec![]),
            (
                "CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB, UNIQUE (zoom_level, tile_column, tile_row))",
                vec![vec![0, 1, 2]],
            ),
        ] {
            let mut bytes = writer::write_database(&[metadata_table(), tiles_table(sql, auto, &rows)], &[], 0, 0).unwrap();
            // Break the overflow chain of tile 3: its pages of bytes 0x43 point past the file.
            let page = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
            for p in (page..bytes.len()).step_by(page) {
                if bytes[p + 4..p + page].iter().all(|&b| b == 0x43) {
                    bytes[p..p + 4].copy_from_slice(&u32::MAX.to_be_bytes());
                }
            }
            let m = MbTiles::from_bytes(bytes).unwrap();
            assert_eq!(m.tile(4, 2, 15).unwrap(), Some(vec![0x42; 20_000]));
            assert!(m.tile(4, 3, 15).is_err());
            assert_eq!(m.tiles_at_zoom(4).unwrap().len(), 16);
        }
    }

    /// The deduplicating layout: `map` rows name their image in `images` by `tile_id`, found by
    /// an index, as the rowid, or by a scan, as a number or the text spelling it.
    #[test]
    fn reads_map_and_images() {
        // Four tiles on the diagonal, and tile (0, 3) whose id is NULL, which matches no image.
        let map_rows = |id: fn(i64) -> W| {
            let mut rows: Vec<_> = (0..4i64).map(|i| (i + 1, vec![W::Int(2), W::Int(i), W::Int(3 - i), id(i % 2)])).collect();
            rows.push((5, vec![W::Int(2), W::Int(0), W::Int(0), W::Null]));
            rows
        };
        // Two images, and one whose id is NULL.
        let image_rows = |id: fn(i64) -> W| {
            let mut rows: Vec<_> = (0..2i64).map(|i| (i + 1, vec![W::Blob(vec![i as u8 + 1; 5000]), id(i)])).collect();
            rows.push((3, vec![W::Blob(vec![9; 10]), W::Null]));
            rows
        };
        let text: fn(i64) -> W = |i| W::Text(format!("{i}"));
        let int: fn(i64) -> W = W::Int;
        let real: fn(i64) -> W = |i| W::Real(i as f64);
        let real_text: fn(i64) -> W = |i| W::Text(format!("{i}.0"));
        let int_text = real_text;
        let view = writer::SchemaObject {
            kind: "view",
            name: "tiles".into(),
            table: "tiles".into(),
            sql: "CREATE VIEW tiles AS SELECT zoom_level, tile_column, tile_row, tile_data FROM map JOIN images ON images.tile_id = map.tile_id".into(),
        };
        // (map SQL, images SQL, images' automatic indexes, map's ids, images' ids)
        type Case = (&'static str, &'static str, Vec<Vec<usize>>, fn(i64) -> W, fn(i64) -> W);
        let cases: [Case; 10] = [
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id TEXT, UNIQUE (zoom_level, tile_column, tile_row))",
                "CREATE TABLE images (tile_data BLOB, tile_id TEXT UNIQUE)",
                vec![vec![1]],
                text,
                text,
            ),
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id INTEGER)",
                "CREATE TABLE images (tile_data BLOB, tile_id TEXT UNIQUE)",
                vec![vec![1]],
                int,
                text,
            ),
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id INTEGER)",
                "CREATE TABLE images (tile_data BLOB, tile_id TEXT)",
                vec![],
                int,
                text,
            ),
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id TEXT)",
                "CREATE TABLE images (tile_data BLOB, tile_id INTEGER PRIMARY KEY)",
                vec![],
                text,
                |_| W::Null,
            ),
            // REAL ids, matched to the text SQLite spells them in, by index and by scan.
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id REAL)",
                "CREATE TABLE images (tile_data BLOB, tile_id TEXT UNIQUE)",
                vec![vec![1]],
                real,
                real_text,
            ),
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id REAL)",
                "CREATE TABLE images (tile_data BLOB, tile_id TEXT)",
                vec![],
                real,
                real_text,
            ),
            // A whole REAL id names the image whose rowid it is.
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id REAL)",
                "CREATE TABLE images (tile_data BLOB, tile_id INTEGER PRIMARY KEY)",
                vec![],
                real,
                |_| W::Null,
            ),
            // Integer ids, matched to text spelling them as reals, by index and by scan.
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id INTEGER)",
                "CREATE TABLE images (tile_data BLOB, tile_id TEXT UNIQUE)",
                vec![vec![1]],
                int,
                int_text,
            ),
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id INTEGER)",
                "CREATE TABLE images (tile_data BLOB, tile_id TEXT)",
                vec![],
                int,
                int_text,
            ),
            // A text id spelling a whole real names the image whose rowid it is.
            (
                "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id TEXT)",
                "CREATE TABLE images (tile_data BLOB, tile_id INTEGER PRIMARY KEY)",
                vec![],
                real_text,
                |_| W::Null,
            ),
        ];
        for (map_sql, images_sql, image_index, map_id, image_id) in cases {
            let map_auto = reader::autoindex_columns(map_sql);
            let mut images = image_rows(image_id);
            if images_sql.contains("PRIMARY KEY") {
                // The rowids are the ids, from 0.
                images = images.into_iter().map(|(i, r)| (i - 1, r)).collect();
            }
            let tables = [
                metadata_table(),
                Table { name: "map".into(), sql: map_sql.into(), rows: map_rows(map_id), autoindexes: map_auto },
                Table { name: "images".into(), sql: images_sql.into(), rows: images, autoindexes: image_index },
            ];
            let m = MbTiles::from_bytes(writer::write_database(&tables, std::slice::from_ref(&view), 0, 0).unwrap()).unwrap();
            for x in 0..4u32 {
                assert_eq!(m.tile(2, x, x).unwrap(), Some(vec![(x % 2) as u8 + 1; 5000]), "{map_sql} {images_sql}");
            }
            assert_eq!(m.tile(2, 0, 1).unwrap(), None);
            assert_eq!(m.tile(2, 0, 3).unwrap(), None, "{map_sql} {images_sql}");
            assert_eq!(m.tiles_at_zoom(2).unwrap(), vec![(0, 0), (1, 1), (2, 2), (0, 3), (3, 3)]);
        }
        // Text ids are compared as text with a text column, by index and by scan: '01' is not '1'.
        for image_index in [vec![vec![1]], vec![]] {
            let tables = [
                metadata_table(),
                Table {
                    name: "map".into(),
                    sql: "CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id TEXT)".into(),
                    rows: vec![(1, vec![W::Int(0), W::Int(0), W::Int(0), W::Text("01".into())])],
                    autoindexes: vec![],
                },
                Table {
                    name: "images".into(),
                    sql: "CREATE TABLE images (tile_data BLOB, tile_id TEXT UNIQUE)".into(),
                    rows: vec![(1, vec![W::Blob(vec![1; 3]), W::Text("1".into())])],
                    autoindexes: image_index,
                },
            ];
            let m = MbTiles::from_bytes(writer::write_database(&tables, std::slice::from_ref(&view), 0, 0).unwrap()).unwrap();
            assert_eq!(m.tile(0, 0, 0).unwrap(), None);
        }
    }

    fn tiles_with(sql: &str, autoindexes: Vec<Vec<usize>>, indexes: &[writer::Index], rows: Vec<(i64, Vec<W>)>) -> MbTiles {
        let tiles = Table { name: "tiles".into(), sql: sql.into(), rows, autoindexes };
        MbTiles::from_bytes(writer::write_database_with_indexes(&[metadata_table(), tiles], indexes, &[], 0, 0).unwrap()).unwrap()
    }

    /// A local file is mapped, not read, and its tiles are read from the map.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn local_files_are_mapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mbtiles");
        let tiles: Vec<_> = (0..4u32).map(|i| (2u8, i, i, vec![i as u8; 9000])).collect();
        std::fs::write(&path, write(&md(), tiles).unwrap()).unwrap();
        let m = MbTiles::open(&path).unwrap();
        assert!(m.db.is_mapped());
        assert!(matches!(m.rows.keys, Keys::Index(_)));
        assert_eq!(m.tile(2, 3, 3).unwrap(), Some(vec![3; 9000]));
    }

    /// Of rows sharing a key, the last is read, with and without an index; keys that are not
    /// whole numbers name no tile, and are not listed.
    #[test]
    fn duplicate_and_fractional_keys() {
        let sql = "CREATE TABLE tiles (zoom_level INTEGER, tile_column, tile_row INTEGER, tile_data BLOB)";
        let rows = vec![
            (1, vec![W::Int(1), W::Int(0), W::Int(1), W::Blob(vec![1])]),
            (2, vec![W::Int(1), W::Int(0), W::Int(1), W::Blob(vec![2])]),
            (3, vec![W::Int(1), W::Real(1.0), W::Int(1), W::Blob(vec![3])]),
            (4, vec![W::Int(1), W::Real(0.5), W::Int(0), W::Blob(vec![4])]),
        ];
        let index = writer::Index {
            name: "tile_index".into(),
            table: "tiles".into(),
            sql: "CREATE INDEX tile_index ON tiles (zoom_level, tile_column, tile_row)".into(),
            columns: vec![0, 1, 2],
        };
        let indexed = tiles_with(sql, vec![], &[index], rows.clone());
        let scanned = tiles_with(sql, vec![], &[], rows);
        assert!(matches!(indexed.rows.keys, Keys::Index(_)));
        assert!(matches!(scanned.rows.keys, Keys::Scanned(_)));
        for m in [indexed, scanned] {
            assert_eq!(m.tile(1, 0, 0).unwrap(), Some(vec![2]));
            assert_eq!(m.tile(1, 1, 0).unwrap(), Some(vec![3]));
            assert_eq!(m.tile(1, 0, 1).unwrap(), None);
            assert_eq!(m.tiles_at_zoom(1).unwrap(), vec![(0, 0), (1, 0)]);
        }
    }

    /// An index the search cannot follow (a descending constraint, a column's own collation)
    /// is not used: the keys are scanned instead.
    #[test]
    fn unsearchable_indexes_are_not_used() {
        let rows = vec![(1, vec![W::Int(0), W::Int(0), W::Int(0), W::Blob(vec![7])])];
        let desc = tiles_with(
            "CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB, UNIQUE (zoom_level, tile_column DESC, tile_row))",
            vec![vec![0, 1, 2]],
            &[],
            rows.clone(),
        );
        let collated = tiles_with(
            "CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER COLLATE NOCASE, tile_row INTEGER, tile_data BLOB, UNIQUE (zoom_level, tile_column, tile_row))",
            vec![vec![0, 1, 2]],
            &[],
            rows.clone(),
        );
        let constraint_collated = tiles_with(
            "CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB, UNIQUE (zoom_level, tile_column COLLATE NOCASE, tile_row))",
            vec![vec![0, 1, 2]],
            &[],
            rows,
        );
        for m in [desc, collated, constraint_collated] {
            assert!(matches!(m.rows.keys, Keys::Scanned(_)));
            assert_eq!(m.tile(0, 0, 0).unwrap(), Some(vec![7]));
        }
        // Nor a partial index, however its WHERE is spaced.
        let partial = writer::Index {
            name: "tile_index".into(),
            table: "tiles".into(),
            sql: "CREATE INDEX tile_index ON tiles (zoom_level, tile_column, tile_row) WHERE+zoom_level>0".into(),
            columns: vec![0, 1, 2],
        };
        let partial = tiles_with(
            "CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB)",
            vec![],
            &[partial],
            vec![(1, vec![W::Int(0), W::Int(0), W::Int(0), W::Blob(vec![7])])],
        );
        assert!(matches!(partial.rows.keys, Keys::Scanned(_)));
        assert_eq!(partial.tile(0, 0, 0).unwrap(), Some(vec![7]));
        // A keyword in a quoted string is not one.
        let quoted = tiles_with(
            "CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB, note TEXT DEFAULT 'desc collate', UNIQUE (zoom_level, tile_column, tile_row))",
            vec![vec![0, 1, 2]],
            &[],
            vec![(1, vec![W::Int(0), W::Int(0), W::Int(0), W::Blob(vec![7]), W::Null])],
        );
        assert!(matches!(quoted.rows.keys, Keys::Index(_)));
        assert_eq!(quoted.tile(0, 0, 0).unwrap(), Some(vec![7]));
    }

    /// Reals are spelled as SQLite spells them in text.
    #[test]
    fn reals_spelled_as_sqlite() {
        for (f, t) in [(1.0, "1.0"), (-0.0, "0.0"), (1.5, "1.5")] {
            assert_eq!(real_text(f), t);
        }
    }

    /// A `map.tile_id` that is the rowid (stored as NULL) names its image by the rowid.
    #[test]
    fn map_ids_that_are_rowids() {
        let view = writer::SchemaObject {
            kind: "view",
            name: "tiles".into(),
            table: "tiles".into(),
            sql: "CREATE VIEW tiles AS SELECT zoom_level, tile_column, tile_row, tile_data FROM map JOIN images USING (tile_id)".into(),
        };
        let map = Table {
            name: "map".into(),
            sql: "CREATE TABLE map (tile_id INTEGER PRIMARY KEY, zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER)".into(),
            rows: (1..=2i64).map(|i| (i, vec![W::Null, W::Int(1), W::Int(i - 1), W::Int(0)])).collect(),
            autoindexes: vec![],
        };
        let images = Table {
            name: "images".into(),
            sql: "CREATE TABLE images (tile_data BLOB, tile_id TEXT UNIQUE)".into(),
            rows: (1..=2i64).map(|i| (i, vec![W::Blob(vec![i as u8; 3]), W::Text(i.to_string())])).collect(),
            autoindexes: vec![vec![1]],
        };
        let m = MbTiles::from_bytes(writer::write_database(&[metadata_table(), map, images], &[view], 0, 0).unwrap()).unwrap();
        assert_eq!(m.tile(1, 0, 1).unwrap(), Some(vec![1; 3]));
        assert_eq!(m.tile(1, 1, 1).unwrap(), Some(vec![2; 3]));
    }
}
