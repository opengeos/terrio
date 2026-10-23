//! Appending points to an EPT dataset in place, as Entwine does when it builds into an existing
//! output: the new points go into the dataset's octree (its cube, `bounds`, unchanged), each node
//! keeping the first new point to land in each free cell of its `span`³ grid and passing the
//! others down; a leaf (a node without children) takes the points that reach it until it holds
//! more than the leaf size, and is then split as the writer splits nodes.
//!
//! The update is staged in a directory of its own in the dataset's (`.terrio-ept-append-N`):
//! the tiles written (new nodes, and nodes with points added), the hierarchy pages changed, the
//! source list and `ept.json`. Once all are written and synced, a `COMMITTED` marker is written,
//! then the files are moved into place in this order: the tiles, the source metadata and list, the
//! hierarchy pages (the root page last), and `ept.json` last of all. An update interrupted before
//! the marker leaves the dataset as it was (the staging directory is removed by the next append);
//! one interrupted after it is completed by the next append, which moves the remaining files in.
//!
//! The points are spilled to a temporary file as they come, then placed one node at a time from
//! the root down, the points of each child in memory or in a temporary file of their own: memory
//! stays bounded whatever the number of points appended (the hierarchy is held whole).

use super::copc::{VoxelKey, gps_offset, scramble};
use super::ept::{EptDataType, EptDimension};
use super::las::{LasHeader, Layout};
use super::values::{ValueType, Values};
use super::{PointCloud, PointWriteOptions};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The prefix of the directories updates are staged in.
const STAGING: &str = ".terrio-ept-append";
/// The file that marks a staged update complete: from then on, it is moved into place.
const COMMITTED: &str = "COMMITTED";
/// The most points of a leaf, by default (as the writer's).
const LEAF_POINTS: u64 = 100_000;
/// The deepest level a node is split to, by default.
const MAX_DEPTH: i32 = 16;
/// The most points held in memory at a time, by default.
const MEMORY_POINTS: u64 = 4_000_000;
/// The indices given to the points of a split leaf, placed again (above any input's).
const REPLACED: u64 = 1 << 62;

fn io_err(p: &Path, e: std::io::Error) -> Error {
    Error::Io(std::io::Error::new(e.kind(), format!("'{}': {e}", p.display())))
}

fn key_name(k: VoxelKey) -> String {
    format!("{}-{}-{}-{}", k.level, k.x, k.y, k.z)
}

fn parse_key(s: &str) -> Result<VoxelKey> {
    let p: Vec<i32> = s.split('-').map(|v| v.parse::<i32>()).collect::<std::result::Result<_, _>>().map_err(|_| bad_key(s))?;
    match p[..] {
        [level, x, y, z] if level >= 0 => Ok(VoxelKey { level, x, y, z }),
        _ => Err(bad_key(s)),
    }
}

fn bad_key(s: &str) -> Error {
    Error::InvalidData(format!("EPT hierarchy: invalid node key '{s}'"))
}

fn parent(k: VoxelKey) -> VoxelKey {
    VoxelKey { level: k.level - 1, x: k.x >> 1, y: k.y >> 1, z: k.z >> 1 }
}

fn children(k: VoxelKey) -> impl Iterator<Item = VoxelKey> {
    (0..8i32).map(move |i| VoxelKey { level: k.level + 1, x: 2 * k.x + (i & 1), y: 2 * k.y + ((i >> 1) & 1), z: 2 * k.z + (i >> 2) })
}

/// A little-endian number of a type, as a float.
fn read_num(b: &[u8], t: ValueType) -> f64 {
    let a = |n: usize| -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..n].copy_from_slice(&b[..n]);
        out
    };
    match t {
        ValueType::U8 => b[0] as f64,
        ValueType::I8 => b[0] as i8 as f64,
        ValueType::U16 => u16::from_le_bytes([b[0], b[1]]) as f64,
        ValueType::I16 => i16::from_le_bytes([b[0], b[1]]) as f64,
        ValueType::U32 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        ValueType::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        ValueType::U64 => u64::from_le_bytes(a(8)) as f64,
        ValueType::I64 => i64::from_le_bytes(a(8)) as f64,
        ValueType::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        ValueType::F64 => f64::from_le_bytes(a(8)),
    }
}

/// Writes a float as a little-endian number of a type (rounded and saturated for integers).
fn write_num(out: &mut [u8], t: ValueType, v: f64) {
    let mut c = Values::with_capacity(t, 1);
    c.push_f64(if matches!(t, ValueType::F32 | ValueType::F64) { v } else { v.round() });
    c.write_le(0, out);
}

/// How the records of the dataset's tiles are held while placed: LAS records of the tiles'
/// format (LASzip tiles), or records of the schema (binary and Zstandard tiles).
enum Codec {
    Las { header: Box<LasHeader>, layout: Layout },
    Schema,
}

/// The statistics of a schema dimension, when `ept.json` has them.
#[derive(Default, Clone)]
struct DimStats {
    n: u64,
    min: f64,
    max: f64,
    mean: f64,
    m2: f64,
    counts: BTreeMap<i64, u64>,
}

impl DimStats {
    fn add(&mut self, v: f64, counts: bool) {
        if self.n == 0 {
            self.min = v;
            self.max = v;
        }
        self.n += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        let d = v - self.mean;
        self.mean += d / self.n as f64;
        self.m2 += d * (v - self.mean);
        if counts {
            *self.counts.entry(v as i64).or_default() += 1;
        }
    }
}

/// A source of the points appended.
struct NewSource {
    path: String,
    lo: [f64; 3],
    hi: [f64; 3],
    points: u64,
    srs: Value,
}

/// Points to place below a node: `(index, record)` entries, in memory or in a temporary file.
struct Batch {
    n: u64,
    mem: Vec<u8>,
    file: Option<tempfile::NamedTempFile>,
}

