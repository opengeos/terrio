//! Writing point clouds: LAS, LAZ, COPC, PLY and E57, whole or in chunks.

use super::las::{
    Encoder, ExtraBytes, LASF_SPEC, LasHeader, Layout, Vlr, WAVEFORM_EXTERNAL_BIT, WAVEFORM_INTERNAL_BIT, standard_record_length,
};
use super::{PointCloud, crs};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use std::io::{Cursor, Seek, SeekFrom, Write};

/// A point cloud file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointFormat {
    /// LAS.
    Las,
    /// LAZ: LASzip-compressed LAS.
    Laz,
    /// COPC: a LAZ 1.4 file organized as an octree.
    Copc,
    /// PLY (Polygon File Format): ASCII or binary vertices.
    Ply,
    /// E57 (ASTM E2807): laser scans.
    E57,
    /// EPT (Entwine Point Tile): a directory of octree nodes, named by its `ept.json`.
    Ept,
}

impl PointFormat {
    /// The format of a path, by its extension: `.copc.laz` is COPC, `.laz` LAZ, `.las` LAS,
    /// `.ply` PLY, `.e57` E57, and an `ept.json` file (or an `ept://` path) EPT.
    pub fn for_path(path: &str) -> Option<Self> {
        if path.starts_with("ept://") {
            return Some(PointFormat::Ept);
        }
        let name = crate::vfs::Location::parse(path).file_name().to_ascii_lowercase();
        if name == "ept.json" {
            Some(PointFormat::Ept)
        } else if name.ends_with(".copc.laz") {
            Some(PointFormat::Copc)
        } else if name.ends_with(".laz") {
            Some(PointFormat::Laz)
        } else if name.ends_with(".las") {
            Some(PointFormat::Las)
        } else if name.ends_with(".ply") {
            Some(PointFormat::Ply)
        } else if name.ends_with(".e57") {
            Some(PointFormat::E57)
        } else {
            None
        }
    }

    /// Parses a format name: `LAS`, `LAZ`, `COPC`, `PLY`, `E57` or `EPT`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "LAS" => Some(PointFormat::Las),
            "LAZ" => Some(PointFormat::Laz),
            "COPC" => Some(PointFormat::Copc),
            "PLY" => Some(PointFormat::Ply),
            "E57" => Some(PointFormat::E57),
            "EPT" => Some(PointFormat::Ept),
            _ => None,
        }
    }

    /// Whether the format is LAS, LAZ or COPC.
    pub fn is_las(self) -> bool {
        matches!(self, PointFormat::Las | PointFormat::Laz | PointFormat::Copc)
    }
}

/// How PLY files store their vertices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlyEncoding {
    /// Text, one vertex per line (PDAL's `storage_mode` `ascii`, its default).
    #[default]
    Ascii,
    /// Little-endian binary.
    BinaryLittleEndian,
    /// Big-endian binary.
    BinaryBigEndian,
}

impl PlyEncoding {
    /// Parses `ascii`, `binary_little_endian` (or `binary`) or `binary_big_endian`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ascii" => Ok(PlyEncoding::Ascii),
            "binary" | "binary_little_endian" | "little_endian" => Ok(PlyEncoding::BinaryLittleEndian),
            "binary_big_endian" | "big_endian" => Ok(PlyEncoding::BinaryBigEndian),
            o => Err(Error::InvalidArgument(format!("unknown PLY encoding '{o}' (ascii, binary_little_endian, binary_big_endian)"))),
        }
    }
}

