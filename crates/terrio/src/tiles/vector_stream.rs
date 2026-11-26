// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/mvt/ogrmvtdataset.cpp; frmts/pmtiles/; frmts/mbtiles/
// No code is translated from these sources.

//! Streaming out-of-core vector tile writer for PMTiles, MBTiles, and MVT directories.
//!
//! Features arrive in chunks per layer, are reprojected to EPSG:3857 and clipped/quantized into
//! compact tile-feature records in parallel, and are spilled into sorted temporary run files
//! beside the destination whenever the in-memory run buffer exceeds its byte budget. At
//! completion, a bounded-fan-in K-way merge groups records by PMTiles Hilbert `tile_id`, encodes
//! and gzips batches of tiles in parallel, and streams PMTiles archives directly through
//! [`SpillSink`] and [`pmtiles::write_archive_spilled`].

use super::clip::{self, Pt, Rect};
use super::mvt::{MvtFeature, MvtLayer, MvtValue};
use super::pyramid::{SpillSink, TileSink};
use super::vector::{
    Crossing, LayerMetaInput, LayerStats, TileFrame, add_attr_stat, build_merged_layer_metadata, collect_points, double_value,
    encode_for_tile, encode_limited_gz, encode_polygons_for_tile, int_value, metadata_items_parts, parse_layer_conf, polygon_rings,
    resolve_layer_zooms, tile_dim,
};
use super::{MAX_GM, TileWriteOptions, mbtiles, pmtiles};
use crate::formats::VectorDriver;
use crate::{Error, Result};
use serde_json::json;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use wbvector::{Feature, FieldValue, Geometry, Layer, Schema};

/// Default in-memory run buffer budget before flushing a sorted run file to disk (128 MiB).
const DEFAULT_RUN_BUDGET_BYTES: usize = 128 << 20;

/// Maximum number of sorted run files opened simultaneously during K-way merge.
const DEFAULT_MAX_MERGE_WAY: usize = 128;

/// Maximum number of features processed in one parallel sub-batch before spilling.
const PARALLEL_SUBBATCH_FEATURES: usize = 2048;

/// Maximum estimated tile intersections buffered in one parallel sub-batch before spilling.
const PARALLEL_SUBBATCH_TILES: u64 = 16_384;

/// Tile-intersection threshold above which a single feature streams directly into the run spiller
/// with per-tile spill backpressure instead of buffering all its records in a parallel worker.
const HUGE_FEATURE_TILES: u64 = 1024;

/// Maximum number of tiles batched together for parallel MVT + gzip encoding.
const TILE_ENCODE_BATCH: usize = 256;

/// Maximum buffered decoded feature bytes in one parallel tile-encoding batch (32 MiB).
const TILE_ENCODE_BATCH_BYTES: usize = 32 << 20;

/// Read buffer per open run file during K-way merge (128 KiB).
const RUN_READ_BUF: usize = 128 << 10;

/// Write buffer for run files and final output (1 MiB).
const RUN_WRITE_BUF: usize = 1 << 20;

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One spilled tile-feature record, sorted by `(tile_id, layer_idx, seq)`.
#[derive(Debug, Clone)]
struct SpillRecord {
    tile_id: u64,
    layer_idx: u16,
    seq: u64,
    geom_type: u8,
    geom_bytes: Vec<u8>,
    tags_bytes: Vec<u8>,
}

impl SpillRecord {
    fn key(&self) -> (u64, u16, u64) {
        (self.tile_id, self.layer_idx, self.seq)
    }

    fn approx_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.geom_bytes.len() + self.tags_bytes.len()
    }

    fn write_to(&self, w: &mut impl Write) -> Result<()> {
        let geom_len =
            u32::try_from(self.geom_bytes.len()).map_err(|_| Error::InvalidData("spilled tile geometry exceeds 4 GiB".into()))?;
        let tags_len =
            u32::try_from(self.tags_bytes.len()).map_err(|_| Error::InvalidData("spilled tile attributes exceed 4 GiB".into()))?;
        let mut hdr = [0u8; 27];
        hdr[0..8].copy_from_slice(&self.tile_id.to_le_bytes());
        hdr[8..10].copy_from_slice(&self.layer_idx.to_le_bytes());
        hdr[10..18].copy_from_slice(&self.seq.to_le_bytes());
        hdr[18] = self.geom_type;
        hdr[19..23].copy_from_slice(&geom_len.to_le_bytes());
        hdr[23..27].copy_from_slice(&tags_len.to_le_bytes());
        w.write_all(&hdr)?;
        w.write_all(&self.geom_bytes)?;
        w.write_all(&self.tags_bytes)?;
        Ok(())
    }

    fn read_from(r: &mut impl Read) -> Result<Option<Self>> {
        let mut hdr = [0u8; 27];
        let mut read = 0usize;
        while read < hdr.len() {
            match r.read(&mut hdr[read..])? {
                0 if read == 0 => return Ok(None),
                0 => return Err(Error::InvalidData("truncated spilled vector-tile run record".into())),
                n => read += n,
            }
        }
        let tile_id = u64::from_le_bytes(hdr[0..8].try_into().expect("8 bytes"));
        let layer_idx = u16::from_le_bytes(hdr[8..10].try_into().expect("2 bytes"));
        let seq = u64::from_le_bytes(hdr[10..18].try_into().expect("8 bytes"));
        let geom_type = hdr[18];
        let geom_len = u32::from_le_bytes(hdr[19..23].try_into().expect("4 bytes")) as usize;
        let tags_len = u32::from_le_bytes(hdr[23..27].try_into().expect("4 bytes")) as usize;
        let mut geom_bytes = vec![0u8; geom_len];
        r.read_exact(&mut geom_bytes)?;
        let mut tags_bytes = vec![0u8; tags_len];
        r.read_exact(&mut tags_bytes)?;
        Ok(Some(Self { tile_id, layer_idx, seq, geom_type, geom_bytes, tags_bytes }))
    }
}

/// RAII tracker removing any leftover temporary run files on drop.
struct TempRunFiles {
    paths: Vec<PathBuf>,
}

impl TempRunFiles {
    fn new() -> Self {
        Self { paths: Vec::new() }
    }

    fn push(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    fn remove_and_clear(&mut self) {
        for p in self.paths.drain(..) {
            let _ = std::fs::remove_file(&p);
        }
    }
}

impl Drop for TempRunFiles {
    fn drop(&mut self) {
        self.remove_and_clear();
    }
}

fn create_temp_run_file(output: &str) -> Result<(PathBuf, File)> {
    let out = Path::new(output);
    let name = out.file_name().and_then(|s| s.to_str()).unwrap_or("tiles");
    let pid = crate::process_id();
    let seq = RUN_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let file_name = format!(".{name}.terrio-vrun-{pid}-{seq}.tmp");
    let dir = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() && p.exists() => p.to_path_buf(),
        Some(p) if p.as_os_str().is_empty() => PathBuf::from("."),
        _ => std::env::temp_dir(),
    };
    let path = dir.join(file_name);
    let _ = std::fs::remove_file(&path);
    let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
    Ok((path, file))
}

// ─── Compact Varint Encoding for Spilled Geometry & Attributes ──────────────────────────────────

fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_uvarint(b: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*pos).ok_or_else(|| Error::InvalidData("truncated varint in tile run".into()))?;
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Error::InvalidData("varint too long in tile run".into()))
}

fn zigzag_i64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag_i64(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn encode_geom_cmds(cmds: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cmds.len() * 2 + 4);
    put_uvarint(&mut out, cmds.len() as u64);
    for &c in cmds {
        put_uvarint(&mut out, u64::from(c));
    }
    out
}

fn decode_geom_cmds(b: &[u8]) -> Result<Vec<u32>> {
    let mut pos = 0;
    let len = get_uvarint(b, &mut pos)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(get_uvarint(b, &mut pos)? as u32);
    }
    Ok(out)
}

