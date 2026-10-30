// SPDX-FileCopyrightText: Copyright (c) 2018, Oslandia <infos at oslandia dot com>
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.12.2:
//     alg/marching_squares/square.h
//     alg/marching_squares/utility.h
//     alg/marching_squares/level_generator.h

//! Contour lines from a raster band. The marching-squares interpolation and nodata subdivision
//! follow GDAL 3.12's `alg/marching_squares` (MIT, Copyright 2018 Oslandia; see `NOTICE`).

use crate::{Error, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, RasterDataset, Result};
use std::collections::HashMap;
use wbvector::Coord;

/// Options for [`contour`]. Bands start at 1. Supply an interval, fixed levels, or both.
#[derive(Debug, Clone)]
pub struct ContourOptions {
    /// Band from 1; default 1.
    pub band: usize,
    /// Positive elevation interval. Levels are `offset + k * interval`.
    pub interval: Option<f64>,
    /// Offset for interval levels; default 0.
    pub offset: f64,
    /// Additional fixed elevation levels; sorted and deduplicated internally.
    pub levels: Vec<f64>,
    /// Override the dataset's nodata value.
    pub nodata: Option<f64>,
    /// Include declared nodata values as data. Non-finite samples are always excluded.
    pub ignore_nodata: bool,
    /// Put elevation in every vertex's Z coordinate.
    pub with_z: bool,
    /// Elevation field; default `ELEV`. Cannot be empty or `ID`.
    pub field_name: String,
    /// Output layer name; default `contour`.
    pub layer_name: String,
}

impl Default for ContourOptions {
    fn default() -> Self {
        Self {
            band: 1,
            interval: None,
            offset: 0.0,
            levels: Vec::new(),
            nodata: None,
            ignore_nodata: false,
            with_z: false,
            field_name: "ELEV".into(),
            layer_name: "contour".into(),
        }
    }
}

