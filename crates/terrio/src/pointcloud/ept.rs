// Written to match the behavior of:
//   PDAL 2.10.2:
//     io/EptReader.cpp
// No code is translated from these sources.

//! EPT (Entwine Point Tile, <https://entwine.io/entwine-point-tile.html>): a point cloud split
//! into an octree of tiles, each a LASzip, binary or Zstandard-compressed binary file, with the
//! octree's hierarchy in JSON pages; local, or remote (HTTP, S3) and read in place.
//!
//! As PDAL's `readers.ept`: a read walks the hierarchy from the root, through the nodes that
//! overlap the bounds asked for and above the depth the resolution asked for needs (the depth at
//! which the cube's width divided by the span, halved at each level, is no larger than the
//! resolution), and keeps the points inside the bounds. The dimensions are the schema's, named as
//! terrio names them (`X` is `x`, `ReturnNumber` `return_number`, `OriginId` `origin_id`).

use super::crs::LasCrs;
use super::las::LasHeader;
use super::read::{FormatReader, apply_options, made_up_header};
use super::values::{Dimension, ValueType, Values};
use super::{PointFilter, PointReadOptions};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

/// A dimension of an EPT schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EptDimension {
    /// PDAL's name (`X`, `Intensity`, `OriginId`...).
    pub name: String,
    /// `signed`, `unsigned` or `float`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The size in bytes.
    pub size: usize,
    /// The scale of values stored as integers.
    pub scale: Option<f64>,
    /// The offset of values stored as integers.
    pub offset: Option<f64>,
}

impl EptDimension {
    /// The type of the values read: float64 for scaled values and coordinates.
    pub(crate) fn value_type(&self) -> Result<ValueType> {
        if self.scale.is_some() || matches!(self.name.as_str(), "X" | "Y" | "Z") {
            return Ok(ValueType::F64);
        }
        self.stored_type()
    }

    /// The type of the values stored.
    pub(crate) fn stored_type(&self) -> Result<ValueType> {
        Ok(match (self.kind.as_str(), self.size) {
            ("signed", 1) => ValueType::I8,
            ("signed", 2) => ValueType::I16,
            ("signed", 4) => ValueType::I32,
            ("signed", 8) => ValueType::I64,
            ("unsigned", 1) => ValueType::U8,
            ("unsigned", 2) => ValueType::U16,
            ("unsigned", 4) => ValueType::U32,
            ("unsigned", 8) => ValueType::U64,
            ("float", 4) => ValueType::F32,
            ("float", 8) => ValueType::F64,
            (k, s) => return Err(Error::InvalidData(format!("EPT: a dimension of type {k} and size {s}"))),
        })
    }
}

/// The metadata of an EPT dataset (`ept.json`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EptInfo {
    /// The octree's cube: `[min_x, min_y, min_z, max_x, max_y, max_z]`.
    pub bounds: [f64; 6],
    /// The bounds of the points.
    pub bounds_conforming: [f64; 6],
    /// `laszip`, `binary` or `zstandard`.
    pub data_type: String,
    /// The number of points.
    pub points: u64,
    /// The number of voxels per side of each node's grid: the root's resolution is the cube's
    /// width divided by it.
    pub span: u64,
    /// The dimensions.
    pub schema: Vec<EptDimension>,
    /// The EPT version.
    pub version: String,
}

/// How the points of an EPT dataset's nodes are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EptDataType {
    /// LASzip-compressed LAS files (`.laz`), Entwine's default.
    #[default]
    Laszip,
    /// Binary records, little endian, in the schema's order (`.bin`).
    Binary,
    /// Binary records compressed with Zstandard (`.zst`).
    Zstandard,
}

impl EptDataType {
    /// Parses `laszip`, `binary` or `zstandard` (EPT's `dataType`).
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "laszip" | "laz" => Ok(EptDataType::Laszip),
            "binary" | "bin" => Ok(EptDataType::Binary),
            "zstandard" | "zstd" | "zst" => Ok(EptDataType::Zstandard),
            o => Err(Error::InvalidArgument(format!("unknown EPT data type '{o}' (laszip, binary, zstandard)"))),
        }
    }

    /// EPT's name of the data type.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn name(self) -> &'static str {
        match self {
            EptDataType::Laszip => "laszip",
            EptDataType::Binary => "binary",
            EptDataType::Zstandard => "zstandard",
        }
    }

    /// The extension of the node files.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn extension(self) -> &'static str {
        match self {
            EptDataType::Laszip => "laz",
            EptDataType::Binary => "bin",
            EptDataType::Zstandard => "zst",
        }
    }
}

/// The classification flags, with their bits in LAS 1.4's flags byte.
const FLAGS: [(&str, u8); 4] = [("withheld", 2), ("overlap", 3), ("synthetic", 0), ("key_point", 1)];

