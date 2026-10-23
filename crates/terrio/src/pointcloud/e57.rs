//! E57 (ASTM E2807): laser scans, read as PDAL's `readers.e57` reads them and written with the
//! pure-Rust [`e57`] crate.
//!
//! The points of every scan are read, in order, with each scan's pose applied. The dimensions
//! are PDAL's: `x`, `y`, `z` (from spherical coordinates when the scans have only those),
//! `spherical_range`, `spherical_azimuth`, `spherical_elevation`, `normal_x`, `normal_y`,
//! `normal_z`, `red`, `green`, `blue`, `intensity` and `classification`. As PDAL does, colours and
//! intensities become 16-bit values and classifications 8-bit ones, scaled by `max / (limit max
//! - limit min)` (the scan's limits, or its prototype's), so that colours of 0 to 255 become 0 to
//! 65535.

use super::crs::LasCrs;
use super::las::LasHeader;
use super::read::{FormatReader, Source, apply_options, made_up_header, stride_for};
use super::values::{Dimension, ValueType, Values};
use super::{PointCloud, PointFilter, PointReadOptions, PointWriteOptions};
use crate::{Error, Result};
use e57::{RecordDataType, RecordName, RecordValue};
use std::io::{Read, Seek, Write};

/// The E57 fields read, PDAL's order, with terrio's name and type.
const FIELDS: &[(&str, &str, ValueType)] = &[
    ("cartesianX", "x", ValueType::F64),
    ("cartesianY", "y", ValueType::F64),
    ("cartesianZ", "z", ValueType::F64),
    ("sphericalRange", "spherical_range", ValueType::F64),
    ("sphericalAzimuth", "spherical_azimuth", ValueType::F64),
    ("sphericalElevation", "spherical_elevation", ValueType::F64),
    ("normalX", "normal_x", ValueType::F64),
    ("normalY", "normal_y", ValueType::F64),
    ("normalZ", "normal_z", ValueType::F64),
    ("colorRed", "red", ValueType::U16),
    ("colorGreen", "green", ValueType::U16),
    ("colorBlue", "blue", ValueType::U16),
    ("intensity", "intensity", ValueType::U16),
    ("classification", "classification", ValueType::U8),
];

/// The namespace terrio writes the dimensions E57 has no field for under.
const NAMESPACE: &str = "terrio";

/// A record's field name, without its namespace for the normals and terrio's own fields.
fn field_name(name: &RecordName) -> String {
    match name {
        RecordName::CartesianX => "cartesianX".into(),
        RecordName::CartesianY => "cartesianY".into(),
        RecordName::CartesianZ => "cartesianZ".into(),
        RecordName::SphericalRange => "sphericalRange".into(),
        RecordName::SphericalAzimuth => "sphericalAzimuth".into(),
        RecordName::SphericalElevation => "sphericalElevation".into(),
        RecordName::Intensity => "intensity".into(),
        RecordName::ColorRed => "colorRed".into(),
        RecordName::ColorGreen => "colorGreen".into(),
        RecordName::ColorBlue => "colorBlue".into(),
        RecordName::Unknown { namespace, name } if namespace.is_empty() || namespace == "nor" || namespace == NAMESPACE => name.clone(),
        RecordName::Unknown { namespace, name } => format!("{namespace}:{name}"),
        other => format!("{other:?}"),
    }
}

/// A limit value as a float.
fn limit(v: &Option<RecordValue>, dt: Option<&RecordDataType>) -> Option<f64> {
    match v.as_ref()? {
        RecordValue::Single(s) => Some(*s as f64),
        RecordValue::Double(d) => Some(*d),
        RecordValue::Integer(i) => Some(*i as f64),
        RecordValue::ScaledInteger(i) => match dt {
            Some(RecordDataType::ScaledInteger { scale, offset, .. }) => Some(*i as f64 * scale + offset),
            _ => Some(*i as f64),
        },
    }
}