/// Extracts line strings with `ID` and elevation attributes, preserving the raster's CRS and
/// full affine geotransform. Values are interpolated between pixel centres, extending to pixel
/// edges at raster/nodata boundaries as GDAL does. Exact-level values use GDAL's 1e-6 perturbation
/// and saddle cells use its fixed connectivity. Feature ordering and line direction may differ
/// from GDAL's. Rasters and generated segments are held in memory; at most 100,000 levels are
/// allowed. Complex rasters and geolocation arrays are unsupported.
///
/// ```no_run
/// use terrio::ops::contour::{contour, ContourOptions};
/// let dem = terrio::open_raster("dem.tif")?;
/// let lines = contour(&dem, &ContourOptions { interval: Some(10.0), ..Default::default() })?;
/// terrio::write_vector(&[lines], "contours.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn contour(ds: &RasterDataset, options: &ContourOptions) -> Result<Layer> {
    super::sample::check_raster(ds)?;
    super::sample::check_band(ds, options.band)?;
    if options.field_name.trim().is_empty() || options.field_name.eq_ignore_ascii_case("ID") {
        return Err(Error::InvalidArgument("the elevation field must be nonempty and different from ID".into()));
    }
    if options.ignore_nodata && options.nodata.is_some() {
        return Err(Error::InvalidArgument("nodata and ignore_nodata cannot be used together".into()));
    }
    if !options.offset.is_finite()
        || options.levels.iter().any(|v| !v.is_finite())
        || options.interval.is_some_and(|v| !v.is_finite() || v <= 0.0)
    {
        return Err(Error::InvalidArgument(
            "contour levels and offset must be finite, and the interval must be positive and finite".into(),
        ));
    }
    if options.interval.is_none() && options.levels.is_empty() {
        return Err(Error::InvalidArgument("give an interval or at least one fixed contour level".into()));
    }
    let r = &ds.raster;
    let values = super::BandView::of(r, options.band - 1);
    let nodata = if options.ignore_nodata { None } else { options.nodata.or(ds.nodata()) };
    // GDALContourGenerateEx compares Float32 nodata at the band's stored precision.
    let nodata = nodata.map(|v| if r.data_type == crate::DataType::F32 { v as f32 as f64 } else { v });
    let valid = |v: f64| v.is_finite() && nodata.is_none_or(|nd| v != nd);
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for i in 0..r.cols * r.rows {
        let v = values.get(i);
        if valid(v) {
            minimum = minimum.min(v);
            maximum = maximum.max(v);
        }
    }
    let mut levels = options.levels.clone();
    if let Some(interval) = options.interval.filter(|_| minimum < maximum) {
        let mut first = ((minimum - options.offset) / interval).ceil();
        first = ((fudge(minimum, options.offset + first * interval) - options.offset) / interval).ceil();
        let mut end = ((maximum - options.offset) / interval).floor() + 1.0;
        end = ((fudge(maximum, options.offset + end * interval) - options.offset) / interval).floor() + 1.0;
        if !first.is_finite()
            || !end.is_finite()
            || first.abs() > (1u64 << 53) as f64
            || end.abs() > (1u64 << 53) as f64
            || end - first > 100_000.0
        {
            return Err(Error::InvalidArgument("the interval and offset produce too many or unrepresentable contour levels".into()));
        }
        for i in 0..(end - first).max(0.0) as usize {
            let v = options.offset + (first + i as f64) * interval;
            if !v.is_finite() {
                return Err(Error::InvalidArgument("contour level overflow".into()));
            }
            levels.push(v);
        }
    }
    levels.sort_by(f64::total_cmp);
    levels.dedup_by(|a, b| *a == *b);
    if levels.len() > 100_000 {
        return Err(Error::InvalidArgument("at most 100000 contour levels are supported".into()));
    }
    let sample = |row: isize, col: isize| {
        if row < 0 || col < 0 || row >= r.rows as isize || col >= r.cols as isize {
            return f64::NAN;
        }
        let v = values.get(row as usize * r.cols + col as usize);
        if valid(v) { v } else { f64::NAN }
    };
    let mut segments = vec![Vec::new(); levels.len()];
    for row in 0..=r.rows as isize {
        for col in -1..r.cols as isize {
            let (x, y) = (col as f64 + 0.5, row as f64 - 0.5);
            let square = [
                Point { x, y, v: sample(row - 1, col) },
                Point { x: x + 1.0, y, v: sample(row - 1, col + 1) },
                Point { x, y: y + 1.0, v: sample(row, col) },
                Point { x: x + 1.0, y: y + 1.0, v: sample(row, col + 1) },
            ];
            process_square(square, false, &levels, &mut segments);
        }
    }
    let mut out = Layer::new(options.layer_name.clone());
    out.geom_type = Some(GeometryType::LineString);
    ds.crs().apply_to_layer(&mut out);
    out.schema.add_field(FieldDef::new("ID", FieldType::Integer));
    out.schema.add_field(FieldDef::new(&options.field_name, FieldType::Float));
    for (level, edges) in levels.into_iter().zip(segments) {
        for line in join_segments(&edges) {
            let coords = line
                .into_iter()
                .map(|p| {
                    let (x, y) = super::apply_gt(&ds.geo_transform(), p.0, p.1);
                    if options.with_z { Coord::xyz(x, y, level) } else { Coord::xy(x, y) }
                })
                .collect();
            let id = out.features.len() as i64;
            out.features.push(Feature {
                fid: id as u64 + 1,
                geometry: Some(Geometry::LineString(coords)),
                attributes: vec![FieldValue::Integer(id), FieldValue::Float(level)],
            });
        }
    }
    Ok(out)
}

#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
    v: f64,
}
type Position = (f64, f64);
type Segment = (Position, Position);

fn fudge(value: f64, level: f64) -> f64 {
    if (level - value).abs() < 1e-6 { value + 1e-6 } else { value }
}

fn midpoint(a: Point, b: Point) -> Point {
    Point {
        x: (a.x + b.x) * 0.5,
        y: (a.y + b.y) * 0.5,
        v: if a.v.is_nan() {
            b.v
        } else if b.v.is_nan() {
            a.v
        } else {
            (a.v + b.v) * 0.5
        },
    }
}