/// Encodes non-null feature attributes compactly by schema field index (avoiding repeating field
/// names across zoom levels and tiles).
fn encode_compact_tags(schema: &Schema, f: &Feature) -> Vec<u8> {
    let mut out = Vec::new();
    let count = schema.fields().iter().zip(&f.attributes).filter(|(_, v)| !matches!(v, FieldValue::Null)).count();
    put_uvarint(&mut out, count as u64);
    for (idx, (_, v)) in schema.fields().iter().zip(&f.attributes).enumerate() {
        let mv = match v {
            FieldValue::Null => continue,
            FieldValue::Integer(i) => int_value(*i),
            FieldValue::Boolean(b) => MvtValue::Bool(*b),
            FieldValue::Float(d) => double_value(*d),
            FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => MvtValue::String(s.clone()),
            FieldValue::Blob(b) => MvtValue::String(b.iter().map(|x| format!("{x:02X}")).collect()),
        };
        put_uvarint(&mut out, idx as u64);
        match mv {
            MvtValue::String(s) => {
                out.push(0);
                put_uvarint(&mut out, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
            MvtValue::Float(x) => {
                out.push(1);
                out.extend_from_slice(&x.to_le_bytes());
            }
            MvtValue::Double(x) => {
                out.push(2);
                out.extend_from_slice(&x.to_le_bytes());
            }
            MvtValue::Int(i) => {
                out.push(3);
                out.extend_from_slice(&i.to_le_bytes());
            }
            MvtValue::UInt(u) => {
                out.push(4);
                put_uvarint(&mut out, u);
            }
            MvtValue::SInt(i) => {
                out.push(5);
                put_uvarint(&mut out, zigzag_i64(i));
            }
            MvtValue::Bool(b) => {
                out.push(6);
                out.push(u8::from(b));
            }
        }
    }
    out
}

fn decode_compact_tags(b: &[u8], field_names: &[String]) -> Result<Vec<(String, MvtValue)>> {
    let mut pos = 0;
    let count = get_uvarint(b, &mut pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let idx = get_uvarint(b, &mut pos)? as usize;
        let name = field_names.get(idx).ok_or_else(|| Error::InvalidData(format!("spilled attribute field index {idx} out of range")))?;
        let tag = *b.get(pos).ok_or_else(|| Error::InvalidData("truncated attribute tag in tile run".into()))?;
        pos += 1;
        let mv = match tag {
            0 => {
                let len = get_uvarint(b, &mut pos)? as usize;
                let end = pos
                    .checked_add(len)
                    .filter(|&e| e <= b.len())
                    .ok_or_else(|| Error::InvalidData("truncated string in tile run".into()))?;
                let s = std::str::from_utf8(&b[pos..end])
                    .map_err(|e| Error::InvalidData(format!("invalid UTF-8 in tile run: {e}")))?
                    .to_string();
                pos = end;
                MvtValue::String(s)
            }
            1 => {
                let end =
                    pos.checked_add(4).filter(|&e| e <= b.len()).ok_or_else(|| Error::InvalidData("truncated f32 in tile run".into()))?;
                let v = f32::from_le_bytes(b[pos..end].try_into().expect("4 bytes"));
                pos = end;
                MvtValue::Float(v)
            }
            2 => {
                let end =
                    pos.checked_add(8).filter(|&e| e <= b.len()).ok_or_else(|| Error::InvalidData("truncated f64 in tile run".into()))?;
                let v = f64::from_le_bytes(b[pos..end].try_into().expect("8 bytes"));
                pos = end;
                MvtValue::Double(v)
            }
            3 => {
                let end =
                    pos.checked_add(8).filter(|&e| e <= b.len()).ok_or_else(|| Error::InvalidData("truncated i64 in tile run".into()))?;
                let v = i64::from_le_bytes(b[pos..end].try_into().expect("8 bytes"));
                pos = end;
                MvtValue::Int(v)
            }
            4 => MvtValue::UInt(get_uvarint(b, &mut pos)?),
            5 => MvtValue::SInt(unzigzag_i64(get_uvarint(b, &mut pos)?)),
            6 => {
                let v = *b.get(pos).ok_or_else(|| Error::InvalidData("truncated bool in tile run".into()))?;
                pos += 1;
                MvtValue::Bool(v != 0)
            }
            t => return Err(Error::InvalidData(format!("unknown attribute tag {t} in tile run"))),
        };
        out.push((name.clone(), mv));
    }
    Ok(out)
}

// ─── K-Way External Merge ───────────────────────────────────────────────────────────────────────

struct HeapEntry {
    record: SpillRecord,
    run_idx: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.record.key() == other.record.key() && self.run_idx == other.run_idx
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap behavior in Rust's max-heap BinaryHeap.
        other.record.key().cmp(&self.record.key()).then_with(|| other.run_idx.cmp(&self.run_idx))
    }
}

struct KWayMerger {
    readers: Vec<BufReader<File>>,
    heap: BinaryHeap<HeapEntry>,
}

impl KWayMerger {
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let mut readers = Vec::with_capacity(paths.len());
        let mut heap = BinaryHeap::with_capacity(paths.len());
        for (run_idx, path) in paths.iter().enumerate() {
            let mut reader = BufReader::with_capacity(RUN_READ_BUF, File::open(path)?);
            if let Some(record) = SpillRecord::read_from(&mut reader)? {
                heap.push(HeapEntry { record, run_idx });
            }
            readers.push(reader);
        }
        Ok(Self { readers, heap })
    }

    fn next_record(&mut self) -> Result<Option<SpillRecord>> {
        let Some(top) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some(next) = SpillRecord::read_from(&mut self.readers[top.run_idx])? {
            self.heap.push(HeapEntry { record: next, run_idx: top.run_idx });
        }
        Ok(Some(top.record))
    }
}

enum RecordStream {
    Memory(std::vec::IntoIter<SpillRecord>),
    Merged(KWayMerger),
}

impl RecordStream {
    fn next_record(&mut self) -> Result<Option<SpillRecord>> {
        match self {
            RecordStream::Memory(it) => Ok(it.next()),
            RecordStream::Merged(m) => m.next_record(),
        }
    }
}

// ─── Streaming Vector Tile Writer ───────────────────────────────────────────────────────────────

struct LayerPlan {
    src_name: String,
    target_name: String,
    description: String,
    min_zoom: u8,
    max_zoom: u8,
    needs_reproject: bool,
    schema: Schema,
    field_names: Vec<String>,
    stats: LayerStats,
    feature_count_seen: usize,
}

/// Computes a zero-allocation bounding box matching `collect_points` (exterior rings only for
/// polygons).
fn geometry_bbox(g: &Geometry) -> Option<Rect> {
    let mut b: Option<Rect> = None;
    let mut extend = |x: f64, y: f64| {
        b = Some(match b {
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            None => (x, y, x, y),
        });
    };
    match g {
        Geometry::Point(c) => extend(c.x, c.y),
        Geometry::MultiPoint(cs) | Geometry::LineString(cs) => {
            for c in cs {
                extend(c.x, c.y);
            }
        }
        Geometry::MultiLineString(ls) => {
            for c in ls.iter().flatten() {
                extend(c.x, c.y);
            }
        }
        Geometry::Polygon { exterior, .. } => {
            for c in &exterior.0 {
                extend(c.x, c.y);
            }
        }
        Geometry::MultiPolygon(polys) => {
            for (exterior, _) in polys {
                for c in &exterior.0 {
                    extend(c.x, c.y);
                }
            }
        }
        Geometry::GeometryCollection(gs) => {
            for child in gs {
                if let Some((x0, y0, x1, y1)) = geometry_bbox(child) {
                    extend(x0, y0);
                    extend(x1, y1);
                }
            }
        }
    }
    b
}

