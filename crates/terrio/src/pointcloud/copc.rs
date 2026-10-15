//! COPC (Cloud Optimized Point Cloud, <https://copc.io>): a LAZ 1.4 file whose points are
//! organized in an octree, each node a LASzip chunk, with the hierarchy of nodes in pages that
//! can be read on demand.

use super::PointReadOptions;
use super::las::{Columns, LasHeader, Layout, record_core};
use super::read::Source;
use crate::{Error, Result};
use laz::record::{RecordCompressor, RecordDecompressor};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Seek, SeekFrom, Write};

/// The user id of the COPC records.
pub(crate) const COPC_USER_ID: &str = "copc";
/// The size of a hierarchy entry.
const ENTRY_LEN: usize = 32;
/// The cells per side of the grid that samples a node's points when writing.
const GRID: u32 = 128;
/// The most points of a leaf node when writing.
const LEAF_POINTS: u64 = 100_000;
/// The deepest octree level when writing.
const MAX_DEPTH: u32 = 16;
/// The most bytes of node data fetched before decoding them, for remote files.
const BATCH_BYTES: u64 = 16 << 20;

/// The COPC information record.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CopcInfo {
    /// The center of the octree's root cube.
    pub center: [f64; 3],
    /// Half the side of the root cube.
    pub halfsize: f64,
    /// The spacing between points at the root level; it halves at each level.
    pub spacing: f64,
    /// The offset of the root hierarchy page.
    pub root_hier_offset: u64,
    /// The size of the root hierarchy page.
    pub root_hier_size: u64,
    /// The smallest GPS time.
    pub gpstime_minimum: f64,
    /// The largest GPS time.
    pub gpstime_maximum: f64,
}

impl CopcInfo {
    fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 160 {
            return Err(Error::InvalidData("the COPC info record is shorter than 160 bytes".into()));
        }
        let f = |o: usize| f64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        let u = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        Ok(Self {
            center: [f(0), f(8), f(16)],
            halfsize: f(24),
            spacing: f(32),
            root_hier_offset: u(40),
            root_hier_size: u(48),
            gpstime_minimum: f(56),
            gpstime_maximum: f(64),
        })
    }

    fn to_bytes(self) -> Vec<u8> {
        let mut b = Vec::with_capacity(160);
        for v in [self.center[0], self.center[1], self.center[2], self.halfsize, self.spacing] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.root_hier_offset.to_le_bytes());
        b.extend_from_slice(&self.root_hier_size.to_le_bytes());
        b.extend_from_slice(&self.gpstime_minimum.to_le_bytes());
        b.extend_from_slice(&self.gpstime_maximum.to_le_bytes());
        b.resize(160, 0);
        b
    }

    /// The bounds of a node: `[min_x, min_y, min_z, max_x, max_y, max_z]`.
    pub fn node_bounds(&self, key: VoxelKey) -> [f64; 6] {
        let side = 2.0 * self.halfsize / f64::from(1u32 << key.level.clamp(0, 30));
        let min = [
            self.center[0] - self.halfsize + key.x as f64 * side,
            self.center[1] - self.halfsize + key.y as f64 * side,
            self.center[2] - self.halfsize + key.z as f64 * side,
        ];
        [min[0], min[1], min[2], min[0] + side, min[1] + side, min[2] + side]
    }

    /// The point spacing at an octree level.
    pub fn level_spacing(&self, level: i32) -> f64 {
        self.spacing / f64::from(1u32 << level.clamp(0, 30))
    }
}

/// The key of an octree node: its level (0 is the root) and position at that level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct VoxelKey {
    /// The level.
    pub level: i32,
    /// The column.
    pub x: i32,
    /// The row.
    pub y: i32,
    /// The layer.
    pub z: i32,
}

impl VoxelKey {
    /// The root node.
    pub const ROOT: VoxelKey = VoxelKey { level: 0, x: 0, y: 0, z: 0 };

    /// The eight children.
    pub fn children(self) -> impl Iterator<Item = VoxelKey> {
        (0..8).map(move |i| VoxelKey {
            level: self.level + 1,
            x: 2 * self.x + (i & 1),
            y: 2 * self.y + ((i >> 1) & 1),
            z: 2 * self.z + (i >> 2),
        })
    }
}

/// A node of the octree.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CopcNode {
    /// The key.
    pub key: VoxelKey,
    /// The offset of its LASzip chunk.
    pub offset: u64,
    /// The size of its LASzip chunk.
    pub byte_size: i32,
    /// The number of points.
    pub point_count: i32,
}

impl CopcNode {
    fn parse(b: &[u8]) -> Self {
        let i = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap_or_default());
        Self {
            key: VoxelKey { level: i(0), x: i(4), y: i(8), z: i(12) },
            offset: u64::from_le_bytes(b[16..24].try_into().unwrap_or_default()),
            byte_size: i(24),
            point_count: i(28),
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [self.key.level, self.key.x, self.key.y, self.key.z] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.byte_size.to_le_bytes());
        out.extend_from_slice(&self.point_count.to_le_bytes());
    }

    /// Whether the entry points at a child hierarchy page rather than at points.
    fn is_page(&self) -> bool {
        self.point_count == -1
    }
}

