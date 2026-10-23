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
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn name(self) -> &'static str {
        match self {
            EptDataType::Laszip => "laszip",
            EptDataType::Binary => "binary",
            EptDataType::Zstandard => "zstandard",
        }
    }

    /// The extension of the node files.
    #[cfg(not(target_arch = "wasm32"))]
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
        Ok(Self { base, info, header, crs, schema_dims, dims, pages: HashMap::new(), cursor: None })
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
    fn decode(&self, key: Key, bytes: Vec<u8>) -> Result<Vec<Dimension>> {
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
                let bytes = if kind == "zstandard" {
                    let mut out = Vec::new();
                    let mut d = ruzstd::decoding::StreamingDecoder::new(std::io::Cursor::new(bytes))
                        .map_err(|e| Error::InvalidData(format!("EPT node {}: {e}", key.name())))?;
                    std::io::Read::read_to_end(&mut d, &mut out)?;
                    out
                } else {
                    bytes
                };
                let rl: usize = schema.iter().map(|s| s.size).sum();
                if rl == 0 {
                    return Err(Error::InvalidData("EPT: an empty schema".into()));
                }
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
        let nodes = self.select(options)?;
        self.cursor = Some(EptCursor { nodes, next: 0, filter: options.filter.clone(), chunk });
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<Option<Vec<Dimension>>> {
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
        let keys: Vec<Key> = c.nodes[c.next..j].iter().filter(|(_, n)| *n > 0).map(|(k, _)| *k).collect();
        let files: Vec<String> = keys.iter().map(|k| self.data_file(*k)).collect();
        // Fetch (in parallel, on native builds) and decode the nodes.
        let base = &self.base;
        #[cfg(not(target_arch = "wasm32"))]
        let parts: Vec<Result<Vec<Dimension>>> = {
            use rayon::prelude::*;
            keys.par_iter().zip(files.par_iter()).map(|(k, f)| self.decode(*k, fetch(base, f)?)).collect()
        };
        #[cfg(target_arch = "wasm32")]
        let parts: Vec<Result<Vec<Dimension>>> = keys.iter().zip(&files).map(|(k, f)| self.decode(*k, fetch(base, f)?)).collect();
        let parts = parts.into_iter().collect::<Result<Vec<_>>>()?;
        let dims = super::read::concat(parts, &self.dims);
        c.next = j;
        let out = apply_options(dims, &c.filter, 1, 0);
        self.cursor = Some(c);
        Ok(Some(out))
    }
}