/// Estimates how many `(z, tx, ty)` tiles a feature intersects across `layer_min..=layer_max`.
fn feature_tile_estimate(g: &Geometry, layer_min: u8, layer_max: u8, o: &TileWriteOptions) -> u64 {
    let part_estimate = |part: &Geometry| -> u64 {
        let Some(bb) = geometry_bbox(part) else {
            return 0;
        };
        let mut total = 0u64;
        for z in layer_min..=layer_max {
            let dim = tile_dim(z);
            let buffer = dim * o.buffer as f64 / o.extent as f64;
            let max_tile = (1u64 << z) as i64 - 1;
            let tx0 = (((bb.0 + MAX_GM - buffer) / dim) as i64).max(0);
            let ty0 = (((MAX_GM - bb.3 - buffer) / dim) as i64).max(0);
            let tx1 = (((bb.2 + MAX_GM + buffer) / dim) as i64).min(max_tile);
            let ty1 = (((MAX_GM - bb.1 + buffer) / dim) as i64).min(max_tile);
            if tx1 >= tx0 && ty1 >= ty0 {
                let nx = (tx1 - tx0 + 1) as u64;
                let ny = (ty1 - ty0 + 1) as u64;
                total = total.saturating_add(nx.saturating_mul(ny));
            }
        }
        total
    };
    match g {
        Geometry::GeometryCollection(gs) => gs.iter().fold(0u64, |acc, part| acc.saturating_add(part_estimate(part))),
        other => part_estimate(other),
    }
}

struct FeatureOutcome {
    counted: bool,
    env: Option<Rect>,
    geom_counts: [usize; 4],
}

/// Encodes all tile intersections for a single feature, passing each spilled record to `emit`.
#[allow(clippy::too_many_arguments)]
fn encode_feature_tiles(
    f: &Feature,
    feature_index: usize,
    layer_u16: u16,
    layer_min: u8,
    layer_max: u8,
    layer_name: &str,
    schema: &Schema,
    o: &TileWriteOptions,
    pts: &mut Vec<Pt>,
    mut emit: impl FnMut(SpillRecord) -> Result<()>,
) -> Result<FeatureOutcome> {
    let Some(g) = &f.geometry else {
        return Ok(FeatureOutcome { counted: false, env: None, geom_counts: [0; 4] });
    };
    let parts: Vec<&Geometry> = match g {
        Geometry::GeometryCollection(gs) => gs.iter().collect(),
        g => vec![g],
    };
    let mut counted = false;
    let mut f_env: Option<Rect> = None;
    let mut geom_counts = [0usize; 4];
    let mut tags_bytes: Option<Vec<u8>> = None;

    for g in parts {
        let polys = polygon_rings(g);
        let (bb, all_rings_bb) = match &polys {
            // Exterior rings for envelope and tile range (matching `collect_points`), and all
            // rings for `encode_polygons_for_tile`'s `inside` check (matching `encode_for_tile`).
            Some(ps) => {
                let Some(ext_bb) = clip::bbox(ps.iter().filter_map(|p| p.first()).flatten()) else {
                    continue;
                };
                let all_bb = clip::bbox(ps.iter().flatten().flatten()).unwrap_or(ext_bb);
                (ext_bb, all_bb)
            }
            None => {
                pts.clear();
                collect_points(g, pts);
                match clip::bbox(pts.iter()) {
                    Some(b) => (b, b),
                    None => continue,
                }
            }
        };
        let mut crossings: Option<Vec<Crossing>> = None;
        f_env = Some(match f_env {
            Some(e) => (e.0.min(bb.0), e.1.min(bb.1), e.2.max(bb.2), e.3.max(bb.3)),
            None => bb,
        });
        counted = true;
        let tags = tags_bytes.get_or_insert_with(|| encode_compact_tags(schema, f));
        for z in layer_min..=layer_max {
            let dim = tile_dim(z);
            let buffer = dim * o.buffer as f64 / o.extent as f64;
            let max_tile = (1u64 << z) as i64 - 1;
            let tx0 = (((bb.0 + MAX_GM - buffer) / dim) as i64).max(0);
            let ty0 = (((MAX_GM - bb.3 - buffer) / dim) as i64).max(0);
            let tx1 = (((bb.2 + MAX_GM + buffer) / dim) as i64).min(max_tile);
            let ty1 = (((MAX_GM - bb.1 + buffer) / dim) as i64).min(max_tile);
            let simpl = if z == layer_max { o.simplification_max_zoom.unwrap_or(o.simplification) } else { o.simplification };
            let tol = dim / o.extent as f64 * simpl;
            for tx in tx0..=tx1 {
                for ty in ty0..=ty1 {
                    let frame = TileFrame::new(z, tx as u32, ty as u32);
                    let encoded = match &polys {
                        Some(ps) => encode_polygons_for_tile(ps, all_rings_bb, &mut crossings, &frame, o.extent, buffer, tol),
                        None => encode_for_tile(g, &frame, o.extent, buffer, tol, &[]),
                    };
                    let encoded = match encoded {
                        Ok(v) => v,
                        Err(Error::InvalidData(message)) => {
                            return Err(Error::InvalidData(format!(
                                "layer {:?}, feature {} (index {}), tile {z}/{tx}/{ty}: {message}",
                                layer_name, f.fid, feature_index
                            )));
                        }
                        Err(other) => return Err(other),
                    };
                    let Some((geom_type, geometry)) = encoded else {
                        continue;
                    };
                    geom_counts[geom_type as usize] += 1;
                    let tile_id = pmtiles::zxy_to_tile_id(z, tx as u32, ty as u32);
                    emit(SpillRecord {
                        tile_id,
                        layer_idx: layer_u16,
                        seq: 0,
                        geom_type: geom_type as u8,
                        geom_bytes: encode_geom_cmds(&geometry),
                        tags_bytes: tags.clone(),
                    })?;
                }
            }
        }
    }
    Ok(FeatureOutcome { counted, env: f_env, geom_counts })
}

/// Manages the bounded in-memory run buffer, sorted temporary run files, and multi-pass K-way
/// merge compaction.
struct RunSpiller {
    output: String,
    next_seq: u64,
    run_buf: Vec<SpillRecord>,
    run_bytes: usize,
    run_budget: usize,
    max_merge_way: usize,
    temp_runs: TempRunFiles,
}

impl RunSpiller {
    fn new(output: String) -> Self {
        Self {
            output,
            next_seq: 0,
            run_buf: Vec::new(),
            run_bytes: 0,
            run_budget: DEFAULT_RUN_BUDGET_BYTES,
            max_merge_way: DEFAULT_MAX_MERGE_WAY,
            temp_runs: TempRunFiles::new(),
        }
    }

    fn push(&mut self, mut rec: SpillRecord) -> Result<()> {
        rec.seq = self.next_seq;
        self.next_seq += 1;
        self.run_bytes += rec.approx_bytes();
        self.run_buf.push(rec);
        if self.run_bytes >= self.run_budget {
            self.flush_run()?;
        }
        Ok(())
    }

    fn flush_run(&mut self) -> Result<()> {
        if self.run_buf.is_empty() {
            return Ok(());
        }
        self.run_buf.sort_unstable_by_key(SpillRecord::key);
        let (path, file) = create_temp_run_file(&self.output)?;
        self.temp_runs.push(path);
        let mut w = BufWriter::with_capacity(RUN_WRITE_BUF, file);
        for rec in self.run_buf.drain(..) {
            rec.write_to(&mut w)?;
        }
        w.flush()?;
        self.run_bytes = 0;
        Ok(())
    }