/// An open COPC file: its information record and the hierarchy pages loaded so far.
pub(crate) struct Copc {
    pub info: CopcInfo,
    items: Vec<laz::LazItem>,
    nodes: HashMap<VoxelKey, CopcNode>,
    loaded_pages: HashSet<u64>,
    /// Where the EVLRs, which hold the hierarchy pages, start.
    evlr_start: u64,
}

impl Copc {
    /// Whether a header is a COPC file's: a COPC info record first among the VLRs.
    pub fn is_copc(h: &LasHeader) -> bool {
        h.vlrs.first().is_some_and(|v| v.is(COPC_USER_ID, 1))
    }

    /// Reads the information record and the root hierarchy page.
    pub fn open(source: &mut Source, h: &LasHeader) -> Result<Self> {
        let info = CopcInfo::parse(&h.vlrs[0].data)?;
        let laz = h
            .find_vlr(super::las::LASZIP_USER_ID, super::las::LASZIP_RECORD_ID)
            .ok_or_else(|| Error::InvalidData("a COPC file without a LASzip record".into()))?;
        let items = super::read::laz_vlr(&laz.data)?.items().clone();
        let mut c = Self { info, items, nodes: HashMap::new(), loaded_pages: HashSet::new(), evlr_start: h.start_of_first_evlr };
        c.load_page(source, info.root_hier_offset, info.root_hier_size)?;
        Ok(c)
    }

    fn load_page(&mut self, source: &mut Source, offset: u64, size: u64) -> Result<()> {
        if self.loaded_pages.contains(&offset) {
            return Ok(());
        }
        if size > 1 << 30 {
            return Err(Error::InvalidData(format!("a COPC hierarchy page of {size} bytes")));
        }
        // Marked loaded once read: a read that fails (bytes not fetched yet) is run again.
        let b = source.read_at(offset, size as usize)?;
        self.loaded_pages.insert(offset);
        for e in b.as_chunks::<ENTRY_LEN>().0.iter().map(|c| CopcNode::parse(c)) {
            // A page reference never replaces the node's own entry.
            if e.is_page() && self.nodes.get(&e.key).is_some_and(|n| !n.is_page()) {
                continue;
            }
            self.nodes.insert(e.key, e);
        }
        Ok(())
    }

    /// The entry of a node, loading the hierarchy page it is in when needed.
    fn node(&mut self, source: &mut Source, key: VoxelKey) -> Result<Option<CopcNode>> {
        let mut n = self.nodes.get(&key).copied();
        if let Some(e) = n
            && e.is_page()
        {
            self.load_page(source, e.offset, e.byte_size.max(0) as u64)?;
            n = self.nodes.get(&key).copied().filter(|e| !e.is_page());
        }
        Ok(n)
    }

    /// Walks the octree from the root, through the nodes that intersect `bounds`, down to
    /// `max_level`, loading the hierarchy pages on the way. Returns the nodes that hold points.
    fn walk(&mut self, source: &mut Source, bounds: Option<[f64; 6]>, max_level: i32) -> Result<Vec<CopcNode>> {
        let mut out = Vec::new();
        let mut stack = vec![VoxelKey::ROOT];
        while let Some(key) = stack.pop() {
            if key.level > max_level {
                continue;
            }
            let Some(node) = self.node(source, key)? else { continue };
            if let Some(q) = bounds {
                let b = self.info.node_bounds(key);
                if b[0] > q[3] || b[3] < q[0] || b[1] > q[4] || b[4] < q[1] || b[2] > q[5] || b[5] < q[2] {
                    continue;
                }
            }
            if node.point_count > 0 {
                out.push(node);
            }
            stack.extend(key.children());
        }
        Ok(out)
    }

    /// Every node, sorted by key.
    pub fn all_nodes(&mut self, source: &mut Source) -> Result<Vec<CopcNode>> {
        // Remote files: fetch the hierarchy pages (in the EVLRs, at the end) in one request.
        if let Some(len) = source.remote_len()
            && self.evlr_start > 0
            && len > self.evlr_start
            && len - self.evlr_start <= BATCH_BYTES
        {
            source.prefetch(&[(self.evlr_start, len - self.evlr_start)])?;
        }
        let mut nodes = self.walk(source, None, i32::MAX)?;
        nodes.sort_by_key(|n| n.key);
        Ok(nodes)
    }

