//! GeoPackage writing, on terrio's SQLite file writer.
//!
//! Tables are laid out as GDAL lays them out: an `fid INTEGER PRIMARY KEY AUTOINCREMENT` column, a
//! `geom` column declared with the type the features share (`GEOMETRY` when they differ), typed
//! columns (`MEDIUMINT`/`INTEGER`, `REAL`, `TEXT`, `BOOLEAN`, `DATE`, `DATETIME`, `BLOB`) and the
//! layer's CRS in `gpkg_spatial_ref_sys`: its EPSG code when it is known for sure, otherwise its
//! WKT under an organization of `NONE`, and GDAL's undefined SRS (99999) without a CRS. Layers
//! without geometries, geometry type or CRS are attribute tables. Each geometry column gets an
//! R-tree spatial index (the `rtree_<table>_<column>` virtual table, its shadow tables filled in
//! and the triggers that keep it up to date), and `gpkg_ogr_contents` holds the feature counts,
//! as GDAL writes them.
//!
//! [`Builder`] gathers vector layers and raster tile tables (written by the raster driver) into
//! one file.

use super::sqlite::{self, BuiltTable, SchemaObject, Table, Value};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

pub(super) const SRS_SQL: &str = "CREATE TABLE gpkg_spatial_ref_sys (srs_name TEXT NOT NULL, srs_id INTEGER PRIMARY KEY, organization TEXT NOT NULL, organization_coordsys_id INTEGER NOT NULL, definition  TEXT NOT NULL, description TEXT)";
pub(super) const CONTENTS_SQL: &str = "CREATE TABLE gpkg_contents (table_name TEXT NOT NULL PRIMARY KEY, data_type TEXT NOT NULL, identifier TEXT UNIQUE, description TEXT DEFAULT '', last_change DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE, srs_id INTEGER, CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id))";
pub(super) const GEOMETRY_COLUMNS_SQL: &str = "CREATE TABLE gpkg_geometry_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL, geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, z TINYINT NOT NULL, m TINYINT NOT NULL, CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name), CONSTRAINT uk_gc_table_name UNIQUE (table_name), CONSTRAINT fk_gc_tn FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name), CONSTRAINT fk_gc_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id))";
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
pub(super) fn now() -> String {
    #[cfg(not(target_family = "wasm"))]
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    #[cfg(target_family = "wasm")]
    let ms = 0;
    let s = super::datetime::format_datetime(ms - ms % 1000, true);
    s.replace('Z', ".000Z")
}