/// The minimum and maximum of a prototype record's type.
fn type_limits(dt: &RecordDataType) -> Option<(f64, f64)> {
    match dt {
        RecordDataType::Integer { min, max } => Some((*min as f64, *max as f64)),
        RecordDataType::ScaledInteger { min, max, scale, offset } => Some((*min as f64 * scale + offset, *max as f64 * scale + offset)),
        RecordDataType::Single { min: Some(a), max: Some(b) } => Some((*a as f64, *b as f64)),
        RecordDataType::Double { min: Some(a), max: Some(b) } => Some((*a, *b)),
        _ => None,
    }
}

/// How to read one scan.
struct ScanPlan {
    /// For each prototype record, the column it goes to and the factor it is scaled by.
    targets: Vec<Option<(usize, f64)>>,
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    pose: bool,
    spherical: bool,
}

/// An E57 file.
pub(crate) struct E57File {
    reader: e57::E57Reader<Source>,
    scans: Vec<e57::PointCloud>,
    header: LasHeader,
    crs: LasCrs,
    dims: Vec<(String, ValueType)>,
    cursor: Option<E57Cursor>,
    _local: Option<crate::vfs::LocalPath>,
}

struct E57Cursor {
    scan: usize,
    done: u64,
    stride: u64,
    filter: PointFilter,
}

impl E57File {
    pub fn new(source: Source, local: Option<crate::vfs::LocalPath>) -> Result<Self> {
        let reader = e57::E57Reader::new(source).map_err(|e| Error::InvalidData(format!("E57: {e}")))?;
        let scans = reader.pointclouds();
        // The dimensions any scan has, in PDAL's order; x, y and z first, also when they come
        // from spherical coordinates.
        let has = |f: &str| scans.iter().any(|s| s.prototype.iter().any(|r| field_name(&r.name) == f));
        let spherical = ["sphericalRange", "sphericalAzimuth", "sphericalElevation"].iter().all(|f| has(f));
        let mut dims: Vec<(String, ValueType)> = Vec::new();
        for (f, name, t) in FIELDS {
            let xyz = matches!(*f, "cartesianX" | "cartesianY" | "cartesianZ");
            if has(f) || (xyz && spherical) {
                dims.push((name.to_string(), *t));
            }
        }
        // terrio's own fields, written by write_e57.
        for s in &scans {
            for r in &s.prototype {
                if let RecordName::Unknown { namespace, name } = &r.name
                    && namespace == NAMESPACE
                    && !dims.iter().any(|(d, _)| d == name)
                {
                    dims.push((name.clone(), ValueType::F64));
                }
            }
        }
        // The bounds: the scans' Cartesian bounds, posed (all eight corners), when they all have
        // them.
        let mut bounds: Option<[f64; 6]> = None;
        let mut known = !scans.is_empty();
        for s in &scans {
            let Some(b) = s.cartesian_bounds.as_ref() else {
                known = false;
                break;
            };
            let (Some(x0), Some(x1), Some(y0), Some(y1), Some(z0), Some(z1)) = (b.x_min, b.x_max, b.y_min, b.y_max, b.z_min, b.z_max)
            else {
                known = false;
                break;
            };
            let plan = plan(s, &dims);
            for &x in &[x0, x1] {
                for &y in &[y0, y1] {
                    for &z in &[z0, z1] {
                        let p = if plan.pose { transform(&plan, [x, y, z]) } else { [x, y, z] };
                        bounds = Some(match bounds {
                            None => [p[0], p[1], p[2], p[0], p[1], p[2]],
                            Some(a) => [a[0].min(p[0]), a[1].min(p[1]), a[2].min(p[2]), a[3].max(p[0]), a[4].max(p[1]), a[5].max(p[2])],
                        });
                    }
                }
            }
        }
        let count = scans.iter().map(|s| s.records).sum();
        let mut f = Self { reader, scans, header: LasHeader::default(), crs: LasCrs::default(), dims, cursor: None, _local: local };
        if !known {
            bounds = None;
            f.start(&PointReadOptions::default(), 0)?;
            while let Some(chunk) = f.next_chunk()? {
                if let Some(b) = PointCloud::new(chunk, Default::default()).bounds() {
                    bounds = Some(match bounds {
                        None => b,
                        Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2]), a[3].max(b[3]), a[4].max(b[4]), a[5].max(b[5])],
                    });
                }
            }
        }
        f.header = made_up_header(&f.dims, count, bounds, &Default::default(), None);
        Ok(f)
    }
}