    /// Reduces temporary run files through multi-pass bounded-fan-in merges until at most
    /// `self.max_merge_way` runs remain.
    fn compact_runs(&mut self) -> Result<()> {
        while self.temp_runs.paths.len() > self.max_merge_way {
            let old_paths = std::mem::take(&mut self.temp_runs.paths);
            let mut next_paths = Vec::with_capacity(old_paths.len().div_ceil(self.max_merge_way));
            let res = (|| -> Result<()> {
                for group in old_paths.chunks(self.max_merge_way) {
                    if group.len() == 1 {
                        next_paths.push(group[0].clone());
                        continue;
                    }
                    let (out_path, file) = create_temp_run_file(&self.output)?;
                    next_paths.push(out_path);
                    let mut w = BufWriter::with_capacity(RUN_WRITE_BUF, file);
                    let mut merger = KWayMerger::open(group)?;
                    while let Some(rec) = merger.next_record()? {
                        rec.write_to(&mut w)?;
                    }
                    w.flush()?;
                    drop(merger);
                    for consumed in group {
                        let _ = std::fs::remove_file(consumed);
                    }
                }
                Ok(())
            })();
            // Ensure any unconsumed old files and newly created files stay tracked for Drop cleanup.
            if res.is_err() {
                for p in old_paths {
                    if p.exists() {
                        self.temp_runs.push(p);
                    }
                }
                for p in next_paths {
                    self.temp_runs.push(p);
                }
                return res;
            }
            self.temp_runs.paths = next_paths;
        }
        Ok(())
    }
}

/// Streams vector layers chunk by chunk into a PMTiles archive, MBTiles archive, or MVT tile
/// directory using bounded memory via external-sort run files and [`SpillSink`].
pub(crate) struct VectorTileStreamWriter {
    driver: VectorDriver,
    options: TileWriteOptions,
    web: crate::crs::SpatialRef,
    layers: Vec<LayerPlan>,
    global_min: u8,
    global_max: u8,
    env: Option<Rect>,
    spiller: RunSpiller,
}

impl VectorTileStreamWriter {
    pub(crate) fn create(output: &str, driver: VectorDriver, schemas: &[Layer], o: &TileWriteOptions) -> Result<Self> {
        let global_min = o.min_zoom.unwrap_or(0);
        let global_max = o.max_zoom.unwrap_or(5);
        if global_min > 30 || global_max > 30 {
            return Err(Error::InvalidArgument(format!("zoom levels must be at most 30 (got {global_min}..={global_max})")));
        }
        if global_min > global_max {
            return Err(Error::InvalidArgument(format!("the minimum zoom level {global_min} is above the maximum {global_max}")));
        }
        if driver == VectorDriver::MVT && Path::new(output).exists() {
            return Err(Error::InvalidArgument(format!("'{output}' already exists")));
        }
        if schemas.len() > u16::MAX as usize {
            return Err(Error::InvalidArgument(format!("at most {} vector tile layers are supported", u16::MAX)));
        }
        let layer_confs = parse_layer_conf(o.conf.as_deref())?;
        let web = crate::crs::SpatialRef::from_epsg(3857)?;
        let mut layers = Vec::with_capacity(schemas.len());
        for schema_layer in schemas {
            let conf = layer_confs.get(&schema_layer.name);
            let (min_zoom, max_zoom) = resolve_layer_zooms(&schema_layer.name, global_min, global_max, conf)?;
            let target_name = conf.and_then(|c| c.target_name.clone()).unwrap_or_else(|| schema_layer.name.clone());
            let description = conf.and_then(|c| c.description.clone()).unwrap_or_default();
            let src_crs = crate::crs::SpatialRef::from_layer(schema_layer);
            let needs_reproject = !src_crs.is_unknown() && src_crs.epsg != Some(3857);
            let field_names = schema_layer.schema.fields().iter().map(|f| f.name.clone()).collect();
            layers.push(LayerPlan {
                src_name: schema_layer.name.clone(),
                target_name,
                description,
                min_zoom,
                max_zoom,
                needs_reproject,
                schema: schema_layer.schema.clone(),
                field_names,
                stats: LayerStats::default(),
                feature_count_seen: 0,
            });
        }
        Ok(Self {
            driver,
            options: o.clone(),
            web,
            layers,
            global_min,
            global_max,
            env: None,
            spiller: RunSpiller::new(output.to_string()),
        })
    }

    /// Overrides the spill run budget and merge fan-in (used in tests to exercise multi-run and
    /// multi-pass external merge on small datasets).
    #[cfg(test)]
    pub(crate) fn with_spill_limits(mut self, run_budget: usize, max_merge_way: usize) -> Self {
        self.spiller.run_budget = run_budget.max(1);
        self.spiller.max_merge_way = max_merge_way.max(2);
        self
    }

    /// Applies one feature's stats and envelope after its tile records have been encoded cleanly.
    fn record_feature_outcome(&mut self, layer_idx: usize, f: &Feature, outcome: &FeatureOutcome) {
        if outcome.counted {
            let stats = &mut self.layers[layer_idx].stats;
            stats.count += 1;
            for (i, v) in f.attributes.iter().enumerate() {
                add_attr_stat(stats, i, v);
            }
            for g in 0..4 {
                stats.geom_counts[g] += outcome.geom_counts[g];
            }
        }
        if let Some(bb) = outcome.env {
            self.env = Some(match self.env {
                Some(e) => (e.0.min(bb.0), e.1.min(bb.1), e.2.max(bb.2), e.3.max(bb.3)),
                None => bb,
            });
        }
    }

    /// Processes one chunk of features for `layer_idx` in bounded parallel sub-batches (or direct
    /// streaming spill for features spanning many tiles) and spills sorted runs when the in-memory
    /// budget is reached.
    pub(crate) fn write_chunk(&mut self, layer_idx: usize, mut chunk: Layer) -> Result<()> {
        if chunk.features.is_empty() {
            return Ok(());
        }
        if self.layers[layer_idx].needs_reproject {
            chunk = crate::vector::reproject_layer_owned(chunk, &self.web)?;
        }

        let n = chunk.features.len();
        let layer_u16 = layer_idx as u16;
        let mut pos = 0usize;
        let mut pts: Vec<Pt> = Vec::new();

        while pos < n {
            let layer_min = self.layers[layer_idx].min_zoom;
            let layer_max = self.layers[layer_idx].max_zoom;
            let est = chunk.features[pos].geometry.as_ref().map_or(0, |g| feature_tile_estimate(g, layer_min, layer_max, &self.options));

            if est > HUGE_FEATURE_TILES {
                // Stream tile records directly into the run spiller so a single wide feature at
                // high zoom cannot spike RAM beyond `run_budget`.
                let feature_index = self.layers[layer_idx].feature_count_seen;
                let f = &chunk.features[pos];
                let outcome = {
                    let plan = &self.layers[layer_idx];
                    encode_feature_tiles(
                        f,
                        feature_index,
                        layer_u16,
                        layer_min,
                        layer_max,
                        &plan.src_name,
                        &plan.schema,
                        &self.options,
                        &mut pts,
                        |rec| self.spiller.push(rec),
                    )?
                };
                self.record_feature_outcome(layer_idx, f, &outcome);
                self.layers[layer_idx].feature_count_seen += 1;
                pos += 1;
                continue;
            }

            // Gather a bounded contiguous slice of normal features to encode in parallel.
            let mut batch_end = pos + 1;
            let mut batch_tiles = est;
            while batch_end < n && (batch_end - pos) < PARALLEL_SUBBATCH_FEATURES {
                let next_est = chunk.features[batch_end]
                    .geometry
                    .as_ref()
                    .map_or(0, |g| feature_tile_estimate(g, layer_min, layer_max, &self.options));
                if next_est > HUGE_FEATURE_TILES || batch_tiles.saturating_add(next_est) > PARALLEL_SUBBATCH_TILES {
                    break;
                }
                batch_tiles = batch_tiles.saturating_add(next_est);
                batch_end += 1;
            }

            let slice = &chunk.features[pos..batch_end];
            let m = slice.len();
            let stride = crate::par::chunk_len(m, 32);
            let base_feature_index = self.layers[layer_idx].feature_count_seen;
            struct FeatureResult {
                outcome: FeatureOutcome,
                records: Vec<SpillRecord>,
            }
            let mut results: Vec<Result<Vec<FeatureResult>>> = (0..m.div_ceil(stride)).map(|_| Ok(Vec::new())).collect();
            {
                let plan = &self.layers[layer_idx];
                let layer_name = plan.src_name.as_str();
                let schema = &plan.schema;
                let o = &self.options;
                crate::par::chunks_mut(&mut results, 1, |chunk_idx, slot| {
                    let start = chunk_idx * stride;
                    let end = (start + stride).min(m);
                    let mut local = Vec::with_capacity(end - start);
                    let mut local_pts: Vec<Pt> = Vec::new();
                    for (offset, f) in slice.iter().enumerate().take(end).skip(start) {
                        let mut records = Vec::new();
                        match encode_feature_tiles(
                            f,
                            base_feature_index + offset,
                            layer_u16,
                            layer_min,
                            layer_max,
                            layer_name,
                            schema,
                            o,
                            &mut local_pts,
                            |rec| {
                                records.push(rec);
                                Ok(())
                            },
                        ) {
                            Ok(outcome) => local.push(FeatureResult { outcome, records }),
                            Err(e) => {
                                slot[0] = Err(e);
                                return;
                            }
                        }
                    }
                    slot[0] = Ok(local);
                });
            }

            // Verify every worker succeeded before mutating any writer state.
            let batches: Vec<Vec<FeatureResult>> = results.into_iter().collect::<Result<Vec<_>>>()?;

            // Merge stats and records in strict deterministic feature order.
            let mut feature_cursor = pos;
            for batch in batches {
                for fr in batch {
                    let f = &chunk.features[feature_cursor];
                    self.record_feature_outcome(layer_idx, f, &fr.outcome);
                    for rec in fr.records {
                        self.spiller.push(rec)?;
                    }
                    feature_cursor += 1;
                }
            }
            self.layers[layer_idx].feature_count_seen += m;
            pos = batch_end;
        }
        Ok(())
    }