/// The key of a node: its depth and position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Key {
    d: u32,
    x: u64,
    y: u64,
    z: u64,
}

impl Key {
    fn name(&self) -> String {
        format!("{}-{}-{}-{}", self.d, self.x, self.y, self.z)
    }

    /// A node's bounds within the cube.
    fn bounds(&self, cube: &[f64; 6]) -> [f64; 6] {
        let n = (1u64 << self.d.min(62)) as f64;
        let w = [(cube[3] - cube[0]) / n, (cube[4] - cube[1]) / n, (cube[5] - cube[2]) / n];
        let lo = [cube[0] + self.x as f64 * w[0], cube[1] + self.y as f64 * w[1], cube[2] + self.z as f64 * w[2]];
        [lo[0], lo[1], lo[2], lo[0] + w[0], lo[1] + w[1], lo[2] + w[2]]
    }

    fn children(self) -> impl Iterator<Item = Key> {
        (0..8u64).map(move |i| Key { d: self.d + 1, x: 2 * self.x + (i & 1), y: 2 * self.y + ((i >> 1) & 1), z: 2 * self.z + (i >> 2) })
    }
}

/// An EPT dataset.
pub(crate) struct EptReader {
    /// The dataset's directory or URL, ending with `/`.
    base: String,
    info: EptInfo,
    header: LasHeader,
    crs: LasCrs,
    /// The schema's dimensions, as stored.
    schema_dims: Vec<(String, ValueType)>,
    /// The dimensions read.
    dims: Vec<(String, ValueType)>,
    /// The hierarchy pages read, by the key of their root.
    pages: HashMap<String, HashMap<String, i64>>,
    cursor: Option<EptCursor>,
    /// Catalog provenance retained across deferred hierarchy and node reads.
    asset_policy: Option<crate::stac::AssetReadPolicy>,
}

struct EptCursor {
    nodes: Vec<(Key, u64)>,
    next: usize,
    filter: PointFilter,
    chunk: usize,
}

/// Reads a file of the dataset.
fn fetch(base: &str, rel: &str) -> Result<Vec<u8>> {
    let path = format!("{base}{rel}");
    let location = crate::vfs::Location::parse(&path);
    #[cfg(remote)]
    if let Some(remote) = location.remote() {
        return crate::http::get(&remote);
    }
    match location {
        crate::vfs::Location::Local(p) => std::fs::read(&p).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}")))),
        _ => Err(Error::Unsupported(format!("cannot read EPT data from '{path}'"))),
    }
}

fn bounds6(v: &Value, what: &str) -> Result<[f64; 6]> {
    let a = v.as_array().filter(|a| a.len() == 6).ok_or_else(|| Error::InvalidData(format!("EPT: no {what}")))?;
    let mut b = [0.0; 6];
    for (o, x) in b.iter_mut().zip(a) {
        *o = x.as_f64().ok_or_else(|| Error::InvalidData(format!("EPT: bad {what}")))?;
    }
    Ok(b)
}