/// The GeoPackage geometry encoding: the `GP` header with the envelope, then ISO WKB.
pub(super) fn gpkg_geometry(g: &Geometry, srs_id: i32) -> Vec<u8> {
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

const UNDEFINED_WKT: &str =
    r#"LOCAL_CS["Undefined SRS",LOCAL_DATUM["unknown",32767],UNIT["unknown",0],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#;
pub(super) const OGR_CONTENTS_SQL: &str =
    "CREATE TABLE gpkg_ogr_contents(table_name TEXT NOT NULL PRIMARY KEY,feature_count INTEGER DEFAULT NULL)";
pub(super) const EXTENSIONS_SQL: &str = "CREATE TABLE gpkg_extensions (table_name TEXT,column_name TEXT,extension_name TEXT NOT NULL,definition TEXT NOT NULL,scope TEXT NOT NULL,CONSTRAINT ge_tce UNIQUE (table_name, column_name, extension_name))";
const TILE_MATRIX_SET_SQL: &str = "CREATE TABLE gpkg_tile_matrix_set (table_name TEXT NOT NULL PRIMARY KEY,srs_id INTEGER NOT NULL,min_x DOUBLE NOT NULL,min_y DOUBLE NOT NULL,max_x DOUBLE NOT NULL,max_y DOUBLE NOT NULL,CONSTRAINT fk_gtms_table_name FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name),CONSTRAINT fk_gtms_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id))";
const TILE_MATRIX_SQL: &str = "CREATE TABLE gpkg_tile_matrix (table_name TEXT NOT NULL,zoom_level INTEGER NOT NULL,matrix_width INTEGER NOT NULL,matrix_height INTEGER NOT NULL,tile_width INTEGER NOT NULL,tile_height INTEGER NOT NULL,pixel_x_size DOUBLE NOT NULL,pixel_y_size DOUBLE NOT NULL,CONSTRAINT pk_ttm PRIMARY KEY (table_name, zoom_level),CONSTRAINT fk_tmm_table_name FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name))";
pub(super) const METADATA_SQL: &str = "CREATE TABLE gpkg_metadata (id INTEGER CONSTRAINT m_pk PRIMARY KEY ASC NOT NULL,md_scope TEXT NOT NULL DEFAULT 'dataset',md_standard_uri TEXT NOT NULL,mime_type TEXT NOT NULL DEFAULT 'text/xml',metadata TEXT NOT NULL DEFAULT '')";
pub(super) const METADATA_REFERENCE_SQL: &str = "CREATE TABLE gpkg_metadata_reference (reference_scope TEXT NOT NULL,table_name TEXT,column_name TEXT,row_id_value INTEGER,timestamp DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),md_file_id INTEGER NOT NULL,md_parent_id INTEGER,CONSTRAINT crmr_mfi_fk FOREIGN KEY (md_file_id) REFERENCES gpkg_metadata(id),CONSTRAINT crmr_mpi_fk FOREIGN KEY (md_parent_id) REFERENCES gpkg_metadata(id))";
const COVERAGE_SQL: &str = "CREATE TABLE gpkg_2d_gridded_coverage_ancillary (id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,tile_matrix_set_name TEXT NOT NULL UNIQUE,datatype TEXT NOT NULL DEFAULT 'integer',scale REAL NOT NULL DEFAULT 1.0,offset REAL NOT NULL DEFAULT 0.0,precision REAL DEFAULT 1.0,data_null REAL,grid_cell_encoding TEXT DEFAULT 'grid-value-is-center',uom TEXT,field_name TEXT DEFAULT 'Height',quantity_definition TEXT DEFAULT 'Height',CONSTRAINT fk_g2dgtct_name FOREIGN KEY(tile_matrix_set_name) REFERENCES gpkg_tile_matrix_set ( table_name ) CHECK (datatype in ('integer','float')))";
const TILE_ANCILLARY_SQL: &str = "CREATE TABLE gpkg_2d_gridded_tile_ancillary (id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,tpudt_name TEXT NOT NULL,tpudt_id INTEGER NOT NULL,scale REAL NOT NULL DEFAULT 1.0,offset REAL NOT NULL DEFAULT 0.0,min REAL DEFAULT NULL,max REAL DEFAULT NULL,mean REAL DEFAULT NULL,std_dev REAL DEFAULT NULL,CONSTRAINT fk_g2dgtat_name FOREIGN KEY (tpudt_name) REFERENCES gpkg_contents(table_name),UNIQUE (tpudt_name, tpudt_id))";
const COVERAGE_EXTENSION: &str = "http://docs.opengeospatial.org/is/17-066r1/17-066r1.html";
pub(super) const METADATA_EXTENSION: &str = "http://www.geopackage.org/spec120/#extension_metadata";

/// The triggers that keep `gpkg_tile_matrix` valid, as GDAL creates them.
fn tile_matrix_triggers() -> Vec<SchemaObject> {
    let mut out = Vec::new();
    for (column, rule, cond) in [
        ("zoom_level", "zoom_level cannot be less than 0", "(NEW.zoom_level < 0)"),
        ("matrix_width", "matrix_width cannot be less than 1", "(NEW.matrix_width < 1)"),
        ("matrix_height", "matrix_height cannot be less than 1", "(NEW.matrix_height < 1)"),
        ("pixel_x_size", "pixel_x_size must be greater than 0", "NOT (NEW.pixel_x_size > 0)"),
        ("pixel_y_size", "pixel_y_size must be greater than 0", "NOT (NEW.pixel_y_size > 0)"),
    ] {
        for (op, of) in [("insert", "INSERT"), ("update", &*format!("UPDATE OF {column}"))] {
            let name = format!("gpkg_tile_matrix_{column}_{op}");
            out.push(SchemaObject {
                kind: "trigger",
                sql: format!(
                    "CREATE TRIGGER '{name}' BEFORE {of} ON 'gpkg_tile_matrix' FOR EACH ROW BEGIN SELECT RAISE(ABORT, '{op} on table ''gpkg_tile_matrix'' violates constraint: {rule}') WHERE {cond}; END"
                ),
                name,
                table: "gpkg_tile_matrix".into(),
            });
        }
    }
    out
}

/// The triggers that keep a tile table consistent with `gpkg_tile_matrix`, as GDAL creates them.
pub(crate) fn tile_table_triggers(table: &str) -> Vec<SchemaObject> {
    let t = table.replace('\'', "''");
    let q = quote(table);
    let mut out = Vec::new();
    for op in ["insert", "update"] {
        let of = if op == "insert" { "INSERT" } else { "UPDATE OF" };
        let col = |c: &str| if op == "insert" { String::new() } else { format!(" {c}") };
        let zoom = format!("{}_zoom_{op}", table);
        out.push(SchemaObject {
            kind: "trigger",
            sql: format!(
                "CREATE TRIGGER {} BEFORE {of}{} ON {q} FOR EACH ROW BEGIN SELECT RAISE(ABORT, '{op} on table ''{t}'' violates constraint: zoom_level not specified for table in gpkg_tile_matrix') WHERE NOT (NEW.zoom_level IN (SELECT zoom_level FROM gpkg_tile_matrix WHERE lower(table_name) = lower('{t}'))) ; END",
                quote(&zoom),
                col("zoom_level")
            ),
            name: zoom,
            table: table.into(),
        });
        for (c, m) in [("tile_column", "matrix_width"), ("tile_row", "matrix_height")] {
            let name = format!("{table}_{c}_{op}");
            out.push(SchemaObject {
                kind: "trigger",
                sql: format!(
                    "CREATE TRIGGER {} BEFORE {of}{} ON {q} FOR EACH ROW BEGIN SELECT RAISE(ABORT, '{op} on table ''{t}'' violates constraint: {c} cannot be < 0') WHERE (NEW.{c} < 0) ; SELECT RAISE(ABORT, '{op} on table ''{t}'' violates constraint: {c} must by < {m} specified for table and zoom level in gpkg_tile_matrix') WHERE NOT (NEW.{c} < (SELECT {m} FROM gpkg_tile_matrix WHERE lower(table_name) = lower('{t}') AND zoom_level = NEW.zoom_level)); END",
                    quote(&name),
                    col(c)
                ),
                name,
                table: table.into(),
            });
        }
    }
    out
}

/// Rounds down to a float, as SQLite's R-tree stores lower bounds.
pub(super) fn f32_down(d: f64) -> f32 {
    let f = d as f32;
    if f64::from(f) > d { (d * if d < 0.0 { 1.0 + 1.0 / 8_388_608.0 } else { 1.0 - 1.0 / 8_388_608.0 }) as f32 } else { f }
}

/// Rounds up to a float, as SQLite's R-tree stores upper bounds.
pub(super) fn f32_up(d: f64) -> f32 {
    let f = d as f32;
    if f64::from(f) < d { (d * if d < 0.0 { 1.0 - 1.0 / 8_388_608.0 } else { 1.0 + 1.0 / 8_388_608.0 }) as f32 } else { f }
}

/// The most entries in a node of SQLite's R-tree (`RTREE_MAXCELLS`).
const RTREE_MAX_CELLS: usize = 51;
/// The bytes of an R-tree node of two dimensions: the header and 51 cells of 24 bytes.
const RTREE_NODE_SIZE: usize = 4 + 24 * RTREE_MAX_CELLS;

/// An R-tree entry: an id (a feature id, or a child node) and its box `[minx, maxx, miny, maxy]`.
pub(super) type RtreeEntry = (i64, [f32; 4]);

fn union(entries: &[RtreeEntry]) -> [f32; 4] {
    entries.iter().fold([f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY], |a, (_, b)| {
        [a[0].min(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].max(b[3])]
    })
}

/// Groups entries into nodes of at most 51 by sort-tile-recursive packing, in place: returns
/// where each node starts in `entries`, then their end.
fn str_pack(entries: &mut [RtreeEntry]) -> Vec<usize> {
    let n = entries.len();
    let nodes = n.div_ceil(RTREE_MAX_CELLS).max(1);
    let slices = (nodes as f64).sqrt().ceil() as usize;
    let per_slice = slices * RTREE_MAX_CELLS;
    let cx = |e: &RtreeEntry| f64::from(e.1[0]) + f64::from(e.1[1]);
    let cy = |e: &RtreeEntry| f64::from(e.1[2]) + f64::from(e.1[3]);
    entries.sort_by(|a, b| cx(a).total_cmp(&cx(b)));
    let mut starts = Vec::with_capacity(nodes + 1);
    for (k, slice) in entries.chunks_mut(per_slice).enumerate() {
        slice.sort_by(|a, b| cy(a).total_cmp(&cy(b)));
        starts.extend((0..slice.len()).step_by(RTREE_MAX_CELLS).map(|i| k * per_slice + i));
    }
    starts.push(n);
    starts
}

/// The shadow tables of a two-dimensional SQLite R-tree holding the boxes, built in one go:
/// `(node rows, rowid rows, parent rows)`.
pub(super) type RtreeRows = (Vec<(i64, Vec<Value>)>, Vec<(i64, Vec<Value>)>, Vec<(i64, Vec<Value>)>);

pub(super) fn build_rtree(entries: Vec<RtreeEntry>) -> RtreeRows {
    let tree = Rtree::new(entries);
    (tree.node_rows().collect(), tree.rowid_rows().collect(), tree.parent_rows().collect())
}

/// A two-dimensional SQLite R-tree of boxes, packed: its levels from the leaves up, each the
/// entries of its nodes in order (features in the leaves, and above them the indexes of the
/// children in the level below) and where each node starts. Its shadow tables' rows are made as
/// they are read, so that only the entries are held.
pub(super) struct Rtree {
    levels: Vec<(Vec<RtreeEntry>, Vec<usize>)>,
}

/// An R-tree node's blob: its depth (set on the root only), number of cells and cells, the ids of
/// `cells` offset by `offset`.
fn rtree_node(depth: u16, cells: &[RtreeEntry], offset: i64) -> Vec<u8> {
    let mut b = Vec::with_capacity(RTREE_NODE_SIZE);
    b.extend_from_slice(&depth.to_be_bytes());
    b.extend_from_slice(&(cells.len() as u16).to_be_bytes());
    for (id, bx) in cells {
        b.extend_from_slice(&(id + offset).to_be_bytes());
        for v in bx {
            b.extend_from_slice(&v.to_be_bytes());
        }
    }
    b.resize(RTREE_NODE_SIZE, 0);
    b
}

impl Rtree {
    pub(super) fn new(mut entries: Vec<RtreeEntry>) -> Self {
        let mut levels = Vec::new();
        if entries.is_empty() {
            return Self { levels };
        }
        let starts = str_pack(&mut entries);
        levels.push((entries, starts));
        while let Some((entries, starts)) = levels.last()
            && starts.len() > 2
        {
            // Placeholder ids: the index of the child in its level.
            let mut up: Vec<RtreeEntry> = starts.windows(2).enumerate().map(|(i, w)| (i as i64, union(&entries[w[0]..w[1]]))).collect();
            let starts = str_pack(&mut up);
            levels.push((up, starts));
        }
        Self { levels }
    }

    /// The number of each level's first node: the root is node 1, and each level's nodes follow
    /// those of the level above.
    fn bases(&self) -> Vec<i64> {
        let mut bases = vec![0; self.levels.len()];
        let mut next = 1;
        for (base, (_, starts)) in bases.iter_mut().zip(&self.levels).rev() {
            *base = next;
            next += starts.len() as i64 - 1;
        }
        bases
    }

    /// The rows of the `_node` table, by node number.
    pub(super) fn node_rows(&self) -> Box<sqlite::Rows<'_>> {
        if self.levels.is_empty() {
            return Box::new(std::iter::once((1, vec![Value::Null, Value::Blob(vec![0; RTREE_NODE_SIZE])])));
        }
        let bases = self.bases();
        let depth = self.levels.len() - 1;
        Box::new((0..=depth).rev().flat_map(move |level| {
            let (entries, starts) = &self.levels[level];
            let (base, children) = (bases[level], if level == 0 { 0 } else { bases[level - 1] });
            let d = if level == depth { depth as u16 } else { 0 };
            starts
                .windows(2)
                .enumerate()
                .map(move |(i, w)| (base + i as i64, vec![Value::Null, Value::Blob(rtree_node(d, &entries[w[0]..w[1]], children))]))
        }))
    }

    /// The rows of the `_rowid` table: each feature's leaf, by feature id.
    pub(super) fn rowid_rows(&self) -> impl Iterator<Item = (i64, Vec<Value>)> + use<> {
        let mut leaves: Vec<(i64, i64)> = Vec::new();
        if let Some((entries, starts)) = self.levels.first() {
            let base = self.bases()[0];
            leaves.reserve_exact(entries.len());
            for (i, w) in starts.windows(2).enumerate() {
                leaves.extend(entries[w[0]..w[1]].iter().map(|(id, _)| (*id, base + i as i64)));
            }
        }
        leaves.sort_unstable();
        leaves.into_iter().map(|(id, node)| (id, vec![Value::Null, Value::Int(node)]))
    }

    /// The rows of the `_parent` table: each node's parent, by node number (the root has none).
    pub(super) fn parent_rows(&self) -> Box<sqlite::Rows<'_>> {
        let bases = self.bases();
        Box::new((1..self.levels.len()).rev().flat_map(move |above| {
            let (entries, starts) = &self.levels[above];
            let mut parents = vec![0; self.levels[above - 1].1.len() - 1];
            for (i, w) in starts.windows(2).enumerate() {
                for (child, _) in &entries[w[0]..w[1]] {
                    parents[*child as usize] = bases[above] + i as i64;
                }
            }
            let base = bases[above - 1];
            parents.into_iter().enumerate().map(move |(c, p)| (base + c as i64, vec![Value::Null, Value::Int(p)]))
        }))
    }
}

