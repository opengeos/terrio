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
        if !self.loaded_pages.insert(offset) {
            return Ok(());
        }
        if size > 1 << 30 {
            return Err(Error::InvalidData(format!("a COPC hierarchy page of {size} bytes")));
        }
        let b = source.read_at(offset, size as usize)?;
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

    /// Reads the points of the nodes the options select, in file order.
    pub fn read(
        &mut self,
        source: &mut Source,
        h: &LasHeader,
        layout: &Layout,
        options: &PointReadOptions,
    ) -> Result<Vec<super::Dimension>> {
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
            // The deepest level whose cumulative point count stays within the limit.
            let mut per_level: Vec<u64> = Vec::new();
            for n in &nodes {
                let l = n.key.level as usize;
                if per_level.len() <= l {
                    per_level.resize(l + 1, 0);
                }
                per_level[l] += n.point_count as u64;
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
            nodes.retain(|n| n.key.level <= deepest);
        }
        nodes.sort_by_key(|n| n.offset);
        let total: usize = nodes.iter().map(|n| n.point_count as usize).sum();
        let capacity = if f.is_empty() { total } else { total.min(1 << 20) };
        let mut columns = Columns::new(layout, capacity);
        let rl = layout.record_len;
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
                let records = self.decompress(data, n.point_count as usize, rl)?;
                for rec in records.chunks_exact(rl) {
                    if !f.is_empty() && !f.keeps(&record_core(rec, h.point_format, &h.scale, &h.offset)) {
                        continue;
                    }
                    columns.push(rec, &h.scale, &h.offset);
                }
            }
            i = j;
        }
        Ok(columns.into_dimensions())
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
#[derive(Debug, Clone, Copy)]
pub(crate) struct CopcBuild {
    /// The deepest level; 16 by default.
    pub max_depth: Option<u32>,
    /// The most points of a leaf node; 100 000 by default.
    pub leaf_points: Option<u64>,
}

