//! Point clouds: LAS 1.0 to 1.4 (point formats 0 to 10, extra bytes, VLRs and EVLRs, waveforms),
//! LAZ (LASzip-compressed LAS), COPC (Cloud Optimized Point Clouds), EPT (Entwine Point Tiles),
//! PLY and E57; read whole or in chunks (for files larger than memory), filtered as PDAL's
//! filters do ([`filters`]) and gridded ([`grid`]).
//!
//! Points are read into columns, one typed vector per dimension, named as laspy names them (`x`,
//! `y`, `z`, `intensity`, `return_number`, `classification`, `gps_time`, `red`...). Filters
//! (bounds, classes, returns) are applied while the points are decoded.
//!
//! COPC files, local or remote, are read in place: opening one fetches its header and the root
//! of its octree hierarchy, and a read fetches only the nodes that intersect the bounds asked for,
//! down to the octree level (or point spacing) asked for, with HTTP range requests.
//!
//! ```no_run
//! use terrio::pointcloud::{self, PointReadOptions};
//!
//! let mut reader = pointcloud::open_points("https://host/autzen.copc.laz")?;
//! let options = PointReadOptions { max_level: Some(3), ..Default::default() };
//! let cloud = reader.read(&options)?;
//! println!("{} points, z from {:?}", cloud.len(), cloud.dim("z").and_then(|d| d.values.min_max()));
//! pointcloud::write_points(&cloud, "subset.laz", &Default::default())?;
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! LAZ decompression and compression come from the pure-Rust [`laz`] crate (the codec of laspy's
//! `lazrs` backend); terrio reads and writes the LAS container and the COPC structures itself.

pub mod copc;
pub(crate) mod crs;
mod e57;
pub mod ept;
#[cfg(not(target_family = "wasm"))]
mod ept_append;
#[cfg(not(target_family = "wasm"))]
mod ept_write;
pub mod filters;
pub mod grid;
pub mod info;
mod kdtree;
pub mod las;
pub mod names;
mod ply;
mod read;
mod values;
pub mod vector;
pub mod waveform;
mod write;

pub use copc::{CopcInfo, CopcNode, VoxelKey};
pub use ept::{EptDataType, EptDimension, EptInfo};
#[cfg(not(target_family = "wasm"))]
pub use ept_append::{EptAppender, append_points};
pub use las::{ExtraBytes, LasHeader, Vlr, standard_value_type};
pub use read::{DEFAULT_CHUNK_POINTS, PointChunks, PointCloudReader, open_points, open_points_bytes, open_points_sparse};
pub use values::{Dimension, ValueType, Values};
pub use waveform::{WavePacketDescriptor, Waveform};
pub use write::{PlyEncoding, PointFormat, PointWriteOptions, PointWriter, from_dimensions, write_points, write_points_bytes};

use crate::Result;
use crate::crs::SpatialRef;
use serde::Serialize;

/// A point cloud in memory: columns of values, one per dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct PointCloud {
    /// The header of the file the points come from. Its point count and bounds describe the
    /// file, not necessarily the points read (see [`PointCloud::len`] and
    /// [`PointCloud::bounds`]).
    pub header: LasHeader,
    /// The horizontal CRS.
    pub crs: SpatialRef,
    /// The dimensions.
    pub dimensions: Vec<Dimension>,
}

impl PointCloud {
    /// A point cloud of these dimensions, with a LAS 1.4 point format 6 header with a 0.01 scale.
    pub fn new(dimensions: Vec<Dimension>, crs: SpatialRef) -> Self {
        Self { header: LasHeader::default(), crs, dimensions }
    }

    /// The number of points.
    pub fn len(&self) -> usize {
        self.dimensions.first().map_or(0, |d| d.values.len())
    }

    /// Whether there are no points.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A dimension by name.
    pub fn dim(&self, name: &str) -> Option<&Dimension> {
        self.dimensions.iter().find(|d| d.name == name)
    }

    /// A dimension by name, to modify it.
    pub fn dim_mut(&mut self, name: &str) -> Option<&mut Dimension> {
        self.dimensions.iter_mut().find(|d| d.name == name)
    }