    fn build_metadata(&self) -> Result<(serde_json::Value, [f64; 4], u8, u8)> {
        let (x0, y0, x1, y1) = self.env.ok_or_else(|| Error::InvalidArgument("no geometries to write as vector tiles".into()))?;
        let (lon0, lat0) = super::mercator_to_lonlat(x0, y0);
        let (lon1, lat1) = super::mercator_to_lonlat(x1, y1);
        let mut archive_min: Option<u8> = None;
        let mut archive_max: Option<u8> = None;
        let mut meta_inputs = Vec::with_capacity(self.layers.len());
        for plan in &self.layers {
            archive_min = Some(archive_min.map_or(plan.min_zoom, |m| m.min(plan.min_zoom)));
            archive_max = Some(archive_max.map_or(plan.max_zoom, |m| m.max(plan.max_zoom)));
            meta_inputs.push(LayerMetaInput {
                target_name: &plan.target_name,
                description: &plan.description,
                min_zoom: plan.min_zoom,
                max_zoom: plan.max_zoom,
                schema: &plan.schema,
                stats: &plan.stats,
            });
        }
        let (vector_layers, stat_layers) = build_merged_layer_metadata(&meta_inputs);
        let json = json!({
            "vector_layers": vector_layers,
            "tilestats": { "layerCount": stat_layers.len(), "layers": stat_layers },
        });
        let min_zoom = archive_min.unwrap_or(self.global_min);
        let max_zoom = archive_max.unwrap_or(self.global_max);
        Ok((json, [lon0, lat0, lon1, lat1], min_zoom, max_zoom))
    }

    /// Reconstructs `Vec<MvtLayer>` for one tile from its sorted spilled records.
    fn build_tile_layers(&self, records: Vec<SpillRecord>) -> Result<Vec<MvtLayer>> {
        let mut layers: Vec<MvtLayer> = Vec::new();
        for rec in records {
            let plan = &self.layers[rec.layer_idx as usize];
            let geometry = decode_geom_cmds(&rec.geom_bytes)?;
            let tags = decode_compact_tags(&rec.tags_bytes, &plan.field_names)?;
            let tl = match layers.iter_mut().position(|t| t.name == plan.target_name) {
                Some(i) => &mut layers[i],
                None => {
                    layers.push(MvtLayer { name: plan.target_name.clone(), extent: self.options.extent, features: Vec::new() });
                    layers.last_mut().expect("just pushed")
                }
            };
            tl.features.push(MvtFeature { id: None, geom_type: u32::from(rec.geom_type), geometry, tags, size: 0.0 });
        }
        Ok(layers)
    }