fn process_square(p: [Point; 4], split: bool, levels: &[f64], segments: &mut [Vec<Segment>]) {
    let n = p.iter().filter(|p| p.v.is_nan()).count();
    if n == 4 {
        return;
    }
    if n > 0 {
        let center = Point {
            x: (p[0].x + p[3].x) * 0.5,
            y: (p[0].y + p[3].y) * 0.5,
            v: [p[2], p[0], p[3], p[1]].iter().map(|p| if p.v.is_nan() { 0.0 } else { p.v }).sum::<f64>() / (4 - n) as f64,
        };
        let (top, left, bottom, right) = (midpoint(p[0], p[1]), midpoint(p[0], p[2]), midpoint(p[2], p[3]), midpoint(p[1], p[3]));
        for (corner, child) in [
            (p[0], [p[0], top, left, center]),
            (p[1], [top, p[1], center, right]),
            (p[2], [left, center, p[2], bottom]),
            (p[3], [center, right, bottom, p[3]]),
        ] {
            if !corner.v.is_nan() {
                process_square(child, true, levels, segments);
            }
        }
        return;
    }
    let low = p.iter().map(|p| p.v).fold(f64::INFINITY, f64::min);
    let high = p.iter().map(|p| p.v).fold(f64::NEG_INFINITY, f64::max);
    if low == high {
        return;
    }
    let first = levels.partition_point(|&l| l < fudge(low, l));
    let end = levels.partition_point(|&l| l <= fudge(high, l));
    for i in first..end {
        let level = levels[i];
        // Case bits use GDAL's NW, SW, SE, NE order; p is stored as NW, NE, SW, SE.
        let bits = u8::from(level < fudge(p[0].v, level))
            | (u8::from(level < fudge(p[2].v, level)) << 1)
            | (u8::from(level < fudge(p[3].v, level)) << 2)
            | (u8::from(level < fudge(p[1].v, level)) << 3);
        let pairs: &[(usize, usize)] = match bits {
            0 | 15 => &[],
            1 => &[(3, 0)],
            2 => &[(0, 1)],
            4 => &[(1, 2)],
            8 => &[(2, 3)],
            3 => &[(3, 1)],
            6 => &[(0, 2)],
            12 => &[(1, 3)],
            9 => &[(2, 0)],
            14 => &[(0, 3)],
            13 => &[(1, 0)],
            11 => &[(2, 1)],
            7 => &[(3, 2)],
            5 | 10 => &[(0, 1), (2, 3)],
            _ => unreachable!(),
        };
        for &(a, b) in pairs {
            let a = interpolate(p, a, level, !split);
            let b = interpolate(p, b, level, !split);
            if a != b {
                segments[i].push((a, b));
            }
        }
    }
}

fn interpolate(p: [Point; 4], edge: usize, level: f64, split: bool) -> Position {
    let (mut a, mut b, horizontal) = match edge {
        0 => (p[2], p[0], false),
        1 => (p[2], p[3], true),
        2 => (p[3], p[1], false),
        _ => (p[0], p[1], true),
    };
    if split {
        let mid = midpoint(a, b);
        let (fa, fm) = (fudge(a.v, level), fudge(mid.v, level));
        if (fa < level && level < fm) || (fa > level && level > fm) {
            b = mid;
        } else {
            a = mid;
        }
    }
    let fa = fudge(a.v, level);
    let ratio = (level - fa) / (fudge(b.v, level) - fa);
    if horizontal { (a.x * (1.0 - ratio) + b.x * ratio, a.y) } else { (a.x, a.y * (1.0 - ratio) + b.y * ratio) }
}

fn key(p: Position) -> (u64, u64) {
    (if p.0 == 0.0 { 0 } else { p.0.to_bits() }, if p.1 == 0.0 { 0 } else { p.1.to_bits() })
}

fn join_segments(edges: &[Segment]) -> Vec<Vec<Position>> {
    let mut adjacency: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (i, &(a, b)) in edges.iter().enumerate() {
        adjacency.entry(key(a)).or_default().push(i);
        adjacency.entry(key(b)).or_default().push(i);
    }
    let mut used = vec![false; edges.len()];
    let mut out = Vec::new();
    // Open contours first, then closed rings. Iterate edges rather than the hash map so that
    // output order is deterministic across platforms and process hash seeds.
    for open in [true, false] {
        for &(a, b) in edges {
            for start in [a, b] {
                if open != (adjacency[&key(start)].len() == 1) {
                    continue;
                }
                let Some(&first) = adjacency[&key(start)].iter().find(|&&i| !used[i]) else { continue };
                let reverse = start == edges[first].1;
                let mut line = vec![start];
                let mut current = start;
                while let Some(&i) = adjacency[&key(current)].iter().find(|&&i| !used[i]) {
                    used[i] = true;
                    current = if current == edges[i].0 { edges[i].1 } else { edges[i].0 };
                    line.push(current);
                    if current == start {
                        break;
                    }
                }
                if reverse {
                    line.reverse();
                }
                out.push(line);
            }
        }
    }
    out
}