/// How to write a point cloud.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointWriteOptions {
    /// The format. By default, chosen from the output path's extension.
    pub format: Option<PointFormat>,
    /// The point data record format (0 to 10). By default, the source's; COPC files use 6, 7 or
    /// 8.
    pub point_format: Option<u8>,
    /// The LAS minor version (`1.x`). By default, the source's, raised when the point format
    /// needs it (1.3 for formats 4 and 5, 1.4 for 6 to 10).
    pub minor_version: Option<u8>,
    /// The scale of the stored X, Y and Z. By default, the source's.
    pub scale: Option<[f64; 3]>,
    /// The offset of the stored X, Y and Z. By default, the source's.
    pub offset: Option<[f64; 3]>,
    /// COPC: the deepest octree level. 16 by default.
    pub copc_max_depth: Option<u32>,
    /// COPC: the most points of a leaf node (a node whose points are not split further among
    /// children). 100 000 by default.
    pub copc_leaf_points: Option<u64>,
    /// COPC written in chunks ([`PointWriter`]): the most points held in memory at a time; more
    /// are spilled to a temporary file and built one octree node of a coarse level at a time.
    /// 4 000 000 by default.
    pub copc_memory_points: Option<u64>,
    /// PLY: the encoding. ASCII by default, as PDAL's `writers.ply`.
    pub ply_encoding: PlyEncoding,
    /// PLY, ASCII: the digits after the decimal point of floating-point values (PDAL's
    /// `precision`). By default, as many as needed to read the same value back.
    pub ply_precision: Option<usize>,
    /// The dimensions written (PLY and E57). All by default.
    pub dimensions: Vec<String>,
    /// EPT: how the nodes' points are stored. LASzip by default, as Entwine writes them.
    pub ept_data_type: super::EptDataType,
    /// EPT: the octree's cube is the cube around these bounds, `[min_x, min_y, min_z, max_x,
    /// max_y, max_z]`, rather than around the points (as Entwine's `--bounds`): points outside
    /// them are refused. Datasets that will take more points ([`super::append_points`]), such
    /// as one built from the first of several tiles, are given the bounds of all.
    pub ept_bounds: Option<[f64; 6]>,
}

/// Writes a point cloud to a file.
pub fn write_points(cloud: &PointCloud, path: &str, options: &PointWriteOptions) -> Result<()> {
    check_local(path)?;
    let format = resolve_format(path, options)?;
    if format == PointFormat::Ept {
        let mut w = PointWriter::create(path, cloud, options)?;
        w.write(cloud)?;
        return w.finish();
    }
    if format == PointFormat::E57 {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}"))))?;
        return super::e57::write_e57(cloud, file, options);
    }
    let file = std::fs::File::create(path).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}"))))?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
    write_to(cloud, &mut w, format, options)?;
    w.flush()?;
    Ok(())
}

/// Writes a point cloud to bytes; `name`'s extension chooses the format unless the options do.
pub fn write_points_bytes(cloud: &PointCloud, name: &str, options: &PointWriteOptions) -> Result<Vec<u8>> {
    let format = resolve_format(name, options)?;
    if format == PointFormat::Ept {
        return Err(Error::Unsupported("EPT datasets are directories: write them with write_points".into()));
    }
    let mut c = Cursor::new(Vec::new());
    if format == PointFormat::E57 {
        super::e57::write_e57(cloud, &mut c, options)?;
    } else {
        write_to(cloud, &mut c, format, options)?;
    }
    Ok(c.into_inner())
}

fn check_local(path: &str) -> Result<()> {
    if !crate::vfs::Location::parse(path).is_local() {
        return Err(Error::Unsupported(format!("cannot write to '{path}': only local outputs are supported")));
    }
    Ok(())
}

fn resolve_format(path: &str, options: &PointWriteOptions) -> Result<PointFormat> {
    let format = options.format.or_else(|| PointFormat::for_path(path)).ok_or_else(|| {
        Error::InvalidArgument(format!("cannot tell the point cloud format of '{path}': use .las, .laz, .copc.laz, .ply, .e57 or ept.json"))
    })?;
    if options.ept_bounds.is_some() && format != PointFormat::Ept {
        crate::warnings::warn(format!("'{path}': the EPT bounds apply to EPT datasets only, and are ignored"));
    }
    Ok(format)
}

/// The point format to write a cloud in: `base` when it holds the cloud's GPS times, colours,
/// near infrared and wave packets, otherwise the smallest format of the same family (0 to 5, or 6
/// to 10 when `base` is one of those or near infrared is needed) that does. COPC files use 6 to
/// 8.
pub(crate) fn format_for(cloud: &PointCloud, base: u8, copc: bool) -> u8 {
    let has = |n: &str| cloud.dim(n).is_some();
    let (gps, rgb, nir, wave) = (has("gps_time"), has("red"), has("nir"), has("wavepacket_index"));
    let fits = |f: u8| {
        let names: Vec<String> = super::las::standard_fields(f).into_iter().map(|s| s.name).collect();
        let holds = |n: &str| names.iter().any(|m| m == n);
        (!gps || holds("gps_time")) && (!rgb || holds("red")) && (!nir || holds("nir")) && (!wave || holds("wavepacket_index"))
    };
    let candidates: Vec<u8> = if copc {
        vec![6, 7, 8]
    } else if base >= 6 || nir {
        (6..=10).collect()
    } else {
        (0..=5).collect()
    };
    if candidates.contains(&base) && fits(base) {
        return base;
    }
    candidates.into_iter().find(|f| fits(*f)).unwrap_or(if copc { 8 } else { 10 })
}

