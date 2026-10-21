//! Point cloud summaries, like `pdal info --summary` and `lasinfo`.

use super::{PointCloudReader, PointReadOptions};
use crate::Result;
use crate::crs::{SpatialRef, Transformer};
use crate::info::CrsSummary;
use serde::Serialize;
use std::fmt::Write as _;

/// A point cloud summary.
#[derive(Debug, Clone, Serialize)]
pub struct PointCloudInfo {
    /// `LAS`, `LAZ` or `COPC`.
    pub driver: String,
    /// The LAS version, such as `1.4`.
    pub version: String,
    /// The point data record format (0 to 10).
    pub point_format: u8,
    /// The size of a point record, extra bytes included.
    pub point_record_length: u16,
    /// The number of points.
    pub point_count: u64,
    /// The number of points by return number, from the header (5 values, or 15 from LAS 1.4).
    pub points_by_return: Vec<u64>,
    /// The bounds from the header: `[min_x, min_y, min_z, max_x, max_y, max_z]`.
    pub bounds: [f64; 6],
    /// The horizontal bounds in WGS 84 longitude/latitude, when the CRS is known.
    pub wgs84_bounds: Option<[f64; 4]>,
    /// The scale of the stored X, Y and Z.
    pub scale: [f64; 3],
    /// The offset of the stored X, Y and Z.
    pub offset: [f64; 3],
    /// The horizontal CRS.
    pub crs: CrsSummary,
    /// The EPSG code of the vertical CRS, when declared.
    pub vertical_epsg: Option<u32>,
    /// The name of the vertical CRS, when declared.
    pub vertical_crs: Option<String>,
    /// The system identifier.
    pub system_identifier: String,
    /// The generating software.
    pub generating_software: String,
    /// The creation day of year and year.
    pub creation: [u16; 2],
    /// The file source id.
    pub file_source_id: u16,
    /// The global encoding bits.
    pub global_encoding: u16,
    /// The project id.
    pub guid: String,
    /// The dimensions.
    pub dimensions: Vec<DimensionInfo>,
    /// The variable-length records.
    pub vlrs: Vec<VlrInfo>,
    /// The extended variable-length records.
    pub evlrs: Vec<VlrInfo>,
    /// The COPC octree, for COPC files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copc: Option<CopcSummary>,
    /// Counts and ranges computed from the points, when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistics: Option<PointStatistics>,
}

/// A dimension's summary.
#[derive(Debug, Clone, Serialize)]
pub struct DimensionInfo {
    /// The name.
    pub name: String,
    /// The type, as NumPy names it.
    #[serde(rename = "type")]
    pub value_type: String,
    /// Whether it is an extra-bytes dimension.
    pub extra: bool,
}

/// A VLR's summary.
#[derive(Debug, Clone, Serialize)]
pub struct VlrInfo {
    /// The user id.
    pub user_id: String,
    /// The record id.
    pub record_id: u16,
    /// The description.
    pub description: String,
    /// The payload size.
    pub length: usize,
}

/// A COPC octree's summary.
#[derive(Debug, Clone, Serialize)]
pub struct CopcSummary {
    /// The center of the root cube.
    pub center: [f64; 3],
    /// Half the side of the root cube.
    pub halfsize: f64,
    /// The point spacing at the root.
    pub spacing: f64,
    /// The GPS time range.
    pub gpstime: [f64; 2],
    /// The number of nodes with points.
    pub node_count: usize,
    /// The depth of the octree: the deepest level with points, plus one.
    pub depth: usize,
    /// The number of points at each level.
    pub points_per_level: Vec<u64>,
}

/// Counts and ranges computed from the points.
#[derive(Debug, Clone, Serialize)]
pub struct PointStatistics {
    /// The number of points of each classification: `[class, count]`.
    pub classification_counts: Vec<(u8, u64)>,
    /// The number of points of each return number: `[return number, count]`.
    pub return_counts: Vec<(u8, u64)>,
    /// Each dimension's minimum, maximum and mean.
    pub dimensions: Vec<DimensionStatistics>,
}

/// A dimension's minimum, maximum and mean.
#[derive(Debug, Clone, Serialize)]
pub struct DimensionStatistics {
    /// The name.
    pub name: String,
    /// The minimum.
    pub min: f64,
    /// The maximum.
    pub max: f64,
    /// The mean.
    pub mean: f64,
}