impl EptReader {
    /// Opens a dataset: its `ept.json`, or an `ept://` path or URL of its directory.
    pub fn open(path: &str) -> Result<Self> {
        super::read::check_driver("EPT", path)?;
        let mut base = path.strip_prefix("ept://").unwrap_or(path).to_string();
        if base.ends_with("ept.json") {
            base.truncate(base.len() - "ept.json".len());
        } else if !base.ends_with('/') {
            base.push('/');
        }
        let json: Value = serde_json::from_slice(&fetch(&base, "ept.json")?).map_err(|e| Error::InvalidData(format!("ept.json: {e}")))?;
        let schema: Vec<EptDimension> = json["schema"]
            .as_array()
            .ok_or_else(|| Error::InvalidData("EPT: no schema".into()))?
            .iter()
            .map(|d| EptDimension {
                name: d["name"].as_str().unwrap_or_default().to_string(),
                kind: d["type"].as_str().unwrap_or_default().to_string(),
                size: d["size"].as_u64().unwrap_or(0) as usize,
                scale: d["scale"].as_f64(),
                offset: d["offset"].as_f64().or_else(|| d["scale"].as_f64().map(|_| 0.0)),
            })
            .collect();
        let hierarchy = json["hierarchyType"].as_str().unwrap_or("json");
        if hierarchy != "json" {
            return Err(Error::Unsupported(format!("EPT hierarchy type '{hierarchy}'")));
        }
        let info = EptInfo {
            bounds: bounds6(&json["bounds"], "bounds")?,
            bounds_conforming: bounds6(&json["boundsConforming"], "boundsConforming").or_else(|_| bounds6(&json["bounds"], "bounds"))?,
            data_type: json["dataType"].as_str().unwrap_or("laszip").to_string(),
            points: json["points"].as_u64().unwrap_or(0),
            span: json["span"].as_u64().or_else(|| json["ticks"].as_u64()).unwrap_or(128),
            schema,
            version: json["version"].as_str().unwrap_or("1.0.0").to_string(),
        };
        if !matches!(info.data_type.as_str(), "laszip" | "binary" | "zstandard") {
            return Err(Error::Unsupported(format!("EPT data type '{}'", info.data_type)));
        }
        let srs = &json["srs"];
        let horizontal = match (srs["wkt"].as_str(), srs["authority"].as_str(), srs["horizontal"].as_str()) {
            (Some(wkt), ..) if !wkt.is_empty() => SpatialRef::parse(wkt).unwrap_or_default(),
            (_, Some(auth), Some(code)) => SpatialRef::parse(&format!("{auth}:{code}")).unwrap_or_default(),
            _ => SpatialRef::default(),
        };
        let vertical_epsg = srs["vertical"].as_str().and_then(|v| v.parse().ok());
        let crs = LasCrs { horizontal, vertical_epsg, vertical_name: None };
        let schema_dims: Vec<(String, ValueType)> =
            info.schema.iter().map(|d| Ok((super::names::from_pdal(&d.name), d.value_type()?))).collect::<Result<_>>()?;
        // As PDAL: `ClassFlags` is read as the four flags it packs, which LASzip tiles have too.
        let mut dims: Vec<(String, ValueType)> = schema_dims.iter().filter(|(n, _)| n != "class_flags").cloned().collect();
        if info.data_type == "laszip" || schema_dims.iter().any(|(n, _)| n == "class_flags") {
            for flag in FLAGS {
                if !dims.iter().any(|(d, _)| d == flag.0) {
                    dims.push((flag.0.to_string(), ValueType::U8));
                }
            }
        }
        let scale_of = |n: &str| info.schema.iter().find(|d| d.name == n).and_then(|d| d.scale.map(|s| (s, d.offset.unwrap_or(0.0))));
        let scale_offset = match (scale_of("X"), scale_of("Y"), scale_of("Z")) {
            (Some(x), Some(y), Some(z)) => Some(([x.0, y.0, z.0], [x.1, y.1, z.1])),
            _ => None,
        };
        let header = made_up_header(&dims, info.points, Some(info.bounds_conforming), &crs.horizontal, scale_offset);
        Ok(Self {
            base,
            info,
            header,
            crs,
            schema_dims,
            dims,
            pages: HashMap::new(),
            cursor: None,
            asset_policy: crate::stac::AssetReadPolicy::current(),
        })
    }

    /// The hierarchy page whose root is `key`.
    fn page(&mut self, key: &str) -> Result<&HashMap<String, i64>> {
        if !self.pages.contains_key(key) {
            let bytes = fetch(&self.base, &format!("ept-hierarchy/{key}.json"))?;
            let v: HashMap<String, i64> =
                serde_json::from_slice(&bytes).map_err(|e| Error::InvalidData(format!("EPT hierarchy page {key}: {e}")))?;
            self.pages.insert(key.to_string(), v);
        }
        Ok(&self.pages[key])
    }

    /// The depth past the last one read for a resolution, as PDAL's `readers.ept` computes it
    /// (0: all).
    fn depth_end(&self, resolution: Option<f64>) -> Result<u32> {
        let Some(r) = resolution else { return Ok(0) };
        if !(r > 0.0 && r.is_finite()) {
            return Err(Error::InvalidArgument(format!("invalid resolution {r}")));
        }
        let mut current = (self.info.bounds[3] - self.info.bounds[0]) / self.info.span as f64;
        let mut end = 1;
        while current > r {
            current /= 2.0;
            end += 1;
        }
        Ok(end)
    }