/// The triggers that keep a layer's `gpkg_ogr_contents` feature count, as GDAL creates them.
pub(super) fn feature_count_triggers(name: &str) -> Vec<SchemaObject> {
    let mut objects = Vec::new();
    let lower = name.replace('\'', "''");
    for (op, sign) in [("insert", "+"), ("delete", "-")] {
        let trigger = format!("trigger_{op}_feature_count_{name}");
        let when = if op == "insert" { "INSERT" } else { "DELETE" };
        objects.push(SchemaObject {
                kind: "trigger",
                sql: format!(
                    "CREATE TRIGGER {} AFTER {when} ON {} BEGIN UPDATE gpkg_ogr_contents SET feature_count = feature_count {sign} 1 WHERE lower(table_name) = lower('{lower}'); END",
                    quote(&trigger),
                    quote(name)
                ),
                name: trigger,
                table: name.into(),
            });
    }
    objects
}

/// The R-tree index of a feature table's geometry column, as GDAL creates it: the packed tree,
/// the names and statements of its shadow tables (`_rowid`, `_node` and `_parent`, whose rows
/// the tree gives), and the virtual table and the triggers that keep it up to date.
pub(super) fn rtree_index(
    table: &str,
    column: &str,
    fid: &str,
    entries: Vec<RtreeEntry>,
) -> (Rtree, [(String, String); 3], Vec<SchemaObject>) {
    let rt = format!("rtree_{table}_{column}");
    let (t, c, r, f) = (quote(table), quote(column), quote(&rt), quote(fid));
    let shadow = |suffix: &str, columns: &str| {
        let name = format!("{rt}_{suffix}");
        let sql = format!("CREATE TABLE {}({columns})", quote(&name));
        (name, sql)
    };
    let tables = [
        shadow("rowid", "rowid INTEGER PRIMARY KEY,nodeno"),
        shadow("node", "nodeno INTEGER PRIMARY KEY,data"),
        shadow("parent", "nodeno INTEGER PRIMARY KEY,parentnode"),
    ];
    let bounds = |g: &str| format!("ST_MinX({g}), ST_MaxX({g}),ST_MinY({g}), ST_MaxY({g})");
    let new = format!("NEW.{c}");
    let triggers = [
        (
            "insert",
            format!(
                "AFTER INSERT ON {t} WHEN (new.{c} NOT NULL AND NOT ST_IsEmpty(NEW.{c})) BEGIN INSERT OR REPLACE INTO {r} VALUES (NEW.{f},{}); END",
                bounds(&new)
            ),
        ),
        (
            "update6",
            format!(
                "AFTER UPDATE OF {c} ON {t} WHEN OLD.{f} = NEW.{f} AND (NEW.{c} NOTNULL AND NOT ST_IsEmpty(NEW.{c})) AND (OLD.{c} NOTNULL AND NOT ST_IsEmpty(OLD.{c})) BEGIN UPDATE {r} SET minx = ST_MinX({new}), maxx = ST_MaxX({new}),miny = ST_MinY({new}), maxy = ST_MaxY({new}) WHERE id = NEW.{f};END"
            ),
        ),
        (
            "update7",
            format!(
                "AFTER UPDATE OF {c} ON {t} WHEN OLD.{f} = NEW.{f} AND (NEW.{c} NOTNULL AND NOT ST_IsEmpty(NEW.{c})) AND (OLD.{c} ISNULL OR ST_IsEmpty(OLD.{c})) BEGIN INSERT INTO {r} VALUES (NEW.{f},{}); END",
                bounds(&new)
            ),
        ),
        (
            "update2",
            format!(
                "AFTER UPDATE OF {c} ON {t} WHEN OLD.{f} = NEW.{f} AND (NEW.{c} ISNULL OR ST_IsEmpty(NEW.{c})) BEGIN DELETE FROM {r} WHERE id = OLD.{f}; END"
            ),
        ),
        (
            "update5",
            format!(
                "AFTER UPDATE ON {t} WHEN OLD.{f} != NEW.{f} AND (NEW.{c} NOTNULL AND NOT ST_IsEmpty(NEW.{c})) BEGIN DELETE FROM {r} WHERE id = OLD.{f}; INSERT OR REPLACE INTO {r} VALUES (NEW.{f},{}); END",
                bounds(&new)
            ),
        ),
        (
            "update4",
            format!(
                "AFTER UPDATE ON {t} WHEN OLD.{f} != NEW.{f} AND (NEW.{c} ISNULL OR ST_IsEmpty(NEW.{c})) BEGIN DELETE FROM {r} WHERE id IN (OLD.{f}, NEW.{f}); END"
            ),
        ),
        ("delete", format!("AFTER DELETE ON {t} WHEN old.{c} NOT NULL BEGIN DELETE FROM {r} WHERE id = OLD.{f}; END")),
    ];
    let mut objects = vec![SchemaObject {
        kind: "table",
        name: rt.clone(),
        table: rt.clone(),
        sql: format!("CREATE VIRTUAL TABLE {r} USING rtree(id, minx, maxx, miny, maxy)"),
    }];
    for (suffix, body) in triggers {
        let name = format!("{rt}_{suffix}");
        objects.push(SchemaObject { kind: "trigger", sql: format!("CREATE TRIGGER {} {body}", quote(&name)), name, table: table.into() });
    }
    (Rtree::new(entries), tables, objects)
}