    /// Finalizes all spilled runs, encodes tiles in parallel batches, and writes the destination
    /// archive or MVT directory.
    pub(crate) fn finish(mut self) -> Result<()> {
        let (meta_json, bounds, min_zoom, max_zoom) = self.build_metadata()?;
        let mut stream = if self.spiller.temp_runs.paths.is_empty() {
            self.spiller.run_buf.sort_unstable_by_key(SpillRecord::key);
            RecordStream::Memory(std::mem::take(&mut self.spiller.run_buf).into_iter())
        } else {
            self.spiller.flush_run()?;
            self.spiller.compact_runs()?;
            RecordStream::Merged(KWayMerger::open(&self.spiller.temp_runs.paths)?)
        };

        let out_path = Path::new(&self.spiller.output);
        let name = match self.driver {
            VectorDriver::MVT => out_path.file_name(),
            _ => out_path.file_stem(),
        }
        .and_then(|s| s.to_str())
        .unwrap_or("tiles")
        .to_string();
        let md = metadata_items_parts(bounds, min_zoom, max_zoom, &self.options, &name);

        match self.driver {
            VectorDriver::PMTiles => {
                let mut spill = SpillSink::beside(&self.spiller.output)?;
                self.encode_stream(&mut stream, |z, x, y, gz| spill.put(z, x, y, gz))?;
                drop(stream);
                self.spiller.temp_runs.remove_and_clear();

                let mut json_map: serde_json::Map<String, serde_json::Value> =
                    md.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
                if let serde_json::Value::Object(o) = meta_json {
                    for (k, v) in o {
                        json_map.insert(k, v);
                    }
                }
                let params = pmtiles::ArchiveParams {
                    tile_type: pmtiles::TILE_MVT,
                    tile_compression: pmtiles::COMPRESSION_GZIP,
                    min_zoom,
                    max_zoom,
                    bounds,
                    center: ((bounds[0] + bounds[2]) / 2.0, (bounds[1] + bounds[3]) / 2.0, min_zoom),
                };
                let metadata_bytes = serde_json::Value::Object(json_map).to_string().into_bytes();
                let tiles = std::mem::take(&mut spill.tiles);
                let mut out = BufWriter::with_capacity(RUN_WRITE_BUF, File::create(&self.spiller.output)?);
                let written = (|| -> Result<()> {
                    pmtiles::write_archive_spilled(tiles, spill.file()?, &metadata_bytes, &params, &mut out)?;
                    out.flush()?;
                    Ok(())
                })();
                drop(out);
                if written.is_err() {
                    let _ = std::fs::remove_file(&self.spiller.output);
                }
                written
            }
            VectorDriver::MBTiles => {
                let mut tiles = Vec::new();
                self.encode_stream(&mut stream, |z, x, y, gz| {
                    tiles.push((z, x, y, gz));
                    Ok(())
                })?;
                drop(stream);
                self.spiller.temp_runs.remove_and_clear();

                let mut md = md;
                md.push(("scheme".into(), "tms".into()));
                md.push(("json".into(), serde_json::to_string_pretty(&meta_json).unwrap_or_default()));
                let bytes = mbtiles::write(&md, tiles)?;
                let mut out = File::create(&self.spiller.output)?;
                let written = out.write_all(&bytes);
                drop(out);
                if written.is_err() {
                    let _ = std::fs::remove_file(&self.spiller.output);
                }
                written?;
                Ok(())
            }
            VectorDriver::MVT => {
                let root = Path::new(&self.spiller.output);
                if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                if let Err(e) = std::fs::create_dir(root) {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        return Err(Error::InvalidArgument(format!("'{}' already exists", root.display())));
                    }
                    return Err(e.into());
                }
                let written = (|| -> Result<()> {
                    let mut md_map: serde_json::Map<String, serde_json::Value> =
                        md.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
                    md_map.insert("json".into(), serde_json::Value::String(serde_json::to_string_pretty(&meta_json).unwrap_or_default()));
                    std::fs::write(
                        root.join("metadata.json"),
                        serde_json::to_string_pretty(&serde_json::Value::Object(md_map)).unwrap_or_default(),
                    )?;
                    self.encode_stream(&mut stream, |z, x, y, gz| {
                        let dir = root.join(z.to_string()).join(x.to_string());
                        std::fs::create_dir_all(&dir)?;
                        std::fs::write(dir.join(format!("{y}.pbf")), gz)?;
                        Ok(())
                    })?;
                    Ok(())
                })();
                drop(stream);
                self.spiller.temp_runs.remove_and_clear();
                if written.is_err() {
                    let _ = std::fs::remove_dir_all(root);
                }
                written
            }
            _ => Err(Error::InvalidArgument(format!("'{}' is not a vector tile driver", self.driver.name()))),
        }
    }

    /// Consumes sorted records grouped by `tile_id`, encodes tiles in parallel batches, and hands
    /// each compressed `(z, x, y, gz_bytes)` tile to `emit`.
    fn encode_stream(&self, stream: &mut RecordStream, mut emit: impl FnMut(u8, u32, u32, Vec<u8>) -> Result<()>) -> Result<()> {
        let max_features = self.options.max_features;
        let mut pending_batch: Vec<(u64, Vec<MvtLayer>, bool)> = Vec::with_capacity(TILE_ENCODE_BATCH);
        let mut batch_bytes = 0usize;
        let mut cur_id: Option<u64> = None;
        let mut cur_recs: Vec<SpillRecord> = Vec::new();
        let mut cur_bytes = 0usize;
        let mut cur_truncated = false;

        let flush_batch =
            |batch: &mut Vec<(u64, Vec<MvtLayer>, bool)>, emit: &mut dyn FnMut(u8, u32, u32, Vec<u8>) -> Result<()>| -> Result<()> {
                if batch.is_empty() {
                    return Ok(());
                }
                let items = std::mem::take(batch);
                let o = &self.options;
                struct TileJob {
                    orig_idx: usize,
                    weight: usize,
                    input: Option<(u64, Vec<MvtLayer>, bool)>,
                    output: Option<Result<(u64, Vec<u8>)>>,
                }
                // Schedule largest tiles first across Rayon threads to avoid straggler tail latency,
                // then emit finished tiles in strict original Hilbert `tile_id` order.
                let mut work: Vec<TileJob> = items
                    .into_iter()
                    .enumerate()
                    .map(|(orig_idx, item)| {
                        let weight: usize = item.1.iter().map(|l| l.features.len()).sum();
                        TileJob { orig_idx, weight, input: Some(item), output: None }
                    })
                    .collect();
                work.sort_unstable_by_key(|job| std::cmp::Reverse(job.weight));
                crate::par::chunks_mut(&mut work, 1, |_chunk_idx, slice| {
                    for job in slice.iter_mut() {
                        let (tile_id, layers, pre_truncated) = job.input.take().expect("owned once");
                        job.output = Some(encode_limited_gz(layers, o, pre_truncated).map(|gz| (tile_id, gz)));
                    }
                });
                work.sort_unstable_by_key(|job| job.orig_idx);
                for job in work {
                    let (tile_id, gz) = job.output.expect("encoded")?;
                    let (z, x, y) = pmtiles::tile_id_to_zxy(tile_id);
                    emit(z, x, y, gz)?;
                }
                Ok(())
            };

        while let Some(rec) = stream.next_record()? {
            let rec_bytes = rec.approx_bytes();
            match cur_id {
                Some(id) if id == rec.tile_id => {
                    if cur_recs.len() < max_features {
                        cur_bytes += rec_bytes;
                        cur_recs.push(rec);
                    } else {
                        cur_truncated = true;
                    }
                }
                Some(prev_id) => {
                    let layers = self.build_tile_layers(std::mem::take(&mut cur_recs))?;
                    pending_batch.push((prev_id, layers, cur_truncated));
                    batch_bytes += cur_bytes;
                    cur_id = Some(rec.tile_id);
                    if max_features > 0 {
                        cur_bytes = rec_bytes;
                        cur_recs.push(rec);
                        cur_truncated = false;
                    } else {
                        cur_bytes = 0;
                        cur_truncated = true;
                    }
                    if pending_batch.len() >= TILE_ENCODE_BATCH || batch_bytes >= TILE_ENCODE_BATCH_BYTES {
                        flush_batch(&mut pending_batch, &mut emit)?;
                        batch_bytes = 0;
                    }
                }
                None => {
                    cur_id = Some(rec.tile_id);
                    if max_features > 0 {
                        cur_bytes = rec_bytes;
                        cur_recs.push(rec);
                        cur_truncated = false;
                    } else {
                        cur_bytes = 0;
                        cur_truncated = true;
                    }
                }
            }
        }
        if let Some(last_id) = cur_id {
            let layers = self.build_tile_layers(cur_recs)?;
            pending_batch.push((last_id, layers, cur_truncated));
        }
        flush_batch(&mut pending_batch, &mut emit)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{ArchiveKind, TileArchive, vector};
    use wbvector::{Coord, FieldDef, FieldType, Geometry, Layer};

    fn sample_layers() -> Vec<Layer> {
        let mut pts = Layer::new("places").with_epsg(4326);
        pts.add_field(FieldDef::new("name", FieldType::Text));
        pts.add_field(FieldDef::new("pop", FieldType::Integer));
        pts.add_field(FieldDef::new("score", FieldType::Float));
        for i in 0..80 {
            let lon = -100.0 + (i % 10) as f64 * 0.4;
            let lat = 44.0 + (i / 10) as f64 * 0.3;
            pts.add_feature(
                Some(Geometry::point(lon, lat)),
                &[("name", format!("p{i}").into()), ("pop", (i as i64 - 20).into()), ("score", (i as f64 * 0.25).into())],
            )
            .unwrap();
        }
        let mut polys = Layer::new("basins").with_epsg(4326);
        polys.add_field(FieldDef::new("id", FieldType::Integer));
        polys.add_field(FieldDef::new("wet", FieldType::Boolean));
        for i in 0..30 {
            let x0 = -99.5 + (i % 6) as f64 * 0.5;
            let y0 = 44.5 + (i / 6) as f64 * 0.4;
            polys
                .add_feature(
                    Some(Geometry::polygon(
                        vec![Coord::xy(x0, y0), Coord::xy(x0 + 0.2, y0), Coord::xy(x0 + 0.2, y0 + 0.2), Coord::xy(x0, y0 + 0.2)],
                        vec![],
                    )),
                    &[("id", (i as i64).into()), ("wet", (i % 2 == 0).into())],
                )
                .unwrap();
        }
        vec![pts, polys]
    }

    #[test]
    fn streamed_spilled_pmtiles_matches_in_memory_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let layers = sample_layers();
        let o = TileWriteOptions {
            min_zoom: Some(3),
            max_zoom: Some(6),
            simplification: 1.0,
            simplification_max_zoom: Some(0.25),
            name: Some("ppr_test".into()),
            description: Some("multi-run spill test".into()),
            ..Default::default()
        };
        let vt = vector::make_tiles(&layers, &o).unwrap();
        let expected = super::super::write_vector_archive(&vt, &o, "streamed", ArchiveKind::PMTiles).unwrap();

        // Force tiny run budget (1 KiB) and small merge fan-in (3) to exercise multi-run + multi-pass K-way merge!
        let out_path = dir.path().join("streamed.pmtiles");
        let out_str = out_path.to_str().unwrap();
        let schemas: Vec<Layer> = layers.iter().map(crate::stream::schema_of).collect();
        let mut writer = VectorTileStreamWriter::create(out_str, VectorDriver::PMTiles, &schemas, &o).unwrap().with_spill_limits(1024, 3);
        for (li, layer) in layers.iter().enumerate() {
            for chunk_feats in layer.features.chunks(7) {
                let mut chunk = crate::stream::schema_of(layer);
                chunk.features = chunk_feats.to_vec();
                writer.write_chunk(li, chunk).unwrap();
            }
        }
        writer.finish().unwrap();

        let got = std::fs::read(&out_path).unwrap();
        assert_eq!(got, expected, "streamed spilled PMTiles must match in-memory PMTiles byte for byte");
        // Verify zero leftover temp files in directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("terrio-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn per_layer_conf_controls_zoom_ranges_and_names() {
        let dir = tempfile::tempdir().unwrap();
        let layers = sample_layers();
        let conf_path = dir.path().join("conf.json");
        std::fs::write(
            &conf_path,
            r#"{
                "places": { "target_name": "sites", "minzoom": 3, "maxzoom": 5, "description": "Points of interest" },
                "basins": { "target_name": "depressions", "minzoom": 5, "maxzoom": 6, "description": "Surface basins" }
            }"#,
        )
        .unwrap();
        let o = TileWriteOptions {
            min_zoom: Some(3),
            max_zoom: Some(6),
            conf: Some(conf_path.to_str().unwrap().to_string()),
            ..Default::default()
        };

        let out_path = dir.path().join("conf.pmtiles");
        let out_str = out_path.to_str().unwrap();
        let schemas: Vec<Layer> = layers.iter().map(crate::stream::schema_of).collect();
        let mut writer = VectorTileStreamWriter::create(out_str, VectorDriver::PMTiles, &schemas, &o).unwrap().with_spill_limits(2048, 4);
        for (li, layer) in layers.iter().enumerate() {
            writer.write_chunk(li, layer.clone()).unwrap();
        }
        writer.finish().unwrap();

        let mut archive = TileArchive::open_named(out_str).unwrap();
        let meta = archive.json();
        let vls = meta["vector_layers"].as_array().unwrap();
        assert_eq!(vls.len(), 2);
        assert_eq!(vls[0]["id"], "sites");
        assert_eq!(vls[0]["minzoom"], 3);
        assert_eq!(vls[0]["maxzoom"], 5);
        assert_eq!(vls[0]["description"], "Points of interest");
        assert_eq!(vls[1]["id"], "depressions");
        assert_eq!(vls[1]["minzoom"], 5);
        assert_eq!(vls[1]["maxzoom"], 6);

        // At zoom 3, only "sites" must exist ("depressions" starts at z5).
        let z3_layers =
            vector::read_layers(&mut archive, &crate::tiles::TileOpenOptions { zoom_level: Some(3), clip: Some(true) }, &[], None).unwrap();
        let sites_z3 = z3_layers.iter().find(|l| l.name == "sites").unwrap();
        let deps_z3 = z3_layers.iter().find(|l| l.name == "depressions").unwrap();
        assert!(!sites_z3.features.is_empty());
        assert!(deps_z3.features.is_empty(), "depressions must not appear below minzoom=5");

        // At zoom 6, only "depressions" must exist ("sites" ends at z5).
        let z6_layers =
            vector::read_layers(&mut archive, &crate::tiles::TileOpenOptions { zoom_level: Some(6), clip: Some(true) }, &[], None).unwrap();
        let sites_z6 = z6_layers.iter().find(|l| l.name == "sites").unwrap();
        let deps_z6 = z6_layers.iter().find(|l| l.name == "depressions").unwrap();
        assert!(sites_z6.features.is_empty(), "sites must not appear above maxzoom=5");
        assert!(!deps_z6.features.is_empty());

        // Byte-for-byte parity with in-memory CONF path as well.
        let vt = vector::make_tiles(&layers, &o).unwrap();
        let expected = super::super::write_vector_archive(&vt, &o, "conf", ArchiveKind::PMTiles).unwrap();
        assert_eq!(std::fs::read(&out_path).unwrap(), expected);
    }

    #[test]
    fn streamed_size_limited_tile_matches_in_memory_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let layers = sample_layers();
        // Tight MAX_SIZE forces extent reduction and lazy geometry_size feature dropping.
        let o = TileWriteOptions { min_zoom: Some(3), max_zoom: Some(4), max_size: 400, max_features: 50, ..Default::default() };
        let vt = vector::make_tiles(&layers, &o).unwrap();
        let expected = super::super::write_vector_archive(&vt, &o, "capped", ArchiveKind::PMTiles).unwrap();

        let out_path = dir.path().join("capped.pmtiles");
        let out_str = out_path.to_str().unwrap();
        let schemas: Vec<Layer> = layers.iter().map(crate::stream::schema_of).collect();
        let mut writer = VectorTileStreamWriter::create(out_str, VectorDriver::PMTiles, &schemas, &o).unwrap().with_spill_limits(512, 3);
        for (li, layer) in layers.iter().enumerate() {
            writer.write_chunk(li, layer.clone()).unwrap();
        }
        writer.finish().unwrap();
        assert_eq!(std::fs::read(&out_path).unwrap(), expected);
    }

    #[test]
    fn stream_convert_multi_layer_gpkg_to_pmtiles_with_conf() {
        let dir = tempfile::tempdir().unwrap();
        let layers = sample_layers();
        let gpkg_path = dir.path().join("input.gpkg");
        let gpkg_str = gpkg_path.to_str().unwrap();
        crate::write_vector(&layers, gpkg_str, &Default::default()).unwrap();

        let pm_path = dir.path().join("output.pmtiles");
        let pm_str = pm_path.to_str().unwrap();
        let mut opts = crate::stream::ConvertOptions::default();
        opts.write.driver = Some(VectorDriver::PMTiles);
        opts.write.tiles.min_zoom = Some(3);
        opts.write.tiles.max_zoom = Some(5);
        opts.write.tiles.conf = Some(r#"{"basins":{"minzoom":5,"maxzoom":5,"target_name":"depressions"}}"#.into());
        opts.chunk_size = 11;
        assert!(crate::stream::can_convert_stream(gpkg_str, pm_str, &opts));
        crate::stream::convert(gpkg_str, pm_str, &opts).unwrap();

        let mut archive = TileArchive::open_named(pm_str).unwrap();
        let meta = archive.json();
        let vls = meta["vector_layers"].as_array().unwrap();
        assert_eq!(vls.len(), 2);
        assert_eq!(vls[0]["id"], "places");
        assert_eq!(vls[0]["minzoom"], 3);
        assert_eq!(vls[1]["id"], "depressions");
        assert_eq!(vls[1]["minzoom"], 5);
        let z5 =
            vector::read_layers(&mut archive, &crate::tiles::TileOpenOptions { zoom_level: Some(5), clip: Some(true) }, &[], None).unwrap();
        assert_eq!(z5.len(), 2);
        assert_eq!(z5[0].features.len(), 80);
        assert_eq!(z5[1].features.len(), 30);
    }

    #[test]
    fn mvt_dir_name_atomic_collision_and_missing_parent_spill() {
        let dir = tempfile::tempdir().unwrap();
        let layers = sample_layers();
        let o = TileWriteOptions { min_zoom: Some(3), max_zoom: Some(4), ..Default::default() };

        // Missing parent directories must fall back to temp_dir for spill runs and create parents on finish.
        let mvt_dir = dir.path().join("nested").join("roads.mvt");
        let mvt_str = mvt_dir.to_str().unwrap();
        let schemas: Vec<Layer> = layers.iter().map(crate::stream::schema_of).collect();
        let mut writer = VectorTileStreamWriter::create(mvt_str, VectorDriver::MVT, &schemas, &o).unwrap().with_spill_limits(512, 3);
        for (li, layer) in layers.iter().enumerate() {
            writer.write_chunk(li, layer.clone()).unwrap();
        }
        writer.finish().unwrap();

        let md_text = std::fs::read_to_string(mvt_dir.join("metadata.json")).unwrap();
        let md: serde_json::Value = serde_json::from_str(&md_text).unwrap();
        assert_eq!(md["name"], "roads.mvt", "MVT directory metadata name must keep full file_name");

        // Writing to an existing MVT directory must fail without deleting it.
        let sentinel = mvt_dir.join("keep.txt");
        std::fs::write(&sentinel, b"precious").unwrap();
        let err = VectorTileStreamWriter::create(mvt_str, VectorDriver::MVT, &schemas, &o).err().unwrap();
        assert!(matches!(err, Error::InvalidArgument(_)));
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"precious");
    }

    #[test]
    fn merged_target_name_deduplicates_metadata_and_matches_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let layers = sample_layers();
        let o = TileWriteOptions {
            min_zoom: Some(3),
            max_zoom: Some(5),
            conf: Some(
                r#"{
                    "places": { "target_name": "combined", "minzoom": 3, "maxzoom": 4, "description": "Merged layer" },
                    "basins": { "target_name": "combined", "minzoom": 4, "maxzoom": 5 }
                }"#
                .into(),
            ),
            ..Default::default()
        };
        let vt = vector::make_tiles(&layers, &o).unwrap();
        let expected = super::super::write_vector_archive(&vt, &o, "merged", ArchiveKind::PMTiles).unwrap();

        let out_path = dir.path().join("merged.pmtiles");
        let out_str = out_path.to_str().unwrap();
        let schemas: Vec<Layer> = layers.iter().map(crate::stream::schema_of).collect();
        let mut writer = VectorTileStreamWriter::create(out_str, VectorDriver::PMTiles, &schemas, &o).unwrap().with_spill_limits(1024, 3);
        for (li, layer) in layers.iter().enumerate() {
            writer.write_chunk(li, layer.clone()).unwrap();
        }
        writer.finish().unwrap();

        assert_eq!(std::fs::read(&out_path).unwrap(), expected);
        let archive = TileArchive::open_named(out_str).unwrap();
        let meta = archive.json();
        let vls = meta["vector_layers"].as_array().unwrap();
        let sls = meta["tilestats"]["layers"].as_array().unwrap();
        assert_eq!(vls.len(), 1, "shared target_name must emit one vector_layers entry");
        assert_eq!(sls.len(), 1, "shared target_name must emit one tilestats entry");
        assert_eq!(vls[0]["id"], "combined");
        assert_eq!(vls[0]["minzoom"], 3);
        assert_eq!(vls[0]["maxzoom"], 5);
        assert_eq!(sls[0]["count"], 110);
    }

    #[test]
    fn max_size_and_max_features_strict_enforcement() {
        let dir = tempfile::tempdir().unwrap();
        let layers = sample_layers();
        // Force both tight MAX_SIZE (220 bytes gzipped) and tight MAX_FEATURES (25).
        let o = TileWriteOptions { min_zoom: Some(2), max_zoom: Some(3), max_size: 220, max_features: 25, ..Default::default() };
        let vt = vector::make_tiles(&layers, &o).unwrap();
        let expected = super::super::write_vector_archive(&vt, &o, "strict", ArchiveKind::PMTiles).unwrap();

        let out_path = dir.path().join("strict.pmtiles");
        let out_str = out_path.to_str().unwrap();
        let schemas: Vec<Layer> = layers.iter().map(crate::stream::schema_of).collect();
        let mut writer = VectorTileStreamWriter::create(out_str, VectorDriver::PMTiles, &schemas, &o).unwrap().with_spill_limits(512, 3);
        for (li, layer) in layers.iter().enumerate() {
            writer.write_chunk(li, layer.clone()).unwrap();
        }
        writer.finish().unwrap();

        assert_eq!(std::fs::read(&out_path).unwrap(), expected);
        // Verify every gzipped tile in the archive respects max_size strictly.
        for (_, _, _, raw) in &vt.tiles {
            let gz = pmtiles::compress(raw, pmtiles::COMPRESSION_GZIP).unwrap();
            assert!(gz.len() <= o.max_size, "compressed tile {} exceeded max_size {}", gz.len(), o.max_size);
        }
    }

    #[test]
    fn failed_stream_conversion_preserves_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        // Empty geometry layer fails in finish() with InvalidArgument before creating dst.
        let mut empty = Layer::new("empty").with_epsg(4326);
        empty.add_field(FieldDef::new("id", FieldType::Integer));
        empty.add_feature(None, &[("id", 1i64.into())]).unwrap();
        let src_path = dir.path().join("empty.gpkg");
        let src_str = src_path.to_str().unwrap();
        crate::write_vector(&[empty], src_str, &Default::default()).unwrap();

        let dst_path = dir.path().join("existing.pmtiles");
        let dst_str = dst_path.to_str().unwrap();
        std::fs::write(&dst_path, b"existing-archive-bytes").unwrap();

        let mut opts = crate::stream::ConvertOptions::default();
        opts.write.driver = Some(VectorDriver::PMTiles);
        let res = crate::stream::convert(src_str, dst_str, &opts);
        assert!(res.is_err());
        assert_eq!(std::fs::read(&dst_path).unwrap(), b"existing-archive-bytes");
    }

    #[test]
    fn raster_only_gpkg_is_not_streamed_as_vector() {
        let dir = tempfile::tempdir().unwrap();
        let gpkg_path = dir.path().join("raster_only.gpkg");
        let gpkg_str = gpkg_path.to_str().unwrap();
        let cfg = wbraster::RasterConfig {
            cols: 4,
            rows: 4,
            bands: 1,
            nodata: -1.0,
            data_type: crate::DataType::U8,
            crs: crate::crs::SpatialRef::from_epsg(4326).unwrap().to_crs_info(),
            ..Default::default()
        };
        let mut raster = crate::Raster::new(cfg);
        raster.x_min = 0.0;
        raster.y_min = 0.0;
        raster.cell_size_x = 1.0;
        raster.cell_size_y = 1.0;
        crate::write_raster(&raster, gpkg_str, &Default::default()).unwrap();

        let pm_path = dir.path().join("out.pmtiles");
        let mut opts = crate::stream::ConvertOptions::default();
        opts.write.driver = Some(VectorDriver::PMTiles);
        assert!(!crate::stream::can_convert_stream(gpkg_str, pm_path.to_str().unwrap(), &opts));
        // A remote one too, read in place to tell.
        #[cfg(remote)]
        {
            let (base, _) = crate::http::test_server(std::fs::read(&gpkg_path).unwrap());
            assert!(!crate::stream::can_convert_stream(&format!("{base}/raster_only.gpkg"), pm_path.to_str().unwrap(), &opts));
        }
    }

    #[test]
    fn polygon_with_hole_outside_exterior_matches_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut layer = Layer::new("invalid").with_epsg(4326);
        layer.add_field(FieldDef::new("name", FieldType::Text));
        // Hole ring lies outside the exterior ring and crosses a zoom-3 tile boundary.
        let poly = Geometry::polygon(
            vec![Coord::xy(10.0, 10.0), Coord::xy(20.0, 10.0), Coord::xy(20.0, 20.0), Coord::xy(10.0, 20.0)],
            vec![vec![Coord::xy(30.0, 30.0), Coord::xy(50.0, 30.0), Coord::xy(50.0, 50.0), Coord::xy(30.0, 50.0)]],
        );
        layer.add_feature(Some(poly), &[("name", "hole_out".into())]).unwrap();

        let o = TileWriteOptions { min_zoom: Some(0), max_zoom: Some(3), ..Default::default() };
        let vt = vector::make_tiles(std::slice::from_ref(&layer), &o).unwrap();
        let expected = super::super::write_vector_archive(&vt, &o, "hole_out", ArchiveKind::PMTiles).unwrap();

        let out_path = dir.path().join("hole_out.pmtiles");
        let out_str = out_path.to_str().unwrap();
        let schema = crate::stream::schema_of(&layer);
        let mut writer = VectorTileStreamWriter::create(out_str, VectorDriver::PMTiles, &[schema], &o).unwrap();
        writer.write_chunk(0, layer).unwrap();
        writer.finish().unwrap();

        assert_eq!(std::fs::read(&out_path).unwrap(), expected);
    }
}