/// The header and record layout of a LAS, LAZ or COPC file for points like `cloud`'s (its
/// header, CRS and dimensions; its points are not looked at). The counts, bounds and offsets are
/// left for the writer.
pub(crate) fn prepare(cloud: &PointCloud, format: PointFormat, options: &PointWriteOptions) -> Result<(LasHeader, Layout)> {
    let src = &cloud.header;
    let point_format = match (format, options.point_format) {
        (PointFormat::Copc, Some(f)) if !(6..=8).contains(&f) => {
            return Err(Error::InvalidArgument(format!("COPC files use point formats 6, 7 or 8, not {f}")));
        }
        (_, Some(f)) if f > 10 => return Err(Error::InvalidArgument(format!("unknown point format {f}"))),
        (_, Some(f)) => f,
        (_, None) => format_for(cloud, src.point_format, format == PointFormat::Copc),
    };
    let needed = match point_format {
        0..=3 => 0,
        4 | 5 => 3,
        _ => 4,
    };
    let mut minor = options.minor_version.unwrap_or(src.version.1).max(needed);
    if format == PointFormat::Copc {
        minor = 4;
    }
    if minor > 4 {
        return Err(Error::InvalidArgument(format!("LAS 1.{minor} is not supported (1.0 to 1.4)")));
    }
    // The dimensions written as extra bytes: those that are not standard.
    let extra: Vec<&super::Dimension> = cloud.dimensions.iter().filter(|d| !d.is_standard()).collect();
    let layout = Layout::with_extra(point_format, &extra);
    let scale = options.scale.unwrap_or(src.scale);
    if scale.iter().any(|s| !(s.is_finite() && *s > 0.0)) {
        return Err(Error::InvalidArgument(format!("invalid scale {scale:?}")));
    }
    let offset = options.offset.unwrap_or(src.offset);

    // The VLRs: the source's, without the ones this writer makes.
    let wkt = minor >= 4;
    let (crs_vlrs, wkt_written) = crs::crs_vlrs(&cloud.crs, wkt);
    let keep = |v: &&Vlr| {
        !v.is_laszip()
            && !crs::is_crs_vlr(v)
            && !v.is(LASF_SPEC, 4)
            && !v.is_waveform_data()
            && v.user_id != super::copc::COPC_USER_ID
            && !v.data.is_empty()
    };
    let mut vlrs: Vec<Vlr> = crs_vlrs;
    if !extra.is_empty() {
        let data = extra.iter().flat_map(|d| ExtraBytes::for_dimension(d).to_bytes()).collect();
        vlrs.push(Vlr::new(LASF_SPEC, 4, "Extra Bytes Record", data));
    }
    vlrs.extend(src.vlrs.iter().filter(keep).cloned());
    let mut evlrs: Vec<Vlr> = if minor >= 4 { src.evlrs.iter().filter(keep).cloned().collect() } else { Vec::new() };
    let mut global_encoding = src.global_encoding & 0x1; // the GPS time type
    if wkt_written {
        global_encoding |= crs::WKT_BIT;
    }
    // Waveforms: the points of formats 4, 5, 9 and 10 keep the waveform data packets they refer
    // to, written after them; or still refer to their `.wdp` file.
    let mut start_of_waveform_data = 0;
    let mut waveform_data_size = 0;
    let waves = super::las::standard_fields(point_format).iter().any(|f| f.name == "wavepacket_index")
        && cloud.dim("wavepacket_index").is_some()
        && format != PointFormat::Copc;
    if waves {
        let record = src.evlrs.iter().find(|v| v.is_waveform_data());
        let internal = src.global_encoding & WAVEFORM_EXTERNAL_BIT == 0;
        match record {
            Some(r) if !r.data.is_empty() => {
                evlrs.insert(0, r.clone());
                global_encoding |= WAVEFORM_INTERNAL_BIT;
                start_of_waveform_data = 1; // set when written
                waveform_data_size = r.data.len() as u64;
            }
            // The packets are still in the source file: a writer finished with
            // `PointWriter::finish_with_waveforms` copies them from it.
            Some(r) if internal && src.waveform_data_size > 0 => {
                evlrs.insert(0, r.clone());
                global_encoding |= WAVEFORM_INTERNAL_BIT;
                start_of_waveform_data = 1;
                waveform_data_size = src.waveform_data_size;
            }
            _ if !internal => global_encoding |= WAVEFORM_EXTERNAL_BIT,
            _ => {}
        }
    }
    let header = LasHeader {
        version: (1, minor),
        point_format,
        point_record_length: layout.record_len as u16,
        compressed: format != PointFormat::Las,
        file_source_id: src.file_source_id,
        global_encoding,
        guid: src.guid.clone(),
        system_identifier: if src.system_identifier.is_empty() { "terrio".into() } else { src.system_identifier.clone() },
        generating_software: format!("terrio {}", crate::VERSION),
        creation_day: src.creation_day,
        creation_year: src.creation_year,
        header_size: super::las::header_size_for((1, minor)) as u16,
        offset_to_point_data: 0,
        point_count: 0,
        points_by_return: Vec::new(),
        scale,
        offset,
        min: [0.0; 3],
        max: [0.0; 3],
        start_of_waveform_data,
        waveform_data_size,
        start_of_first_evlr: 0,
        vlrs,
        evlrs,
    };
    Ok((header, layout))
}