    /// The nodes the options select (by bounds, level, resolution and point budget), in file
    /// order.
    pub fn select(&mut self, source: &mut Source, options: &PointReadOptions) -> Result<Vec<CopcNode>> {
        let f = &options.filter;
        let bounds = match (f.bounds, f.z_range) {
            (None, None) => None,
            (b, z) => {
                let [x0, y0, x1, y1] = b.unwrap_or([f64::NEG_INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::INFINITY]);
                let (z0, z1) = z.unwrap_or((f64::NEG_INFINITY, f64::INFINITY));
                Some([x0, y0, z0, x1, y1, z1])
            }
        };
        let mut max_level = options.max_level.map_or(i32::MAX, |l| l.min(i32::MAX as u32) as i32);
        if let Some(r) = options.resolution {
            if !(r > 0.0 && r.is_finite()) {
                return Err(Error::InvalidArgument(format!("invalid resolution {r}")));
            }
            let needed = (self.info.spacing / r).log2().ceil().max(0.0) as i32;
            max_level = max_level.min(needed);
        }
        let mut nodes = self.walk(source, bounds, max_level)?;
        if let Some(limit) = options.max_points {
            let deepest = deepest_level_within(nodes.iter().map(|n| (n.key.level as usize, n.point_count.max(0) as u64)), limit);
            nodes.retain(|n| n.key.level <= deepest);
        }
        nodes.sort_by_key(|n| n.offset);
        Ok(nodes)
    }

    /// Reads the points of the nodes the options select, in file order.
    pub fn read(
        &mut self,
        source: &mut Source,
        h: &LasHeader,
        layout: &Layout,
        options: &PointReadOptions,
    ) -> Result<Vec<super::Dimension>> {
        let nodes = self.select(source, options)?;
        let total: usize = nodes.iter().map(|n| n.point_count.max(0) as usize).sum();
        let capacity = if options.filter.is_empty() { total } else { total.min(1 << 20) };
        let mut columns = Columns::new(layout, capacity);
        self.decode(source, h, layout, &nodes, &options.filter, &mut columns)?;
        Ok(columns.into_dimensions())
    }

    /// Decodes the points of nodes (in file order) that pass a filter into columns.
    pub fn decode(
        &self,
        source: &mut Source,
        h: &LasHeader,
        layout: &Layout,
        nodes: &[CopcNode],
        f: &super::PointFilter,
        columns: &mut Columns,
    ) -> Result<()> {
        let rl = layout.record_len;
        // Bytes the caller fetches: ask for them all at once, the read being run again until
        // they are there.
        if source.is_sparse() {
            let ranges: Vec<(u64, u64)> = nodes.iter().map(|n| (n.offset, n.byte_size.max(0) as u64)).collect();
            source.prefetch(&ranges)?;
        }
        let mut i = 0;
        while i < nodes.len() {
            // Fetch a batch of nodes in as few requests as possible, then decode them.
            let mut j = i;
            let mut bytes = 0u64;
            while j < nodes.len() && (j == i || bytes + nodes[j].byte_size as u64 <= BATCH_BYTES) {
                bytes += nodes[j].byte_size.max(0) as u64;
                j += 1;
            }
            if source.is_remote() {
                let ranges: Vec<(u64, u64)> = nodes[i..j].iter().map(|n| (n.offset, n.byte_size.max(0) as u64)).collect();
                source.prefetch(&ranges)?;
            }
            for n in &nodes[i..j] {
                let data = source.read_at(n.offset, n.byte_size.max(0) as usize)?;
                let records = self.decompress(data, n.point_count.max(0) as usize, rl)?;
                for rec in records.chunks_exact(rl) {
                    if !f.is_empty() && !f.keeps(&record_core(rec, h.point_format, &h.scale, &h.offset)) {
                        continue;
                    }
                    columns.push(rec, &h.scale, &h.offset);
                }
            }
            i = j;
        }
        Ok(())
    }

    fn decompress(&self, data: Vec<u8>, count: usize, record_len: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; count * record_len];
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let mut d = laz::record::LayeredPointRecordDecompressor::new(Cursor::new(data));
            d.set_fields_from(&self.items).map_err(|e| Error::InvalidData(format!("COPC: {e}")))?;
            d.decompress_many(&mut out).map_err(|e| Error::InvalidData(format!("COPC node: {e}")))?;
            Ok(())
        }));
        match r {
            Ok(r) => r.map(|_| out),
            Err(_) => Err(Error::InvalidData("a COPC node could not be decompressed".into())),
        }
    }
}

/// How to build a COPC octree.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CopcBuild {
    /// The deepest level; 16 by default.
    pub max_depth: Option<u32>,
    /// The most points of a leaf node; 100 000 by default.
    pub leaf_points: Option<u64>,
}

/// The most points a [`CopcStreamWriter`] holds in memory at a time, by default: more are built
/// out of core, one octree node of a coarse level at a time.
#[cfg(not(target_arch = "wasm32"))]
const MEMORY_POINTS: u64 = 4_000_000;

/// The octree's cube and parameters.
#[derive(Debug, Clone, Copy)]
struct Cube {
    center: [f64; 3],
    half: f64,
    max_depth: i32,
    leaf_points: u64,
}