/// How to read a scan: where each field goes, PDAL's scale factors, and the pose.
fn plan(scan: &e57::PointCloud, dims: &[(String, ValueType)]) -> ScanPlan {
    let column = |name: &str| dims.iter().position(|(d, _)| d == name);
    let targets = scan
        .prototype
        .iter()
        .map(|r| {
            let f = field_name(&r.name);
            let (name, t) = match FIELDS.iter().find(|(e, ..)| *e == f) {
                Some((_, n, t)) => (n.to_string(), *t),
                None => match &r.name {
                    RecordName::Unknown { namespace, name } if namespace == NAMESPACE => (name.clone(), ValueType::F64),
                    _ => return None,
                },
            };
            let col = column(&name)?;
            // PDAL scales colours, intensities and classifications to the full range of their
            // type, by a factor computed in single precision.
            let scaled_max = match (f.as_str(), t) {
                ("colorRed" | "colorGreen" | "colorBlue" | "intensity", _) => Some(65535.0),
                ("classification", _) => Some(255.0),
                _ => None,
            };
            let factor = match scaled_max {
                None => 1.0,
                Some(top) => {
                    // Scaled-integer limits take the scale of the field's.
                    let dt = Some(&r.data_type);
                    let limits = match f.as_str() {
                        "colorRed" => scan.color_limits.as_ref().map(|l| (limit(&l.red_min, dt), limit(&l.red_max, dt))),
                        "colorGreen" => scan.color_limits.as_ref().map(|l| (limit(&l.green_min, dt), limit(&l.green_max, dt))),
                        "colorBlue" => scan.color_limits.as_ref().map(|l| (limit(&l.blue_min, dt), limit(&l.blue_max, dt))),
                        "intensity" => scan.intensity_limits.as_ref().map(|l| (limit(&l.intensity_min, dt), limit(&l.intensity_max, dt))),
                        _ => None,
                    };
                    let range = match limits {
                        Some((Some(a), Some(b))) => Some((a, b)),
                        _ => type_limits(&r.data_type),
                    };
                    match range {
                        // Equal limits give PDAL an infinite factor, and values it cannot store.
                        Some((a, b)) => (top / (b - a)) as f32 as f64,
                        None => 1.0,
                    }
                }
            };
            Some((col, factor))
        })
        .collect();
    let has = |f: &str| scan.prototype.iter().any(|r| field_name(&r.name) == f);
    let spherical = ["sphericalRange", "sphericalAzimuth", "sphericalElevation"].iter().all(|f| has(f));
    let (rotation, translation, pose) = match &scan.transform {
        Some(t) => {
            let q = [t.rotation.w, t.rotation.x, t.rotation.y, t.rotation.z];
            let (q11, q22, q33) = (q[1] * q[1], q[2] * q[2], q[3] * q[3]);
            let (q03, q13, q23, q02, q12, q01) = (q[0] * q[3], q[1] * q[3], q[2] * q[3], q[0] * q[2], q[1] * q[2], q[0] * q[1]);
            (
                [
                    [1.0 - 2.0 * (q22 + q33), 2.0 * (q12 - q03), 2.0 * (q13 + q02)],
                    [2.0 * (q12 + q03), 1.0 - 2.0 * (q11 + q33), 2.0 * (q23 - q01)],
                    [2.0 * (q13 - q02), 2.0 * (q23 + q01), 1.0 - 2.0 * (q11 + q22)],
                ],
                [t.translation.x, t.translation.y, t.translation.z],
                true,
            )
        }
        None => ([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], [0.0; 3], false),
    };
    ScanPlan { targets, rotation, translation, pose, spherical }
}