/// Encodes the points of a cloud into records of a layout.
pub(crate) fn encode(cloud: &PointCloud, layout: &Layout, scale: [f64; 3], offset: [f64; 3]) -> Result<Vec<u8>> {
    let rl = layout.record_len;
    let encoder = Encoder::new(layout, &cloud.dimensions, scale, offset);
    let mut records = vec![0u8; cloud.len() * rl];
    for (i, rec) in records.chunks_exact_mut(rl).enumerate() {
        encoder.encode(i, rec)?;
    }
    Ok(records)
}

fn write_to<W: Write + Seek + Send + Sync>(cloud: &PointCloud, w: &mut W, format: PointFormat, options: &PointWriteOptions) -> Result<()> {
    match format {
        PointFormat::Ply => return super::ply::write_ply(cloud, w, options),
        PointFormat::E57 => return Err(Error::Unsupported("E57 is written with write_points or write_points_bytes".into())),
        PointFormat::Ept => return Err(Error::Unsupported("EPT datasets are written with write_points".into())),
        _ => {}
    }
    let (header, layout) = prepare(cloud, format, options)?;
    if format == PointFormat::Copc {
        let records = encode(cloud, &layout, header.scale, header.offset)?;
        return super::copc::write_copc(
            w,
            header,
            &layout,
            &records,
            super::copc::CopcBuild { max_depth: options.copc_max_depth, leaf_points: options.copc_leaf_points, bounds: None },
        );
    }
    let mut lw = LasStreamWriter::new(w, header, layout)?;
    // Encode in chunks rather than all at once: the records of a large cloud take much memory.
    let n = cloud.len();
    let step = 1 << 20;
    if n <= step {
        lw.write_cloud(cloud)?;
    }
    let mut i = 0;
    while n > step && i < n {
        let j = (i + step).min(n);
        lw.write_cloud(&cloud.slice(i, j))?;
        i = j;
    }
    lw.finish(None)
}

/// Reads `len` bytes of the source's waveform data packet record from an offset (its header
/// included).
pub(crate) type WaveformSource<'a> = &'a mut dyn FnMut(u64, usize) -> Result<Vec<u8>>;

/// The most bytes of waveform data packets copied at a time.
const WAVEFORM_BLOCK: u64 = 16 << 20;

/// Writes LAS or LAZ records as they come: the header first (its counts and bounds rewritten at
/// the end), the points (compressed chunk by chunk for LAZ), then the waveform data packets and
/// the EVLRs.
pub(crate) struct LasStreamWriter<'a, W: Write + Seek + Send + Sync + 'a> {
    out: Option<W>,
    laz: Option<laz::LasZipCompressor<'a, W>>,
    header: LasHeader,
    layout: Layout,
    start: u64,
    count: u64,
    lo: [f64; 3],
    hi: [f64; 3],
    by_return: Vec<u64>,
}