/// The columns of a layer's table (see [`Builder::layer_columns`]).
pub(crate) struct LayerColumns {
    pub sql: String,
    pub spatial: bool,
    pub srs_id: i64,
    pub type_name: String,
    pub has_z: bool,
    fid_field: Option<usize>,
    kept: Vec<usize>,
}

impl LayerColumns {
    /// A feature's row: its rowid (the `fid` field's value, or `default_rowid`), values, and the
    /// geometry's bounds.
    pub(crate) fn row(&self, f: &Feature, default_rowid: i64, fields: &[FieldDef]) -> (i64, Vec<Value>, Option<wbvector::BBox>) {
        let rowid = self.fid_field.and_then(|i| f.attributes.get(i)?.as_i64()).unwrap_or(default_rowid);
        let mut bbox = None;
        let geom = match &f.geometry {
            Some(g) => {
                bbox = g.bbox().filter(|b| b.min_x.is_finite() && b.min_y.is_finite());
                Value::Blob(gpkg_geometry(g, self.srs_id as i32))
            }
            None => Value::Null,
        };
        let mut values = if self.spatial { vec![Value::Null, geom] } else { vec![Value::Null] };
        for &i in &self.kept {
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
        (rowid, values, bbox)
    }
}

/// A layer's extent and the R-tree entries of its geometries, accumulated in rowid order.
#[derive(Default)]
pub(crate) struct LayerExtent {
    bounds: Option<[f64; 4]>,
    entries: Vec<RtreeEntry>,
}

impl LayerExtent {
    pub(crate) fn add(&mut self, rowid: i64, bbox: Option<wbvector::BBox>) {
        if let Some(b) = bbox {
            self.bounds = Some(match self.bounds {
                None => [b.min_x, b.min_y, b.max_x, b.max_y],
                Some(a) => [a[0].min(b.min_x), a[1].min(b.min_y), a[2].max(b.max_x), a[3].max(b.max_y)],
            });
            self.entries.push((rowid, [f32_down(b.min_x), f32_up(b.max_x), f32_down(b.min_y), f32_up(b.max_y)]));
        }
    }
}

/// Writes a table (its rows, then those given) after the tables already written.
pub(crate) type WriteTable<'a> = dyn FnMut(Table, &mut sqlite::Rows) -> Result<BuiltTable> + 'a;

/// A layer's table: its rows, or already written by a [`sqlite::StreamTable`].
pub(crate) enum LayerTable {
    Rows(Table),
    Built(BuiltTable),
}

/// A GeoPackage being built: vector layers and raster tile tables, written in one go.
pub(crate) struct Builder {
    srs: Vec<Vec<Value>>,
    contents: Vec<Vec<Value>>,
    geometry_columns: Vec<Vec<Value>>,
    ogr_contents: Vec<Vec<Value>>,
    extensions: Vec<Vec<Value>>,
    tile_matrix_set: Vec<Vec<Value>>,
    tile_matrix: Vec<Vec<Value>>,
    metadata: Vec<Vec<Value>>,
    metadata_reference: Vec<Vec<Value>>,
    coverage: Vec<Vec<Value>>,
    tile_ancillary: Vec<Vec<Value>>,
    tables: Vec<Table>,
    /// Tables already written, each listed before the table of `tables` at its index.
    built: Vec<(usize, BuiltTable)>,
    objects: Vec<SchemaObject>,
    /// `sqlite_sequence`: the largest id of each AUTOINCREMENT table.
    sequence: Vec<(String, i64)>,
    names: Vec<String>,
    custom_srs: i64,
    /// The name of the feature id column of the layers added.
    fid_column: String,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// A GeoPackage with the undefined and WGS 84 SRSs.
    pub(crate) fn new() -> Self {
        let srs = vec![
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
        Self {
            srs,
            contents: Vec::new(),
            geometry_columns: Vec::new(),
            ogr_contents: Vec::new(),
            extensions: Vec::new(),
            tile_matrix_set: Vec::new(),
            tile_matrix: Vec::new(),
            metadata: Vec::new(),
            metadata_reference: Vec::new(),
            coverage: Vec::new(),
            tile_ancillary: Vec::new(),
            tables: Vec::new(),
            built: Vec::new(),
            objects: tile_matrix_triggers(),
            sequence: Vec::new(),
            names: Vec::new(),
            custom_srs: 100000,
            fid_column: "fid".into(),
        }
    }

    /// Names the feature id column of the layers added from now on (`fid` by default).
    pub(crate) fn set_fid_column(&mut self, name: &str) {
        self.fid_column = name.to_string();
    }

    /// Reserves a table name (case-insensitively unique).
    pub(crate) fn claim(&mut self, name: &str) -> Result<()> {
        if self.names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(Error::InvalidArgument(format!("two tables are named '{name}'")));
        }
        self.names.push(name.to_string());
        Ok(())
    }