/// A point posed as PDAL poses it.
fn transform(plan: &ScanPlan, p: [f64; 3]) -> [f64; 3] {
    let (r, t) = (&plan.rotation, &plan.translation);
    [
        p[0] * r[0][0] + p[1] * r[0][1] + p[2] * r[0][2] + t[0],
        p[0] * r[1][0] + p[1] * r[1][1] + p[2] * r[1][2] + t[1],
        p[0] * r[2][0] + p[1] * r[2][1] + p[2] * r[2][2] + t[2],
    ]
}

impl FormatReader for E57File {
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
        "E57"
    }

    fn start(&mut self, options: &PointReadOptions, _chunk: usize) -> Result<()> {
        let n = self.scans.iter().map(|s| s.records).sum();
        self.cursor = Some(E57Cursor { scan: 0, done: 0, stride: stride_for(n, options.max_points), filter: options.filter.clone() });
        Ok(())
    }

    /// The points of the next scan.
    fn next_chunk(&mut self) -> Result<Option<Vec<Dimension>>> {
        let Some(mut c) = self.cursor.take() else { return Ok(None) };
        if c.scan >= self.scans.len() {
            return Ok(None);
        }
        let scan = self.scans[c.scan].clone();
        let plan = plan(&scan, &self.dims);
        let m = scan.records as usize;
        let mut cols: Vec<Vec<f64>> = vec![Vec::with_capacity(m); self.dims.len()];
        let col = |name: &str| self.dims.iter().position(|(d, _)| d == name);
        let (cx, cy, cz) = (col("x"), col("y"), col("z"));
        let (cr, ca, ce) = (col("spherical_range"), col("spherical_azimuth"), col("spherical_elevation"));
        let mut row = vec![0.0; self.dims.len()];
        let raw = self.reader.pointcloud_raw(&scan).map_err(|e| Error::InvalidData(format!("E57: {e}")))?;
        for values in raw {
            let values = values.map_err(|e| Error::InvalidData(format!("E57: {e}")))?;
            row.iter_mut().for_each(|v| *v = 0.0);
            for ((v, r), target) in values.iter().zip(&scan.prototype).zip(&plan.targets) {
                if let Some((k, factor)) = target {
                    let x = v.to_f64(&r.data_type).map_err(|e| Error::InvalidData(format!("E57: {e}")))?;
                    row[*k] = factor * x;
                }
            }
            if plan.spherical
                && let (Some(r), Some(a), Some(e), Some(x), Some(y), Some(z)) = (cr, ca, ce, cx, cy, cz)
            {
                let (range, az, el) = (row[r], row[a], row[e]);
                row[x] = range * el.cos() * az.cos();
                row[y] = range * el.cos() * az.sin();
                row[z] = range * el.sin();
            }
            if plan.pose
                && let (Some(x), Some(y), Some(z)) = (cx, cy, cz)
            {
                let p = transform(&plan, [row[x], row[y], row[z]]);
                row[x] = p[0];
                row[y] = p[1];
                row[z] = p[2];
            }
            for (c, v) in cols.iter_mut().zip(&row) {
                c.push(*v);
            }
        }
        // The values in their types, rounded as PDAL rounds them.
        let dims: Vec<Dimension> = self
            .dims
            .iter()
            .zip(cols)
            .map(|((name, t), v)| {
                let values = match t {
                    ValueType::F64 => Values::F64(v),
                    t => {
                        // PDAL rounds, and leaves 0 for values out of the type's range.
                        let max = if *t == ValueType::U8 { 255.0 } else { 65535.0 };
                        let mut out = Values::with_capacity(*t, v.len());
                        for x in v {
                            let r = x.round();
                            out.push_f64(if (0.0..=max).contains(&r) { r } else { 0.0 });
                        }
                        out
                    }
                };
                Dimension::new(name, values)
            })
            .collect();
        let first = c.done;
        c.done += m as u64;
        c.scan += 1;
        let out = apply_options(dims, &c.filter, c.stride, first);
        self.cursor = Some(c);
        Ok(Some(out))
    }
}

