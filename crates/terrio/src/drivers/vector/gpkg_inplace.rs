//! GeoPackage updates in place, as GDAL makes them through SQLite: features appended to a
//! layer's table (new b-tree cells and pages), its R-tree index grown by inserting boxes and
//! splitting nodes, and the tables describing layers (`gpkg_contents`, `gpkg_ogr_contents`,
//! `sqlite_sequence` and others) changed; layers dropped (their pages freed) and added. Only the
//! pages that change are written, in a transaction protected by a rollback journal
//! ([`Editor`]), so appending to a large file costs about as much as the features appended.
//!
//! Files that cannot be changed this way (see [`Editor::open`], or a layer with triggers or
//! indexes this module does not maintain) are left untouched, and [`update_in_place`] says so.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use super::gpkg_update::{self as gu, Database, Encoded, UpdateMode};
use super::sqlite::{self as w, SchemaObject, Table};
use crate::drivers::sqlite::{self as r, Db};
use crate::drivers::sqlite_edit::{Editor, NotInPlace};
use crate::{Error, Result};
use wbvector::Layer;

/// The tables describing layers, read whole and rewritten when they change.
const SMALL_TABLES: [&str; 8] = [
    "gpkg_spatial_ref_sys",
    "gpkg_contents",
    "gpkg_geometry_columns",
    "gpkg_ogr_contents",
    "gpkg_extensions",
    "gpkg_metadata_reference",
    "gpkg_data_columns",
    "sqlite_sequence",
];

/// An entry of the schema table.
#[derive(Debug, Clone)]
struct Entry {
    rowid: i64,
    kind: String,
    name: String,
    table: String,
    root: u32,
    sql: String,
    /// The record as stored, rewritten unchanged.
    values: Vec<w::Value>,
}

type Outcome = std::result::Result<(), NotInPlace>;

/// A small table's name and rows as read, to tell whether they changed.
type Loaded = (String, Vec<(i64, Vec<w::Value>)>);