    /// The `srs_id` of a CRS, adding it to `gpkg_spatial_ref_sys` when needed. `epsg` is the
    /// code it is known by for sure, and `wkt` the definition to store (by default, the CRS's).
    pub(crate) fn srs_id(&mut self, s: &SpatialRef, epsg: Option<u32>, wkt: Option<String>) -> i64 {
        if s.is_unknown() {
            // GDAL's undefined SRS.
            if !self.srs.iter().any(|r| r[3] == Value::Int(99999)) {
                self.srs.push(vec![
                    Value::Text("Undefined SRS".into()),
                    Value::Null,
                    Value::Text("GDAL".into()),
                    Value::Int(99999),
                    Value::Text(UNDEFINED_WKT.into()),
                    Value::Text("Custom undefined coordinate reference system".into()),
                ]);
            }
            return 99999;
        }
        match epsg {
            Some(4326) => 4326,
            Some(code) => {
                if !self.srs.iter().any(|r| r[2] == Value::Text("EPSG".into()) && r[3] == Value::Int(code as i64)) {
                    self.srs.push(vec![
                        Value::Text(s.name().unwrap_or_else(|| format!("EPSG:{code}"))),
                        Value::Null,
                        Value::Text("EPSG".into()),
                        Value::Int(code as i64),
                        Value::Text(wkt.unwrap_or_default()),
                        Value::Null,
                    ]);
                }
                code as i64
            }
            None => {
                let def = wkt.or_else(|| s.to_wkt()).unwrap_or_default();
                if let Some(r) = self.srs.iter().find(|r| r[2] == Value::Text("NONE".into()) && r[4] == Value::Text(def.clone()))
                    && let Value::Int(id) = r[3]
                {
                    return id;
                }
                self.custom_srs += 1;
                self.srs.push(vec![
                    Value::Text(s.name().unwrap_or_else(|| "Unknown".into())),
                    Value::Null,
                    Value::Text("NONE".into()),
                    Value::Int(self.custom_srs),
                    Value::Text(def),
                    Value::Null,
                ]);
                self.custom_srs
            }
        }
    }