/// Summarizes an open point cloud; `statistics` reads every point.
pub fn point_cloud_info(r: &mut PointCloudReader, statistics: bool) -> Result<PointCloudInfo> {
    let h = r.header().clone();
    let crs = r.crs().clone();
    let (vertical_epsg, vertical_crs) = r.vertical_crs();
    let bounds = [h.min[0], h.min[1], h.min[2], h.max[0], h.max[1], h.max[2]];
    let copc = match r.copc_info().copied() {
        Some(info) => {
            let nodes = r.copc_nodes()?;
            let depth = nodes.iter().map(|n| n.key.level as usize + 1).max().unwrap_or(0);
            let mut per_level = vec![0u64; depth];
            for n in &nodes {
                per_level[n.key.level as usize] += n.point_count.max(0) as u64;
            }
            Some(CopcSummary {
                center: info.center,
                halfsize: info.halfsize,
                spacing: info.spacing,
                gpstime: [info.gpstime_minimum, info.gpstime_maximum],
                node_count: nodes.len(),
                depth,
                points_per_level: per_level,
            })
        }
        None => None,
    };
    let statistics = if statistics { Some(statistics_of(r)?) } else { None };
    let vlr_info = |v: &super::Vlr| VlrInfo {
        user_id: v.user_id.clone(),
        record_id: v.record_id,
        description: v.description.clone(),
        // The waveform packets are not read with the header.
        length: if v.is_waveform_data() { h.waveform_data_size as usize } else { v.data.len() },
    };
    let standard = super::las::STANDARD_DIMENSIONS;
    Ok(PointCloudInfo {
        driver: r.format_name().to_string(),
        version: h.version_string(),
        point_format: h.point_format,
        point_record_length: h.point_record_length,
        point_count: h.point_count,
        points_by_return: h.points_by_return.clone(),
        bounds,
        wgs84_bounds: wgs84_bounds(&crs, bounds),
        scale: h.scale,
        offset: h.offset,
        crs: CrsSummary::new(&crs),
        vertical_epsg,
        vertical_crs,
        system_identifier: h.system_identifier.clone(),
        generating_software: h.generating_software.clone(),
        creation: [h.creation_day, h.creation_year],
        file_source_id: h.file_source_id,
        global_encoding: h.global_encoding,
        guid: h.guid.clone(),
        dimensions: r
            .dimensions()
            .into_iter()
            .map(|(name, t)| DimensionInfo { extra: !standard.contains(&name.as_str()), name, value_type: t.name().to_string() })
            .collect(),
        vlrs: h.vlrs.iter().map(vlr_info).collect(),
        evlrs: h.evlrs.iter().map(vlr_info).collect(),
        copc,
        statistics,
    })
}

/// Counts and ranges of every point, read in chunks.
fn statistics_of(r: &mut PointCloudReader) -> Result<PointStatistics> {
    let names: Vec<String> = r.dimensions().into_iter().map(|(n, _)| n).collect();
    let mut acc: Vec<(f64, f64, f64, u64)> = vec![(f64::INFINITY, f64::NEG_INFINITY, 0.0, 0); names.len()];
    let mut classes = [0u64; 256];
    let mut returns = [0u64; 256];
    r.start_chunks(&PointReadOptions::default(), 0)?;
    while let Some(chunk) = r.next_chunk()? {
        for (a, d) in acc.iter_mut().zip(&chunk.dimensions) {
            for v in d.to_f64() {
                if v.is_nan() {
                    continue;
                }
                a.0 = a.0.min(v);
                a.1 = a.1.max(v);
                a.2 += v;
                a.3 += 1;
            }
        }
        for (c, n) in chunk.classification_counts() {
            classes[c as usize] += n;
        }
        for (c, n) in chunk.return_counts() {
            returns[c as usize] += n;
        }
    }
    let counts = |a: &[u64; 256]| a.iter().enumerate().filter(|(_, n)| **n > 0).map(|(c, n)| (c as u8, *n)).collect();
    Ok(PointStatistics {
        classification_counts: counts(&classes),
        return_counts: counts(&returns),
        dimensions: names
            .into_iter()
            .zip(acc)
            .filter(|(_, a)| a.3 > 0)
            .map(|(name, a)| DimensionStatistics { name, min: a.0, max: a.1, mean: a.2 / a.3 as f64 })
            .collect(),
    })
}

fn wgs84_bounds(crs: &SpatialRef, b: [f64; 6]) -> Option<[f64; 4]> {
    if crs.is_unknown() || !(b[0] <= b[3] && b[1] <= b[4]) {
        return None;
    }
    let wgs84 = SpatialRef::from_epsg(4326).ok()?;
    let t = Transformer::new(crs, &wgs84).ok()?;
    let r = t.transform_bounds((b[0], b[1], b[3], b[4])).ok()?;
    Some([r.0, r.1, r.2, r.3])
}