/// Writes records (all of `layout`'s format, 6 to 8) as a COPC file. `header` has the VLRs to
/// write besides the COPC and LASzip ones, the scale, offset and version; its counts and bounds
/// are computed here.
pub(crate) fn write_copc<W: Write + Seek>(
    w: &mut W,
    mut header: LasHeader,
    layout: &Layout,
    records: &[u8],
    build: CopcBuild,
) -> Result<()> {
    let rl = layout.record_len;
    let n = records.len() / rl;
    let format = layout.format;
    let core = |i: usize| record_core(&records[i * rl..(i + 1) * rl], format, &header.scale, &header.offset);
    // The cube: the bounds' center and their largest half extent.
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    let (mut tmin, mut tmax) = (f64::INFINITY, f64::NEG_INFINITY);
    let gps = super::las::standard_fields(format).iter().find(|f| f.name == "gps_time").map(|f| f.field);
    for i in 0..n {
        let p = core(i);
        for (k, v) in [p.x, p.y, p.z].into_iter().enumerate() {
            lo[k] = lo[k].min(v);
            hi[k] = hi[k].max(v);
        }
        if let Some(super::las::Field::Plain(o)) = gps {
            let t = f64::from_le_bytes(records[i * rl + o..i * rl + o + 8].try_into().unwrap_or_default());
            tmin = tmin.min(t);
            tmax = tmax.max(t);
        }
    }
    if n == 0 {
        lo = [0.0; 3];
        hi = [0.0; 3];
        tmin = 0.0;
        tmax = 0.0;
    }
    let center = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
    let half = ((hi[0] - lo[0]).max(hi[1] - lo[1]).max(hi[2] - lo[2]) / 2.0).max(header.scale[0]) * (1.0 + 1e-9);
    let spacing = 2.0 * half / GRID as f64;
    let max_depth = build.max_depth.unwrap_or(MAX_DEPTH).min(MAX_DEPTH) as i32;
    let leaf_points = build.leaf_points.unwrap_or(LEAF_POINTS).max(1);
    let rel = |i: usize| {
        let p = core(i);
        [
            ((p.x - (center[0] - half)) / (2.0 * half)).clamp(0.0, 1.0 - 1e-12),
            ((p.y - (center[1] - half)) / (2.0 * half)).clamp(0.0, 1.0 - 1e-12),
            ((p.z - (center[2] - half)) / (2.0 * half)).clamp(0.0, 1.0 - 1e-12),
        ]
    };
    let key_at = |r: &[f64; 3], level: i32| {
        let cells = (1u64 << level) as f64;
        VoxelKey { level, x: (r[0] * cells) as i32, y: (r[1] * cells) as i32, z: (r[2] * cells) as i32 }
    };
    // The points in each node's cube, level by level, down to the nodes small enough to be
    // leaves.
    let mut counts: HashMap<VoxelKey, u64> = HashMap::new();
    for level in 0..=max_depth {
        let mut any = false;
        for i in 0..n {
            let key = key_at(&rel(i), level);
            let parent = VoxelKey { level: level - 1, x: key.x >> 1, y: key.y >> 1, z: key.z >> 1 };
            if level == 0 || counts.get(&parent).is_some_and(|c| *c > leaf_points) {
                *counts.entry(key).or_default() += 1;
                any = true;
            }
        }
        if !any {
            break;
        }
    }

    // Assign points to nodes, top down: a leaf (a node of at most `leaf_points` points, or at the
    // deepest level) takes all the points that reach it; another node keeps the first point to
    // land in each cell of its grid, in a pseudo-random order so that each level is an even
    // sample, and passes the others down.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| scramble(i as u64));
    let mut occupied: HashMap<VoxelKey, HashSet<u32>> = HashMap::new();
    let mut members: HashMap<VoxelKey, Vec<usize>> = HashMap::new();
    for &i in &order {
        let r = rel(i);
        for level in 0..=max_depth {
            let key = key_at(&r, level);
            let leaf = level == max_depth || counts.get(&key).is_none_or(|c| *c <= leaf_points);
            if !leaf {
                let cells = (1u64 << level) as f64;
                let g = GRID as f64;
                let c = |k: usize, at: i32| (((r[k] * cells) - at as f64) * g).clamp(0.0, g - 1.0) as u32;
                let cell = c(0, key.x) | (c(1, key.y) << 7) | (c(2, key.z) << 14);
                if !occupied.entry(key).or_default().insert(cell) {
                    continue;
                }
            }
            members.entry(key).or_default().push(i);
            break;
        }
    }
    drop(occupied);
    // Within a node, the points in acquisition (GPS time) order, which LASzip's predictors
    // compress best; without GPS times, in the input's order.
    let time = |i: usize| match gps {
        Some(super::las::Field::Plain(o)) => f64::from_le_bytes(records[i * rl + o..i * rl + o + 8].try_into().unwrap_or_default()),
        _ => 0.0,
    };
    for m in members.values_mut() {
        m.sort_unstable_by(|&a, &b| time(a).total_cmp(&time(b)).then(a.cmp(&b)));
    }
    // The nodes in key order (level by level); every ancestor of a node needs an entry.
    let mut keys: Vec<VoxelKey> = members.keys().copied().collect();
    let mut all: HashSet<VoxelKey> = keys.iter().copied().collect();
    for k in &keys {
        let mut k = *k;
        while k.level > 0 {
            k = VoxelKey { level: k.level - 1, x: k.x >> 1, y: k.y >> 1, z: k.z >> 1 };
            all.insert(k);
        }
    }
    all.insert(VoxelKey::ROOT);
    keys.sort();
    let mut empty: Vec<VoxelKey> = all.into_iter().filter(|k| !members.contains_key(k)).collect();
    empty.sort();

    // The header and VLRs: the COPC info record first, then the LASzip record.
    let items = laz::LazItemRecordBuilder::default_for_point_format_id(format, (rl - super::las::standard_record_length(format)) as u16)
        .map_err(|e| Error::InvalidArgument(format!("LAZ: {e}")))?;
    let vlr = laz::LazVlrBuilder::new(items.clone()).with_variable_chunk_size().build();
    let mut laz_data = Vec::new();
    vlr.write_to(&mut laz_data)?;
    let mut info =
        CopcInfo { center, halfsize: half, spacing, root_hier_offset: 0, root_hier_size: 0, gpstime_minimum: tmin, gpstime_maximum: tmax };
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
    header.point_count = n as u64;
    header.min = lo;
    header.max = hi;
    let mut by_return = vec![0u64; 15];
    for i in 0..n {
        let r = core(i).return_number as usize;
        if (1..=15).contains(&r) {
            by_return[r - 1] += 1;
        }
    }
    header.points_by_return = by_return;
    header.offset_to_point_data = header.data_offset() as u32;
    let start = w.stream_position()?;
    header.write(w)?;

    // The points: one LASzip chunk per node, then the chunk table.
    let data_start = w.stream_position()?;
    w.write_all(&(-1i64).to_le_bytes())?;
    let mut table = laz::laszip::ChunkTable::default();
    let mut entries = Vec::with_capacity(keys.len() + empty.len());
    let mut buf = Vec::new();
    for key in &keys {
        let idx = &members[key];
        buf.clear();
        for &i in idx {
            buf.extend_from_slice(&records[i * rl..(i + 1) * rl]);
        }
        let mut c = laz::record::LayeredPointRecordCompressor::new(Cursor::new(Vec::new()));
        c.set_fields_from(&items).map_err(|e| Error::InvalidArgument(format!("LAZ: {e}")))?;
        c.compress_many(&buf)?;
        c.done()?;
        let chunk = c.into_inner().into_inner();
        let offset = w.stream_position()?;
        w.write_all(&chunk)?;
        table.push(laz::laszip::ChunkTableEntry { point_count: idx.len() as u64, byte_count: chunk.len() as u64 });
        entries.push(CopcNode { key: *key, offset, byte_size: chunk.len() as i32, point_count: idx.len() as i32 });
    }
    let table_pos = w.stream_position()?;
    table.write_to(&mut *w, &vlr)?;
    let end_of_points = w.stream_position()?;
    w.seek(SeekFrom::Start(data_start))?;
    w.write_all(&(table_pos as i64).to_le_bytes())?;
    w.seek(SeekFrom::Start(end_of_points))?;

    // The hierarchy: one page, in an EVLR, then the other EVLRs.
    for key in &empty {
        entries.push(CopcNode { key: *key, offset: 0, byte_size: 0, point_count: 0 });
    }
    entries.sort_by_key(|e| e.key);
    let mut page = Vec::with_capacity(entries.len() * ENTRY_LEN);
    for e in &entries {
        e.write(&mut page);
    }
    info.root_hier_offset = end_of_points + super::las::EVLR_HEADER_LEN as u64;
    info.root_hier_size = page.len() as u64;
    let mut evlrs = vec![super::las::Vlr::new(COPC_USER_ID, 1000, "EPT hierarchy", page)];
    evlrs.append(&mut header.evlrs);
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

/// A bijective scramble of an index (a 64-bit mix), for a deterministic pseudo-random order.
fn scramble(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}