fn not_in_place(why: impl Into<String>) -> Result<Outcome> {
    Ok(Err(NotInPlace(why.into())))
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Updates a GeoPackage file in place: see [`gu::update`]. `Ok(false)` when the file cannot be
/// changed in place; it is then untouched.
pub(super) fn update_in_place(path: &Path, layers: &[Layer], mode: UpdateMode) -> Result<bool> {
    let mut ed = match Editor::open(path)? {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };
    match apply(&mut ed, layers, mode)? {
        Ok(()) => {
            ed.commit()?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

fn apply(ed: &mut Editor, layers: &[Layer], mode: UpdateMode) -> Result<Outcome> {
    // The schema.
    let mut entries = Vec::new();
    for (rowid, payload) in ed.scan(1)? {
        let values: Vec<w::Value> = Db::record_values(&payload).into_iter().map(gu::to_w).collect();
        let text = |i: usize| values.get(i).and_then(gu::text).unwrap_or_default().to_string();
        let root = values.get(3).and_then(gu::int).unwrap_or(0) as u32;
        entries.push(Entry { rowid, kind: text(0), name: text(1), table: text(2), root, sql: text(4), values: values.clone() });
    }
    if !entries.iter().any(|e| e.kind == "table" && eq(&e.name, "gpkg_contents")) {
        return Err(Error::InvalidData("not a GeoPackage: no gpkg_contents table".into()));
    }

    // The small tables, and every trigger, view and virtual table.
    let mut tables = Vec::new();
    let mut objects = Vec::new();
    for e in &entries {
        match e.kind.as_str() {
            "table" if e.root > 0 && SMALL_TABLES.iter().any(|t| eq(t, &e.name)) => {
                if gu::keywords_contain(&e.sql, "WITHOUT ROWID") {
                    return not_in_place(format!("'{}' is a WITHOUT ROWID table", e.name));
                }
                let rows =
                    ed.scan(e.root)?.into_iter().map(|(id, p)| (id, Db::record_values(&p).into_iter().map(gu::to_w).collect())).collect();
                tables.push(Table { name: e.name.clone(), sql: e.sql.clone(), rows, autoindexes: r::autoindex_columns(&e.sql) });
            }
            "table" if e.root == 0 => objects.push(object("table", e)),
            "trigger" => objects.push(object("trigger", e)),
            "view" => objects.push(object("view", e)),
            _ => {}
        }
    }
    let loaded: Vec<Loaded> = tables.iter().map(|t| (t.name.clone(), t.rows.clone())).collect();
    let user_version = ed.header_u32(60)?;
    let application_id = ed.header_u32(68)?;
    let mut d = Database { tables, indexes: Vec::new(), objects, user_version, application_id };

    // The vector layers.
    let existing: Vec<String> = d
        .table("gpkg_contents")
        .map(|t| {
            let cols = gu::columns(&t.sql);
            let name = cols.iter().position(|c| eq(c, "table_name"));
            let kind = cols.iter().position(|c| eq(c, "data_type"));
            t.rows
                .iter()
                .filter(|(_, v)| kind.and_then(|k| v.get(k)).and_then(gu::text).is_some_and(|k| eq(k, "features") || eq(k, "attributes")))
                .filter_map(|(_, v)| name.and_then(|n| v.get(n)).and_then(gu::text).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut dropped: BTreeSet<i64> = BTreeSet::new();
    for layer in layers {
        let found = existing.iter().find(|n| eq(n, &layer.name)).cloned();
        match (found, mode) {
            (Some(name), UpdateMode::Append) => {
                if let Err(e) = append(ed, &mut d, &entries, &dropped, &name, layer)? {
                    return Ok(Err(e));
                }
            }
            (Some(name), UpdateMode::Overwrite) => {
                let rtree = d.geometry_column(&name).map(|(g, ..)| format!("rtree_{name}_{g}"));
                for e in &entries {
                    if owned_by(e, &name, rtree.as_deref()) {
                        dropped.insert(e.rowid);
                    }
                }
                d.remove_layer(&name);
                d.add_layer(layer)?;
            }
            (None, _) => {
                let taken = entries.iter().any(|e| !dropped.contains(&e.rowid) && eq(&e.name, &layer.name)) || d.has(&layer.name);
                if taken {
                    return Err(Error::InvalidArgument(format!(
                        "the GeoPackage has a table named '{}' that is not a vector layer",
                        layer.name
                    )));
                }
                d.add_layer(layer)?;
            }
        }
    }
    write_back(ed, d, &entries, &loaded, &dropped)
}

fn object(kind: &'static str, e: &Entry) -> SchemaObject {
    SchemaObject { kind, name: e.name.clone(), table: e.table.clone(), sql: e.sql.clone() }
}

/// Whether a schema entry belongs to a layer: its table, R-tree (virtual and shadow tables),
/// triggers and indexes.
fn owned_by(e: &Entry, layer: &str, rtree: Option<&str>) -> bool {
    let rt = |n: &str| rtree.is_some_and(|rt| eq(n, rt) || ["_rowid", "_node", "_parent"].iter().any(|s| eq(n, &format!("{rt}{s}"))));
    eq(&e.name, layer) || rt(&e.name) || eq(&e.table, layer) || rt(&e.table)
}

/// The indexes of a table: `(root, columns)`, or why they cannot be maintained.
fn table_indexes(entries: &[Entry], table: &Entry) -> std::result::Result<Vec<(u32, Vec<usize>)>, NotInPlace> {
    let autos = r::autoindex_columns(&table.sql);
    let mut out = Vec::new();
    for e in entries.iter().filter(|e| e.kind == "index" && eq(&e.table, &table.name)) {
        let cols =
            if e.sql.is_empty() {
                let n =
                    e.name.rsplit('_').next().and_then(|n| n.parse::<usize>().ok()).filter(|_| {
                        e.name.to_ascii_lowercase().starts_with(&format!("sqlite_autoindex_{}_", table.name.to_ascii_lowercase()))
                    });
                n.and_then(|n| autos.get(n.wrapping_sub(1)).cloned())
            } else {
                r::index_columns(&e.sql, &table.sql)
            };
        match cols {
            Some(c) if e.root > 0 => out.push((e.root, c)),
            _ => return Err(NotInPlace(format!("index '{}' cannot be maintained", e.name))),
        }
    }
    Ok(out)
}

/// Appends the features of `layer` to the layer `name`, in place.
fn append(ed: &mut Editor, d: &mut Database, entries: &[Entry], dropped: &BTreeSet<i64>, name: &str, layer: &Layer) -> Result<Outcome> {
    let live = |e: &&Entry| !dropped.contains(&e.rowid);
    let Some(table) = entries.iter().filter(live).find(|e| e.kind == "table" && eq(&e.name, name)).cloned() else {
        return Err(Error::InvalidData(format!("GeoPackage: no table '{name}'")));
    };
    if table.root == 0 || gu::keywords_contain(&table.sql, "WITHOUT ROWID") {
        return not_in_place(format!("'{name}' is not a rowid table"));
    }
    let geometry = d.geometry_column(name);
    let rtree = geometry.as_ref().map(|(g, ..)| format!("rtree_{name}_{g}"));
    // Triggers run by SQLite on inserts are emulated: the R-tree's and GDAL's feature count.
    for e in entries.iter().filter(live).filter(|e| e.kind == "trigger" && eq(&e.table, name)) {
        let n = e.name.to_ascii_lowercase();
        let known = rtree.as_ref().is_some_and(|rt| n.starts_with(&format!("{}_", rt.to_ascii_lowercase())))
            || n == format!("trigger_insert_feature_count_{}", name.to_ascii_lowercase())
            || n == format!("trigger_delete_feature_count_{}", name.to_ascii_lowercase());
        if !known {
            return not_in_place(format!("trigger '{}'", e.name));
        }
    }
    let indexes = match table_indexes(entries, &table) {
        Ok(i) => i,
        Err(e) => return Ok(Err(e)),
    };
    let tree = match &rtree {
        Some(rt) if entries.iter().filter(live).any(|e| e.kind == "table" && eq(&e.name, &format!("{rt}_node"))) => {
            match Rtree::open(ed, entries, rt)? {
                Ok(t) => Some(t),
                Err(e) => return Ok(Err(e)),
            }
        }
        _ => None,
    };

    let next = d.sequence(name).max(ed.max_rowid(table.root)?) + 1;
    let enc: Encoded = gu::encode_append(&table.sql, geometry.as_ref(), layer, next);
    let records: Vec<(i64, Vec<u8>)> = crate::par::map_range(enc.rows.len(), |i| (enc.rows[i].0, w::record(&enc.rows[i].1)));
    ed.append(table.root, records)?;
    if !indexes.is_empty() {
        let rows: Vec<(i64, Vec<w::Value>)> =
            ed.scan(table.root)?.into_iter().map(|(id, p)| (id, Db::record_values(&p).into_iter().map(gu::to_w).collect())).collect();
        for (root, cols) in indexes {
            ed.rebuild_index(root, &w::index_keys(&rows, &cols))?;
        }
    }
    if let (Some(mut tree), Some(gi)) = (tree, enc.geom_col) {
        for (id, v) in &enc.rows {
            if let Some(w::Value::Blob(b)) = v.get(gi)
                && let Some(e) = gu::envelope(b)
            {
                tree.insert(ed, *id, gu::rtree_box(e))?;
            }
        }
        tree.flush(ed)?;
    }
    let count = d.lookup("gpkg_ogr_contents", "table_name", name, "feature_count").as_ref().and_then(gu::int);
    let total = match count {
        Some(c) => c + enc.rows.len() as i64,
        None => ed.count_rows(table.root)? as i64,
    };
    d.appended(name, &table.sql, &enc, total);
    Ok(Ok(()))
}

/// Writes the changed small tables, the dropped and added layers and the schema.
fn write_back(ed: &mut Editor, d: Database, entries: &[Entry], loaded: &[Loaded], dropped: &BTreeSet<i64>) -> Result<Outcome> {
    // Dropped tables and indexes free their pages.
    for e in entries.iter().filter(|e| dropped.contains(&e.rowid)) {
        if e.root > 0 && (e.kind == "table" || e.kind == "index") {
            ed.free_tree(e.root, false)?;
        }
    }

    // Small tables that changed, with their indexes.
    let mut new_tables = Vec::new();
    for mut t in d.tables {
        t.rows.sort_by_key(|(id, _)| *id);
        let Some((_, before)) = loaded.iter().find(|(n, _)| eq(n, &t.name)) else {
            new_tables.push(t);
            continue;
        };
        if *before == t.rows {
            continue;
        }
        let Some(entry) = entries.iter().find(|e| e.kind == "table" && eq(&e.name, &t.name)) else { continue };
        let indexes = match table_indexes(entries, entry) {
            Ok(i) => i,
            Err(e) => return Ok(Err(e)),
        };
        ed.rebuild_table(entry.root, &t.rows)?;
        for (root, cols) in indexes {
            ed.rebuild_index(root, &w::index_keys(&t.rows, &cols))?;
        }
    }

    // The schema: the entries kept, then the new tables, their indexes, and new objects.
    let mut schema: Vec<(i64, Vec<w::Value>)> =
        entries.iter().filter(|e| !dropped.contains(&e.rowid)).map(|e| (e.rowid, e.values.clone())).collect();
    let mut next = entries.iter().map(|e| e.rowid).max().unwrap_or(0) + 1;
    let mut add = |schema: &mut Vec<(i64, Vec<w::Value>)>, kind: &str, name: &str, table: &str, root: u32, sql: Option<&str>| {
        schema.push((
            next,
            vec![
                w::Value::Text(kind.into()),
                w::Value::Text(name.into()),
                w::Value::Text(table.into()),
                w::Value::Int(i64::from(root)),
                sql.map_or(w::Value::Null, |s| w::Value::Text(s.into())),
            ],
        ));
        next += 1;
    };
    let changed = !dropped.is_empty() || !new_tables.is_empty();
    // `sqlite_sequence` last, as SQLite creates it after the first AUTOINCREMENT table.
    new_tables.sort_by_key(|t| t.name == "sqlite_sequence");
    for t in &new_tables {
        let root = ed.new_table(&t.rows)?;
        add(&mut schema, "table", &t.name, &t.name, root, Some(&t.sql));
        for (i, cols) in t.autoindexes.iter().enumerate() {
            let root = ed.new_index(&w::index_keys(&t.rows, cols))?;
            add(&mut schema, "index", &format!("sqlite_autoindex_{}_{}", t.name, i + 1), &t.name, root, None);
        }
    }
    let live: Vec<&Entry> = entries.iter().filter(|e| !dropped.contains(&e.rowid)).collect();
    let mut objects_added = false;
    for o in &d.objects {
        let exists = live.iter().any(|e| e.kind == o.kind && eq(&e.name, &o.name) && e.root == 0);
        if !exists {
            add(&mut schema, o.kind, &o.name, &o.table, 0, Some(&o.sql));
            objects_added = true;
        }
    }
    if changed || objects_added {
        schema.sort_by_key(|(id, _)| *id);
        ed.rebuild_table(1, &schema)?;
        ed.schema_changed = true;
    }
    Ok(Ok(()))
}

/// An R-tree cell: a feature id or child node, and its box `[minx, maxx, miny, maxy]`.
type Cell = (i64, [f32; 4]);

fn union(cells: &[Cell]) -> [f32; 4] {
    cells.iter().fold([f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY], |a, (_, b)| grow(a, *b))
}

fn grow(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0].min(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].max(b[3])]
}

fn area(b: [f32; 4]) -> f64 {
    (f64::from(b[1]) - f64::from(b[0])) * (f64::from(b[3]) - f64::from(b[2]))
}

fn margin(b: [f32; 4]) -> f64 {
    (f64::from(b[1]) - f64::from(b[0])) + (f64::from(b[3]) - f64::from(b[2]))
}

fn overlap(a: [f32; 4], b: [f32; 4]) -> f64 {
    let x = f64::from(a[1].min(b[1])) - f64::from(a[0].max(b[0]));
    let y = f64::from(a[3].min(b[3])) - f64::from(a[2].max(b[2]));
    if x > 0.0 && y > 0.0 { x * y } else { 0.0 }
}

/// A two-dimensional SQLite R-tree changed in place: nodes read from its `_node` table when
/// needed, boxes inserted as SQLite's R*-tree inserts them (the child needing the least
/// enlargement, nodes split along the axis of least margin), then the nodes, `_rowid` and
/// `_parent` rows changed written back.
struct Rtree {
    node_root: u32,
    rowid_root: u32,
    parent_root: u32,
    node_size: usize,
    max_cells: usize,
    min_cells: usize,
    /// The height of the tree: 0 when the root is a leaf.
    depth: usize,
    nodes: HashMap<i64, Vec<Cell>>,
    dirty: BTreeSet<i64>,
    old_max_node: i64,
    next_node: i64,
    rowids: BTreeMap<i64, i64>,
    parents: BTreeMap<i64, i64>,
}

impl Rtree {
    fn open(ed: &mut Editor, entries: &[Entry], rt: &str) -> Result<std::result::Result<Self, NotInPlace>> {
        let find = |suffix: &str| entries.iter().find(|e| e.kind == "table" && eq(&e.name, &format!("{rt}{suffix}")));
        let virt = find("");
        let usable = virt.is_some_and(|v| {
            let s = v.sql.to_ascii_lowercase();
            v.root == 0 && s.contains("using rtree(") && r::column_types(&v.sql).len() == 5
        });
        let (Some(node), Some(rowid), Some(parent)) = (find("_node"), find("_rowid"), find("_parent")) else {
            return Ok(Err(NotInPlace(format!("R-tree '{rt}' lacks a shadow table"))));
        };
        if !usable || r::column_types(&rowid.sql).len() != 2 || r::column_types(&parent.sql).len() != 2 {
            return Ok(Err(NotInPlace(format!("R-tree '{rt}' has auxiliary columns or more dimensions"))));
        }
        let mut t = Self {
            node_root: node.root,
            rowid_root: rowid.root,
            parent_root: parent.root,
            node_size: 0,
            max_cells: 0,
            min_cells: 0,
            depth: 0,
            nodes: HashMap::new(),
            dirty: BTreeSet::new(),
            old_max_node: 0,
            next_node: 0,
            rowids: BTreeMap::new(),
            parents: BTreeMap::new(),
        };
        let root = ed.lookup(t.node_root, 1)?.ok_or_else(|| Error::InvalidData(format!("R-tree '{rt}' has no root node")))?;
        let blob = match Db::record_values(&root).pop() {
            Some(r::Value::Blob(b)) => b,
            _ => return Err(Error::InvalidData(format!("R-tree '{rt}': invalid root node"))),
        };
        t.node_size = blob.len();
        t.max_cells = (t.node_size.saturating_sub(4)) / 24;
        if t.max_cells < 6 {
            return Ok(Err(NotInPlace(format!("R-tree '{rt}': node size {}", t.node_size))));
        }
        // RTREE_MINCELLS: a third of the most cells.
        t.min_cells = t.max_cells / 3;
        t.depth = u16::from_be_bytes([blob[0], blob[1]]) as usize;
        t.nodes.insert(1, Self::decode(&blob));
        t.old_max_node = ed.max_rowid(t.node_root)?;
        t.next_node = t.old_max_node + 1;
        Ok(Ok(t))
    }

    fn decode(blob: &[u8]) -> Vec<Cell> {
        let n = u16::from_be_bytes([blob[2], blob[3]]) as usize;
        (0..n)
            .filter_map(|i| {
                let c = blob.get(4 + 24 * i..4 + 24 * (i + 1))?;
                let id = i64::from_be_bytes(c[..8].try_into().ok()?);
                let f = |k: usize| f32::from_be_bytes([c[8 + 4 * k], c[9 + 4 * k], c[10 + 4 * k], c[11 + 4 * k]]);
                Some((id, [f(0), f(1), f(2), f(3)]))
            })
            .collect()
    }

    fn encode(&self, id: i64, cells: &[Cell]) -> Vec<u8> {
        let depth = if id == 1 { self.depth as u16 } else { 0 };
        let mut b = Vec::with_capacity(self.node_size);
        b.extend_from_slice(&depth.to_be_bytes());
        b.extend_from_slice(&(cells.len() as u16).to_be_bytes());
        for (id, bx) in cells {
            b.extend_from_slice(&id.to_be_bytes());
            for v in bx {
                b.extend_from_slice(&v.to_be_bytes());
            }
        }
        b.resize(self.node_size, 0);
        b
    }

    fn node(&mut self, ed: &mut Editor, id: i64) -> Result<&mut Vec<Cell>> {
        if !self.nodes.contains_key(&id) {
            let rec = ed.lookup(self.node_root, id)?.ok_or_else(|| Error::InvalidData(format!("R-tree node {id} missing")))?;
            let cells = match Db::record_values(&rec).pop() {
                Some(r::Value::Blob(b)) if b.len() >= 4 => Self::decode(&b),
                _ => return Err(Error::InvalidData(format!("R-tree node {id} invalid"))),
            };
            self.nodes.insert(id, cells);
        }
        Ok(self.nodes.get_mut(&id).expect("the node is loaded"))
    }

    fn new_node(&mut self, cells: Vec<Cell>) -> i64 {
        let id = self.next_node;
        self.next_node += 1;
        self.nodes.insert(id, cells);
        self.dirty.insert(id);
        id
    }

    /// Records where the cells of a node now are: features in `_rowid`, nodes in `_parent`.
    fn adopt(&mut self, node: i64, cells: &[Cell], leaf: bool) {
        let map = if leaf { &mut self.rowids } else { &mut self.parents };
        for (id, _) in cells {
            map.insert(*id, node);
        }
    }

    /// Splits cells in two as SQLite's R*-tree does: along the axis whose splits have the least
    /// margin, at the split with the least overlap (then area).
    fn split(&self, mut cells: Vec<Cell>) -> (Vec<Cell>, Vec<Cell>) {
        let n = cells.len();
        let lo = self.min_cells.max(1);
        let hi = n - lo;
        let sort = |cells: &mut Vec<Cell>, axis: usize| {
            cells.sort_by(|a, b| {
                let (a0, a1, b0, b1) = (a.1[2 * axis], a.1[2 * axis + 1], b.1[2 * axis], b.1[2 * axis + 1]);
                a0.total_cmp(&b0).then(a1.total_cmp(&b1))
            });
        };
        let mut best_axis = 0;
        let mut best_margin = f64::INFINITY;
        for axis in 0..2 {
            sort(&mut cells, axis);
            let total: f64 = (lo..=hi).map(|k| margin(union(&cells[..k])) + margin(union(&cells[k..]))).sum();
            if total < best_margin {
                best_margin = total;
                best_axis = axis;
            }
        }
        sort(&mut cells, best_axis);
        let mut best = (f64::INFINITY, f64::INFINITY, lo);
        for k in lo..=hi {
            let (a, b) = (union(&cells[..k]), union(&cells[k..]));
            let key = (overlap(a, b), area(a) + area(b), k);
            if key.0 < best.0 || (key.0 == best.0 && key.1 < best.1) {
                best = key;
            }
        }
        let right = cells.split_off(best.2);
        (cells, right)
    }

    /// Inserts a feature's box.
    fn insert(&mut self, ed: &mut Editor, id: i64, bx: [f32; 4]) -> Result<()> {
        // The path from the root to the leaf whose box grows least.
        let mut path = vec![1i64];
        for _ in 0..self.depth {
            let cur = *path.last().expect("the root");
            let cells = self.node(ed, cur)?;
            let mut best: Option<(f64, f64, i64)> = None;
            for (child, cb) in cells.iter() {
                let a = area(*cb);
                let growth = area(grow(*cb, bx)) - a;
                if best.is_none_or(|(g, ar, _)| growth < g || (growth == g && a < ar)) {
                    best = Some((growth, a, *child));
                }
            }
            let next = best.ok_or_else(|| Error::InvalidData("R-tree: empty interior node".into()))?.2;
            path.push(next);
        }
        let leaf = *path.last().expect("the leaf");
        self.node(ed, leaf)?.push((id, bx));
        self.dirty.insert(leaf);
        self.rowids.insert(id, leaf);

        // Up the path: split full nodes, grow the boxes of the others' parents.
        let mut split_box: Option<[f32; 4]> = None;
        for level in (0..path.len()).rev() {
            let n = path[level];
            let leaf_level = level == path.len() - 1;
            if let Some(b) = split_box.take() {
                // The child at `level + 1` was split: its box is exact now.
                let child = path[level + 1];
                let cells = self.node(ed, n)?;
                if let Some(c) = cells.iter_mut().find(|(i, _)| *i == child) {
                    c.1 = b;
                }
            } else if !leaf_level {
                let child = path[level + 1];
                let cells = self.node(ed, n)?;
                if let Some(c) = cells.iter_mut().find(|(i, _)| *i == child) {
                    c.1 = grow(c.1, bx);
                }
            }
            self.dirty.insert(n);
            let len = self.node(ed, n)?.len();
            if len <= self.max_cells {
                continue;
            }
            let cells = std::mem::take(self.node(ed, n)?);
            let (left, right) = self.split(cells);
            if n == 1 {
                // The root keeps its number: its halves move to two new nodes below it.
                let (lb, rb) = (union(&left), union(&right));
                let a = self.new_node(left.clone());
                let b = self.new_node(right.clone());
                self.adopt(a, &left, leaf_level);
                self.adopt(b, &right, leaf_level);
                self.parents.insert(a, 1);
                self.parents.insert(b, 1);
                *self.node(ed, 1)? = vec![(a, lb), (b, rb)];
                self.depth += 1;
            } else {
                let parent = path[level - 1];
                let (lb, rb) = (union(&left), union(&right));
                *self.node(ed, n)? = left;
                let b = self.new_node(right.clone());
                self.adopt(b, &right, leaf_level);
                self.parents.insert(b, parent);
                self.node(ed, parent)?.push((b, rb));
                split_box = Some(lb);
            }
        }
        Ok(())
    }

    /// Writes the changed nodes and the `_rowid` and `_parent` rows that changed.
    fn flush(self, ed: &mut Editor) -> Result<()> {
        let nodes: Vec<(i64, Vec<u8>)> =
            self.dirty.iter().map(|id| (*id, w::record(&[w::Value::Null, w::Value::Blob(self.encode(*id, &self.nodes[id]))]))).collect();
        let rows = |map: &BTreeMap<i64, i64>| -> Vec<(i64, Vec<u8>)> {
            map.iter().map(|(k, v)| (*k, w::record(&[w::Value::Null, w::Value::Int(*v)]))).collect()
        };
        for (root, all) in [(self.node_root, nodes), (self.rowid_root, rows(&self.rowids)), (self.parent_root, rows(&self.parents))] {
            // Rows replaced (or inserted between others), then rows past the last appended.
            let max = ed.max_rowid(root)?;
            let split = all.partition_point(|(k, _)| *k <= max);
            let mut all = all;
            let tail = all.split_off(split);
            ed.upsert(root, all)?;
            ed.append(root, tail)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::gpkg_read;
    use super::*;
    use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry};

    fn layer(name: &str, n: u64, x0: f64) -> Layer {
        let mut l = Layer::new(name).with_epsg(32633);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("v", FieldType::Integer));
        for i in 0..n {
            let x = x0 + (i % 1000) as f64;
            let y = (i / 1000) as f64;
            let g = Geometry::polygon(vec![Coord::xy(x, y), Coord::xy(x + 1.0, y), Coord::xy(x, y + 1.0)], vec![]);
            l.push(Feature {
                fid: i,
                geometry: Some(g),
                attributes: vec![FieldValue::Text(format!("f{i}")), FieldValue::Integer(i as i64)],
            });
        }
        l
    }

    fn read(path: &Path, spat: Option<(f64, f64, f64, f64)>) -> Vec<Layer> {
        let db = Db::new(std::fs::read(path).unwrap()).unwrap();
        gpkg_read::read_db(&db, &[], spat).unwrap()
    }

    #[test]
    fn appends_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.gpkg");
        std::fs::write(&path, super::super::geopackage::to_bytes(&[layer("a", 3000, 0.0), layer("b", 5, 0.0)]).unwrap()).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        // Twice, so that the second append grows the R-tree built by the first.
        for k in 0..2 {
            let extra = layer("A", 20_000, 2000.0 + 1000.0 * k as f64);
            assert!(update_in_place(&path, &[extra], UpdateMode::Append).unwrap());
        }
        assert!(std::fs::metadata(&path).unwrap().len() > before);
        let layers = read(&path, None);
        assert_eq!(layers[0].len(), 43_000);
        assert_eq!(layers[0].features[42_999].fid, 43_000);
        assert_eq!(layers[0].features[42_999].attributes, [FieldValue::Text("f19999".into()), FieldValue::Integer(19_999)]);
        assert_eq!(layers[1].len(), 5);
        let db = Db::new(std::fs::read(&path).unwrap()).unwrap();
        let counts = db.table("gpkg_ogr_contents").unwrap();
        assert_eq!(counts.get(&counts.rows[0], "feature_count").and_then(r::Value::as_i64), Some(43_000));
        let contents = db.table("gpkg_contents").unwrap();
        assert_eq!(contents.get(&contents.rows[0], "max_x").and_then(r::Value::as_f64), Some(4000.0));
        let seq = db.table("sqlite_sequence").unwrap();
        assert!(seq.rows.iter().any(|r| r[0] == r::Value::Text("a".into()) && r[1] == r::Value::Int(43_000)));
        // Spatial filters through the R-tree find old and new features alike.
        for (spat, expected) in
            [((10.1, 0.1, 10.2, 0.2), vec![11]), ((2500.1, 3.1, 2500.2, 3.2), vec![6501]), ((3500.1, 19.1, 3500.2, 19.2), vec![42501])]
        {
            let hits = read(&path, Some(spat));
            assert_eq!(hits[0].features.iter().map(|f| f.fid).collect::<Vec<_>>(), expected, "{spat:?}");
        }
        // The R-tree holds every feature once, with consistent parents.
        let rowids = db.table("rtree_a_geom_rowid").unwrap();
        assert_eq!(rowids.rows.len(), 43_000);
    }

    #[test]
    fn overwrites_and_adds_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.gpkg");
        std::fs::write(&path, super::super::geopackage::to_bytes(&[layer("a", 3000, 0.0), layer("b", 5, 0.0)]).unwrap()).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        assert!(update_in_place(&path, &[layer("a", 10, 50.0), layer("c", 3, 0.0).with_epsg(4326)], UpdateMode::Overwrite).unwrap());
        // The pages of the old layer are reused.
        assert!(std::fs::metadata(&path).unwrap().len() <= before);
        let layers = read(&path, None);
        assert_eq!(layers.iter().map(|l| (l.name.as_str(), l.len())).collect::<Vec<_>>(), [("b", 5), ("a", 10), ("c", 3)]);
        assert_eq!(layers[2].crs_epsg(), Some(4326));
        assert_eq!(read(&path, Some((55.1, 0.1, 55.2, 0.2)))[1].features.iter().map(|f| f.fid).collect::<Vec<_>>(), [6]);
        assert!(update_in_place(&path, &[layer("gpkg_contents", 1, 0.0)], UpdateMode::Overwrite).is_err());
    }
}