    /// The nodes (with their point counts) that overlap the bounds, above `depth_end`.
    fn select(&mut self, options: &PointReadOptions) -> Result<Vec<(Key, u64)>> {
        let f = &options.filter;
        let query = match (f.bounds, f.z_range) {
            (None, None) => None,
            (b, z) => {
                let [x0, y0, x1, y1] = b.unwrap_or([f64::MIN, f64::MIN, f64::MAX, f64::MAX]);
                let (z0, z1) = z.unwrap_or((f64::MIN, f64::MAX));
                Some([x0, y0, z0, x1, y1, z1])
            }
        };
        let mut depth_end = self.depth_end(options.resolution)?;
        if let Some(l) = options.max_level {
            let l = l.saturating_add(1);
            depth_end = if depth_end == 0 { l } else { depth_end.min(l) };
        }
        let cube = self.info.bounds;
        let mut out = Vec::new();
        // (key, the page it is in)
        let mut stack = vec![(Key { d: 0, x: 0, y: 0, z: 0 }, "0-0-0-0".to_string())];
        while let Some((key, page)) = stack.pop() {
            let name = key.name();
            let Some(count) = self.page(&page)?.get(&name).copied() else { continue };
            if let Some(q) = query {
                let b = key.bounds(&cube);
                if !(b[0] <= q[3] && b[3] >= q[0] && b[1] <= q[4] && b[4] >= q[1] && b[2] <= q[5] && b[5] >= q[2]) {
                    continue;
                }
            }
            if depth_end > 0 && key.d >= depth_end {
                continue;
            }
            if count == -1 {
                // The node and its descendants are in their own page.
                if page != name {
                    stack.push((key, name));
                }
                continue;
            }
            if count < 0 {
                return Err(Error::InvalidData(format!("EPT: invalid point count for node {name}")));
            }
            out.push((key, count as u64));
            for c in key.children() {
                stack.push((c, page.clone()));
            }
        }
        if let Some(limit) = options.max_points {
            let deepest = super::copc::deepest_level_within(out.iter().map(|(k, n)| (k.d as usize, *n)), limit);
            out.retain(|(k, _)| k.d as i32 <= deepest);
        }
        out.sort();
        Ok(out)
    }

    /// Decodes a node's points into the schema's dimensions.
    fn decode(&self, key: Key, points: u64, bytes: Vec<u8>) -> Result<Vec<Dimension>> {
        let schema = &self.info.schema;
        match self.info.data_type.as_str() {
            "laszip" => {
                let cloud = super::read::open_points_bytes(bytes, &format!("{}.laz", key.name()))?.read(&Default::default())?;
                let n = cloud.len();
                self.dims
                    .iter()
                    .map(|(name, t)| {
                        let pdal_name = super::names::to_pdal(name);
                        let from = |src: &str| cloud.dim(src).or_else(|| cloud.dim(&pdal_name));
                        let values = match name.as_str() {
                            // LAS 1.4 stores scan angles in 0.006 degree units; PDAL's
                            // ScanAngleRank is in degrees.
                            "scan_angle_rank" => match (cloud.dim("scan_angle_rank"), cloud.dim("scan_angle")) {
                                (Some(d), _) => d.values.cast(*t),
                                (None, Some(d)) => {
                                    let mut v = Values::with_capacity(*t, n);
                                    for i in 0..n {
                                        v.push_f64(d.values.get_f64(i) * 0.006);
                                    }
                                    v
                                }
                                _ => Values::zeros(*t, n),
                            },
                            _ => match from(name) {
                                Some(d) if d.scale.is_some() || d.offset.is_some() => Values::F64(d.to_f64()).cast(*t),
                                Some(d) => d.values.cast(*t),
                                None => Values::zeros(*t, n),
                            },
                        };
                        Ok(Dimension::new(name, values))
                    })
                    .collect()
            }
            kind => {
                let rl: usize = schema.iter().map(|s| s.size).sum();
                if rl == 0 {
                    return Err(Error::InvalidData("EPT: an empty schema".into()));
                }
                let bytes = if kind == "zstandard" {
                    // Every frame, to at most the records of the node's points in the hierarchy.
                    let limit = usize::try_from(points).ok().and_then(|n| n.checked_mul(rl)).unwrap_or(usize::MAX);
                    crate::zstandard::decompress(&bytes, limit).map_err(|e| Error::InvalidData(format!("EPT node {}: {e}", key.name())))?
                } else {
                    bytes
                };
                let n = bytes.len() / rl;
                let mut cols: Vec<Values> = self.schema_dims.iter().map(|(_, t)| Values::with_capacity(*t, n)).collect();
                let stored: Vec<ValueType> = schema.iter().map(EptDimension::stored_type).collect::<Result<_>>()?;
                let mut tmp: Vec<Values> = stored.iter().map(|t| Values::with_capacity(*t, 1)).collect();
                for rec in bytes.chunks_exact(rl) {
                    let mut at = 0;
                    for (k, s) in schema.iter().enumerate() {
                        let raw = &rec[at..at + s.size];
                        at += s.size;
                        if cols[k].value_type() == stored[k] && s.scale.is_none() {
                            cols[k].push_le(raw);
                            continue;
                        }
                        let t = &mut tmp[k];
                        *t = Values::with_capacity(stored[k], 1);
                        t.push_le(raw);
                        let v = t.get_f64(0);
                        cols[k].push_f64(match s.scale {
                            Some(sc) => v * sc + s.offset.unwrap_or(0.0),
                            None => v,
                        });
                    }
                }
                let flags = self.schema_dims.iter().position(|(d, _)| d == "class_flags").map(|k| cols[k].clone());
                let mut by_name: HashMap<&str, Values> = self.schema_dims.iter().map(|(d, _)| d.as_str()).zip(cols).collect();
                Ok(self
                    .dims
                    .iter()
                    .map(|(name, t)| {
                        let values = match (by_name.remove(name.as_str()), FLAGS.iter().find(|f| f.0 == name), &flags) {
                            (Some(v), ..) => v,
                            (None, Some((_, bit)), Some(f)) => Values::U8((0..n).map(|i| ((f.get_f64(i) as u8) >> bit) & 1).collect()),
                            _ => Values::zeros(*t, n),
                        };
                        Dimension::new(name, values)
                    })
                    .collect())
            }
        }
    }