impl<'a, W: Write + Seek + Send + Sync + 'a> LasStreamWriter<'a, W> {
    /// Writes the header (with zero counts) and the VLRs.
    pub fn new(mut w: W, mut header: LasHeader, layout: Layout) -> Result<Self> {
        let rl = layout.record_len;
        let laz_vlr = if header.compressed {
            let items = laz::LazItemRecordBuilder::default_for_point_format_id(
                header.point_format,
                (rl - standard_record_length(header.point_format)) as u16,
            )
            .map_err(|e| Error::InvalidArgument(format!("LAZ: {e}")))?;
            let vlr = laz::LazVlr::from_laz_items(items);
            let mut data = Vec::new();
            vlr.write_to(&mut data)?;
            header.vlrs.push(Vlr::new(super::las::LASZIP_USER_ID, super::las::LASZIP_RECORD_ID, "https://laszip.org", data));
            Some(vlr)
        } else {
            None
        };
        header.offset_to_point_data = u32::try_from(header.data_offset()).map_err(|_| Error::InvalidArgument("too many VLRs".into()))?;
        let start = w.stream_position()?;
        header.write(&mut w)?;
        let (out, laz) = match laz_vlr {
            Some(vlr) => (None, Some(laz::LasZipCompressor::new(w, vlr).map_err(|e| Error::InvalidArgument(format!("LAZ: {e}")))?)),
            None => (Some(w), None),
        };
        let slots = if header.version.1 >= 4 { 15 } else { 5 };
        Ok(Self {
            out,
            laz,
            header,
            layout,
            start,
            count: 0,
            lo: [f64::INFINITY; 3],
            hi: [f64::NEG_INFINITY; 3],
            by_return: vec![0; slots],
        })
    }

    /// Encodes and writes points.
    pub fn write_cloud(&mut self, cloud: &PointCloud) -> Result<()> {
        let records = encode(cloud, &self.layout, self.header.scale, self.header.offset)?;
        self.write_records(&records)
    }

    /// Writes encoded records.
    pub fn write_records(&mut self, records: &[u8]) -> Result<()> {
        let rl = self.layout.record_len;
        let h = &self.header;
        let slots = self.by_return.len();
        for rec in records.chunks_exact(rl) {
            let p = super::las::record_core(rec, h.point_format, &h.scale, &h.offset);
            for (k, v) in [p.x, p.y, p.z].into_iter().enumerate() {
                self.lo[k] = self.lo[k].min(v);
                self.hi[k] = self.hi[k].max(v);
            }
            let r = p.return_number as usize;
            if (1..=slots).contains(&r) {
                self.by_return[r - 1] += 1;
            }
        }
        self.count += (records.len() / rl) as u64;
        match (&mut self.laz, &mut self.out) {
            (Some(c), _) => c.compress_many(records)?,
            (None, Some(w)) => w.write_all(records)?,
            _ => {}
        }
        Ok(())
    }

    /// Writes the waveform data packets (copied in blocks from `waveforms` when the header has
    /// them without their bytes) and EVLRs, and rewrites the header.
    pub fn finish(mut self, waveforms: Option<WaveformSource<'_>>) -> Result<()> {
        let mut w = match (self.laz.take(), self.out.take()) {
            (Some(mut c), _) => {
                c.done()?;
                c.into_inner()
            }
            (None, Some(w)) => w,
            _ => return Err(Error::InvalidArgument("the writer is finished".into())),
        };
        let mut header = self.header;
        if self.count == 0 {
            self.lo = [0.0; 3];
            self.hi = [0.0; 3];
        }
        header.min = self.lo;
        header.max = self.hi;
        header.point_count = self.count;
        header.points_by_return = self.by_return;
        let end_of_points = w.stream_position()?;
        if !header.evlrs.is_empty() {
            header.start_of_first_evlr = end_of_points;
            if header.start_of_waveform_data != 0 {
                // The waveform data packet record comes first.
                header.start_of_waveform_data = end_of_points;
            }
            if header.version.1 < 4 {
                // LAS 1.3: the waveform data packet record is the only one after the points.
                header.evlrs.truncate(usize::from(header.start_of_waveform_data != 0));
            }
            let pending = header.start_of_waveform_data != 0 && header.evlrs[0].data.is_empty() && header.waveform_data_size > 0;
            match (pending, waveforms) {
                (true, Some(read)) => {
                    let size = header.waveform_data_size;
                    header.evlrs[0].write_evlr_header(&mut w, size)?;
                    let mut at = 0;
                    while at < size {
                        let n = (size - at).min(WAVEFORM_BLOCK);
                        w.write_all(&read(super::las::EVLR_HEADER_LEN as u64 + at, n as usize)?)?;
                        at += n;
                    }
                    let rest = Vec::from(&header.evlrs[1..]);
                    for v in &rest {
                        v.write(&mut w, true)?;
                    }
                }
                (true, None) => {
                    // No packets to write: the points no longer have them.
                    header.evlrs.remove(0);
                    header.start_of_waveform_data = 0;
                    header.waveform_data_size = 0;
                    header.global_encoding &= !WAVEFORM_INTERNAL_BIT;
                    header.write_evlrs(&mut w)?;
                }
                _ => header.write_evlrs(&mut w)?,
            }
            if header.evlrs.is_empty() {
                header.start_of_first_evlr = 0;
            }
        }
        let end = w.stream_position()?;
        w.seek(SeekFrom::Start(self.start))?;
        header.write(&mut w)?;
        w.seek(SeekFrom::Start(end))?;
        w.flush()?;
        Ok(())
    }
}