impl Cube {
    /// The cube of points within bounds: the bounds' center and their largest half extent.
    fn new(lo: [f64; 3], hi: [f64; 3], scale: f64, build: CopcBuild) -> Self {
        let center = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
        let half = ((hi[0] - lo[0]).max(hi[1] - lo[1]).max(hi[2] - lo[2]) / 2.0).max(scale) * (1.0 + 1e-9);
        Self {
            center,
            half,
            max_depth: build.max_depth.unwrap_or(MAX_DEPTH).min(MAX_DEPTH) as i32,
            leaf_points: build.leaf_points.unwrap_or(LEAF_POINTS).max(1),
        }
    }

    /// A point's position in the cube, each coordinate in [0, 1).
    fn rel(&self, p: &super::las::RecordCore) -> [f64; 3] {
        let (c, h) = (self.center, self.half);
        [
            ((p.x - (c[0] - h)) / (2.0 * h)).clamp(0.0, 1.0 - 1e-12),
            ((p.y - (c[1] - h)) / (2.0 * h)).clamp(0.0, 1.0 - 1e-12),
            ((p.z - (c[2] - h)) / (2.0 * h)).clamp(0.0, 1.0 - 1e-12),
        ]
    }

    /// The node a position is in at a level.
    fn key(r: &[f64; 3], level: i32) -> VoxelKey {
        let cells = (1u64 << level) as f64;
        VoxelKey { level, x: (r[0] * cells) as i32, y: (r[1] * cells) as i32, z: (r[2] * cells) as i32 }
    }

    /// The cell of a node's sampling grid a position is in.
    fn cell(r: &[f64; 3], key: VoxelKey) -> u32 {
        let cells = (1u64 << key.level) as f64;
        let g = GRID as f64;
        let c = |k: usize, at: i32| (((r[k] * cells) - at as f64) * g).clamp(0.0, g - 1.0) as u32;
        c(0, key.x) | (c(1, key.y) << 7) | (c(2, key.z) << 14)
    }
}

fn parent(k: VoxelKey) -> VoxelKey {
    VoxelKey { level: k.level - 1, x: k.x >> 1, y: k.y >> 1, z: k.z >> 1 }
}

/// What the levels above the buckets share across buckets: the number of points in each node
/// (counted for the nodes whose parent is not a leaf, as within a bucket), the sampling grid
/// cells taken, and the points kept.
#[derive(Default)]
struct Upper {
    levels: i32,
    counts: HashMap<VoxelKey, u64>,
    occupied: HashMap<VoxelKey, HashSet<u32>>,
    /// The points of each node: `(index, record)`.
    members: HashMap<VoxelKey, Vec<(u64, Vec<u8>)>>,
}

/// Point records with the index of each in the whole cloud (which sets their pseudo-random
/// sampling order).
struct Bucket<'a> {
    records: &'a [u8],
    /// The index of each record; `None` when they are `0..n`.
    index: Option<&'a [u64]>,
}

/// Writes node chunks and keeps their hierarchy entries.
struct ChunkSink<'w, W: Write + Seek> {
    w: &'w mut W,
    items: Vec<laz::LazItem>,
    table: laz::laszip::ChunkTable,
    entries: Vec<CopcNode>,
}

impl<W: Write + Seek> ChunkSink<'_, W> {
    fn write_node(&mut self, key: VoxelKey, records: &[u8], rl: usize) -> Result<()> {
        let mut c = laz::record::LayeredPointRecordCompressor::new(Cursor::new(Vec::new()));
        c.set_fields_from(&self.items).map_err(|e| Error::InvalidArgument(format!("LAZ: {e}")))?;
        c.compress_many(records)?;
        c.done()?;
        let chunk = c.into_inner().into_inner();
        let offset = self.w.stream_position()?;
        self.w.write_all(&chunk)?;
        let count = (records.len() / rl) as u64;
        self.table.push(laz::laszip::ChunkTableEntry { point_count: count, byte_count: chunk.len() as u64 });
        self.entries.push(CopcNode { key, offset, byte_size: chunk.len() as i32, point_count: count as i32 });
        Ok(())
    }
}

/// The GPS time of a record, or 0 for formats without.
fn gps_time(rec: &[u8], gps: Option<usize>) -> f64 {
    gps.map_or(0.0, |o| f64::from_le_bytes(rec[o..o + 8].try_into().unwrap_or_default()))
}