    fn extension(&mut self, table: Option<&str>, column: Option<&str>, name: &str, definition: &str, scope: &str) {
        let text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.into()));
        let row = vec![text(table), text(column), Value::Text(name.into()), Value::Text(definition.into()), Value::Text(scope.into())];
        if !self.extensions.contains(&row) {
            self.extensions.push(row);
        }
    }

    /// Adds a vector layer, with its R-tree index.
    pub(crate) fn add_layer(&mut self, layer: &Layer) -> Result<()> {
        self.claim(&layer.name)?;
        // A layer without geometries, a geometry type or a CRS is an attribute table, as GDAL
        // writes one.
        let all_none = layer.features.iter().all(|f| f.geometry.is_none());
        // The geometry type the features share, as GDAL declares it (GEOMETRY when they differ).
        let declared = if all_none { layer.geom_type } else { super::uniform_type(layer) };
        let has_z = layer.features.iter().filter_map(|f| f.geometry.as_ref()).any(crate::vector::geometry_has_z);
        // Columns: a field named fid holding unique integers gives the feature ids, as in GDAL.
        let fields = layer.schema.fields();
        let fid_field =
            fields.iter().position(|f| f.name.eq_ignore_ascii_case(&self.fid_column) && f.field_type == FieldType::Integer).filter(|&i| {
                let mut ids: Vec<i64> = layer.features.iter().filter_map(|f| f.attributes.get(i)?.as_i64()).collect();
                let n = ids.len();
                ids.sort_unstable();
                ids.dedup();
                n == layer.len() && ids.len() == n
            });
        let small = |i: usize| layer.features.iter().filter_map(|ft| ft.attributes.get(i)?.as_i64()).all(|v| i32::try_from(v).is_ok());
        let cols = self.layer_columns(layer, all_none, declared, has_z, fid_field, small);

        // Rows (with each geometry's bounds) are encoded in parallel, then combined in order.
        let encoded = crate::par::map_range(layer.len(), |n| cols.row(&layer.features[n], n as i64 + 1, fields));
        let mut rows = Vec::with_capacity(layer.len());
        let mut extent = LayerExtent::default();
        for (rowid, values, bbox) in encoded {
            extent.add(rowid, bbox);
            rows.push((rowid, values));
        }
        rows.sort_by_key(|(id, _)| *id);
        let max_fid = rows.last().map_or(0, |(id, _)| *id);
        let count = layer.len();
        let table = Table { name: layer.name.clone(), sql: cols.sql.clone(), rows, autoindexes: vec![] };
        self.finish_layer(&layer.name, &cols, count, max_fid, extent, LayerTable::Rows(table), None)?;
        Ok(())
    }

    /// The columns of a layer's table, as GDAL creates them: `fid`, the geometry column of a
    /// spatial layer (declared with `declared`, GEOMETRY when `None`), and the fields but
    /// `fid_field`, integers as MEDIUMINT when `small` says they fit 32 bits.
    pub(crate) fn layer_columns(
        &mut self,
        layer: &Layer,
        all_none: bool,
        declared: Option<GeometryType>,
        has_z: bool,
        fid_field: Option<usize>,
        small: impl Fn(usize) -> bool,
    ) -> LayerColumns {
        let s = SpatialRef::from_layer(layer);
        let spatial = !(all_none && layer.geom_type.is_none() && s.is_unknown());
        let srs_id = if spatial { self.srs_id(&s, super::trusted_epsg(layer), super::output_wkt(layer)) } else { 0 };
        let type_name = declared.map_or("GEOMETRY".to_string(), |t| t.as_str().to_ascii_uppercase());
        let fields = layer.schema.fields();
        let mut col_names: Vec<String> = vec![self.fid_column.clone()];
        let mut kept = Vec::new();
        let mut sql =
            format!("CREATE TABLE {} ( {} INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL", quote(&layer.name), quote(&self.fid_column));
        if spatial {
            col_names.push("geom".into());
            sql.push_str(&format!(", \"geom\" {type_name}"));
        }
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
                    // DBF widths >= 10 declare Integer64, irrespective of current values.
                    if f.width < 10 && small(i) { "MEDIUMINT".to_string() } else { "INTEGER".to_string() }
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
        LayerColumns { sql, spatial, srs_id, type_name, has_z, fid_field, kept }
    }

    /// Records a layer's table and its metadata: `gpkg_contents` with the extent,
    /// `gpkg_geometry_columns`, `gpkg_ogr_contents` and its triggers, the sequence, and the
    /// R-tree of the geometries' bounds. With `write`, the R-tree's tables are handed to it to be
    /// written as they are built (after the layer's table), rather than kept.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_layer(
        &mut self,
        name: &str,
        cols: &LayerColumns,
        count: usize,
        max_fid: i64,
        extent: LayerExtent,
        table: LayerTable,
        mut write: Option<&mut WriteTable>,
    ) -> Result<()> {
        let b = extent.bounds.map(|b| b.map(Value::Real)).unwrap_or([Value::Null, Value::Null, Value::Null, Value::Null]);
        let [min_x, min_y, max_x, max_y] = b;
        let (spatial, srs_id) = (cols.spatial, cols.srs_id);
        self.contents.push(vec![
            Value::Text(name.into()),
            Value::Text(if spatial { "features" } else { "attributes" }.into()),
            Value::Text(name.into()),
            Value::Text(String::new()),
            Value::Text(now()),
            min_x,
            min_y,
            max_x,
            max_y,
            Value::Int(srs_id),
        ]);
        if spatial {
            self.geometry_columns.push(vec![
                Value::Text(name.into()),
                Value::Text("geom".into()),
                Value::Text(cols.type_name.clone()),
                Value::Int(srs_id),
                Value::Int(i64::from(cols.has_z)),
                Value::Int(0),
            ]);
        }
        self.ogr_contents.push(vec![Value::Text(name.into()), Value::Int(count as i64)]);
        self.objects.extend(feature_count_triggers(name));
        self.sequence.push((name.into(), max_fid));
        match table {
            LayerTable::Rows(t) => self.tables.push(t),
            LayerTable::Built(b) => self.built.push((self.tables.len(), b)),
        }
        if spatial {
            let (tree, rtree_tables, rtree_objects) = rtree_index(name, "geom", &self.fid_column, extent.entries);
            self.extension(
                Some(name),
                Some("geom"),
                "gpkg_rtree_index",
                "http://www.geopackage.org/spec120/#extension_rtree",
                "write-only",
            );
            let [rowid, node, parent] = rtree_tables;
            let shadow: [(_, Box<sqlite::Rows>); 3] =
                [(rowid, Box::new(tree.rowid_rows())), (node, tree.node_rows()), (parent, tree.parent_rows())];
            for ((name, sql), mut rows) in shadow {
                let t = Table { name, sql, rows: Vec::new(), autoindexes: vec![] };
                match &mut write {
                    None => self.tables.push(Table { rows: rows.collect(), ..t }),
                    Some(write) => {
                        let built = write(t, &mut rows)?;
                        self.built.push((self.tables.len(), built));
                    }
                }
            }
            self.objects.extend(rtree_objects);
        }
        Ok(())
    }

    /// Adds a tile table: its `gpkg_contents` row (`data_type` is `tiles` or
    /// `2d-gridded-coverage`), tile matrix set and matrices, and tiles
    /// `(zoom_level, tile_column, tile_row, tile_data)`. Returns the tiles' ids, in order.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_tiles(
        &mut self,
        name: &str,
        data_type: &str,
        srs_id: i64,
        extent: [f64; 4],
        matrix_set: [f64; 4],
        matrices: &[(i64, i64, i64, i64, i64, f64, f64)],
        tiles: Vec<(i64, i64, i64, Vec<u8>)>,
    ) -> Result<Vec<i64>> {
        self.claim(name)?;
        self.contents.push(vec![
            Value::Text(name.into()),
            Value::Text(data_type.into()),
            Value::Text(name.into()),
            Value::Text(String::new()),
            Value::Text(now()),
            Value::Real(extent[0]),
            Value::Real(extent[1]),
            Value::Real(extent[2]),
            Value::Real(extent[3]),
            Value::Int(srs_id),
        ]);
        self.tile_matrix_set.push(vec![
            Value::Text(name.into()),
            Value::Int(srs_id),
            Value::Real(matrix_set[0]),
            Value::Real(matrix_set[1]),
            Value::Real(matrix_set[2]),
            Value::Real(matrix_set[3]),
        ]);
        for &(zoom, mw, mh, tw, th, px, py) in matrices {
            self.tile_matrix.push(vec![
                Value::Text(name.into()),
                Value::Int(zoom),
                Value::Int(mw),
                Value::Int(mh),
                Value::Int(tw),
                Value::Int(th),
                Value::Real(px),
                Value::Real(py),
            ]);
        }
        let mut ids = Vec::with_capacity(tiles.len());
        let rows: Vec<(i64, Vec<Value>)> = tiles
            .into_iter()
            .enumerate()
            .map(|(i, (z, c, r, data))| {
                ids.push(i as i64 + 1);
                (i as i64 + 1, vec![Value::Null, Value::Int(z), Value::Int(c), Value::Int(r), Value::Blob(data)])
            })
            .collect();
        self.sequence.push((name.into(), rows.len() as i64));
        self.tables.push(Table {
            name: name.into(),
            sql: format!(
                "CREATE TABLE {} (id INTEGER PRIMARY KEY AUTOINCREMENT,zoom_level INTEGER NOT NULL,tile_column INTEGER NOT NULL,tile_row INTEGER NOT NULL,tile_data BLOB NOT NULL,UNIQUE (zoom_level, tile_column, tile_row))",
                quote(name)
            ),
            rows,
            autoindexes: vec![vec![1, 2, 3]],
        });
        self.objects.extend(tile_table_triggers(name));
        Ok(ids)
    }

    /// Declares a tile table a 2D gridded coverage, with its ancillary rows: the coverage's
    /// `(datatype, scale, offset, data_null)` and each tile's `(id, scale, offset, min, max,
    /// mean, std_dev)`.
    pub(crate) fn add_coverage(
        &mut self,
        name: &str,
        coverage: (&str, f64, f64, Option<f64>),
        tiles: Vec<(i64, f64, f64, [Option<f64>; 4])>,
    ) {
        let (datatype, scale, offset, null) = coverage;
        // The extension asks for the WGS 84 3D SRS, which GDAL defines without a WKT 1 form.
        if !self.srs.iter().any(|r| r[3] == Value::Int(4979)) {
            self.srs.push(vec![
                Value::Text("WGS 84 3D".into()),
                Value::Null,
                Value::Text("EPSG".into()),
                Value::Int(4979),
                Value::Text("undefined".into()),
                Value::Null,
            ]);
        }
        self.coverage.push(vec![
            Value::Null,
            Value::Text(name.into()),
            Value::Text(datatype.into()),
            Value::Real(scale),
            Value::Real(offset),
            Value::Real(1.0),
            null.map_or(Value::Null, Value::Real),
            Value::Text("grid-value-is-area".into()),
            Value::Null,
            Value::Text("Height".into()),
            Value::Text("Height".into()),
        ]);
        for (id, scale, offset, stats) in tiles {
            let mut row = vec![Value::Null, Value::Text(name.into()), Value::Int(id), Value::Real(scale), Value::Real(offset)];
            row.extend(stats.map(|v| v.map_or(Value::Null, Value::Real)));
            self.tile_ancillary.push(row);
        }
        self.extension(Some("gpkg_2d_gridded_coverage_ancillary"), None, "gpkg_2d_gridded_coverage", COVERAGE_EXTENSION, "read-write");
        self.extension(Some("gpkg_2d_gridded_tile_ancillary"), None, "gpkg_2d_gridded_coverage", COVERAGE_EXTENSION, "read-write");
        self.extension(Some(name), Some("tile_data"), "gpkg_2d_gridded_coverage", COVERAGE_EXTENSION, "read-write");
    }

    /// Attaches GDAL's metadata XML (`<GDALMultiDomainMetadata>`) to a table.
    pub(crate) fn add_metadata(&mut self, table: &str, xml: String) {
        let id = self.metadata.len() as i64 + 1;
        self.metadata.push(vec![
            Value::Null,
            Value::Text("dataset".into()),
            Value::Text("http://gdal.org".into()),
            Value::Text("text/xml".into()),
            Value::Text(xml),
        ]);
        self.metadata_reference.push(vec![
            Value::Text("table".into()),
            Value::Text(table.into()),
            Value::Null,
            Value::Null,
            Value::Text(now()),
            Value::Int(id),
            Value::Null,
        ]);
        self.extension(Some("gpkg_metadata"), None, "gpkg_metadata", METADATA_EXTENSION, "read-write");
        self.extension(Some("gpkg_metadata_reference"), None, "gpkg_metadata", METADATA_EXTENSION, "read-write");
    }

    /// What the layers added so far contribute, for adding them to an existing file: the rows
    /// of `gpkg_spatial_ref_sys` (the SRS id being `organization_coordsys_id`), `gpkg_contents`,
    /// `gpkg_geometry_columns`, `gpkg_ogr_contents` and `gpkg_extensions` (in the columns of the
    /// tables this module creates), the tables (features and R-tree shadow tables), the schema
    /// objects (triggers and R-tree virtual tables) and the `sqlite_sequence` entries.
    pub(crate) fn into_layer_parts(self) -> LayerParts {
        LayerParts {
            srs: self.srs,
            contents: self.contents,
            geometry_columns: self.geometry_columns,
            ogr_contents: self.ogr_contents,
            extensions: self.extensions,
            tables: self.tables,
            objects: self.objects.into_iter().filter(|o| o.table != "gpkg_tile_matrix").collect(),
            sequence: self.sequence,
        }
    }

    /// Writes the file.
    pub(crate) fn finish(self) -> Result<Vec<u8>> {
        let (page1, rest) = self.finish_parts(2)?;
        Ok([page1, rest].concat())
    }

    /// The parts of the file when pages 2 to `base - 1` hold the tables already written: page 1,
    /// and the pages from `base` on (see [`sqlite::write_database_parts`]).
    pub(crate) fn finish_parts(self, base: u32) -> Result<(Vec<u8>, Vec<u8>)> {
        let numbered =
            |rows: Vec<Vec<Value>>| -> Vec<(i64, Vec<Value>)> { rows.into_iter().enumerate().map(|(i, r)| (i as i64 + 1, r)).collect() };
        // srs_id is the rowid of gpkg_spatial_ref_sys.
        let mut srs_rows: Vec<(i64, Vec<Value>)> = self
            .srs
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
        let table = |name: &str, sql: &str, rows: Vec<(i64, Vec<Value>)>, autoindexes: Vec<Vec<usize>>| Table {
            name: name.into(),
            sql: sql.into(),
            rows,
            autoindexes,
        };
        let mut sequence = self.sequence;
        let mut tables = vec![
            table("gpkg_spatial_ref_sys", SRS_SQL, srs_rows, vec![]),
            table("gpkg_contents", CONTENTS_SQL, numbered(self.contents), vec![vec![0], vec![2]]),
            table("gpkg_ogr_contents", OGR_CONTENTS_SQL, numbered(self.ogr_contents), vec![vec![0]]),
            table("gpkg_geometry_columns", GEOMETRY_COLUMNS_SQL, numbered(self.geometry_columns), vec![vec![0, 1], vec![0]]),
            table("gpkg_tile_matrix_set", TILE_MATRIX_SET_SQL, numbered(self.tile_matrix_set), vec![vec![0]]),
            table("gpkg_tile_matrix", TILE_MATRIX_SQL, numbered(self.tile_matrix), vec![vec![0, 1]]),
            table("gpkg_extensions", EXTENSIONS_SQL, numbered(self.extensions), vec![vec![0, 1, 2]]),
        ];
        if !self.metadata.is_empty() {
            tables.push(table("gpkg_metadata", METADATA_SQL, numbered(self.metadata), vec![]));
            tables.push(table("gpkg_metadata_reference", METADATA_REFERENCE_SQL, numbered(self.metadata_reference), vec![]));
        }
        if !self.coverage.is_empty() {
            sequence.push(("gpkg_2d_gridded_coverage_ancillary".into(), self.coverage.len() as i64));
            sequence.push(("gpkg_2d_gridded_tile_ancillary".into(), self.tile_ancillary.len() as i64));
            tables.push(table("gpkg_2d_gridded_coverage_ancillary", COVERAGE_SQL, numbered(self.coverage), vec![vec![1]]));
            tables.push(table("gpkg_2d_gridded_tile_ancillary", TILE_ANCILLARY_SQL, numbered(self.tile_ancillary), vec![vec![1, 2]]));
        }
        let offset = tables.len();
        tables.extend(self.tables);
        let built: Vec<(usize, BuiltTable)> = self.built.into_iter().map(|(at, b)| (at + offset, b)).collect();
        let seq = numbered(sequence.into_iter().map(|(n, max)| vec![Value::Text(n), Value::Int(max)]).collect());
        tables.push(table("sqlite_sequence", "CREATE TABLE sqlite_sequence(name,seq)", seq, vec![]));
        // GeoPackage 1.4: application id "GPKG", user version 10400.
        sqlite::write_database_parts(&tables, &built, &[], &self.objects, 10400, 0x4750_4B47, base)
    }
}