    /// The dimension names.
    pub fn dimension_names(&self) -> Vec<&str> {
        self.dimensions.iter().map(|d| d.name.as_str()).collect()
    }

    /// The coordinates of a dimension as floats (`x`, `y` and `z` included).
    pub fn coords(&self, name: &str) -> Option<&[f64]> {
        match &self.dim(name)?.values {
            Values::F64(v) => Some(v),
            _ => None,
        }
    }

    /// The bounds of the points: `[min_x, min_y, min_z, max_x, max_y, max_z]`. `None` when there
    /// are no points.
    pub fn bounds(&self) -> Option<[f64; 6]> {
        let (x0, x1) = self.dim("x")?.values.min_max()?;
        let (y0, y1) = self.dim("y")?.values.min_max()?;
        let (z0, z1) = self.dim("z")?.values.min_max()?;
        Some([x0, y0, z0, x1, y1, z1])
    }

    /// The points where `keep` is true.
    pub fn select(&self, keep: &[bool]) -> PointCloud {
        PointCloud {
            header: self.header.clone(),
            crs: self.crs.clone(),
            dimensions: self.dimensions.iter().map(|d| d.with_values(d.values.select(keep))).collect(),
        }
    }

    /// The points from `start` to `end` (excluded).
    pub fn slice(&self, start: usize, end: usize) -> PointCloud {
        PointCloud {
            header: self.header.clone(),
            crs: self.crs.clone(),
            dimensions: self.dimensions.iter().map(|d| d.with_values(d.values.slice(start, end))).collect(),
        }
    }

    /// The points at these positions, in this order.
    pub fn take(&self, idx: &[usize]) -> PointCloud {
        PointCloud {
            header: self.header.clone(),
            crs: self.crs.clone(),
            dimensions: self.dimensions.iter().map(|d| d.with_values(d.values.take(idx))).collect(),
        }
    }

    /// The points that pass a filter.
    pub fn filter(&self, filter: &PointFilter) -> PointCloud {
        if filter.is_empty() {
            return self.clone();
        }
        self.select(&self.filter_mask(filter))
    }

    /// Whether each point passes a filter.
    pub fn filter_mask(&self, filter: &PointFilter) -> Vec<bool> {
        let get = |name: &str| self.dim(name);
        let (x, y, z) = (get("x"), get("y"), get("z"));
        let (rn, nr, class) = (get("return_number"), get("number_of_returns"), get("classification"));
        (0..self.len())
            .map(|i| {
                let v = |d: Option<&Dimension>| d.map_or(0.0, |d| d.values.get_f64(i));
                filter.keeps(&las::RecordCore {
                    x: v(x),
                    y: v(y),
                    z: v(z),
                    return_number: v(rn) as u8,
                    number_of_returns: v(nr) as u8,
                    classification: v(class) as u8,
                })
            })
            .collect()
    }

    /// Reprojects the `x` and `y` coordinates (and `z`, for transformations that change heights)
    /// into another CRS.
    pub fn reproject(&mut self, dst: &SpatialRef) -> Result<()> {
        if self.crs.is_unknown() {
            return Err(crate::Error::Crs("the point cloud has no CRS to reproject from".into()));
        }
        let t = crate::crs::Transformer::new(&self.crs, dst)?;
        self.transform_coords(&t)?;
        self.crs = dst.clone();
        // The file's scale and offset may no longer suit the coordinates: the writer picks new
        // ones from these.
        let bounds = self.bounds().map(|b| [b[0], b[1]]);
        reproject_header(&mut self.header, dst, bounds);
        Ok(())
    }

    /// Transforms the `x` and `y` coordinates, leaving the CRS and header alone.
    pub(crate) fn transform_coords(&mut self, t: &crate::crs::Transformer) -> Result<()> {
        let n = self.len();
        let xs = self.coords("x").map(<[f64]>::to_vec).unwrap_or_default();
        let ys = self.coords("y").map(<[f64]>::to_vec).unwrap_or_default();
        let mut nx = Vec::with_capacity(n);
        let mut ny = Vec::with_capacity(n);
        for i in 0..n.min(xs.len()).min(ys.len()) {
            let (a, b) = t.transform(xs[i], ys[i])?;
            nx.push(a);
            ny.push(b);
        }
        if let Some(d) = self.dim_mut("x") {
            d.values = Values::F64(nx);
        }
        if let Some(d) = self.dim_mut("y") {
            d.values = Values::F64(ny);
        }
        Ok(())
    }