/// Builds the nodes of one bucket: all the points, or (out of core) those of one node of level
/// `upper.levels`. A leaf (a node of at most `leaf_points` points, or at the deepest level) takes
/// all the points that reach it; another node keeps the first point to land in each cell of its
/// grid, in a pseudo-random order so that each level is an even sample, and passes the others
/// down. The bucket's nodes are written; the points of the levels above go to `upper`.
#[allow(clippy::too_many_arguments)]
fn build_bucket<W: Write + Seek>(
    bucket: &Bucket<'_>,
    rl: usize,
    h: &LasHeader,
    gps: Option<usize>,
    cube: &Cube,
    upper: &mut Upper,
    sink: &mut ChunkSink<'_, W>,
) -> Result<()> {
    let n = bucket.records.len() / rl;
    let rec = |i: usize| &bucket.records[i * rl..(i + 1) * rl];
    let index = |i: usize| bucket.index.map_or(i as u64, |x| x[i]);
    let rel: Vec<[f64; 3]> = (0..n).map(|i| cube.rel(&record_core(rec(i), h.point_format, &h.scale, &h.offset))).collect();
    let l0 = upper.levels;
    // The points in each node's cube, level by level, down to the nodes small enough to be
    // leaves.
    let mut counts: HashMap<VoxelKey, u64> = HashMap::new();
    let count_of = |counts: &HashMap<VoxelKey, u64>, upper: &Upper, k: VoxelKey| -> Option<u64> {
        if k.level < l0 { upper.counts.get(&k).copied() } else { counts.get(&k).copied() }
    };
    for level in l0..=cube.max_depth {
        let mut any = false;
        for r in &rel {
            let key = Cube::key(r, level);
            if level == 0 || count_of(&counts, upper, parent(key)).is_some_and(|c| c > cube.leaf_points) {
                *counts.entry(key).or_default() += 1;
                any = true;
            }
        }
        if !any {
            break;
        }
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| scramble(index(i)));
    let mut occupied: HashMap<VoxelKey, HashSet<u32>> = HashMap::new();
    let mut members: HashMap<VoxelKey, Vec<usize>> = HashMap::new();
    for &i in &order {
        let r = &rel[i];
        for level in 0..=cube.max_depth {
            let key = Cube::key(r, level);
            let leaf = level == cube.max_depth || count_of(&counts, upper, key).is_none_or(|c| c <= cube.leaf_points);
            if !leaf {
                let taken = if level < l0 { &mut upper.occupied } else { &mut occupied };
                if !taken.entry(key).or_default().insert(Cube::cell(r, key)) {
                    continue;
                }
            }
            if level < l0 {
                upper.members.entry(key).or_default().push((index(i), rec(i).to_vec()));
            } else {
                members.entry(key).or_default().push(i);
            }
            break;
        }
    }
    drop(occupied);
    // Within a node, the points in acquisition (GPS time) order, which LASzip's predictors
    // compress best; without GPS times, in the input's order.
    let mut keys: Vec<VoxelKey> = members.keys().copied().collect();
    keys.sort();
    let mut buf = Vec::new();
    for key in keys {
        let m = members.get_mut(&key).expect("a node");
        m.sort_unstable_by(|&a, &b| gps_time(rec(a), gps).total_cmp(&gps_time(rec(b), gps)).then(index(a).cmp(&index(b))));
        buf.clear();
        for &i in m.iter() {
            buf.extend_from_slice(rec(i));
        }
        sink.write_node(key, &buf, rl)?;
    }
    Ok(())
}

/// Counts, bounds and GPS time range of the records written, to fill in the header.
#[derive(Debug, Clone)]
struct Stats {
    n: u64,
    lo: [f64; 3],
    hi: [f64; 3],
    tmin: f64,
    tmax: f64,
    by_return: Vec<u64>,
}

impl Stats {
    fn new() -> Self {
        Self {
            n: 0,
            lo: [f64::INFINITY; 3],
            hi: [f64::NEG_INFINITY; 3],
            tmin: f64::INFINITY,
            tmax: f64::NEG_INFINITY,
            by_return: vec![0; 15],
        }
    }

    fn add(&mut self, records: &[u8], rl: usize, h: &LasHeader, gps: Option<usize>) {
        for rec in records.chunks_exact(rl) {
            let p = record_core(rec, h.point_format, &h.scale, &h.offset);
            for (k, v) in [p.x, p.y, p.z].into_iter().enumerate() {
                self.lo[k] = self.lo[k].min(v);
                self.hi[k] = self.hi[k].max(v);
            }
            if gps.is_some() {
                let t = gps_time(rec, gps);
                self.tmin = self.tmin.min(t);
                self.tmax = self.tmax.max(t);
            }
            let r = p.return_number as usize;
            if (1..=15).contains(&r) {
                self.by_return[r - 1] += 1;
            }
            self.n += 1;
        }
    }

    fn finish(&mut self) {
        if self.n == 0 {
            self.lo = [0.0; 3];
            self.hi = [0.0; 3];
        }
        if !self.tmin.is_finite() {
            self.tmin = 0.0;
            self.tmax = 0.0;
        }
    }
}