impl Batch {
    /// Calls `f` with the entries, at most `chunk` at a time.
    fn for_each_chunk(&self, entry: usize, chunk: u64, mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        if !self.mem.is_empty() {
            for c in self.mem.chunks(entry * chunk.max(1) as usize) {
                f(c)?;
            }
        }
        if let Some(file) = &self.file {
            let mut r = std::io::BufReader::with_capacity(1 << 20, file.as_file());
            r.seek(SeekFrom::Start(0))?;
            let total = self.n - (self.mem.len() / entry) as u64;
            let mut left = total;
            let mut buf = Vec::new();
            while left > 0 {
                let m = left.min(chunk.max(1));
                buf.resize(m as usize * entry, 0);
                r.read_exact(&mut buf)?;
                f(&buf)?;
                left -= m;
            }
        }
        Ok(())
    }

    /// All the entries.
    fn load(&self, entry: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.n as usize * entry);
        self.for_each_chunk(entry, u64::MAX / entry as u64, |c| {
            out.extend_from_slice(c);
            Ok(())
        })?;
        Ok(out)
    }
}

/// The points passed down to a child while its parent is placed.
#[derive(Default)]
struct ChildBuf {
    n: u64,
    mem: Vec<u8>,
    file: Option<BufWriter<tempfile::NamedTempFile>>,
}

impl ChildBuf {
    fn push(&mut self, e: &[u8], limit: usize) -> Result<()> {
        self.mem.extend_from_slice(e);
        self.n += 1;
        if self.mem.len() >= limit {
            self.spill()?;
        }
        Ok(())
    }

    /// Moves the entries in memory to the file.
    fn spill(&mut self) -> Result<()> {
        if self.mem.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            self.file = Some(BufWriter::with_capacity(1 << 20, tempfile::NamedTempFile::new()?));
        }
        if let Some(f) = &mut self.file {
            f.write_all(&self.mem)?;
        }
        self.mem = Vec::new();
        Ok(())
    }

    fn into_batch(mut self) -> Result<Batch> {
        match self.file.take() {
            Some(f) => {
                let mut f = f;
                f.write_all(&self.mem)?;
                let file = f.into_inner().map_err(|e| Error::Io(e.into_error()))?;
                Ok(Batch { n: self.n, mem: Vec::new(), file: Some(file) })
            }
            None => Ok(Batch { n: self.n, mem: self.mem, file: None }),
        }
    }
}

/// Appends the points of one or more point clouds to an existing EPT dataset, in place.
///
/// The points go into the dataset's octree: its cube (`bounds`) stays, and points outside it are
/// refused (the dataset is left as it was). Each node keeps the first new point to land in each
/// free cell of its grid (`span` cells per side) and passes the others down to its children; a
/// leaf takes the points that reach it until it holds more than
/// [`PointWriteOptions::copc_leaf_points`] (100 000 by default), and is then split as the
/// writer splits nodes, its points placed again. The hierarchy, `ept.json` (`points`,
/// `boundsConforming`, the schema's statistics when it has them) and the source list
/// (`ept-sources/manifest.json`, one entry per source, created when missing) are updated.
///
/// The points take the dataset's schema: dimensions the dataset has and the points do not are
/// written as zeros, those the points have and the dataset does not are dropped (with a warning,
/// see [`crate::warnings`]). `OriginId`, when the schema has it, is the source's position in the
/// source list. The points must be in the dataset's CRS (when both have one).
///
/// The update is written beside the dataset and moved into place once complete, `ept.json`
/// last: an append interrupted before it is complete leaves the dataset as it was, and one
/// interrupted while the files are moved in is completed by the next append (see the module's
/// documentation). One append at a time.
///
/// ```no_run
/// use terrio::pointcloud::{self, EptAppender};
///
/// let mut appender = EptAppender::open("tiles-ept/ept.json", &Default::default())?;
/// let mut reader = pointcloud::open_points("tile-2.laz")?;
/// appender.start_source("tile-2.laz", &reader.template()?)?;
/// for chunk in reader.chunks(&Default::default(), 1_000_000)? {
///     appender.write(&chunk?)?;
/// }
/// appender.finish()?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub struct EptAppender {
    dir: PathBuf,
    json: Value,
    cube: [f64; 6],
    span: u64,
    data_type: EptDataType,
    schema: Vec<EptDimension>,
    /// The stored type and offset of each schema dimension in a binary record.
    stored: Vec<(ValueType, usize)>,
    codec: Codec,
    /// The length of a record.
    rl: usize,
    /// Where X, Y and Z are in a binary record: offset, type, scale, offset.
    xyz: [(usize, ValueType, f64, f64); 3],
    /// Where the GPS time of a LAS record is.
    gps: Option<usize>,
    /// Where the origin ids are in a record, and their type.
    origin: Option<(usize, ValueType)>,
    crs: SpatialRef,
    hierarchy: BTreeMap<VoxelKey, u64>,
    pages: BTreeSet<VoxelKey>,
    leaf_points: u64,
    max_depth: i32,
    memory_points: u64,
    spill: BufWriter<tempfile::NamedTempFile>,
    n: u64,
    lo: [f64; 3],
    hi: [f64; 3],
    /// The source list read, its file name, and whether it exists.
    sources: Vec<Value>,
    sources_file: &'static str,
    sources_exist: bool,
    new_sources: Vec<NewSource>,
    /// The statistics of the points appended, for the schema dimensions that have some.
    stats: Vec<Option<DimStats>>,
    /// Whether the source being written is of a LAS point format of 0 to 5.
    legacy: bool,
}

impl std::fmt::Debug for EptAppender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EptAppender").field("dir", &self.dir).field("points", &self.n).finish_non_exhaustive()
    }
}

/// Completes the updates of a dataset interrupted while they were moved into place, and removes
/// those interrupted before they were complete.
fn recover(dir: &Path) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| io_err(dir, e))?;
    let mut staged: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING))
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    staged.sort();
    for s in staged {
        if s.join(COMMITTED).exists() {
            move_in(dir, &s)?;
        } else {
            std::fs::remove_dir_all(&s).map_err(|e| io_err(&s, e))?;
        }
    }
    Ok(())
}