/// Writes points as they come, in chunks, to a LAS, LAZ, COPC, PLY or E57 file or an EPT dataset:
/// for point clouds larger than memory.
///
/// ```no_run
/// use terrio::pointcloud::{self, PointWriter};
///
/// let mut reader = pointcloud::open_points("huge.laz")?;
/// let template = reader.read(&pointcloud::PointReadOptions { max_points: Some(1), ..Default::default() })?;
/// let mut writer = PointWriter::create("huge.copc.laz", &template, &Default::default())?;
/// for chunk in reader.chunks(&Default::default(), 1_000_000)? {
///     writer.write(&chunk?)?;
/// }
/// writer.finish()?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub struct PointWriter {
    inner: WriterKind,
}

enum WriterKind {
    Las(Box<LasStreamWriter<'static, std::io::BufWriter<std::fs::File>>>),
    #[cfg(not(target_arch = "wasm32"))]
    Copc(Box<super::copc::CopcStreamWriter>),
    Ply(Box<super::ply::PlyStreamWriter<std::io::BufWriter<std::fs::File>>>),
    #[cfg(not(target_arch = "wasm32"))]
    E57(Box<super::e57::E57StreamWriter>),
}

impl std::fmt::Debug for PointWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PointWriter").finish_non_exhaustive()
    }
}