    /// Appends the points of another cloud with the same dimensions (or any, when this one has
    /// none).
    pub fn append(&mut self, other: &PointCloud) -> Result<()> {
        if self.dimensions.is_empty() {
            self.dimensions = other.dimensions.clone();
            return Ok(());
        }
        for d in &mut self.dimensions {
            let o = other
                .dim(&d.name)
                .ok_or_else(|| crate::Error::InvalidArgument(format!("the point clouds differ: no '{}' in the other", d.name)))?;
            d.values.extend_from(&o.values);
        }
        Ok(())
    }

    /// The number of points of each classification, by class.
    pub fn classification_counts(&self) -> Vec<(u8, u64)> {
        let mut counts = [0u64; 256];
        if let Some(Values::U8(v)) = self.dim("classification").map(|d| &d.values) {
            for c in v {
                counts[*c as usize] += 1;
            }
        }
        counts.iter().enumerate().filter(|(_, n)| **n > 0).map(|(c, n)| (c as u8, *n)).collect()
    }

    /// The number of points of each return number, by return number.
    pub fn return_counts(&self) -> Vec<(u8, u64)> {
        let mut counts = [0u64; 256];
        if let Some(Values::U8(v)) = self.dim("return_number").map(|d| &d.values) {
            for c in v {
                counts[*c as usize] += 1;
            }
        }
        counts.iter().enumerate().filter(|(_, n)| **n > 0).map(|(c, n)| (c as u8, *n)).collect()
    }
}

/// A scale and offset for coordinates reprojected into `dst`, whose minimum x and y are `min`:
/// 1e-7 for geographic CRSs, otherwise the old scale (within 0.1 mm and 1 cm).
pub(crate) fn reproject_header(header: &mut LasHeader, dst: &SpatialRef, min: Option<[f64; 2]>) {
    let s = if dst.is_angular() { 1e-7 } else { header.scale[0].max(header.scale[1]).clamp(1e-4, 0.01) };
    header.scale[0] = s;
    header.scale[1] = s;
    if let Some(m) = min {
        header.offset[0] = round_offset(m[0], s);
        header.offset[1] = round_offset(m[1], s);
    }
}

/// An offset for coordinates starting at `min`: `min` rounded down to a round number, so that
/// the stored integers stay small.
pub(crate) fn round_offset(min: f64, scale: f64) -> f64 {
    if !min.is_finite() {
        return 0.0;
    }
    let unit = if scale >= 1e-3 { 1000.0 } else { 1.0 };
    (min / unit).floor() * unit
}

/// Which returns of each pulse to keep, as PDAL's `filters.returns` groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReturnKind {
    /// The first of several returns.
    First,
    /// The last of several returns.
    Last,
    /// Returns between the first and the last.
    Intermediate,
    /// The only return of a pulse.
    Only,
}

impl ReturnKind {
    /// Parses `first`, `last`, `intermediate` or `only`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "first" => Ok(ReturnKind::First),
            "last" => Ok(ReturnKind::Last),
            "intermediate" => Ok(ReturnKind::Intermediate),
            "only" | "single" => Ok(ReturnKind::Only),
            other => Err(crate::Error::InvalidArgument(format!("unknown return kind '{other}' (first, last, intermediate, only)"))),
        }
    }

    fn matches(self, rn: u8, nr: u8) -> bool {
        match self {
            ReturnKind::First => rn == 1 && nr > 1,
            ReturnKind::Last => rn == nr && nr > 1,
            ReturnKind::Intermediate => rn > 1 && rn < nr,
            ReturnKind::Only => nr == 1,
        }
    }
}

/// Which points to keep. An empty filter keeps all points.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointFilter {
    /// Keep the points in `[min_x, min_y, max_x, max_y]` (bounds included), in the file's CRS.
    pub bounds: Option<[f64; 4]>,
    /// Keep the points with `z` in `[min, max]`.
    pub z_range: Option<(f64, f64)>,
    /// Keep the points of these classifications. Empty means all.
    pub classes: Vec<u8>,
    /// Keep these returns of each pulse. Empty means all.
    pub returns: Vec<ReturnKind>,
}