/// Moves a staged update into the dataset: the tiles, the source metadata and list, the
/// hierarchy pages (the root last), then `ept.json`. Files already moved (by an interrupted
/// move) are skipped.
fn move_in(dir: &Path, stage: &Path) -> Result<()> {
    let mv = |rel: &Path| -> Result<()> {
        let from = stage.join(rel);
        if from.exists() {
            let to = dir.join(rel);
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p).map_err(|e| io_err(p, e))?;
            }
            std::fs::rename(&from, &to).map_err(|e| io_err(&to, e))?;
        }
        Ok(())
    };
    let list = |sub: &str| -> Result<Vec<String>> {
        let d = stage.join(sub);
        if !d.exists() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = std::fs::read_dir(&d)
            .map_err(|e| io_err(&d, e))?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names)
    };
    for name in list("ept-data")? {
        mv(&Path::new("ept-data").join(name))?;
    }
    let sources = list("ept-sources")?;
    let is_list = |n: &str| n == "manifest.json" || n == "list.json";
    for name in sources.iter().filter(|n| !is_list(n)).chain(sources.iter().filter(|n| is_list(n))) {
        mv(&Path::new("ept-sources").join(name))?;
    }
    let pages = list("ept-hierarchy")?;
    for name in pages.iter().filter(|n| *n != "0-0-0-0.json").chain(pages.iter().filter(|n| *n == "0-0-0-0.json")) {
        mv(&Path::new("ept-hierarchy").join(name))?;
    }
    mv(Path::new("ept.json"))?;
    std::fs::remove_dir_all(stage).map_err(|e| io_err(stage, e))
}

/// Writes a file and syncs it to disk.
fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::File::create(path).map_err(|e| io_err(path, e))?;
    f.write_all(bytes).map_err(|e| io_err(path, e))?;
    f.sync_all().map_err(|e| io_err(path, e))
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(v).map_err(|e| Error::InvalidData(format!("JSON: {e}")))?;
    write_synced(path, text.as_bytes())
}

fn read_json(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::InvalidData(format!("'{}': {e}", path.display())))
}

fn bounds6(v: &Value) -> Option<[f64; 6]> {
    let a = v.as_array().filter(|a| a.len() == 6)?;
    let mut b = [0.0; 6];
    for (o, x) in b.iter_mut().zip(a) {
        *o = x.as_f64()?;
    }
    Some(b)
}

/// The statistics keys of a schema dimension.
const STATS_KEYS: [&str; 7] = ["count", "minimum", "maximum", "mean", "variance", "stddev", "counts"];

impl EptAppender {
    /// Opens an EPT dataset to append points to: its `ept.json`, its directory, or an `ept://`
    /// path. An update interrupted earlier is completed (or discarded) first. Of `options`,
    /// `copc_leaf_points` (the most points of a leaf), `copc_max_depth` (the deepest level nodes
    /// are split to) and `copc_memory_points` (the most points held in memory) apply.
    pub fn open(path: &str, options: &PointWriteOptions) -> Result<Self> {
        if !crate::vfs::Location::parse(path).is_local() {
            return Err(Error::Unsupported(format!("cannot append to '{path}': only local EPT datasets are updated in place")));
        }
        let dir = super::ept_write::dataset_dir(path);
        if !dir.join("ept.json").exists() {
            return Err(Error::InvalidArgument(format!("'{}' is not an EPT dataset: it has no ept.json", dir.display())));
        }
        recover(&dir)?;
        let json = read_json(&dir.join("ept.json"))?;
        // The dataset as the reader sees it: its schema, data type and CRS.
        let reader = super::ept::EptReader::open(&dir.join("ept.json").to_string_lossy())?;
        let info = super::read::FormatReader::ept_info(&reader).cloned().ok_or_else(|| Error::InvalidData("EPT: no metadata".into()))?;
        let crs = super::read::FormatReader::crs(&reader).horizontal.clone();
        drop(reader);
        let data_type = EptDataType::parse(&info.data_type)?;
        let span = info.span;
        if span == 0 {
            return Err(Error::InvalidData("EPT: a span of 0".into()));
        }
        let cube = info.bounds;
        let schema = info.schema.clone();
        let mut stored = Vec::new();
        let mut at = 0;
        for d in &schema {
            let t = d.stored_type()?;
            if t.size() != d.size {
                return Err(Error::InvalidData(format!("EPT: dimension {} of size {}", d.name, d.size)));
            }
            stored.push((t, at));
            at += d.size;
        }
        let mut xyz = [(0, ValueType::I32, 1.0, 0.0); 3];
        for (k, axis) in ["X", "Y", "Z"].iter().enumerate() {
            let i =
                schema.iter().position(|d| d.name == *axis).ok_or_else(|| Error::InvalidData(format!("EPT: no {axis} in the schema")))?;
            xyz[k] = (stored[i].1, stored[i].0, schema[i].scale.unwrap_or(1.0), schema[i].offset.unwrap_or(0.0));
        }
        // The hierarchy, all its pages.
        let (hierarchy, pages) = load_hierarchy(&dir)?;
        let (codec, rl, gps, origin) = match data_type {
            EptDataType::Laszip => {
                // The tiles' LAS header: the root's (or any node's).
                let key = hierarchy
                    .iter()
                    .find(|(_, n)| **n > 0)
                    .map(|(k, _)| *k)
                    .ok_or_else(|| Error::InvalidArgument("the EPT dataset has no points to append to: write it anew".into()))?;
                let bytes = std::fs::read(tile_path(&dir, key, data_type)).map_err(|e| io_err(&tile_path(&dir, key, data_type), e))?;
                let header = LasHeader::read(&mut Cursor::new(&bytes))?;
                let layout = Layout::new(&header);
                let origin = layout.fields.iter().find(|f| is_origin(&f.name)).and_then(|f| match f.field {
                    super::las::Field::Extra(o) | super::las::Field::Plain(o) => Some((o, f.value_type)),
                    _ => None,
                });
                let rl = layout.record_len;
                let gps = gps_offset(header.point_format);
                (Codec::Las { header: Box::new(header), layout }, rl, gps, origin)
            }
            _ => {
                let origin = schema.iter().position(|d| is_origin(&d.name)).map(|i| (stored[i].1, stored[i].0));
                let gps = schema.iter().position(|d| d.name == "GpsTime" && d.kind == "float" && d.size == 8).map(|i| stored[i].1);
                (Codec::Schema, at, gps, origin)
            }
        };
        if rl == 0 {
            return Err(Error::InvalidData("EPT: an empty schema".into()));
        }
        // The source list: Entwine 2's `manifest.json`, or Entwine 1's `list.json`.
        let (sources, sources_file, sources_exist) = {
            let m = dir.join("ept-sources/manifest.json");
            let l = dir.join("ept-sources/list.json");
            if m.exists() {
                (read_json(&m)?.as_array().cloned().unwrap_or_default(), "manifest.json", true)
            } else if l.exists() {
                (read_json(&l)?.as_array().cloned().unwrap_or_default(), "list.json", true)
            } else {
                (Vec::new(), "manifest.json", false)
            }
        };
        let stats = json["schema"]
            .as_array()
            .map(|a| a.iter().map(|d| STATS_KEYS.iter().any(|k| d.get(*k).is_some()).then(DimStats::default)).collect())
            .unwrap_or_else(|| vec![None; schema.len()]);
        Self {
            dir,
            json,
            cube,
            span,
            data_type,
            schema,
            stored,
            codec,
            rl,
            xyz,
            gps,
            origin,
            crs,
            hierarchy,
            pages,
            leaf_points: options.copc_leaf_points.unwrap_or(LEAF_POINTS).max(1),
            max_depth: options.copc_max_depth.map_or(MAX_DEPTH, |d| d.min(30) as i32),
            memory_points: options.copc_memory_points.unwrap_or(MEMORY_POINTS).max(1),
            spill: BufWriter::with_capacity(1 << 20, tempfile::NamedTempFile::new()?),
            n: 0,
            lo: [f64::INFINITY; 3],
            hi: [f64::NEG_INFINITY; 3],
            sources,
            sources_file,
            sources_exist,
            new_sources: Vec::new(),
            stats,
            legacy: false,
        }
        .with_source_list()
    }