    /// The file of a node's points.
    fn data_file(&self, key: Key) -> String {
        let ext = match self.info.data_type.as_str() {
            "laszip" => "laz",
            "zstandard" => "zst",
            _ => "bin",
        };
        format!("ept-data/{}.{ext}", key.name())
    }
}

impl FormatReader for EptReader {
    fn header(&self) -> &LasHeader {
        &self.header
    }

    fn crs(&self) -> &LasCrs {
        &self.crs
    }

    fn dimensions(&self) -> Vec<(String, ValueType)> {
        self.dims.clone()
    }

    fn format_name(&self) -> &'static str {
        "EPT"
    }

    fn ept_info(&self) -> Option<&EptInfo> {
        Some(&self.info)
    }

    fn read(&mut self, options: &PointReadOptions) -> Result<Vec<Dimension>> {
        // All the nodes at once: fetched in parallel.
        self.start(options, usize::MAX)?;
        let mut parts = Vec::new();
        while let Some(c) = self.next_chunk()? {
            parts.push(c);
        }
        Ok(super::read::concat(parts, &self.dims))
    }

    fn start(&mut self, options: &PointReadOptions, chunk: usize) -> Result<()> {
        let _asset_scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let nodes = self.select(options)?;
        self.cursor = Some(EptCursor { nodes, next: 0, filter: options.filter.clone(), chunk });
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<Option<Vec<Dimension>>> {
        let _asset_scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let Some(mut c) = self.cursor.take() else { return Ok(None) };
        // Skip the nodes without points.
        while c.next < c.nodes.len() && c.nodes[c.next].1 == 0 {
            c.next += 1;
        }
        if c.next >= c.nodes.len() {
            return Ok(None);
        }
        let mut j = c.next;
        let mut count = 0u64;
        while j < c.nodes.len() && (j == c.next || count + c.nodes[j].1 <= c.chunk as u64) {
            count += c.nodes[j].1;
            j += 1;
        }
        let keys: Vec<(Key, u64)> = c.nodes[c.next..j].iter().filter(|(_, n)| *n > 0).copied().collect();
        let files: Vec<String> = keys.iter().map(|(k, _)| self.data_file(*k)).collect();
        // Fetch and decode on the shared pool, retaining its process-wide concurrency cap.
        // These are blocking leaf jobs: neither fetch nor decode starts nested pool work,
        // so the caller's options cannot leak into unrelated tasks while a scope is active.
        let base = &self.base;
        #[cfg(not(target_family = "wasm"))]
        let parts: Vec<Result<Vec<Dimension>>> = {
            let options = crate::config::thread_options();
            let context = crate::stac::AssetReadContext::capture(crate::stac::AssetReadPolicy::current());
            crate::par::map_range(keys.len(), |i| {
                crate::config::with_thread_options(options.clone(), || {
                    let _scope = context.enter()?;
                    self.decode(keys[i].0, keys[i].1, fetch(base, &files[i])?)
                })
            })
        };
        #[cfg(target_family = "wasm")]
        let parts: Vec<Result<Vec<Dimension>>> = keys.iter().zip(&files).map(|((k, n), f)| self.decode(*k, *n, fetch(base, f)?)).collect();
        let parts = parts.into_iter().collect::<Result<Vec<_>>>()?;
        let dims = super::read::concat(parts, &self.dims);
        c.next = j;
        let out = apply_options(dims, &c.filter, 1, 0);
        self.cursor = Some(c);
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{EptDataType, PointCloud, PointWriteOptions};
    use super::{Dimension, Values};
    use std::path::Path;

    /// The 40 points of eight clusters of five, one at each corner of a 1,000-unit cube. In nodes
    /// of at most 10 points, the root keeps one point of each cluster, and each of its eight
    /// children the four others of one.
    fn corners() -> PointCloud {
        let mut xyz: [Vec<f64>; 3] = Default::default();
        for corner in 0..8u32 {
            for k in 0..5u32 {
                for (axis, values) in xyz.iter_mut().enumerate() {
                    values.push(f64::from((corner >> axis) & 1) * 1000.0 + f64::from(k) * 0.1);
                }
            }
        }
        let [x, y, z] = xyz;
        let dims = vec![Dimension::new("x", Values::F64(x)), Dimension::new("y", Values::F64(y)), Dimension::new("z", Values::F64(z))];
        PointCloud::new(dims, Default::default())
    }

    /// The points of [`corners`] written in `dir` as an EPT dataset of `nodes` nodes (one, or nine:
    /// the root and its eight children), in `data_type`. Returns its `ept.json`.
    fn dataset(dir: &Path, nodes: usize, data_type: EptDataType) -> String {
        let leaf_points = if nodes == 1 { 100 } else { 10 };
        let options = PointWriteOptions { copc_leaf_points: Some(leaf_points), ept_data_type: data_type, ..Default::default() };
        let path = dir.join("ept.json").to_string_lossy().into_owned();
        crate::pointcloud::write_points(&corners(), &path, &options).unwrap();
        assert_eq!(std::fs::read_dir(dir.join("ept-data")).unwrap().count(), nodes);
        path
    }

    /// The points read from `path` under the calling thread's `options`, or the error's message.
    fn read(path: &str, options: &[(&str, &str)]) -> std::result::Result<usize, String> {
        crate::config::with_options(options, || crate::pointcloud::read_points(path, &Default::default()))
            .map(|cloud| cloud.len())
            .map_err(|e| e.to_string())
    }

    /// The points read from `path` in chunks of 10 points, exercising repeated multi-node
    /// chunks of a nine-node [`dataset`], under the calling thread's options.
    fn read_in_chunks(path: &str, options: &[(&str, &str)]) -> std::result::Result<usize, String> {
        let chunks = || -> crate::Result<usize> {
            let mut reader = crate::pointcloud::open_points(path)?;
            reader.chunks(&Default::default(), 10)?.map(|chunk| chunk.map(|c| c.len())).sum()
        };
        crate::config::with_options(options, chunks).map_err(|e| e.to_string())
    }

    /// Under the calling thread's `TERRIO_ALLOWED_DRIVERS=EPT` (`config::push_scope`, as
    /// `terrio.rio.Env` sets it), a dataset whose nodes are LAZ is refused, whether a read takes
    /// one node or nine, whole or in chunks: the LAS reader would read them. With LAZ allowed too,
    /// or with nodes in another encoding, the dataset is read.
    #[test]
    fn the_calling_threads_allow_list_applies_to_every_node() {
        let refused = "'0-0-0-0.laz' holds LAS data, and the LAS driver is not allowed (allowed: EPT)";
        for nodes in [1, 9] {
            let dir = tempfile::tempdir().unwrap();
            let laz = dataset(&dir.path().join("laz"), nodes, EptDataType::Laszip);
            let binary = dataset(&dir.path().join("binary"), nodes, EptDataType::Binary);
            let list = |drivers| [("TERRIO_ALLOWED_DRIVERS", drivers)];
            assert_eq!(read(&laz, &list("EPT")), Err(refused.to_string()), "{nodes} node(s)");
            assert_eq!(read_in_chunks(&laz, &list("EPT")), Err(refused.to_string()), "{nodes} node(s), in chunks");
            assert_eq!(read(&laz, &list("EPT,LAZ")), Ok(40), "{nodes} node(s)");
            assert_eq!(read_in_chunks(&laz, &list("EPT,LAZ")), Ok(40), "{nodes} node(s), in chunks");
            assert_eq!(read(&binary, &list("EPT")), Ok(40), "{nodes} node(s)");
        }
    }

    #[cfg(remote)]
    mod over_http {
        use super::{EptDataType, Path, dataset, read, read_in_chunks};
        use std::io::{BufRead, BufReader, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        /// A request a [`serve`] server saw: its path, and its `X-Probe` and `User-Agent` headers.
        type Request = (String, Option<String>, Option<String>);

        /// What a [`serve`] server saw: its requests, and the node requests that arrived, are in
        /// flight, and were in flight at once at most.
        #[derive(Default)]
        struct Seen {
            requests: Mutex<Vec<Request>>,
            arrived: AtomicUsize,
            in_flight: AtomicUsize,
            most: AtomicUsize,
        }

        /// Serves the files in `dir` over HTTP, a thread for each connection. A request for a node
        /// is answered once `hold` node requests have arrived, or after 5 seconds: so that nodes
        /// fetched fewer than `hold` at a time are seen to be.
        fn serve(dir: &Path, hold: usize) -> (String, Arc<Seen>) {
            serve_with(dir, hold, None)
        }

        /// [`serve`], a request for a node redirected to the same path on `nodes_to` when given.
        fn serve_with(dir: &Path, hold: usize, nodes_to: Option<String>) -> (String, Arc<Seen>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let (root, seen) = (dir.canonicalize().unwrap(), Arc::new(Seen::default()));
            let record = seen.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (root, record, nodes_to) = (root.clone(), record.clone(), nodes_to.clone());
                    std::thread::spawn(move || answer(stream, &root, hold, nodes_to.as_deref(), &record));
                }
            });
            (base, seen)
        }

        fn answer(mut stream: TcpStream, root: &Path, hold: usize, nodes_to: Option<&str>, seen: &Seen) {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (mut probe, mut agent) = (None, None);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    match name.trim().to_ascii_lowercase().as_str() {
                        "x-probe" => probe = Some(value.trim().to_string()),
                        "user-agent" => agent = Some(value.trim().to_string()),
                        _ => {}
                    }
                }
            }
            seen.requests.lock().unwrap().push((path.clone(), probe, agent));
            let node = path.starts_with("/ept-data/");
            if node && let Some(to) = nodes_to {
                let redirect = format!("HTTP/1.1 302 Found\r\nLocation: {to}{path}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = stream.write_all(redirect.as_bytes());
                return;
            }
            if node {
                seen.arrived.fetch_add(1, SeqCst);
                seen.most.fetch_max(seen.in_flight.fetch_add(1, SeqCst) + 1, SeqCst);
                let deadline = Instant::now() + Duration::from_secs(5);
                while seen.arrived.load(SeqCst) < hold && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            let body = root
                .join(path.trim_start_matches('/'))
                .canonicalize()
                .ok()
                .filter(|file| file.starts_with(root))
                .and_then(|file| std::fs::read(file).ok());
            let response = match body {
                Some(body) => {
                    [format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes(), body].concat()
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            };
            let _ = stream.write_all(&response);
            if node {
                seen.in_flight.fetch_sub(1, SeqCst);
            }
        }

        /// Every request of a read of a remote EPT dataset carries the HTTP options the calling
        /// thread set for itself (a header and the user agent, as `terrio.rio.Env` sets them), its
        /// nodes' too, whether the read takes one node or nine.
        #[test]
        fn every_request_of_a_read_carries_the_calling_threads_http_options() {
            let options = [("GDAL_HTTP_HEADERS", "X-Probe: set"), ("GDAL_HTTP_USERAGENT", "probe/1.0")];
            for nodes in [1, 9] {
                let dir = tempfile::tempdir().unwrap();
                dataset(dir.path(), nodes, EptDataType::Binary);
                let (base, seen) = serve(dir.path(), nodes.min(crate::par::threads()));
                let path = format!("{base}/ept.json");
                assert_eq!(read(&path, &options), Ok(40), "{nodes} node(s)");
                assert_eq!(read_in_chunks(&path, &options), Ok(40), "{nodes} node(s), in chunks");
                let requests = seen.requests.lock().unwrap();
                assert_eq!(requests.iter().filter(|(path, ..)| path.starts_with("/ept-data/")).count(), 2 * nodes);
                let without: Vec<_> = requests
                    .iter()
                    .filter(|(_, probe, agent)| probe.as_deref() != Some("set") || agent.as_deref() != Some("probe/1.0"))
                    .collect();
                assert!(without.is_empty(), "{nodes} node(s), requests without the options: {without:?}");
            }
        }

        /// Under the calling thread's `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN=NO`, a read whose node requests
        /// are redirected to another origin is refused, whether it takes one node or nine, and the
        /// other origin gets no request. Without the option, the read follows the redirects.
        #[test]
        fn the_calling_threads_cross_origin_rule_applies_to_every_node() {
            let option = [(crate::http::FOLLOW_CROSS_ORIGIN, "NO")];
            for nodes in [1, 9] {
                let dir = tempfile::tempdir().unwrap();
                dataset(dir.path(), nodes, EptDataType::Binary);
                let (other, other_seen) = serve(dir.path(), 1);
                let (asked, _) = serve_with(dir.path(), 1, Some(other.clone()));
                let url = format!("{asked}/ept.json");
                let refused = read(&url, &option);
                let kept = format!(
                    "a redirect to another origin ({other}) was not followed: reads stay on the origin they were given \
                     (TERRIO_HTTP_FOLLOW_CROSS_ORIGIN is not YES)"
                );
                assert!(refused.as_ref().is_err_and(|e| e.contains(&kept)), "{nodes} node(s): {refused:?}");
                assert_eq!(other_seen.requests.lock().unwrap().len(), 0, "{nodes} node(s): the other origin was asked");
                assert_eq!(read(&url, &[]), Ok(40), "{nodes} node(s), without the option");
                assert_eq!(other_seen.requests.lock().unwrap().len(), nodes, "{nodes} node(s), without the option");
            }
        }

        /// Deferred node reads retain remote-document provenance: credentials require consent
        /// on every worker even when metadata and hierarchy were opened before this read.
        #[test]
        fn node_reads_keep_the_stac_asset_consent_policy() {
            use super::super::{EptReader, FormatReader};
            for nodes in [1, 9] {
                let dir = tempfile::tempdir().unwrap();
                dataset(dir.path(), nodes, EptDataType::Binary);
                let (base, seen) = serve(dir.path(), 1);
                for (consent, allowed) in [("NO", false), ("YES", true)] {
                    let mut reader = EptReader::open(&format!("{base}/ept.json")).unwrap();
                    reader.start(&Default::default(), usize::MAX).unwrap();
                    seen.requests.lock().unwrap().clear();
                    let policy = crate::stac::AssetReadPolicy::for_tests("https://catalog.example/item.json", false);
                    let result = crate::config::with_options(
                        &[("GDAL_HTTP_HEADERS", "X-Probe: set"), ("TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS", consent)],
                        || {
                            let _scope = policy.enter().unwrap();
                            reader.next_chunk()
                        },
                    );
                    if allowed {
                        assert!(result.unwrap().is_some());
                        assert_eq!(seen.requests.lock().unwrap().len(), nodes);
                        // Auth observed on a worker must mark the caller's shared policy too.
                        crate::config::with_options(&[("TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS", "NO")], || {
                            assert!(policy.check_consent().is_err());
                        });
                    } else {
                        assert!(result.err().is_some_and(|e| e.to_string().contains("TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS")));
                        assert!(seen.requests.lock().unwrap().is_empty());
                    }
                }
            }
        }

        /// A deferred reader retains the catalog provenance from open, including hierarchy
        /// reads started after the opening scope has ended.
        #[test]
        fn reader_keeps_its_open_time_asset_policy() {
            use super::super::{EptReader, FormatReader};
            for nodes in [1, 9] {
                let dir = tempfile::tempdir().unwrap();
                dataset(dir.path(), nodes, EptDataType::Binary);
                let (base, seen) = serve(dir.path(), 1);
                for preload in [false, true] {
                    for (consent, allowed) in [("NO", false), ("YES", true)] {
                        let policy = crate::stac::AssetReadPolicy::for_tests("https://catalog.example/item.json", false);
                        let mut reader = crate::config::with_options(&[("GDAL_HTTP_HEADERS", "")], || {
                            let _scope = policy.enter().unwrap();
                            let mut reader = EptReader::open(&format!("{base}/ept.json")).unwrap();
                            if preload {
                                reader.start(&Default::default(), usize::MAX).unwrap();
                            }
                            reader
                        });
                        seen.requests.lock().unwrap().clear();
                        let result = crate::config::with_options(
                            &[("GDAL_HTTP_HEADERS", "X-Probe: set"), ("TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS", consent)],
                            || {
                                if !preload {
                                    reader.start(&Default::default(), usize::MAX)?;
                                }
                                reader.next_chunk()
                            },
                        );
                        if allowed {
                            assert!(result.unwrap().is_some());
                            let requests = seen.requests.lock().unwrap();
                            assert_eq!(requests.iter().filter(|(path, ..)| path.starts_with("/ept-data/")).count(), nodes);
                        } else {
                            assert!(result.err().is_some_and(|e| e.to_string().contains("TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS")));
                            assert!(seen.requests.lock().unwrap().is_empty());
                        }
                    }
                }
            }
        }

        /// The nodes of a read are fetched as many at a time as terrio's pool has threads (at most
        /// the nine there are): the server answers a node once that many have been asked for.
        /// On a one-thread pool a pass does not establish concurrency.
        #[test]
        fn the_nodes_of_a_read_are_fetched_as_many_at_a_time_as_the_pool_has_threads() {
            let scenario = || {
                let at_once = crate::par::threads().min(9);
                if at_once < 2 {
                    eprintln!("EPT concurrency assertion skipped: the pool has fewer than two threads");
                    return true;
                }
                let dir = tempfile::tempdir().unwrap();
                dataset(dir.path(), 9, EptDataType::Binary);
                let (base, seen) = serve(dir.path(), at_once);
                assert_eq!(read(&format!("{base}/ept.json"), &[]), Ok(40));
                let most = seen.most.load(SeqCst);
                assert!(most >= at_once, "at most {most} node request(s) in flight at once, {at_once} expected");
                true
            };
            #[cfg(unix)]
            crate::par::alone(30, scenario);
            #[cfg(not(unix))]
            assert!(scenario());
        }
    }
}