/// A GUID made of the time and a counter (E57 files and scans need one).
fn guid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // The clock is not available in WebAssembly builds without a host.
    #[cfg(not(target_family = "wasm"))]
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    #[cfg(target_family = "wasm")]
    let t = 0x5eed_u64;
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let a = super::copc::scramble(t ^ c.rotate_left(32));
    let b = super::copc::scramble(a ^ 0x9e37_79b9_7f4a_7c15);
    format!("{{{:08x}-{:04x}-{:04x}-{:04x}-{:012x}}}", a >> 32, (a >> 16) & 0xffff, a & 0xffff, b >> 48, b & 0xffff_ffff_ffff)
}

/// What an E57 file holds: its records, and the dimension each comes from.
pub(crate) struct E57Plan {
    prototype: Vec<e57::Record>,
    sources: Vec<String>,
    normals: bool,
    own: bool,
}

/// The records of an E57 file of these dimensions (`names`, or all when empty): coordinates,
/// colours (red, green and blue together), intensity, normals and the others in terrio's own
/// namespace.
pub(crate) fn plan_e57(cloud: &PointCloud, names: &[String]) -> Result<E57Plan> {
    let pick: Vec<&Dimension> = if names.is_empty() {
        cloud.dimensions.iter().collect()
    } else {
        names.iter().map(|n| cloud.dim(n).ok_or_else(|| Error::InvalidArgument(format!("no dimension '{n}'")))).collect::<Result<_>>()?
    };
    for c in ["x", "y", "z"] {
        if !pick.iter().any(|d| d.name == c) {
            return Err(Error::InvalidArgument(format!("E57 needs '{c}'")));
        }
    }
    // E57 has colours as all of red, green and blue, or none.
    let colours = ["red", "green", "blue"].iter().all(|c| pick.iter().any(|d| d.name == *c));
    let mut plan = E57Plan { prototype: Vec::new(), sources: Vec::new(), normals: false, own: false };
    let int16 = RecordDataType::Integer { min: 0, max: 65535 };
    for d in &pick {
        let name = match d.name.as_str() {
            "x" => RecordName::CartesianX,
            "y" => RecordName::CartesianY,
            "z" => RecordName::CartesianZ,
            "spherical_range" => RecordName::SphericalRange,
            "spherical_azimuth" => RecordName::SphericalAzimuth,
            "spherical_elevation" => RecordName::SphericalElevation,
            "red" if colours => RecordName::ColorRed,
            "green" if colours => RecordName::ColorGreen,
            "blue" if colours => RecordName::ColorBlue,
            "intensity" => RecordName::Intensity,
            "normal_x" | "normal_y" | "normal_z" => {
                plan.normals = true;
                let n = match d.name.as_str() {
                    "normal_x" => "normalX",
                    "normal_y" => "normalY",
                    _ => "normalZ",
                };
                RecordName::Unknown { namespace: "nor".into(), name: n.into() }
            }
            other => {
                if !other.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    || other.to_ascii_lowercase().starts_with("xml")
                {
                    continue;
                }
                plan.own = true;
                RecordName::Unknown { namespace: NAMESPACE.into(), name: other.into() }
            }
        };
        let data_type = match name {
            RecordName::ColorRed | RecordName::ColorGreen | RecordName::ColorBlue | RecordName::Intensity => int16.clone(),
            _ => RecordDataType::Double { min: None, max: None },
        };
        plan.prototype.push(e57::Record { name, data_type });
        plan.sources.push(d.name.clone());
    }
    Ok(plan)
}

