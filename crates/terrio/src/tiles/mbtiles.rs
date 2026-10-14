//! MBTiles: tiles in an SQLite database, read and written with terrio's own SQLite reader and
//! writer (the ones GeoPackage uses).
//!
//! Tiles are in the `tiles` table, or in the `tiles` view over `map` and `images` that some
//! producers write; rows are numbered from the bottom (TMS), and are turned into XYZ rows here.
//! The `metadata` table holds `name`/`value` pairs (`format`, `bounds`, `minzoom`, `maxzoom`, and
//! for vector tiles the `json` item with `vector_layers`).

use crate::drivers::sqlite::{Db, Value};
use crate::drivers::vector::sqlite::{self as writer, Table};
use crate::{Error, Result};
use std::collections::HashMap;

/// An MBTiles file read into memory.
pub(crate) struct MbTiles {
    /// The `metadata` table, in file order.
    pub metadata: Vec<(String, String)>,
    /// Tile bytes by `(z, x, y)`, rows counted from the top.
    tiles: HashMap<(u8, u32, u32), Vec<u8>>,
}

impl std::fmt::Debug for MbTiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MBTiles").field("metadata", &self.metadata).field("tiles", &self.tiles.len()).finish()
    }
}

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("MBTiles: {msg}"))
}

impl MbTiles {
    /// Reads an MBTiles file from its bytes.
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let db = Db::new(bytes)?;
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
        let mut tiles = HashMap::new();
        let mut put = |z: Option<i64>, x: Option<i64>, row: Option<i64>, blob: Option<&Value>| {
            if let (Some(z), Some(x), Some(row), Some(Value::Blob(b))) = (z, x, row, blob)
                && (0..=30).contains(&z)
                && x >= 0
                && row >= 0
                && row < 1i64 << z
            {
                let y = (1i64 << z) - 1 - row;
                tiles.insert((z as u8, x as u32, y as u32), b.clone());
            }
        };
        if is_view {
            // The usual deduplicating layout: `map` rows point to `images` by tile_id.
            let map = db.table("map")?;
            let images = db.table("images")?;
            let by_id: HashMap<String, &Value> =
                images.rows.iter().filter_map(|r| Some((id_key(images.get(r, "tile_id")?), images.get(r, "tile_data")?))).collect();
            for r in &map.rows {
                let blob = map.get(r, "tile_id").and_then(|id| by_id.get(&id_key(id)).copied());
                let i = |c: &str| map.get(r, c).and_then(Value::as_i64);
                put(i("zoom_level"), i("tile_column"), i("tile_row"), blob);
            }
        } else {
            let t = db.table("tiles").map_err(|_| bad("no tiles table"))?;
            for r in &t.rows {
                let i = |c: &str| t.get(r, c).and_then(Value::as_i64);
                put(i("zoom_level"), i("tile_column"), i("tile_row"), t.get(r, "tile_data"));
            }
        }
        Ok(Self { metadata, tiles })
    }

    /// A metadata item.
    pub(crate) fn item(&self, name: &str) -> Option<&str> {
        self.metadata.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The zoom levels: from `minzoom` and `maxzoom` when the metadata has them, as GDAL reads
    /// them, otherwise from the tiles.
    pub(crate) fn zoom_range(&self) -> Option<(u8, u8)> {
        let md = |n: &str| self.item(n).and_then(|v| v.trim().parse::<u8>().ok());
        if let (Some(a), Some(b)) = (md("minzoom"), md("maxzoom")) {
            return Some((a, b));
        }
        let min = self.tiles.keys().map(|k| k.0).min()?;
        let max = self.tiles.keys().map(|k| k.0).max()?;
        Some((min, max))
    }

    /// A tile's bytes (as stored: vector tiles are usually gzipped).
    pub(crate) fn tile(&self, z: u8, x: u32, y: u32) -> Option<&[u8]> {
        self.tiles.get(&(z, x, y)).map(Vec::as_slice)
    }

    /// The tiles `(x, y)` at zoom `z`, by row then column.
    pub(crate) fn tiles_at_zoom(&self, z: u8) -> Vec<(u32, u32)> {
        let mut v: Vec<(u32, u32)> = self.tiles.keys().filter(|k| k.0 == z).map(|k| (k.1, k.2)).collect();
        v.sort_by_key(|&(x, y)| (y, x));
        v
    }

    /// The first tile of the archive, at the highest zoom level that has one.
    pub(crate) fn any_tile(&self, z: u8) -> Option<&[u8]> {
        let mut keys: Vec<&(u8, u32, u32)> = self.tiles.keys().filter(|k| k.0 == z).collect();
        keys.sort();
        keys.first().and_then(|k| self.tiles.get(k)).map(Vec::as_slice)
    }
}

fn id_key(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
        Value::Null => String::new(),
    }
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

    #[test]
    fn round_trips() {
        let md = vec![("name".to_string(), "t".to_string()), ("format".to_string(), "png".to_string())];
        let tiles: Vec<_> = (0..300u32).map(|i| (9u8, i, 511 - i, vec![i as u8; 1000])).collect();
        let bytes = write(&md, tiles).unwrap();
        let m = MbTiles::from_bytes(bytes).unwrap();
        assert_eq!(m.item("format"), Some("png"));
        assert_eq!(m.tile(9, 5, 506), Some(&[5u8; 1000][..]));
        assert_eq!(m.tiles_at_zoom(9).len(), 300);
        assert_eq!(m.zoom_range(), Some((9, 9)));
    }
}
