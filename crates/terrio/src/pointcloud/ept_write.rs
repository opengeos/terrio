//! Writing EPT (Entwine Point Tile) datasets: the points organized in an octree whose nodes are
//! files of their own, as Entwine writes them (<https://entwine.io/entwine-point-tile.html>).
//!
//! The octree is the one terrio builds for COPC files (a cube around the points, a 128-cell
//! sampling grid per node, leaves of at most a set number of points), built in bounded memory
//! for clouds larger than memory. Each node becomes `ept-data/D-X-Y-Z.laz` (LASzip), `.bin`
//! (binary records in the schema's order) or `.zst` (the same, Zstandard-compressed); the
//! hierarchy is one JSON page, `ept-hierarchy/0-0-0-0.json`, and `ept.json` describes the dataset:
//! its cube, the points' bounds, their number, the schema (PDAL's dimension names, with the scale
//! and offset of X, Y and Z) and the CRS. PDAL's `readers.ept` reads the result.

use super::copc::{NodeSink, VoxelKey};
use super::ept::EptDataType;
use super::las::{LasHeader, Layout};
use super::values::{ValueType, Values};
use super::write::LasStreamWriter;
use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

/// The directory of an EPT dataset written: `dir` for `dir/ept.json`, `ept://dir` or `dir`.
pub(crate) fn dataset_dir(path: &str) -> PathBuf {
    let p = path.strip_prefix("ept://").unwrap_or(path);
    let p = Path::new(p);
    if p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("ept.json")) {
        p.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        p.to_path_buf()
    }
}

/// An EPT dataset being written.
pub(crate) struct EptTarget {
    /// The dataset's directory.
    dir: PathBuf,
    /// Where the dataset is built (in `dir`), moved into place once complete.
    work: PathBuf,
    data_type: EptDataType,
    crs: SpatialRef,
}

/// A dimension of the schema written, and where its values come from.
struct SchemaDim {
    /// PDAL's name.
    name: String,
    /// terrio's name of the dimension the values are read from.
    source: String,
    /// The type stored.
    stored: ValueType,
    /// For X, Y and Z: the scale and offset of the stored integers.
    scale: Option<(f64, f64)>,
    /// A factor applied to the values read (scan angles stored in 0.006° units are written in
    /// degrees, as PDAL reads them).
    factor: f64,
    /// A function of the values read (PDAL's view of legacy LAS overlap points).
    derive: Option<fn(f64) -> f64>,
}

/// LAS formats 0 to 5 mark overlap points with class 12: PDAL reads them as class 1 with the
/// overlap flag.
fn legacy_class(c: f64) -> f64 {
    if c == 12.0 { 1.0 } else { c }
}

fn legacy_overlap(c: f64) -> f64 {
    if c == 12.0 { 1.0 } else { 0.0 }
}

impl EptTarget {
    /// A dataset to write in `dir` (created when missing). It is built in a directory of its
    /// own in `dir`, and replaces the `ept.json`, `ept-data` and `ept-hierarchy` already there
    /// only once complete: a write that fails leaves the previous dataset as it was.
    pub fn new(dir: PathBuf, data_type: EptDataType, crs: SpatialRef) -> Result<Self> {
        let io = |d: &Path, e: std::io::Error| Error::Io(std::io::Error::new(e.kind(), format!("'{}': {e}", d.display())));
        let work = dir.join(".terrio-ept-new");
        // Left by a write that was interrupted.
        if work.exists() {
            std::fs::remove_dir_all(&work).map_err(|e| io(&work, e))?;
        }
        for sub in ["ept-data", "ept-hierarchy"] {
            let d = work.join(sub);
            std::fs::create_dir_all(&d).map_err(|e| io(&d, e))?;
        }
        Ok(Self { dir, work, data_type, crs })
    }

    /// Moves the dataset built into place, replacing the previous one: the previous entries are
    /// first moved aside, and moved back if the new ones cannot all be moved in. `ept.json` goes
    /// first aside and last in, so that readers find either dataset whole or no `ept.json`.
    fn commit(&self) -> Result<()> {
        let io = |d: &Path, e: std::io::Error| Error::Io(std::io::Error::new(e.kind(), format!("'{}': {e}", d.display())));
        let old = self.dir.join(".terrio-ept-old");
        if old.exists() {
            std::fs::remove_dir_all(&old).map_err(|e| io(&old, e))?;
        }
        std::fs::create_dir(&old).map_err(|e| io(&old, e))?;
        let names = ["ept.json", "ept-data", "ept-hierarchy"];
        let mut aside = Vec::new();
        let mut moved_in = Vec::new();
        let swapped = (|| -> Result<()> {
            for name in names {
                let current = self.dir.join(name);
                if current.exists() {
                    std::fs::rename(&current, old.join(name)).map_err(|e| io(&current, e))?;
                    aside.push(name);
                }
            }
            for name in names.iter().rev() {
                let target = self.dir.join(name);
                std::fs::rename(self.work.join(name), &target).map_err(|e| io(&target, e))?;
                moved_in.push(*name);
            }
            Ok(())
        })();
        if let Err(e) = swapped {
            // Back as it was: the new entries out, the previous ones in.
            for name in &moved_in {
                let _ = std::fs::rename(self.dir.join(name), self.work.join(name));
            }
            for name in &aside {
                let _ = std::fs::rename(old.join(name), self.dir.join(name));
            }
            let _ = std::fs::remove_dir_all(&old);
            return Err(e);
        }
        std::fs::remove_dir_all(&old).map_err(|e| io(&old, e))?;
        std::fs::remove_dir(&self.work).map_err(|e| io(&self.work, e))
    }