impl PointCloudInfo {
    /// The summary as text.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "Driver: {} (LAS {}, point format {}, {} bytes per point)",
            self.driver, self.version, self.point_format, self.point_record_length
        );
        let _ = writeln!(s, "Points: {}", self.point_count);
        let returns: Vec<String> =
            self.points_by_return.iter().enumerate().filter(|(_, n)| **n > 0).map(|(i, n)| format!("{}: {n}", i + 1)).collect();
        if !returns.is_empty() {
            let _ = writeln!(s, "Points by return: {}", returns.join(", "));
        }
        let _ = writeln!(s, "CRS: {}", self.crs.line());
        if let Some(v) = &self.vertical_crs {
            let _ = writeln!(s, "Vertical CRS: {v}{}", self.vertical_epsg.map(|c| format!(" (EPSG:{c})")).unwrap_or_default());
        }
        let b = self.bounds;
        let _ = writeln!(s, "Bounds: ({}, {}, {}) - ({}, {}, {})", b[0], b[1], b[2], b[3], b[4], b[5]);
        if let Some(w) = self.wgs84_bounds {
            let _ = writeln!(s, "WGS 84 bounds: ({}, {}) - ({}, {})", w[0], w[1], w[2], w[3]);
        }
        let _ = writeln!(s, "Scale: ({}, {}, {})", self.scale[0], self.scale[1], self.scale[2]);
        let _ = writeln!(s, "Offset: ({}, {}, {})", self.offset[0], self.offset[1], self.offset[2]);
        let _ = writeln!(s, "System identifier: {}", self.system_identifier);
        let _ = writeln!(s, "Generating software: {}", self.generating_software);
        let _ = writeln!(s, "Created: day {} of {}", self.creation[0], self.creation[1]);
        let dims: Vec<String> = self.dimensions.iter().map(|d| format!("{} ({})", d.name, d.value_type)).collect();
        let _ = writeln!(s, "Dimensions: {}", dims.join(", "));
        let extra: Vec<&str> = self.dimensions.iter().filter(|d| d.extra).map(|d| d.name.as_str()).collect();
        if !extra.is_empty() {
            let _ = writeln!(s, "Extra dimensions: {}", extra.join(", "));
        }
        for (label, list) in [("VLR", &self.vlrs), ("EVLR", &self.evlrs)] {
            for v in list {
                let _ = writeln!(s, "{label}: {} {} \"{}\" ({} bytes)", v.user_id, v.record_id, v.description, v.length);
            }
        }
        if let Some(c) = &self.copc {
            let _ = writeln!(
                s,
                "COPC: {} nodes, depth {}, spacing {}, center ({}, {}, {}), halfsize {}",
                c.node_count, c.depth, c.spacing, c.center[0], c.center[1], c.center[2], c.halfsize
            );
            let levels: Vec<String> = c.points_per_level.iter().enumerate().map(|(l, n)| format!("{l}: {n}")).collect();
            let _ = writeln!(s, "COPC points per level: {}", levels.join(", "));
        }
        if let Some(st) = &self.statistics {
            let classes: Vec<String> = st.classification_counts.iter().map(|(c, n)| format!("{c} ({}): {n}", class_name(*c))).collect();
            let _ = writeln!(s, "Classifications: {}", classes.join(", "));
            let returns: Vec<String> = st.return_counts.iter().map(|(r, n)| format!("{r}: {n}")).collect();
            let _ = writeln!(s, "Return numbers: {}", returns.join(", "));
            for d in &st.dimensions {
                let _ = writeln!(s, "  {}: min={} max={} mean={:.6}", d.name, d.min, d.max, d.mean);
            }
        }
        s
    }
}

/// The ASPRS name of a classification.
pub fn class_name(c: u8) -> &'static str {
    match c {
        0 => "never classified",
        1 => "unassigned",
        2 => "ground",
        3 => "low vegetation",
        4 => "medium vegetation",
        5 => "high vegetation",
        6 => "building",
        7 => "low point",
        8 => "model key point",
        9 => "water",
        10 => "rail",
        11 => "road surface",
        12 => "overlap",
        13 => "wire guard",
        14 => "wire conductor",
        15 => "transmission tower",
        16 => "wire connector",
        17 => "bridge deck",
        18 => "high noise",
        19 => "overhead structure",
        20 => "ignored ground",
        21 => "snow",
        22 => "temporal exclusion",
        _ => "user defined",
    }
}