    /// The source list of a dataset without one, made now: an entry for the points already
    /// there, or, when the schema has `OriginId`, one for each origin id of theirs (read from
    /// every tile), so that the new sources' ids follow theirs.
    fn with_source_list(mut self) -> Result<Self> {
        let before = self.json["points"].as_u64().unwrap_or(0);
        if self.sources_exist || before == 0 {
            return Ok(self);
        }
        let Some((at, t)) = self.origin else {
            let b = bounds6(&self.json["boundsConforming"]).unwrap_or(self.cube);
            self.sources = vec![json!({ "path": "(the points before the first append)", "bounds": b, "inserted": true, "points": before })];
            return Ok(self);
        };
        // (points, bounds) of each origin id.
        let mut origins: BTreeMap<u64, (u64, [f64; 6])> = BTreeMap::new();
        let keys: Vec<VoxelKey> = self.hierarchy.iter().filter(|(_, n)| **n > 0).map(|(k, _)| *k).collect();
        for key in keys {
            for rec in self.read_tile(key)?.chunks_exact(self.rl) {
                let id = read_num(&rec[at..], t).max(0.0) as u64;
                let p = xyz_of(&self.codec, &self.xyz, rec);
                let e = origins
                    .entry(id)
                    .or_insert((0, [f64::INFINITY, f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY]));
                e.0 += 1;
                for (k, v) in p.into_iter().enumerate() {
                    e.1[k] = e.1[k].min(v);
                    e.1[k + 3] = e.1[k + 3].max(v);
                }
            }
        }
        let last = origins.keys().next_back().copied().unwrap_or(0);
        if last > 1 << 20 {
            return Err(Error::InvalidData(format!("EPT: an OriginId of {last} and no source list")));
        }
        self.sources = (0..=last)
            .map(|id| {
                let (n, b) = origins.get(&id).copied().unwrap_or((0, [0.0; 6]));
                json!({ "path": format!("(origin {id} of the points before the first append)"), "bounds": b, "inserted": true, "points": n })
            })
            .collect();
        Ok(self)
    }

    /// Starts a source: the points written next are its own, an entry of the source list. `name`
    /// is its path, as listed; `template` has its header, CRS and dimensions (its points are not
    /// written). Fails when its CRS is not the dataset's.
    pub fn start_source(&mut self, name: &str, template: &PointCloud) -> Result<()> {
        if !self.crs.is_unknown() && !template.crs.is_unknown() && !self.crs.same_as(&template.crs) {
            return Err(Error::Crs(format!(
                "'{name}' is in {}, the EPT dataset in {}: reproject the points first (--t-srs)",
                template.crs.label(),
                self.crs.label()
            )));
        }
        // The dimensions dropped: those the dataset does not store.
        let stored: Vec<String> = match &self.codec {
            // Formats 0 to 5 hold overlap points as class 12.
            Codec::Las { layout, .. } if layout.format < 6 => {
                layout.fields.iter().map(|f| f.name.clone()).chain(std::iter::once("overlap".to_string())).collect()
            }
            Codec::Las { layout, .. } => layout.fields.iter().map(|f| f.name.clone()).collect(),
            Codec::Schema => self.schema.iter().map(|d| super::names::from_pdal(&d.name)).collect(),
        };
        let kept = |n: &str| {
            stored.iter().any(|s| s == n || super::names::from_pdal(s) == n)
                || (n == "scan_angle" && stored.iter().any(|s| s == "scan_angle_rank"))
                || (n == "scan_angle_rank" && stored.iter().any(|s| s == "scan_angle"))
                || (["synthetic", "key_point", "withheld", "overlap"].contains(&n) && stored.iter().any(|s| s == "class_flags"))
                || n.starts_with("wavepacket")
                || n.starts_with("waveform")
                || n.starts_with("return_point")
                || ["x_t", "y_t", "z_t"].contains(&n)
        };
        let dropped: Vec<&str> = template.dimensions.iter().map(|d| d.name.as_str()).filter(|n| !kept(n)).collect();
        if !dropped.is_empty() {
            crate::warnings::warn(format!("'{name}': dimensions the EPT dataset does not have, not appended: {}", dropped.join(", ")));
        }
        let mut srs = serde_json::Map::new();
        if let Some(code) = template.crs.epsg {
            srs.insert("authority".into(), json!("EPSG"));
            srs.insert("horizontal".into(), json!(code.to_string()));
        }
        if let Some(wkt) = template.crs.to_wkt() {
            srs.insert("wkt".into(), json!(wkt));
        }
        self.legacy = template.header.point_format < 6;
        self.new_sources.push(NewSource {
            path: name.to_string(),
            lo: [f64::INFINITY; 3],
            hi: [f64::NEG_INFINITY; 3],
            points: 0,
            srs: Value::Object(srs),
        });
        Ok(())
    }