/// The parts of a GeoPackage that vector layers make: see [`Builder::into_layer_parts`].
pub(crate) struct LayerParts {
    pub srs: Vec<Vec<Value>>,
    pub contents: Vec<Vec<Value>>,
    pub geometry_columns: Vec<Vec<Value>>,
    pub ogr_contents: Vec<Vec<Value>>,
    pub extensions: Vec<Vec<Value>>,
    pub tables: Vec<Table>,
    pub objects: Vec<SchemaObject>,
    pub sequence: Vec<(String, i64)>,
}

/// Writes layers as a GeoPackage whose feature id columns are named `fid_column`.
pub(crate) fn write_with_fid(layers: &[Layer], path: &str, fid_column: &str) -> Result<()> {
    let mut b = Builder::new();
    b.set_fid_column(fid_column);
    for layer in layers {
        b.add_layer(layer)?;
    }
    std::fs::write(path, b.finish()?)?;
    Ok(())
}

/// Encodes layers as a GeoPackage file.
pub fn to_bytes(layers: &[Layer]) -> Result<Vec<u8>> {
    let mut b = Builder::new();
    for layer in layers {
        b.add_layer(layer)?;
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Coord, Feature, FieldDef};

    /// The R-tree as it was built whole, before its rows were made as they are read.
    fn old_str_pack(mut entries: Vec<RtreeEntry>) -> Vec<Vec<RtreeEntry>> {
        let n = entries.len();
        let nodes = n.div_ceil(RTREE_MAX_CELLS).max(1);
        let slices = (nodes as f64).sqrt().ceil() as usize;
        let per_slice = slices * RTREE_MAX_CELLS;
        let cx = |e: &RtreeEntry| f64::from(e.1[0]) + f64::from(e.1[1]);
        let cy = |e: &RtreeEntry| f64::from(e.1[2]) + f64::from(e.1[3]);
        entries.sort_by(|a, b| cx(a).total_cmp(&cx(b)));
        let mut out = Vec::with_capacity(nodes);
        for slice in entries.chunks_mut(per_slice) {
            slice.sort_by(|a, b| cy(a).total_cmp(&cy(b)));
            out.extend(slice.chunks(RTREE_MAX_CELLS).map(<[RtreeEntry]>::to_vec));
        }
        out
    }

    fn old_rtree(entries: Vec<RtreeEntry>) -> RtreeRows {
        let encode = |depth: u16, cells: &[RtreeEntry]| {
            let mut b = Vec::with_capacity(RTREE_NODE_SIZE);
            b.extend_from_slice(&depth.to_be_bytes());
            b.extend_from_slice(&(cells.len() as u16).to_be_bytes());
            for (id, bx) in cells {
                b.extend_from_slice(&id.to_be_bytes());
                for v in bx {
                    b.extend_from_slice(&v.to_be_bytes());
                }
            }
            b.resize(RTREE_NODE_SIZE, 0);
            b
        };
        let (mut nodes, mut rowids, mut parents) = (Vec::new(), Vec::new(), Vec::new());
        if entries.is_empty() {
            nodes.push((1, vec![Value::Null, Value::Blob(vec![0; RTREE_NODE_SIZE])]));
            return (nodes, rowids, parents);
        }
        // Levels from the leaves up; node numbers are given once the root (node 1) is known.
        let mut levels: Vec<Vec<Vec<RtreeEntry>>> = vec![old_str_pack(entries)];
        while levels.last().is_some_and(|l| l.len() > 1) {
            let last = levels.last().map(Vec::as_slice).unwrap_or_default();
            // Placeholder ids: the index of the child in its level.
            let up: Vec<RtreeEntry> = last.iter().enumerate().map(|(i, cells)| (i as i64, union(cells))).collect();
            levels.push(old_str_pack(up));
        }
        // Number the nodes from the root down.
        let depth = levels.len() - 1;
        let mut numbers: Vec<Vec<i64>> = levels.iter().map(|l| vec![0; l.len()]).collect();
        let mut next = 1;
        for level in (0..levels.len()).rev() {
            for n in numbers[level].iter_mut() {
                *n = next;
                next += 1;
            }
        }
        for (level, nodes_at) in levels.iter().enumerate() {
            for (i, cells) in nodes_at.iter().enumerate() {
                let me = numbers[level][i];
                let cells: Vec<RtreeEntry> = if level == 0 {
                    for (id, _) in cells {
                        rowids.push((*id, vec![Value::Null, Value::Int(me)]));
                    }
                    cells.clone()
                } else {
                    cells
                        .iter()
                        .map(|(child, bx)| {
                            let child = numbers[level - 1][*child as usize];
                            parents.push((child, vec![Value::Null, Value::Int(me)]));
                            (child, *bx)
                        })
                        .collect()
                };
                let d = if level == depth { depth as u16 } else { 0 };
                nodes.push((me, vec![Value::Null, Value::Blob(encode(d, &cells))]));
            }
        }
        nodes.sort_by_key(|(n, _)| *n);
        rowids.sort_by_key(|(n, _)| *n);
        parents.sort_by_key(|(n, _)| *n);
        (nodes, rowids, parents)
    }

    #[test]
    fn rtree_rows_are_those_built_whole() {
        // Up to four levels, with boxes in no order and repeated.
        for n in [0, 1, 51, 52, 2601, 2602, 140_000] {
            let entries: Vec<RtreeEntry> = (0..n)
                .map(|i: i64| {
                    let (x, y) = (((i * 7919) % 1000) as f32, ((i * 104_729) % 997) as f32 / 3.0);
                    (i + 1, [x, x + (i % 5) as f32, y, y + 1.0])
                })
                .collect();
            let (nodes, rowids, parents) = build_rtree(entries.clone());
            let want = old_rtree(entries);
            assert!(nodes == want.0 && rowids == want.1 && parents == want.2, "{n} entries");
        }
    }

    #[test]
    fn retains_declared_wide_integer_fields() {
        let mut layer = Layer::new("places");
        layer.add_field(FieldDef::new("population", FieldType::Integer).width(10));
        layer.push(Feature { fid: 1, geometry: None, attributes: vec![FieldValue::Integer(42)] });
        let mut builder = Builder::new();
        let columns = builder.layer_columns(&layer, true, None, false, None, |_| true);
        assert!(columns.sql.contains("\"population\" INTEGER"), "{}", columns.sql);
    }

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