/// Writes a COPC file whose nodes `build` writes: the header and VLRs (the COPC info record
/// first, then the LASzip record), the node chunks, the chunk table, the hierarchy, then the
/// header again with the counts, bounds and offsets.
fn write_copc_with<W: Write + Seek>(
    w: &mut W,
    mut header: LasHeader,
    layout: &Layout,
    stats: &Stats,
    cube: &Cube,
    build: impl FnOnce(&mut ChunkSink<'_, W>) -> Result<()>,
) -> Result<()> {
    let rl = layout.record_len;
    let format = layout.format;
    let items = laz::LazItemRecordBuilder::default_for_point_format_id(format, (rl - super::las::standard_record_length(format)) as u16)
        .map_err(|e| Error::InvalidArgument(format!("LAZ: {e}")))?;
    let vlr = laz::LazVlrBuilder::new(items.clone()).with_variable_chunk_size().build();
    let mut laz_data = Vec::new();
    vlr.write_to(&mut laz_data)?;
    let spacing = 2.0 * cube.half / GRID as f64;
    let mut info = CopcInfo {
        center: cube.center,
        halfsize: cube.half,
        spacing,
        root_hier_offset: 0,
        root_hier_size: 0,
        gpstime_minimum: stats.tmin,
        gpstime_maximum: stats.tmax,
    };
    let mut vlrs = vec![
        super::las::Vlr::new(COPC_USER_ID, 1, "COPC info VLR", info.to_bytes()),
        super::las::Vlr::new(super::las::LASZIP_USER_ID, super::las::LASZIP_RECORD_ID, "https://laszip.org", laz_data),
    ];
    vlrs.append(&mut header.vlrs);
    header.vlrs = vlrs;
    header.version = (1, 4);
    header.compressed = true;
    header.point_format = format;
    header.point_record_length = rl as u16;
    header.point_count = stats.n;
    header.min = stats.lo;
    header.max = stats.hi;
    header.points_by_return = stats.by_return.clone();
    header.start_of_waveform_data = 0;
    header.waveform_data_size = 0;
    header.offset_to_point_data = header.data_offset() as u32;
    let start = w.stream_position()?;
    header.write(w)?;

    // The points: one LASzip chunk per node, then the chunk table.
    let data_start = w.stream_position()?;
    w.write_all(&(-1i64).to_le_bytes())?;
    let mut sink = ChunkSink { w: &mut *w, items, table: laz::laszip::ChunkTable::default(), entries: Vec::new() };
    build(&mut sink)?;
    let ChunkSink { table, mut entries, .. } = sink;
    let table_pos = w.stream_position()?;
    table.write_to(&mut *w, &vlr)?;
    let end_of_points = w.stream_position()?;
    w.seek(SeekFrom::Start(data_start))?;
    w.write_all(&(table_pos as i64).to_le_bytes())?;
    w.seek(SeekFrom::Start(end_of_points))?;

    // The hierarchy: one page, in an EVLR, then the other EVLRs. Every ancestor of a node with
    // points needs an entry.
    let mut all: HashSet<VoxelKey> = entries.iter().map(|e| e.key).collect();
    let with_points: HashSet<VoxelKey> = all.clone();
    for k in with_points.iter() {
        let mut k = *k;
        while k.level > 0 {
            k = parent(k);
            all.insert(k);
        }
    }
    all.insert(VoxelKey::ROOT);
    for key in all.into_iter().filter(|k| !with_points.contains(k)) {
        entries.push(CopcNode { key, offset: 0, byte_size: 0, point_count: 0 });
    }
    entries.sort_by_key(|e| e.key);
    let mut page = Vec::with_capacity(entries.len() * ENTRY_LEN);
    for e in &entries {
        e.write(&mut page);
    }
    info.root_hier_offset = end_of_points + super::las::EVLR_HEADER_LEN as u64;
    info.root_hier_size = page.len() as u64;
    let mut evlrs = vec![super::las::Vlr::new(COPC_USER_ID, 1000, "EPT hierarchy", page)];
    evlrs.extend(header.evlrs.drain(..).filter(|v| !v.is_waveform_data()));
    header.evlrs = evlrs;
    header.start_of_first_evlr = end_of_points;
    header.write_evlrs(w)?;
    let end = w.stream_position()?;
    // Rewrite the header and the info record now that the offsets are known.
    header.vlrs[0].data = info.to_bytes();
    w.seek(SeekFrom::Start(start))?;
    header.write(w)?;
    w.seek(SeekFrom::Start(end))?;
    Ok(())
}

/// Where the GPS time of a format's records is.
fn gps_offset(format: u8) -> Option<usize> {
    super::las::standard_fields(format).iter().find(|f| f.name == "gps_time").and_then(|f| match f.field {
        super::las::Field::Plain(o) => Some(o),
        _ => None,
    })
}

/// Writes records (all of `layout`'s format, 6 to 8) as a COPC file. `header` has the VLRs to
/// write besides the COPC and LASzip ones, the scale, offset and version; its counts and bounds
/// are computed here.
pub(crate) fn write_copc<W: Write + Seek>(w: &mut W, header: LasHeader, layout: &Layout, records: &[u8], build: CopcBuild) -> Result<()> {
    let rl = layout.record_len;
    let gps = gps_offset(layout.format);
    let mut stats = Stats::new();
    stats.add(records, rl, &header, gps);
    stats.finish();
    let cube = Cube::new(stats.lo, stats.hi, header.scale[0], build);
    let h = header.clone();
    write_copc_with(w, header, layout, &stats, &cube, |sink| {
        build_bucket(&Bucket { records, index: None }, rl, &h, gps, &cube, &mut Upper::default(), sink)
    })
}

/// A function called with each spilled point: its index, record and position in the cube.
#[cfg(not(target_arch = "wasm32"))]
type EachPoint<'a> = dyn FnMut(u64, &[u8], [f64; 3]) -> Result<()> + 'a;

/// Writes a COPC file of points that come in chunks, larger than memory: the records are spilled
/// to a temporary file, then the octree is built one node of a coarse level (a bucket) at a time,
/// giving the same nodes as building it in memory.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct CopcStreamWriter {
    out: std::io::BufWriter<std::fs::File>,
    header: LasHeader,
    layout: Layout,
    build: CopcBuild,
    gps: Option<usize>,
    stats: Stats,
    /// `(index, record)` of every point, in order.
    spill: std::io::BufWriter<tempfile::NamedTempFile>,
    /// The most points held in memory at a time.
    memory_points: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl CopcStreamWriter {
    pub fn new(
        out: std::io::BufWriter<std::fs::File>,
        header: LasHeader,
        layout: Layout,
        build: CopcBuild,
        memory_points: Option<u64>,
    ) -> Result<Self> {
        let spill = std::io::BufWriter::with_capacity(1 << 20, tempfile::NamedTempFile::new()?);
        let gps = gps_offset(layout.format);
        let memory_points = memory_points.unwrap_or(MEMORY_POINTS).max(1);
        Ok(Self { out, header, layout, build, gps, stats: Stats::new(), spill, memory_points })
    }

    pub fn write_cloud(&mut self, cloud: &super::PointCloud) -> Result<()> {
        let records = super::write::encode(cloud, &self.layout, self.header.scale, self.header.offset)?;
        let rl = self.layout.record_len;
        self.stats.add(&records, rl, &self.header, self.gps);
        let first = self.stats.n - (records.len() / rl) as u64;
        for (i, rec) in records.chunks_exact(rl).enumerate() {
            self.spill.write_all(&(first + i as u64).to_le_bytes())?;
            self.spill.write_all(rec)?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        use std::io::{BufRead, Read};
        self.stats.finish();
        let rl = self.layout.record_len;
        let entry = rl + 8;
        let spill = self.spill.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        let n = self.stats.n;
        let cube = Cube::new(self.stats.lo, self.stats.hi, self.header.scale[0], self.build);
        let h = self.header.clone();
        let gps = self.gps;
        let mut out = self.out;
        let read_all = |mut f: &std::fs::File| -> Result<Vec<u8>> {
            f.seek(SeekFrom::Start(0))?;
            let mut b = Vec::with_capacity((n as usize) * entry);
            f.read_to_end(&mut b)?;
            Ok(b)
        };
        if n <= self.memory_points || cube.max_depth == 0 {
            // Small enough to build in memory.
            let all = read_all(spill.as_file())?;
            let mut records = Vec::with_capacity(n as usize * rl);
            for e in all.chunks_exact(entry) {
                records.extend_from_slice(&e[8..]);
            }
            drop(all);
            write_copc_with(&mut out, self.header, &self.layout, &self.stats, &cube, |sink| {
                build_bucket(&Bucket { records: &records, index: None }, rl, &h, gps, &cube, &mut Upper::default(), sink)
            })?;
            out.flush()?;
            return Ok(());
        }
        // Pass 1: the points per node at the levels above the buckets (counted as in memory, for
        // nodes whose parent is not a leaf), and at each level whatever its parent, to choose the
        // buckets' level: the coarsest whose nodes fit in memory.
        let pass = |f: &std::fs::File, mut each: Box<EachPoint<'_>>| -> Result<()> {
            let mut f = f;
            f.seek(SeekFrom::Start(0))?;
            let mut r = std::io::BufReader::with_capacity(1 << 22, f);
            let mut e = vec![0u8; entry];
            loop {
                if r.fill_buf()?.is_empty() {
                    break;
                }
                r.read_exact(&mut e)?;
                let idx = u64::from_le_bytes(e[..8].try_into().unwrap_or_default());
                let rel = cube.rel(&record_core(&e[8..], h.point_format, &h.scale, &h.offset));
                each(idx, &e[8..], rel)?;
            }
            Ok(())
        };
        let deepest = cube.max_depth.min(7);
        let mut plain: Vec<HashMap<VoxelKey, u64>> = vec![HashMap::new(); deepest as usize + 1];
        pass(
            spill.as_file(),
            Box::new(|_, _, r| {
                for (l, m) in plain.iter_mut().enumerate() {
                    *m.entry(Cube::key(&r, l as i32)).or_default() += 1;
                }
                Ok(())
            }),
        )?;
        let l0 = (1..=deepest).find(|&l| plain[l as usize].values().all(|c| *c <= self.memory_points)).unwrap_or(deepest);
        let mut upper = Upper { levels: l0, ..Default::default() };
        // The counts above the buckets, from the plain counts: a node is counted when its parent
        // is not a leaf.
        for level in 0..l0 {
            for (k, c) in &plain[level as usize] {
                if level == 0 || upper.counts.get(&parent(*k)).is_some_and(|p| *p > cube.leaf_points) {
                    upper.counts.insert(*k, *c);
                }
            }
        }
        let buckets = std::mem::take(&mut plain[l0 as usize]);
        drop(plain);
        // Pass 2: the points of each bucket together, in segments of a second temporary file.
        let mut parts = std::io::BufWriter::with_capacity(1 << 20, tempfile::NamedTempFile::new()?);
        let flush_at = ((64usize << 20) / buckets.len().max(1)).clamp(16 << 10, 1 << 20);
        let mut buffers: HashMap<VoxelKey, Vec<u8>> = HashMap::new();
        let mut segments: HashMap<VoxelKey, Vec<(u64, usize)>> = HashMap::new();
        let mut at = 0u64;
        {
            let parts = &mut parts;
            pass(
                spill.as_file(),
                Box::new(|idx, rec, r| {
                    let key = Cube::key(&r, l0);
                    let b = buffers.entry(key).or_default();
                    b.extend_from_slice(&idx.to_le_bytes());
                    b.extend_from_slice(rec);
                    if b.len() >= flush_at {
                        parts.write_all(b)?;
                        segments.entry(key).or_default().push((at, b.len()));
                        at += b.len() as u64;
                        b.clear();
                    }
                    Ok(())
                }),
            )?;
        }
        for (key, b) in buffers.iter_mut() {
            if !b.is_empty() {
                parts.write_all(b)?;
                segments.entry(*key).or_default().push((at, b.len()));
                at += b.len() as u64;
            }
        }
        drop(buffers);
        drop(spill);
        let parts = parts.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        let mut keys: Vec<VoxelKey> = segments.keys().copied().collect();
        keys.sort();
        write_copc_with(&mut out, self.header, &self.layout, &self.stats, &cube, |sink| {
            let mut f = parts.as_file();
            for key in keys {
                let mut bytes = Vec::new();
                for (offset, len) in &segments[&key] {
                    f.seek(SeekFrom::Start(*offset))?;
                    let start = bytes.len();
                    bytes.resize(start + len, 0);
                    f.read_exact(&mut bytes[start..])?;
                }
                let m = bytes.len() / entry;
                let mut index = Vec::with_capacity(m);
                let mut records = Vec::with_capacity(m * rl);
                for e in bytes.chunks_exact(entry) {
                    index.push(u64::from_le_bytes(e[..8].try_into().unwrap_or_default()));
                    records.extend_from_slice(&e[8..]);
                }
                drop(bytes);
                build_bucket(&Bucket { records: &records, index: Some(&index) }, rl, &h, gps, &cube, &mut upper, sink)?;
            }
            // The nodes above the buckets.
            let mut keys: Vec<VoxelKey> = upper.members.keys().copied().collect();
            keys.sort();
            let mut buf = Vec::new();
            for key in keys {
                let m = upper.members.get_mut(&key).expect("a node");
                m.sort_unstable_by(|a, b| gps_time(&a.1, gps).total_cmp(&gps_time(&b.1, gps)).then(a.0.cmp(&b.0)));
                buf.clear();
                for (_, r) in m.iter() {
                    buf.extend_from_slice(r);
                }
                sink.write_node(key, &buf, rl)?;
            }
            Ok(())
        })?;
        out.flush()?;
        Ok(())
    }
}

/// The deepest octree level whose cumulative point count stays within a budget (at least the
/// root's), from `(level, points)` of each node.
pub(crate) fn deepest_level_within(nodes: impl Iterator<Item = (usize, u64)>, limit: u64) -> i32 {
    let mut per_level: Vec<u64> = Vec::new();
    for (l, c) in nodes {
        if per_level.len() <= l {
            per_level.resize(l + 1, 0);
        }
        per_level[l] += c;
    }
    let mut total = 0;
    let mut deepest = 0;
    for (l, c) in per_level.iter().enumerate() {
        total += c;
        if total > limit && l > 0 {
            break;
        }
        deepest = l as i32;
    }
    deepest
}

/// A bijective scramble of an index (a 64-bit mix), for a deterministic pseudo-random order.
pub(crate) fn scramble(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}