    /// The position of the current source in the source list (its `OriginId`).
    fn origin_id(&self) -> usize {
        self.sources.len() + self.new_sources.len() - 1
    }

    /// The values of the schema's dimensions for the points of a cloud, as stored.
    fn schema_values(&self, cloud: &PointCloud) -> Vec<Values> {
        let n = cloud.len();
        let f64s = |name: &str| -> Option<Vec<f64>> { cloud.dim(name).map(|d| d.to_f64()) };
        self.schema
            .iter()
            .zip(&self.stored)
            .map(|(d, (t, _))| {
                let name = super::names::from_pdal(&d.name);
                let values: Option<Vec<f64>> = match name.as_str() {
                    "scan_angle_rank" => {
                        f64s("scan_angle_rank").or_else(|| f64s("scan_angle").map(|v| v.iter().map(|a| a * 0.006).collect()))
                    }
                    "class_flags" => f64s("class_flags").or_else(|| {
                        let flags: Vec<Option<Vec<f64>>> =
                            ["synthetic", "key_point", "withheld", "overlap"].iter().map(|f| f64s(f)).collect();
                        flags.iter().any(Option::is_some).then(|| {
                            (0..n)
                                .map(|i| {
                                    flags.iter().enumerate().map(|(b, f)| f.as_ref().map_or(0, |f| ((f[i] != 0.0) as u8) << b)).sum::<u8>()
                                        as f64
                                })
                                .collect()
                        })
                    }),
                    // LAS formats 0 to 5 mark overlap points with class 12: PDAL reads them as
                    // class 1 with the overlap flag, as the writer stores them.
                    "classification" if self.legacy && cloud.dim("overlap").is_none() => {
                        f64s("classification").map(|v| v.into_iter().map(|c| if c == 12.0 { 1.0 } else { c }).collect())
                    }
                    "overlap" if self.legacy && cloud.dim("overlap").is_none() => {
                        f64s("classification").map(|v| v.into_iter().map(|c| (c == 12.0) as u8 as f64).collect())
                    }
                    _ => f64s(&name),
                };
                let mut out = Values::with_capacity(*t, n);
                match values {
                    Some(v) => {
                        for x in v {
                            let raw = match d.scale {
                                Some(s) => ((x - d.offset.unwrap_or(0.0)) / s).round(),
                                None if matches!(t, ValueType::F32 | ValueType::F64) => x,
                                None => x.round(),
                            };
                            out.push_f64(raw);
                        }
                    }
                    None => out = Values::zeros(*t, n),
                }
                out
            })
            .collect()
    }

    /// Writes points of the current source.
    pub fn write(&mut self, cloud: &PointCloud) -> Result<()> {
        if self.new_sources.is_empty() {
            return Err(Error::InvalidArgument("EptAppender: start a source before writing points".into()));
        }
        let n = cloud.len();
        if n == 0 {
            return Ok(());
        }
        let rl = self.rl;
        let need_values = matches!(self.codec, Codec::Schema) || self.stats.iter().any(Option::is_some);
        let cols = if need_values { self.schema_values(cloud) } else { Vec::new() };
        let mut records = match &self.codec {
            Codec::Las { header, layout } => super::write::encode(cloud, layout, header.scale, header.offset)?,
            Codec::Schema => {
                let mut out = vec![0u8; n * rl];
                for (rec, i) in out.chunks_exact_mut(rl).zip(0..) {
                    for (c, (t, at)) in cols.iter().zip(&self.stored) {
                        c.write_le(i, &mut rec[*at..*at + t.size()]);
                    }
                }
                out
            }
        };
        // LAS formats 0 to 5 store overlap points as class 12.
        if let (Codec::Las { header, .. }, Some(overlap)) = (&self.codec, cloud.dim("overlap"))
            && header.point_format < 6
        {
            for (i, rec) in records.chunks_exact_mut(rl).enumerate() {
                if overlap.values.get_f64(i) != 0.0 {
                    rec[15] = (rec[15] & !31) | 12;
                }
            }
        }
        let origin = self.origin_id();
        if let Some((at, t)) = self.origin {
            for rec in records.chunks_exact_mut(rl) {
                write_num(&mut rec[at..], t, origin as f64);
            }
        }
        // The schema's statistics.
        for (k, s) in self.stats.iter_mut().enumerate() {
            let Some(s) = s else { continue };
            let d = &self.schema[k];
            let counts = self.json["schema"][k].get("counts").is_some();
            let is_origin = is_origin(&d.name) && self.origin.is_some();
            for i in 0..n {
                let raw = if is_origin { origin as f64 } else { cols[k].get_f64(i) };
                let v = match d.scale {
                    Some(sc) => raw * sc + d.offset.unwrap_or(0.0),
                    None => raw,
                };
                s.add(v, counts);
            }
        }
        // Inside the cube, the bounds, and spilled.
        let (c0, c1) = ([self.cube[0], self.cube[1], self.cube[2]], [self.cube[3], self.cube[4], self.cube[5]]);
        let tolerance = [0, 1, 2].map(|k| (c1[k] - c0[k]).abs() * 1e-9);
        let source = self.new_sources.last_mut().expect("a source");
        for (i, rec) in records.chunks_exact(rl).enumerate() {
            let p = xyz_of(&self.codec, &self.xyz, rec);
            if (0..3).any(|k| !(p[k] >= c0[k] - tolerance[k] && p[k] <= c1[k] + tolerance[k])) {
                return Err(Error::InvalidArgument(format!(
                    "'{}': the point ({}, {}, {}) is outside the EPT dataset's cube {:?}: EPT datasets keep their bounds, \
                     and points outside them cannot be appended (write the dataset with --ept-bounds covering all the points)",
                    source.path, p[0], p[1], p[2], self.cube
                )));
            }
            for (k, v) in p.into_iter().enumerate() {
                self.lo[k] = self.lo[k].min(v);
                self.hi[k] = self.hi[k].max(v);
                source.lo[k] = source.lo[k].min(v);
                source.hi[k] = source.hi[k].max(v);
            }
            self.spill.write_all(&(self.n + i as u64).to_le_bytes())?;
            self.spill.write_all(rec)?;
        }
        source.points += n as u64;
        self.n += n as u64;
        Ok(())
    }