impl PointWriter {
    /// Creates a file for points like `template`'s: its header (version, point format, scale,
    /// offset, VLRs), CRS and dimensions; its points are not written. The chunks written then
    /// need the same dimensions (missing ones are written as zeros).
    ///
    /// EPT datasets are written in the directory of the path's `ept.json` (or of an `ept://`
    /// path), their octree built as COPC files' are, in bounded memory.
    pub fn create(path: &str, template: &PointCloud, options: &PointWriteOptions) -> Result<Self> {
        check_local(path)?;
        let format = resolve_format(path, options)?;
        let open = || -> Result<std::io::BufWriter<std::fs::File>> {
            let file = std::fs::File::create(path).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}"))))?;
            Ok(std::io::BufWriter::with_capacity(1 << 20, file))
        };
        let inner = match format {
            PointFormat::Las | PointFormat::Laz => {
                let (header, layout) = prepare(template, format, options)?;
                WriterKind::Las(Box::new(LasStreamWriter::new(open()?, header, layout)?))
            }
            #[cfg(not(target_arch = "wasm32"))]
            PointFormat::Copc => {
                let (header, layout) = prepare(template, format, options)?;
                let build =
                    super::copc::CopcBuild { max_depth: options.copc_max_depth, leaf_points: options.copc_leaf_points, bounds: None };
                WriterKind::Copc(Box::new(super::copc::CopcStreamWriter::new(
                    super::copc::OctreeTarget::Copc(open()?),
                    header,
                    layout,
                    build,
                    options.copc_memory_points,
                )?))
            }
            #[cfg(target_arch = "wasm32")]
            PointFormat::Copc => return Err(Error::Unsupported("COPC files cannot be written in chunks in WebAssembly".into())),
            PointFormat::Ply => WriterKind::Ply(Box::new(super::ply::PlyStreamWriter::new(open()?, template, options)?)),
            #[cfg(not(target_arch = "wasm32"))]
            PointFormat::E57 => {
                // The dimensions checked before the file is created (and an existing one emptied).
                super::e57::plan_e57(template, &options.dimensions)?;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)
                    .map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}"))))?;
                WriterKind::E57(Box::new(super::e57::E57StreamWriter::new(file, template, options)?))
            }
            #[cfg(not(target_arch = "wasm32"))]
            PointFormat::Ept => {
                // The octree's records: LAS ones of the source's point format (without waveforms,
                // which refer to the source file).
                let mut opts = options.clone();
                if opts.point_format.is_none() {
                    let f = format_for(template, template.header.point_format, false);
                    opts.point_format = Some(match f {
                        4 => 1,
                        5 => 3,
                        9 => 6,
                        10 => 8,
                        f => f,
                    });
                }
                let (mut header, layout) = prepare(template, PointFormat::Laz, &opts)?;
                header.evlrs.retain(|v| !v.is_waveform_data());
                header.start_of_waveform_data = 0;
                header.waveform_data_size = 0;
                let dir = super::ept_write::dataset_dir(path);
                let target = super::ept_write::EptTarget::new(dir, options.ept_data_type, template.crs.clone())?;
                let build = super::copc::CopcBuild {
                    max_depth: options.copc_max_depth,
                    leaf_points: options.copc_leaf_points,
                    bounds: options.ept_bounds,
                };
                WriterKind::Copc(Box::new(super::copc::CopcStreamWriter::new(
                    super::copc::OctreeTarget::Ept(target),
                    header,
                    layout,
                    build,
                    options.copc_memory_points,
                )?))
            }
            #[cfg(target_arch = "wasm32")]
            PointFormat::E57 | PointFormat::Ept => {
                return Err(Error::Unsupported("E57 files and EPT datasets cannot be written in chunks in WebAssembly".into()));
            }
        };
        Ok(Self { inner })
    }

    /// Writes points.
    pub fn write(&mut self, cloud: &PointCloud) -> Result<()> {
        match &mut self.inner {
            WriterKind::Las(w) => w.write_cloud(cloud),
            #[cfg(not(target_arch = "wasm32"))]
            WriterKind::Copc(w) => w.write_cloud(cloud),
            WriterKind::Ply(w) => w.write_cloud(cloud),
            #[cfg(not(target_arch = "wasm32"))]
            WriterKind::E57(w) => w.write_cloud(cloud),
        }
    }

    /// Finishes the file: the counts, bounds and (COPC) the octree.
    pub fn finish(self) -> Result<()> {
        match self.inner {
            WriterKind::Las(w) => w.finish(None),
            #[cfg(not(target_arch = "wasm32"))]
            WriterKind::Copc(w) => w.finish(),
            WriterKind::Ply(w) => w.finish(),
            #[cfg(not(target_arch = "wasm32"))]
            WriterKind::E57(w) => w.finish(),
        }
    }
}

impl PointWriter {
    /// Finishes the file, copying the waveform data packets the points refer to from the file
    /// they are read from (in blocks: the packets are not held in memory), when the template was
    /// that file's ([`super::PointCloudReader::template`]).
    pub fn finish_with_waveforms(self, reader: &mut super::PointCloudReader) -> Result<()> {
        match self.inner {
            WriterKind::Las(w) => w.finish(Some(&mut |offset, len| reader.waveform_bytes(offset, len))),
            _ => self.finish(),
        }
    }
}

/// A point cloud of coordinates and other dimensions made in memory, with scale and offset suited
/// to the coordinates and the given CRS.
pub fn from_dimensions(dimensions: Vec<super::Dimension>, crs: SpatialRef) -> PointCloud {
    let mut cloud = PointCloud::new(dimensions, crs);
    cloud.header.point_format = format_for(&cloud, 6, false);
    let scale = if cloud.crs.is_angular() { 1e-7 } else { 0.001 };
    cloud.header.scale = [scale, scale, 0.001];
    if let Some(b) = cloud.bounds() {
        cloud.header.offset = [super::round_offset(b[0], scale), super::round_offset(b[1], scale), super::round_offset(b[2], 0.001)];
    }
    cloud
}