    /// Writes the nodes `build` makes, then the hierarchy and `ept.json`. `bounds` is the octree's
    /// cube, `conforming` the points' bounds and `span` the cells per side of each node's grid.
    pub fn write(
        self,
        header: LasHeader,
        layout: &Layout,
        bounds: [f64; 6],
        conforming: [f64; 6],
        span: u64,
        build: impl FnOnce(&mut dyn NodeSink) -> Result<()>,
    ) -> Result<()> {
        let written = self.build_all(header, layout, bounds, conforming, span, build).and_then(|()| self.commit());
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&self.work);
        }
        written
    }

    fn build_all(
        &self,
        header: LasHeader,
        layout: &Layout,
        bounds: [f64; 6],
        conforming: [f64; 6],
        span: u64,
        build: impl FnOnce(&mut dyn NodeSink) -> Result<()>,
    ) -> Result<()> {
        let schema = schema(&header, layout)?;
        let mut sink = EptSink { target: self, header: &header, layout, schema: &schema, counts: BTreeMap::new() };
        build(&mut sink)?;
        let counts = std::mem::take(&mut sink.counts);
        let points: u64 = counts.values().sum();
        // The hierarchy: one page, nodes in octree order.
        let page: serde_json::Map<String, Value> = counts.iter().map(|(k, n)| (key_name(*k), json!(n))).collect();
        write_json(&self.work.join("ept-hierarchy/0-0-0-0.json"), &Value::Object(page))?;
        let schema_json: Vec<Value> = schema
            .iter()
            .map(|d| {
                let (kind, size) = type_name(d.stored);
                let mut o = json!({ "name": d.name, "type": kind, "size": size });
                if let Some((s, off)) = d.scale {
                    o["scale"] = json!(s);
                    o["offset"] = json!(off);
                }
                o
            })
            .collect();
        let mut srs = serde_json::Map::new();
        if let Some(code) = self.crs.epsg {
            srs.insert("authority".into(), json!("EPSG"));
            srs.insert("horizontal".into(), json!(code.to_string()));
        }
        if let Some(wkt) = self.crs.to_wkt() {
            srs.insert("wkt".into(), json!(wkt));
        }
        let ept = json!({
            "bounds": bounds,
            "boundsConforming": conforming,
            "dataType": self.data_type.name(),
            "hierarchyType": "json",
            "points": points,
            "schema": schema_json,
            "span": span,
            "srs": Value::Object(srs),
            "version": "1.1.0",
        });
        write_json(&self.work.join("ept.json"), &ept)
    }
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(v).map_err(|e| Error::InvalidData(format!("JSON: {e}")))?;
    std::fs::write(path, text).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{}': {e}", path.display()))))
}

fn key_name(k: VoxelKey) -> String {
    format!("{}-{}-{}-{}", k.level, k.x, k.y, k.z)
}

/// EPT's type name and size of a value type.
fn type_name(t: ValueType) -> (&'static str, usize) {
    let kind = match t {
        ValueType::I8 | ValueType::I16 | ValueType::I32 | ValueType::I64 => "signed",
        ValueType::U8 | ValueType::U16 | ValueType::U32 | ValueType::U64 => "unsigned",
        ValueType::F32 | ValueType::F64 => "float",
    };
    (kind, t.size())
}