impl PointFilter {
    /// Whether the filter keeps every point.
    pub fn is_empty(&self) -> bool {
        self.bounds.is_none() && self.z_range.is_none() && self.classes.is_empty() && self.returns.is_empty()
    }

    pub(crate) fn keeps(&self, p: &las::RecordCore) -> bool {
        if let Some([x0, y0, x1, y1]) = self.bounds
            && !(p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1)
        {
            return false;
        }
        if let Some((z0, z1)) = self.z_range
            && !(p.z >= z0 && p.z <= z1)
        {
            return false;
        }
        if !self.classes.is_empty() && !self.classes.contains(&p.classification) {
            return false;
        }
        if !self.returns.is_empty() && !self.returns.iter().any(|r| r.matches(p.return_number, p.number_of_returns)) {
            return false;
        }
        true
    }
}

/// How to read points.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointReadOptions {
    /// Which points to keep.
    pub filter: PointFilter,
    /// COPC: the deepest octree level to read (0 is the root). Other formats: ignored.
    pub max_level: Option<u32>,
    /// COPC: read the levels needed for this point spacing, in CRS units (as PDAL's
    /// `readers.copc` `resolution`). Other formats: ignored.
    pub resolution: Option<f64>,
    /// At most about this many points: COPC files are read down to the deepest octree level that
    /// keeps the count within it; other files are thinned by keeping every n-th point.
    pub max_points: Option<u64>,
}

/// Reads the points of a LAS, LAZ or COPC file (a path, URL, `s3://` object or zip archive
/// member).
pub fn read_points(path: &str, options: &PointReadOptions) -> Result<PointCloud> {
    open_points(path)?.read(options)
}

/// Whether a path names a point cloud, by its name: `.las`, `.laz` (and `.copc.laz`), `.ply`,
/// `.e57`, an EPT dataset's `ept.json`, or an `ept://` path.
pub fn is_point_cloud_path(path: &str) -> bool {
    if path.starts_with("ept://") {
        return true;
    }
    let name = crate::vfs::Location::parse(path).dataset_name().to_ascii_lowercase();
    let name = name.split(['?', '#']).next().unwrap_or_default();
    name.ends_with(".las") || name.ends_with(".laz") || name.ends_with(".ply") || name.ends_with(".e57") || name == "ept.json"
}

/// How to convert a point cloud.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConvertOptions {
    /// Which points to read.
    pub read: PointReadOptions,
    /// Assign this CRS to the points, without reprojecting (when the file has none or a wrong
    /// one).
    pub assign_srs: Option<SpatialRef>,
    /// Reproject `x` and `y` into this CRS.
    pub target_srs: Option<SpatialRef>,
    /// How to write point cloud outputs.
    pub write: PointWriteOptions,
    /// How to write vector outputs.
    pub layer: vector::ToLayerOptions,
    /// The points read and written at a time, for outputs written as the points are read
    /// ([`DEFAULT_CHUNK_POINTS`] when 0).
    pub chunk_points: usize,
    /// Add the points to the existing EPT dataset of the output ([`append_points`]) rather than
    /// write a new one.
    pub append: bool,
}