    /// Places the points written in the octree and updates the dataset (see the module's
    /// documentation for the order of the writes).
    pub fn finish(mut self) -> Result<()> {
        if self.new_sources.is_empty() || self.n == 0 {
            return Ok(());
        }
        self.spill.flush()?;
        let spill = std::mem::replace(&mut self.spill, BufWriter::new(tempfile::NamedTempFile::new()?));
        let spill = spill.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        let stage = unique_dir(&self.dir)?;
        let result = (|| -> Result<()> {
            for sub in ["ept-data", "ept-hierarchy", "ept-sources"] {
                let d = stage.join(sub);
                std::fs::create_dir_all(&d).map_err(|e| io_err(&d, e))?;
            }
            let mut ctx = Ctx { stage: stage.clone(), changed: BTreeSet::new(), replaced: 0 };
            let root = Batch { n: self.n, mem: Vec::new(), file: Some(spill) };
            self.place(&mut ctx, VoxelKey::ROOT, root)?;
            self.write_hierarchy(&ctx)?;
            self.write_sources(&stage)?;
            self.write_ept_json(&stage)?;
            write_synced(&stage.join(COMMITTED), b"")
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
        move_in(&self.dir, &stage)
    }

    /// The position of a point in the cube, each coordinate in [0, 1).
    fn rel(&self, p: [f64; 3]) -> [f64; 3] {
        let c = &self.cube;
        [0, 1, 2].map(|k| ((p[k] - c[k]) / (c[k + 3] - c[k])).clamp(0.0, 1.0 - 1e-12))
    }

    /// Places points below a node (the node included).
    fn place(&mut self, ctx: &mut Ctx, key: VoxelKey, batch: Batch) -> Result<()> {
        let entry = self.rl + 8;
        let existing = self.hierarchy.get(&key).copied().unwrap_or(0);
        let has_children = children(key).any(|c| self.hierarchy.contains_key(&c));
        let mut loose: Vec<u8> = Vec::new();
        let mut fixed: Vec<u8> = Vec::new();
        if !has_children {
            if existing + batch.n <= self.leaf_points || key.level >= self.max_depth {
                // A leaf: all the points.
                let mut records = if existing > 0 { self.read_tile(key)? } else { Vec::new() };
                let new = batch.load(entry)?;
                drop(batch);
                self.append_sorted(&mut records, &new);
                return self.write_tile(ctx, key, &records);
            }
            // A leaf split: its points are placed again with the new ones.
            if existing > 0 {
                let records = self.read_tile(key)?;
                loose.reserve(records.len() / self.rl * entry);
                for rec in records.chunks_exact(self.rl) {
                    loose.extend_from_slice(&(REPLACED + ctx.replaced).to_le_bytes());
                    loose.extend_from_slice(rec);
                    ctx.replaced += 1;
                }
            }
        } else if existing > 0 {
            fixed = self.read_tile(key)?;
        }
        // The cells taken, by the points kept in the node and those added.
        let mut occupied: HashSet<u64> = HashSet::new();
        for rec in fixed.chunks_exact(self.rl) {
            occupied.insert(self.cell(self.rel(xyz_of(&self.codec, &self.xyz, rec)), key));
        }
        let mut kept: Vec<u8> = Vec::new();
        let mut bufs: Vec<ChildBuf> = (0..8).map(|_| ChildBuf::default()).collect();
        let limit = (self.memory_points / 8).max(1) as usize * entry;
        let mut route = |this: &Self, chunk: &[u8]| -> Result<()> {
            let m = chunk.len() / entry;
            let idx = |i: usize| u64::from_le_bytes(chunk[i * entry..i * entry + 8].try_into().unwrap_or_default());
            let mut order: Vec<usize> = (0..m).collect();
            order.sort_by_key(|&i| scramble(idx(i)));
            for i in order {
                let e = &chunk[i * entry..(i + 1) * entry];
                let r = this.rel(xyz_of(&this.codec, &this.xyz, &e[8..]));
                if occupied.insert(this.cell(r, key)) {
                    kept.extend_from_slice(e);
                } else {
                    let cells = (1u64 << (key.level + 1)) as f64;
                    let c = |k: usize| (((r[k] * cells) as i64) & 1) as usize;
                    bufs[c(0) | (c(1) << 1) | (c(2) << 2)].push(e, limit)?;
                }
            }
            Ok(())
        };
        if !loose.is_empty() {
            route(self, &loose)?;
        }
        drop(loose);
        batch.for_each_chunk(entry, self.memory_points, |c| route(self, c))?;
        drop(batch);
        drop(occupied);
        if !kept.is_empty() {
            self.append_sorted(&mut fixed, &kept);
            drop(kept);
            self.write_tile(ctx, key, &fixed)?;
        }
        drop(fixed);
        // The children, one at a time: those of many points wait in files.
        for b in bufs.iter_mut() {
            if b.n > (self.memory_points / 64).max(1) {
                b.spill()?;
            }
        }
        for (child, b) in children(key).zip(bufs) {
            if b.n > 0 {
                let batch = b.into_batch()?;
                self.place(ctx, child, batch)?;
            }
        }
        Ok(())
    }

    /// The cell of a node's grid a position is in.
    fn cell(&self, r: [f64; 3], key: VoxelKey) -> u64 {
        let cells = (1u64 << key.level) as f64;
        let s = self.span as f64;
        let at = [key.x, key.y, key.z];
        let c = |k: usize| ((r[k] * cells - at[k] as f64) * s).clamp(0.0, s - 1.0) as u64;
        c(0) + self.span * (c(1) + self.span * c(2))
    }

    /// Adds the records of entries to records, in GPS time order (which LASzip compresses best).
    fn append_sorted(&self, records: &mut Vec<u8>, entries: &[u8]) {
        let entry = self.rl + 8;
        let m = entries.len() / entry;
        let mut order: Vec<usize> = (0..m).collect();
        if let Some(g) = self.gps.filter(|_| matches!(self.codec, Codec::Las { .. })) {
            let t = |i: usize| f64::from_le_bytes(entries[i * entry + 8 + g..i * entry + 16 + g].try_into().unwrap_or_default());
            order.sort_by(|&a, &b| t(a).total_cmp(&t(b)).then(a.cmp(&b)));
        }
        records.reserve(m * self.rl);
        for i in order {
            records.extend_from_slice(&entries[i * entry + 8..(i + 1) * entry]);
        }
    }

    /// The records of a node's tile.
    fn read_tile(&self, key: VoxelKey) -> Result<Vec<u8>> {
        let path = tile_path(&self.dir, key, self.data_type);
        let bytes = std::fs::read(&path).map_err(|e| io_err(&path, e))?;
        let records = match (&self.codec, self.data_type) {
            (Codec::Las { header, layout }, _) => {
                let h = LasHeader::read(&mut Cursor::new(&bytes))?;
                if h.point_format != header.point_format || h.point_record_length as usize != layout.record_len {
                    return Err(Error::InvalidData(format!(
                        "EPT: tile {} has point format {} ({} bytes), the dataset's tiles {} ({} bytes)",
                        key_name(key),
                        h.point_format,
                        h.point_record_length,
                        header.point_format,
                        layout.record_len
                    )));
                }
                if h.scale != header.scale || h.offset != header.offset {
                    return Err(Error::InvalidData(format!("EPT: tile {} has a scale or offset of its own", key_name(key))));
                }
                let vlr = h
                    .find_vlr(super::las::LASZIP_USER_ID, super::las::LASZIP_RECORD_ID)
                    .ok_or_else(|| Error::InvalidData(format!("EPT: tile {} is not LASzip-compressed", key_name(key))))?;
                let vlr = super::read::laz_vlr(&vlr.data)?;
                let mut c = Cursor::new(&bytes[..]);
                c.seek(SeekFrom::Start(h.offset_to_point_data as u64))?;
                let mut out = vec![0u8; h.point_count as usize * layout.record_len];
                let mut d = laz::LasZipDecompressor::new(c, vlr).map_err(|e| Error::InvalidData(format!("LAZ: {e}")))?;
                d.decompress_many(&mut out).map_err(|e| Error::InvalidData(format!("EPT tile {}: {e}", key_name(key))))?;
                out
            }
            (_, EptDataType::Zstandard) => {
                zstd::decode_all(&bytes[..]).map_err(|e| Error::InvalidData(format!("EPT tile {}: {e}", key_name(key))))?
            }
            _ => bytes,
        };
        if records.len() % self.rl != 0 {
            return Err(Error::InvalidData(format!("EPT: tile {} is not a whole number of records", key_name(key))));
        }
        Ok(records)
    }

    /// Writes a node's tile in the staging directory.
    fn write_tile(&mut self, ctx: &mut Ctx, key: VoxelKey, records: &[u8]) -> Result<()> {
        let bytes = match &self.codec {
            Codec::Las { header, layout } => super::ept_write::las_bytes(header, layout, records, true)?,
            Codec::Schema if self.data_type == EptDataType::Zstandard => zstd::bulk::compress(records, 3)?,
            Codec::Schema => records.to_vec(),
        };
        write_synced(&tile_path(&ctx.stage, key, self.data_type), &bytes)?;
        self.hierarchy.insert(key, (records.len() / self.rl) as u64);
        ctx.changed.insert(key);
        Ok(())
    }

    /// The page a node is listed in (with its count): the deepest page whose root is the node or
    /// an ancestor of it.
    fn page_of(&self, mut k: VoxelKey) -> VoxelKey {
        loop {
            if self.pages.contains(&k) || k.level == 0 {
                return k;
            }
            k = parent(k);
        }
    }

    /// Writes the hierarchy pages with nodes changed or added.
    fn write_hierarchy(&self, ctx: &Ctx) -> Result<()> {
        let dirty: BTreeSet<VoxelKey> = ctx.changed.iter().map(|k| self.page_of(*k)).collect();
        let mut out: BTreeMap<VoxelKey, serde_json::Map<String, Value>> = dirty.iter().map(|p| (*p, serde_json::Map::new())).collect();
        for (k, n) in &self.hierarchy {
            let p = self.page_of(*k);
            if let Some(m) = out.get_mut(&p) {
                m.insert(key_name(*k), json!(n));
            }
            // A page's root is also listed in the page above, as -1.
            if p == *k && k.level > 0 {
                let above = self.page_of(parent(*k));
                if let Some(m) = out.get_mut(&above) {
                    m.insert(key_name(*k), json!(-1));
                }
            }
        }
        for (p, m) in out {
            write_json(&ctx.stage.join(format!("ept-hierarchy/{}.json", key_name(p))), &Value::Object(m))?;
        }
        Ok(())
    }

    /// Writes the source list with the new sources, and their metadata.
    fn write_sources(&self, stage: &Path) -> Result<()> {
        let mut list = self.sources.clone();
        for s in &self.new_sources {
            let b = if s.points > 0 { [s.lo[0], s.lo[1], s.lo[2], s.hi[0], s.hi[1], s.hi[2]] } else { [0.0; 6] };
            let mut name = format!("{}.json", list.len());
            let mut k = 0;
            while self.dir.join("ept-sources").join(&name).exists() {
                k += 1;
                name = format!("{}-{k}.json", list.len());
            }
            let meta = json!({ "path": s.path, "bounds": b, "points": s.points, "srs": s.srs });
            write_json(&stage.join("ept-sources").join(&name), &meta)?;
            list.push(json!({ "path": s.path, "bounds": b, "inserted": true, "points": s.points, "metadataPath": name }));
        }
        write_json(&stage.join("ept-sources").join(self.sources_file), &Value::Array(list))
    }

    /// Writes `ept.json` with the new count, bounds and statistics.
    fn write_ept_json(&self, stage: &Path) -> Result<()> {
        let mut json = self.json.clone();
        let before = json["points"].as_u64().unwrap_or(0);
        json["points"] = json!(before + self.n);
        let mut b = match bounds6(&json["boundsConforming"]) {
            Some(b) if before > 0 => b,
            _ => [f64::INFINITY, f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY],
        };
        for k in 0..3 {
            b[k] = b[k].min(self.lo[k]);
            b[k + 3] = b[k + 3].max(self.hi[k]);
        }
        json["boundsConforming"] = json!(b);
        if let Some(schema) = json["schema"].as_array_mut() {
            for (d, s) in schema.iter_mut().zip(&self.stats) {
                if let Some(s) = s {
                    merge_stats(d, s, before);
                }
            }
        }
        write_json(&stage.join("ept.json"), &json)
    }
}

/// Merges the statistics of the points appended into a schema dimension's.
fn merge_stats(d: &mut Value, s: &DimStats, before: u64) {
    if s.n == 0 {
        return;
    }
    let n0 = d["count"].as_u64().unwrap_or(before) as f64;
    let n1 = s.n as f64;
    let total = n0 + n1;
    let mean0 = d["mean"].as_f64().unwrap_or(s.mean);
    let var0 = d["variance"].as_f64().or_else(|| d["stddev"].as_f64().map(|v| v * v)).unwrap_or(0.0);
    let delta = s.mean - mean0;
    let mean = mean0 + delta * n1 / total;
    let m2 = var0 * n0 + s.m2 + delta * delta * n0 * n1 / total;
    let o = d.as_object_mut().expect("a schema dimension");
    let set = |o: &mut serde_json::Map<String, Value>, k: &str, v: Value| {
        if o.contains_key(k) {
            o.insert(k.to_string(), v);
        }
    };
    set(o, "count", json!(total as u64));
    let min = o.get("minimum").and_then(Value::as_f64).map_or(s.min, |m| m.min(s.min));
    let max = o.get("maximum").and_then(Value::as_f64).map_or(s.max, |m| m.max(s.max));
    set(o, "minimum", json!(min));
    set(o, "maximum", json!(max));
    set(o, "mean", json!(mean));
    set(o, "variance", json!(m2 / total));
    set(o, "stddev", json!((m2 / total).sqrt()));
    if let Some(counts) = o.get_mut("counts").and_then(Value::as_array_mut) {
        let mut merged: BTreeMap<i64, u64> = BTreeMap::new();
        for c in counts.iter() {
            if let (Some(v), Some(n)) = (c["value"].as_f64(), c["count"].as_u64()) {
                *merged.entry(v as i64).or_default() += n;
            }
        }
        for (v, n) in &s.counts {
            *merged.entry(*v).or_default() += n;
        }
        *counts = merged.into_iter().map(|(v, n)| json!({ "value": v, "count": n })).collect();
    }
}

/// What an update writes as it goes.
struct Ctx {
    stage: PathBuf,
    /// The nodes whose tiles were written.
    changed: BTreeSet<VoxelKey>,
    /// The points of split leaves placed again so far.
    replaced: u64,
}

fn is_origin(name: &str) -> bool {
    name == "OriginId" || name == "origin_id"
}

fn tile_path(dir: &Path, key: VoxelKey, t: EptDataType) -> PathBuf {
    dir.join(format!("ept-data/{}.{}", key_name(key), t.extension()))
}

/// The coordinates of a record.
fn xyz_of(codec: &Codec, xyz: &[(usize, ValueType, f64, f64); 3], rec: &[u8]) -> [f64; 3] {
    match codec {
        Codec::Las { header, .. } => {
            let p = super::las::record_core(rec, header.point_format, &header.scale, &header.offset);
            [p.x, p.y, p.z]
        }
        Codec::Schema => xyz.map(|(at, t, s, o)| read_num(&rec[at..], t) * s + o),
    }
}

/// A directory of the dataset's, named for an update, no other one uses.
fn unique_dir(dir: &Path) -> Result<PathBuf> {
    for k in 0u32.. {
        let d = dir.join(format!("{STAGING}-{k}"));
        match std::fs::create_dir(&d) {
            Ok(()) => return Ok(d),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err(&d, e)),
        }
    }
    Err(Error::Io(std::io::Error::other(format!("'{}': no free directory name", dir.display()))))
}

/// The node counts of every page of a dataset's hierarchy, and the pages' roots.
fn load_hierarchy(dir: &Path) -> Result<(BTreeMap<VoxelKey, u64>, BTreeSet<VoxelKey>)> {
    let mut counts = BTreeMap::new();
    let mut pages = BTreeSet::new();
    let mut stack = vec![VoxelKey::ROOT];
    pages.insert(VoxelKey::ROOT);
    while let Some(p) = stack.pop() {
        let path = dir.join(format!("ept-hierarchy/{}.json", key_name(p)));
        let page: BTreeMap<String, i64> = serde_json::from_slice(&std::fs::read(&path).map_err(|e| io_err(&path, e))?)
            .map_err(|e| Error::InvalidData(format!("'{}': {e}", path.display())))?;
        for (name, n) in page {
            let k = parse_key(&name)?;
            match n {
                -1 if k != p => {
                    if pages.insert(k) {
                        stack.push(k);
                    }
                }
                n if n >= 0 => {
                    counts.insert(k, n as u64);
                }
                _ => return Err(Error::InvalidData(format!("EPT hierarchy: invalid point count {n} for node {name}"))),
            }
        }
    }
    Ok((counts, pages))
}

/// Appends the points of point clouds (LAS, LAZ, COPC, EPT, PLY, E57: paths or URLs) to an
/// existing EPT dataset, in place, read in chunks with `options.read`'s filters and reprojected
/// into `options.target_srs` when given (see [`EptAppender`]). Each input is a source of its
/// own. Of `options.write`, `copc_leaf_points`, `copc_max_depth` and `copc_memory_points` apply.
///
/// Appending another EPT dataset merges it into this one (its points placed in this one's
/// octree): datasets built in parts with the same `ept_bounds` merge this way.
pub fn append_points(dataset: &str, inputs: &[&str], options: &super::ConvertOptions) -> Result<()> {
    let mut appender = EptAppender::open(dataset, &options.write)?;
    for input in inputs {
        let mut reader = super::open_points(input)?;
        let (template, transformer) = super::source_template(&mut reader, options)?;
        appender.start_source(input, &template)?;
        reader.start_chunks(&options.read, options.chunk_points)?;
        while let Some(mut chunk) = reader.next_chunk()? {
            if let Some(t) = &transformer {
                chunk.transform_coords(t)?;
            }
            appender.write(&chunk)?;
        }
    }
    appender.finish()
}