/// The schema of the points of a header and layout: the dimensions PDAL reads from such LAS
/// records, with PDAL's names and types.
fn schema(header: &LasHeader, layout: &Layout) -> Result<Vec<SchemaDim>> {
    // The dimensions terrio reads from these records: those of an empty file of them.
    let empty = las_bytes(header, layout, &[], false)?;
    let reader = super::read::open_points_bytes(empty, "schema.las")?;
    let legacy = header.point_format < 6;
    let mut out = Vec::new();
    for (name, t) in reader.dimensions() {
        let axis = ["x", "y", "z"].iter().position(|a| *a == name);
        let dim = match (name.as_str(), axis) {
            (_, Some(k)) => SchemaDim {
                name: super::names::to_pdal(&name),
                source: name.clone(),
                stored: ValueType::I32,
                scale: Some((header.scale[k], header.offset[k])),
                factor: 1.0,
                derive: None,
            },
            // PDAL reads scan angles as degrees, in floats.
            ("scan_angle", _) => SchemaDim {
                name: "ScanAngleRank".into(),
                source: name.clone(),
                stored: ValueType::F32,
                scale: None,
                factor: 0.006,
                derive: None,
            },
            ("scan_angle_rank", _) => SchemaDim {
                name: "ScanAngleRank".into(),
                source: name.clone(),
                stored: ValueType::F32,
                scale: None,
                factor: 1.0,
                derive: None,
            },
            // Waveform references point into the source file: not kept.
            (n, _)
                if n.starts_with("wavepacket")
                    || n.starts_with("waveform")
                    || n.starts_with("return_point")
                    || n == "x_t"
                    || n == "y_t"
                    || n == "z_t" =>
            {
                continue;
            }
            ("classification", _) if legacy => SchemaDim {
                name: "Classification".into(),
                source: name.clone(),
                stored: t,
                scale: None,
                factor: 1.0,
                derive: Some(legacy_class),
            },
            _ => SchemaDim { name: super::names::to_pdal(&name), source: name.clone(), stored: t, scale: None, factor: 1.0, derive: None },
        };
        out.push(dim);
        if legacy && name == "withheld" {
            out.push(SchemaDim {
                name: "Overlap".into(),
                source: "classification".into(),
                stored: ValueType::U8,
                scale: None,
                factor: 1.0,
                derive: Some(legacy_overlap),
            });
        }
    }
    Ok(out)
}

/// A LAS (or LAZ) file of records.
fn las_bytes(header: &LasHeader, layout: &Layout, records: &[u8], compressed: bool) -> Result<Vec<u8>> {
    let mut h = header.clone();
    h.compressed = compressed;
    h.vlrs.retain(|v| !v.is_laszip());
    h.evlrs.retain(|v| !v.is_waveform_data());
    h.start_of_waveform_data = 0;
    h.waveform_data_size = 0;
    let mut c = Cursor::new(Vec::new());
    let mut w = LasStreamWriter::new(&mut c, h, layout.clone())?;
    w.write_records(records)?;
    w.finish(None)?;
    Ok(c.into_inner())
}

/// Writes the nodes of an EPT dataset.
struct EptSink<'a> {
    target: &'a EptTarget,
    header: &'a LasHeader,
    layout: &'a Layout,
    schema: &'a [SchemaDim],
    counts: BTreeMap<VoxelKey, u64>,
}

impl NodeSink for EptSink<'_> {
    fn write_node(&mut self, key: VoxelKey, records: &[u8], rl: usize) -> Result<()> {
        let n = records.len() / rl;
        if n == 0 {
            return Ok(());
        }
        let t = self.target;
        // Each node once: its count goes to the hierarchy as written.
        if self.counts.contains_key(&key) {
            return Err(Error::InvalidData(format!("EPT: node {} written twice", key_name(key))));
        }
        let path = t.work.join(format!("ept-data/{}.{}", key_name(key), t.data_type.extension()));
        let bytes = match t.data_type {
            EptDataType::Laszip => las_bytes(self.header, self.layout, records, true)?,
            kind => {
                let cloud = super::read::open_points_bytes(las_bytes(self.header, self.layout, records, false)?, "node.las")?
                    .read(&Default::default())?;
                let cols: Vec<Values> = self
                    .schema
                    .iter()
                    .map(|d| {
                        let src = cloud.dim(&d.source).ok_or_else(|| Error::InvalidData(format!("EPT: no '{}' to write", d.source)))?;
                        Ok(match d.scale {
                            // The stored integers, from the coordinates with the header's scale
                            // and offset (those of the records, so exact).
                            Some((s, o)) => Values::I32(src.to_f64().iter().map(|v| ((v - o) / s).round() as i32).collect()),
                            None => match d.derive {
                                Some(f) => Values::F64(src.to_f64().into_iter().map(f).collect()).cast(d.stored),
                                None if d.factor != 1.0 => Values::F64(src.to_f64().iter().map(|v| v * d.factor).collect()).cast(d.stored),
                                None => src.values.cast(d.stored),
                            },
                        })
                    })
                    .collect::<Result<_>>()?;
                let len: usize = self.schema.iter().map(|d| d.stored.size()).sum();
                let mut out = vec![0u8; n * len];
                for (i, rec) in out.chunks_exact_mut(len).enumerate() {
                    let mut at = 0;
                    for (c, d) in cols.iter().zip(self.schema) {
                        let size = d.stored.size();
                        c.write_le(i, &mut rec[at..at + size]);
                        at += size;
                    }
                }
                if kind == EptDataType::Zstandard { zstd::bulk::compress(&out, 3)? } else { out }
            }
        };
        let mut f = std::io::BufWriter::new(
            std::fs::File::create(&path).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{}': {e}", path.display()))))?,
        );
        f.write_all(&bytes)?;
        f.flush()?;
        *self.counts.entry(key).or_default() += n as u64;
        Ok(())
    }
}