/// Converts a point cloud to another point cloud format (LAS, LAZ, COPC, PLY, E57, EPT) or to a
/// vector point layer (any vector format terrio writes, chosen by the output's extension),
/// filtering and reprojecting on the way.
///
/// Point cloud outputs are written as the points are read, in chunks of
/// [`ConvertOptions::chunk_points`], so that files larger than memory convert in bounded memory
/// (vector layers are written whole).
pub fn convert(src: &str, dst: &str, options: &ConvertOptions) -> Result<()> {
    let format = options.write.format.or_else(|| PointFormat::for_path(dst));
    if options.append {
        let w = &options.write;
        if w.ept_bounds.is_some()
            || w.ept_data_type != EptDataType::default()
            || w.point_format.is_some()
            || w.scale.is_some()
            || w.offset.is_some()
        {
            return Err(crate::Error::InvalidArgument(
                "an append keeps the EPT dataset's cube, data type, point format, scale and offset: \
                 --ept-bounds, --ept-data-type, --point-format, --scale and --offset apply to new datasets"
                    .into(),
            ));
        }
        #[cfg(not(target_family = "wasm"))]
        if format == Some(PointFormat::Ept) {
            return append_points(dst, &[src], options);
        }
        return Err(crate::Error::InvalidArgument(format!(
            "cannot append to '{dst}': points are appended to EPT datasets only (an ept.json or ept:// output)"
        )));
    }
    match format {
        // WebAssembly builds write E57 files whole.
        Some(PointFormat::E57) if cfg!(target_family = "wasm") => {}
        Some(_) => return convert_streaming(src, dst, options),
        None => {
            if crate::formats::VectorDriver::for_extension(dst).is_none() {
                return Err(crate::Error::InvalidArgument(format!(
                    "cannot tell the output format of '{dst}': use .las, .laz, .copc.laz, .ply, .e57, ept.json or a vector format"
                )));
            }
        }
    }
    let mut cloud = read_points(src, &options.read)?;
    if let Some(crs) = &options.assign_srs {
        cloud.crs = crs.clone();
    }
    if let Some(dst_srs) = &options.target_srs {
        cloud.reproject(dst_srs)?;
    }
    if format.is_some() {
        return write_points(&cloud, dst, &options.write);
    }
    let layer = vector::to_layer(&cloud, &options.layer)?;
    crate::write_vector(&[layer], dst, &Default::default())
}

/// Whether two paths are the same file (hard links included).
#[cfg(unix)]
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_: &std::path::Path, _: &std::path::Path) -> bool {
    false
}

/// The template of a reader's points, as converted (the CRS assigned or reprojected into), and
/// the transformer that reprojects them.
pub(crate) fn source_template(
    reader: &mut PointCloudReader,
    options: &ConvertOptions,
) -> Result<(PointCloud, Option<crate::crs::Transformer>)> {
    let mut template = reader.template()?;
    if let Some(crs) = &options.assign_srs {
        template.crs = crs.clone();
    }
    let transformer = match &options.target_srs {
        Some(dst_srs) => {
            if template.crs.is_unknown() {
                return Err(crate::Error::Crs("the point cloud has no CRS to reproject from".into()));
            }
            let t = crate::crs::Transformer::new(&template.crs, dst_srs)?;
            // The new scale and offset, from the header's bounds reprojected.
            let h = &template.header;
            let min = t.transform_bounds((h.min[0], h.min[1], h.max[0], h.max[1])).ok().map(|b| [b.0, b.1]);
            reproject_header(&mut template.header, dst_srs, min);
            template.crs = dst_srs.clone();
            Some(t)
        }
        None => None,
    };
    Ok((template, transformer))
}

/// Converts chunk by chunk.
fn convert_streaming(src: &str, dst: &str, options: &ConvertOptions) -> Result<()> {
    let mut reader = open_points(src)?;
    let (template, transformer) = source_template(&mut reader, options)?;
    let mut writer = PointWriter::create(dst, &template, &options.write)?;
    reader.start_chunks(&options.read, options.chunk_points)?;
    while let Some(mut chunk) = reader.next_chunk()? {
        if let Some(t) = &transformer {
            chunk.transform_coords(t)?;
        }
        writer.write(&chunk)?;
    }
    writer.finish_with_waveforms(&mut reader)?;
    // Points that keep their waveforms in a `.wdp` file next to the input keep them next to the
    // output (unless that is the same file).
    if let Some(wdp) = reader.waveform_file()
        && PointFormat::for_path(dst).is_some_and(|f| matches!(f, PointFormat::Las | PointFormat::Laz))
    {
        let target = std::path::Path::new(dst).with_extension("wdp");
        let same = match (std::fs::canonicalize(wdp), std::fs::canonicalize(&target)) {
            (Ok(a), Ok(b)) => a == b || same_file(&a, &b),
            _ => false,
        };
        if !same {
            std::fs::copy(wdp, target)?;
        }
    }
    Ok(())
}
