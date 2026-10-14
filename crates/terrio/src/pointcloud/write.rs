//! Writing LAS, LAZ and COPC files.

use super::las::{Encoder, ExtraBytes, LASF_SPEC, LasHeader, Layout, Vlr, standard_record_length};
use super::{PointCloud, crs};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use std::io::{Cursor, Seek, Write};

/// A point cloud file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointFormat {
    /// LAS.
    Las,
    /// LAZ: LASzip-compressed LAS.
    Laz,
    /// COPC: a LAZ 1.4 file organized as an octree.
    Copc,
}

impl PointFormat {
    /// The format of a path, by its extension: `.copc.laz` is COPC, `.laz` LAZ, `.las` LAS.
    pub fn for_path(path: &str) -> Option<Self> {
        let name = crate::vfs::Location::parse(path).file_name().to_ascii_lowercase();
        if name.ends_with(".copc.laz") {
            Some(PointFormat::Copc)
        } else if name.ends_with(".laz") {
            Some(PointFormat::Laz)
        } else if name.ends_with(".las") {
            Some(PointFormat::Las)
        } else {
            None
        }
    }

    /// Parses a format name: `LAS`, `LAZ` or `COPC`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "LAS" => Some(PointFormat::Las),
            "LAZ" => Some(PointFormat::Laz),
            "COPC" => Some(PointFormat::Copc),
            _ => None,
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
}

/// Writes a point cloud to a file.
pub fn write_points(cloud: &PointCloud, path: &str, options: &PointWriteOptions) -> Result<()> {
    if !crate::vfs::Location::parse(path).is_local() {
        return Err(Error::Unsupported(format!("cannot write to '{path}': only local outputs are supported")));
    }
    let format = resolve_format(path, options)?;
    let file = std::fs::File::create(path).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}"))))?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
    write_to(cloud, &mut w, format, options)?;
    w.flush()?;
    Ok(())
}

/// Writes a point cloud to bytes; `name`'s extension chooses the format unless the options do.
pub fn write_points_bytes(cloud: &PointCloud, name: &str, options: &PointWriteOptions) -> Result<Vec<u8>> {
    let format = resolve_format(name, options)?;
    let mut c = Cursor::new(Vec::new());
    write_to(cloud, &mut c, format, options)?;
    Ok(c.into_inner())
}

fn resolve_format(path: &str, options: &PointWriteOptions) -> Result<PointFormat> {
    options
        .format
        .or_else(|| PointFormat::for_path(path))
        .ok_or_else(|| Error::InvalidArgument(format!("cannot tell the point cloud format of '{path}': use .las, .laz or .copc.laz")))
}

/// The point format to write a cloud in: `base` when it holds the cloud's GPS times, colours,
/// near infrared and wave packets, otherwise the smallest format of the same family (0 to 5, or 6
/// to 10 when `base` is one of those or near infrared is needed) that does. COPC files use 6 to
/// 8.
fn format_for(cloud: &PointCloud, base: u8, copc: bool) -> u8 {
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

fn write_to<W: Write + Seek + Send + Sync>(cloud: &PointCloud, w: &mut W, format: PointFormat, options: &PointWriteOptions) -> Result<()> {
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
        !v.is_laszip() && !crs::is_crs_vlr(v) && !v.is(LASF_SPEC, 4) && v.user_id != super::copc::COPC_USER_ID && !v.data.is_empty()
    };
    let mut vlrs: Vec<Vlr> = crs_vlrs;
    if !extra.is_empty() {
        let data = extra.iter().flat_map(|d| ExtraBytes::for_dimension(d).to_bytes()).collect();
        vlrs.push(Vlr::new(LASF_SPEC, 4, "Extra Bytes Record", data));
    }
    vlrs.extend(src.vlrs.iter().filter(keep).cloned());
    let evlrs: Vec<Vlr> = if minor >= 4 { src.evlrs.iter().filter(keep).cloned().collect() } else { Vec::new() };
    let mut global_encoding = src.global_encoding & 0x1; // the GPS time type
    if wkt_written {
        global_encoding |= crs::WKT_BIT;
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
        point_count: cloud.len() as u64,
        points_by_return: Vec::new(),
        scale,
        offset,
        min: [0.0; 3],
        max: [0.0; 3],
        start_of_waveform_data: 0,
        start_of_first_evlr: 0,
        vlrs,
        evlrs,
    };

    // The records.
    let n = cloud.len();
    let rl = layout.record_len;
    let encoder = Encoder::new(&layout, &cloud.dimensions, scale, offset);
    let mut records = vec![0u8; n * rl];
    for (i, rec) in records.chunks_exact_mut(rl).enumerate() {
        encoder.encode(i, rec)?;
    }
    if format == PointFormat::Copc {
        return super::copc::write_copc(
            w,
            header,
            &layout,
            &records,
            super::copc::CopcBuild { max_depth: options.copc_max_depth, leaf_points: options.copc_leaf_points },
        );
    }
    write_las(w, header, &layout, &records)
}

/// Writes a LAS or LAZ file of encoded records; the header's counts, bounds and offsets are
/// computed here.
fn write_las<W: Write + Seek + Send + Sync>(w: &mut W, mut header: LasHeader, layout: &Layout, records: &[u8]) -> Result<()> {
    let rl = layout.record_len;
    let n = records.len() / rl;
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    let slots = if header.version.1 >= 4 { 15 } else { 5 };
    let mut by_return = vec![0u64; slots];
    for rec in records.chunks_exact(rl) {
        let p = super::las::record_core(rec, header.point_format, &header.scale, &header.offset);
        for (k, v) in [p.x, p.y, p.z].into_iter().enumerate() {
            lo[k] = lo[k].min(v);
            hi[k] = hi[k].max(v);
        }
        let r = p.return_number as usize;
        if (1..=slots).contains(&r) {
            by_return[r - 1] += 1;
        }
    }
    if n == 0 {
        lo = [0.0; 3];
        hi = [0.0; 3];
    }
    header.min = lo;
    header.max = hi;
    header.points_by_return = by_return;
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
    header.write(w)?;
    match laz_vlr {
        Some(vlr) => {
            let mut c = laz::LasZipCompressor::new(&mut *w, vlr).map_err(|e| Error::InvalidArgument(format!("LAZ: {e}")))?;
            c.compress_many(records)?;
            c.done()?;
        }
        None => w.write_all(records)?,
    }
    let end_of_points = w.stream_position()?;
    if !header.evlrs.is_empty() {
        header.start_of_first_evlr = end_of_points;
        header.write_evlrs(w)?;
    }
    let end = w.stream_position()?;
    w.seek(std::io::SeekFrom::Start(start))?;
    header.write(w)?;
    w.seek(std::io::SeekFrom::Start(end))?;
    Ok(())
}

/// A point cloud of coordinates and other dimensions made in memory, with scale and offset suited
/// to the coordinates and the given CRS.
pub fn from_dimensions(dimensions: Vec<super::Dimension>, crs: SpatialRef) -> PointCloud {
    let mut cloud = PointCloud::new(dimensions, crs);
    cloud.header.point_format = format_for(&cloud, 6, false);
    let scale = if cloud.crs.is_geographic() { 1e-7 } else { 0.001 };
    cloud.header.scale = [scale, scale, 0.001];
    if let Some(b) = cloud.bounds() {
        cloud.header.offset = [super::round_offset(b[0], scale), super::round_offset(b[1], scale), super::round_offset(b[2], 0.001)];
    }
    cloud
}