/// Writes an E57 file of one point cloud (scan) whose points `feed` passes, chunk after chunk,
/// to the function it is given: the points are written in packets as they come.
pub(crate) fn write_e57_with<T: Read + Write + Seek>(
    out: T,
    plan: &E57Plan,
    feed: impl FnOnce(&mut dyn FnMut(&PointCloud) -> Result<()>) -> Result<()>,
) -> Result<()> {
    let err = |e: e57::Error| Error::InvalidData(format!("E57: {e}"));
    let mut w = e57::E57Writer::new(out, &guid()).map_err(err)?;
    if plan.normals {
        w.register_extension(e57::Extension { namespace: "nor".into(), url: "http://www.libe57.org/E57_NOR_surface_normals.txt".into() })
            .map_err(err)?;
    }
    if plan.own {
        w.register_extension(e57::Extension { namespace: NAMESPACE.into(), url: "https://github.com/opengeos/terrio".into() })
            .map_err(err)?;
    }
    {
        let mut pc = w.add_pointcloud(&guid(), plan.prototype.clone()).map_err(err)?;
        let mut add = |cloud: &PointCloud| -> Result<()> {
            // Dimensions missing from a chunk are written as zeros.
            let dims: Vec<Option<&Dimension>> = plan.sources.iter().map(|n| cloud.dim(n)).collect();
            for i in 0..cloud.len() {
                let values = plan
                    .prototype
                    .iter()
                    .zip(&dims)
                    .map(|(r, d)| {
                        let v = d.map_or(0.0, |d| d.get(i));
                        match r.data_type {
                            RecordDataType::Integer { .. } => RecordValue::Integer((v.round() as i64).clamp(0, 65535)),
                            _ => RecordValue::Double(v),
                        }
                    })
                    .collect();
                pc.add_point(values).map_err(err)?;
            }
            Ok(())
        };
        feed(&mut add)?;
        pc.finalize().map_err(err)?;
    }
    w.finalize().map_err(err)?;
    Ok(())
}

/// Writes a point cloud as an E57 file.
pub(crate) fn write_e57<T: Read + Write + Seek>(cloud: &PointCloud, out: T, options: &PointWriteOptions) -> Result<()> {
    let plan = plan_e57(cloud, &options.dimensions)?;
    write_e57_with(out, &plan, |add| add(cloud))
}

/// Writes an E57 file of points that come in chunks, in bounded memory: a thread writes the
/// packets of each chunk as it comes (the E57 writer holds no more than a packet).
#[cfg(not(target_family = "wasm"))]
pub(crate) struct E57StreamWriter {
    tx: Option<std::sync::mpsc::SyncSender<PointCloud>>,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
    /// Set when the writer is dropped unfinished: the thread then stops without finishing the
    /// file, which is not left looking complete.
    aborted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(not(target_family = "wasm"))]
impl E57StreamWriter {
    /// Creates the file for points like `template`'s.
    pub fn new(file: std::fs::File, template: &PointCloud, options: &PointWriteOptions) -> Result<Self> {
        let plan = plan_e57(template, &options.dimensions)?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<PointCloud>(1);
        let aborted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = aborted.clone();
        let handle = std::thread::Builder::new().name("terrio-e57-writer".into()).spawn(move || {
            write_e57_with(file, &plan, |add| {
                for chunk in rx {
                    add(&chunk)?;
                }
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(Error::InvalidData("E57: the writer was dropped before the file was finished".into()));
                }
                Ok(())
            })
        })?;
        Ok(Self { tx: Some(tx), handle: Some(handle), aborted })
    }

    /// The writing thread's result, once it has stopped.
    fn join(&mut self) -> Result<()> {
        self.tx = None;
        match self.handle.take() {
            Some(h) => h.join().map_err(|_| Error::InvalidData("E57: the writing thread panicked".into()))?,
            None => Ok(()),
        }
    }

    /// Writes points.
    pub fn write_cloud(&mut self, cloud: &PointCloud) -> Result<()> {
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(cloud.clone()).is_ok());
        if sent {
            return Ok(());
        }
        // The thread stopped: its error.
        self.join()?;
        Err(Error::InvalidData("E57: the file is already finished".into()))
    }

    /// Finishes the file.
    pub fn finish(mut self) -> Result<()> {
        self.join()
    }
}

#[cfg(not(target_family = "wasm"))]
impl Drop for E57StreamWriter {
    fn drop(&mut self) {
        self.aborted.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.join();
    }
}
