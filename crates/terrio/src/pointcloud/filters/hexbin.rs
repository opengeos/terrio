// SPDX-FileCopyrightText: Copyright (c) 2025, Hobu, Inc. (howard@hobu.co)
// SPDX-FileCopyrightText: Copyright (c) 2013, Andrew Bell (andrew.bell.ia@gmail.com)
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   PDAL (vendored hexer) fa80ee0a922716ad5c4f86de4b8ba26e94ae294e:
//     filters/private/hexer/HexGrid.cpp
//     filters/private/hexer/BaseGrid.cpp
//     filters/private/hexer/HexId.hpp
//   PDAL fa80ee0a922716ad5c4f86de4b8ba26e94ae294e:
//     filters/HexBinFilter.cpp

//! The boundary and density of points on a grid of hexagons, as PDAL's `filters.hexbin`.
//!
//! The points are counted in flat-topped hexagons of a given edge length (PDAL's
//! `edge_length`, or its older name `edge_size`; when it is 0, the hexagons' height is
//! estimated from the first `sample_size` points as `threshold` times their mean distance to the
//! next point, as PDAL estimates it); the hexagons with at least `threshold` points are "dense",
//! and the boundary is the union of the dense hexagons, traced as polygons with holes (and
//! islands in holes as polygons of their own). As PDAL's, the grid is anchored at the first
//! point (it is the lower-left corner of its hexagon), and, with `smooth`, the boundary is
//! simplified (topology-preserving Douglas-Peucker with a tolerance of `1.1 × h / 2`, `h` the
//! hexagons' height `√3 × edge_length`) and its polygons and holes smaller than `6 ×
//! tolerance²` (or `hole_cull_area_tolerance`) are dropped. The boundary's coordinates are
//! rounded to `precision` decimals first, as PDAL reads them back from hexer's text.
//!
//! Points are assigned to hexagons as hexer (the library PDAL vendors) assigns them, a port of
//! its `HexGrid::findHexagon`, and the hexagons' vertices use hexer's arithmetic (its column
//! width is `3 / (2 × 1.732050808) × h`), so that the hexagons and their counts are PDAL's.
//! hexer's assignment is not exactly the hexagons' tessellation: in the strips where two
//! columns of hexagons interlock, it takes the half rows of points below the origin's y for the
//! other half rows (it truncates rather than floors their index), so that some points there
//! are counted in a hexagon next to the one around them.
//!
//! The boundary is traced on an integer lattice of hexagon vertices rather than by hexer's path
//! walk: in a hexagonal grid three hexagons meet at every vertex, so every vertex of the boundary
//! has one boundary edge in and one out, and the rings never touch. The polygons and holes are
//! hexer's, in hexer's order and starting at hexer's vertices (which smoothing depends on), but
//! holes run clockwise (hexer writes them counter-clockwise, as its exteriors). The area and
//! density are those of the hexagons, before smoothing, as PDAL's. On geographic coordinates
//! the area is in square degrees (PDAL measures it in the UTM zone of the points). H3 grids are
//! not implemented.

use crate::crs::SpatialRef;
use crate::pointcloud::PointCloud;
use crate::{Error, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

/// hexer's √3, with which it computes its columns' width.
const HEXER_SQRT_3: f64 = 1.732050808;
/// The farthest a point may be from the grid's origin, in hexagon heights along x or y: the
/// columns and rows stay within 2⁴⁸ (hexer's are `int`s), so that their arithmetic cannot
/// overflow and their corners' lattice coordinates are exact in `f64`.
const MAX_OFFSET: f64 = (1u64 << 47) as f64;
/// The decimals beyond which rounding is a no-op: every `f64` is a multiple of 2⁻¹⁰⁷⁴, whose
/// decimal expansion has 1074 decimals.
const MAX_DECIMALS: usize = 1074;

/// How to bin points into hexagons (PDAL's `filters.hexbin`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HexbinOptions {
    /// The length of the hexagons' edges (PDAL's `edge_length`, or `edge_size`). 0 (the
    /// default) estimates it from the first `sample_size` points.
    pub edge_length: f64,
    /// The points the edge length is estimated from. 5000 by default.
    pub sample_size: usize,
    /// The fewest points of a hexagon in the boundary. 15 by default.
    pub threshold: u64,
    /// Simplify the boundary and drop its small polygons and holes. True by default.
    pub smooth: bool,
    /// Simplify preserving topology, as GEOS's `TopologyPreservingSimplifier` (the only
    /// simplification implemented). True by default.
    pub preserve_topology: bool,
    /// With `smooth`, polygons and holes smaller than this are dropped. By default, `6 ×
    /// tolerance²`.
    pub hole_cull_area_tolerance: Option<f64>,
    /// The grid's origin: the lower-left corner of a hexagon. By default, the first point, as
    /// PDAL anchors it.
    pub origin: Option<[f64; 2]>,
    /// The decimals of the boundary's coordinates (hexer writes the boundary with them, and PDAL
    /// reads it back before smoothing it). 8 by default.
    pub precision: usize,
}

impl Default for HexbinOptions {
    fn default() -> Self {
        Self {
            edge_length: 0.0,
            sample_size: 5000,
            threshold: 15,
            smooth: true,
            preserve_topology: true,
            hole_cull_area_tolerance: None,
            origin: None,
            precision: 8,
        }
    }
}

/// A hexagon with points: its column and row in the grid and its point count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hexagon {
    /// The column (from the origin's hexagon, 0; increasing with x).
    pub column: i64,
    /// The row (from the origin's hexagon, 0; increasing with y). Odd columns are half a
    /// hexagon higher than even ones.
    pub row: i64,
    /// The points in the hexagon.
    pub count: u64,
}

/// The result of binning points into hexagons.
#[derive(Debug, Clone, PartialEq)]
pub struct Hexbin {
    /// The union of the hexagons with at least `threshold` points, smoothed with `smooth`: a
    /// multipolygon (with no polygon when no hexagon is dense enough).
    pub boundary: Geometry,
    /// The hexagons with at least `threshold` points, by column then row.
    pub hexagons: Vec<Hexagon>,
    /// The points binned.
    pub point_count: u64,
    /// The hexagons' edge length: the one given, or the one estimated (0 without points).
    pub edge_length: f64,
    /// Whether the edge length was estimated from the points.
    pub estimated: bool,
    /// The threshold.
    pub threshold: u64,
    /// The grid's origin.
    pub origin: [f64; 2],
    /// The area of the dense hexagons (the boundary before smoothing, as PDAL's `area`), in the
    /// CRS's units squared.
    pub area: f64,
    /// Points per unit area: all the points (those of sparse hexagons included, as PDAL counts
    /// them) over `area`; 0 without a boundary.
    pub density: f64,
    /// The mean spacing of the points, `sqrt(1 / density)`; 0 without a boundary.
    pub avg_pt_spacing: f64,
    /// The points' CRS.
    pub crs: SpatialRef,
}

/// Bins points into hexagons, chunk after chunk, as PDAL's streamed `filters.hexbin` does.
#[derive(Debug, Clone)]
pub struct HexBinner {
    options: HexbinOptions,
    /// The hexagons' height (between their flat sides), once known.
    height: Option<f64>,
    origin: Option<[f64; 2]>,
    /// The points kept to estimate the height from.
    samples: Vec<[f64; 2]>,
    counts: HashMap<(i64, i64), u64>,
    points: u64,
    crs: Option<SpatialRef>,
}

impl HexBinner {
    /// A binner with these options.
    pub fn new(options: &HexbinOptions) -> Result<Self> {
        if !(options.edge_length.is_finite() && options.edge_length >= 0.0) {
            return Err(Error::InvalidArgument("the hexagons' edge length must be positive (or 0 to estimate it)".into()));
        }
        if options.edge_length == 0.0 && options.sample_size == 0 {
            return Err(Error::InvalidArgument("the sample size must be positive to estimate the edge length".into()));
        }
        if options.threshold == 0 {
            return Err(Error::InvalidArgument("the threshold must be at least 1".into()));
        }
        if options.smooth && !options.preserve_topology {
            return Err(Error::InvalidArgument("smoothing without preserving topology is not implemented".into()));
        }
        if options.hole_cull_area_tolerance.is_some_and(|a| a.is_nan() || a < 0.0) {
            return Err(Error::InvalidArgument("the hole cull area tolerance must be a non-negative number".into()));
        }
        if let Some([x, y]) = options.origin
            && !(x.is_finite() && y.is_finite())
        {
            return Err(Error::InvalidArgument("the origin must be finite".into()));
        }
        let height = (options.edge_length > 0.0).then(|| options.edge_length * 3f64.sqrt());
        Ok(Self { options: *options, height, origin: options.origin, samples: Vec::new(), counts: HashMap::new(), points: 0, crs: None })
    }

    /// Counts the points of a cloud (the result's CRS is the first cloud's, unless set).
    pub fn add(&mut self, cloud: &PointCloud) -> Result<()> {
        if self.crs.is_none() {
            self.crs = Some(cloud.crs.clone());
        }
        if cloud.is_empty() {
            return Ok(());
        }
        let get = |n: &str| cloud.coords(n).ok_or_else(|| Error::InvalidArgument(format!("the points have no {n}")));
        let (xs, ys) = (get("x")?, get("y")?);
        for (&x, &y) in xs.iter().zip(ys) {
            self.add_xy(x, y)?;
        }
        Ok(())
    }

    /// Sets the CRS of the result (by default, the first cloud's).
    pub fn set_crs(&mut self, crs: SpatialRef) {
        self.crs = Some(crs);
    }

    /// Counts a point (points with a non-finite x or y count towards the density only).
    pub fn add_xy(&mut self, x: f64, y: f64) -> Result<()> {
        self.points += 1;
        if !(x.is_finite() && y.is_finite()) {
            return Ok(());
        }
        let Some(height) = self.height else {
            self.samples.push([x, y]);
            if self.samples.len() >= self.options.sample_size {
                self.flush_samples()?;
            }
            return Ok(());
        };
        let [x0, y0] = *self.origin.get_or_insert([x, y]);
        let (u, v) = (x - x0, y - y0);
        if !(u.abs() <= MAX_OFFSET * height && v.abs() <= MAX_OFFSET * height) {
            return Err(Error::InvalidArgument(format!(
                "the point ({x}, {y}) is too far from the grid's origin ({x0}, {y0}) for hexagons of height {height}"
            )));
        }
        let key = locate(u, v, height);
        *self.counts.entry(key).or_insert(0) += 1;
        Ok(())
    }

    /// Estimates the height from the samples, as hexer's `computeHexSize` (`threshold` times the
    /// mean distance from a point to the next, the sum divided by the number of points), then
    /// counts them.
    fn flush_samples(&mut self) -> Result<()> {
        let samples = std::mem::take(&mut self.samples);
        if samples.is_empty() {
            return Ok(());
        }
        let dist: f64 = samples.windows(2).map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1])).sum();
        let height = self.options.threshold as f64 * dist / samples.len() as f64;
        if !(height.is_finite() && height > 0.0) {
            return Err(Error::InvalidArgument("cannot estimate the edge length: the points sampled are all at one place; give it".into()));
        }
        self.height = Some(height);
        self.points -= samples.len() as u64;
        for [x, y] in samples {
            self.add_xy(x, y)?;
        }
        Ok(())
    }

    /// The boundary and the dense hexagons.
    pub fn finish(mut self) -> Result<Hexbin> {
        if self.height.is_none() {
            self.flush_samples()?;
        }
        let o = self.options;
        let h = self.height.unwrap_or(0.0);
        let origin = self.origin.unwrap_or([0.0, 0.0]);
        let mut hexagons: Vec<Hexagon> =
            self.counts.iter().filter(|(_, n)| **n >= o.threshold).map(|(&(column, row), &count)| Hexagon { column, row, count }).collect();
        hexagons.sort_by_key(|h| (h.column, h.row));
        let dense: HashSet<(i64, i64)> = hexagons.iter().map(|h| center(h.column, h.row)).collect();
        let to_xy = |p: Corner| vertex(origin, h, (p.1, p.2), p.3);
        let polygons: Vec<(Ring, Vec<Ring>)> = trace(&dense)
            .into_iter()
            .map(|(ext, holes)| {
                (Ring(ext.into_iter().map(to_xy).collect()), holes.into_iter().map(|r| Ring(r.into_iter().map(to_xy).collect())).collect())
            })
            .collect();
        // PDAL reads the boundary back from hexer's text, with `precision` decimals. The area is
        // the hexagons', before smoothing, as PDAL's.
        let mut boundary = round_coords(Geometry::MultiPolygon(polygons), o.precision);
        let area: f64 = match &boundary {
            Geometry::MultiPolygon(ps) => ps.iter().map(|(e, hs)| ring_area(e) - hs.iter().map(ring_area).sum::<f64>()).sum(),
            _ => 0.0,
        };
        if o.smooth && !dense.is_empty() {
            let tolerance = 1.1 * h / 2.0;
            let cull = o.hole_cull_area_tolerance.unwrap_or(6.0 * tolerance * tolerance);
            // Simplified with the holes counter-clockwise, as hexer writes them, from the same
            // first vertex; then clockwise again.
            let hexer_holes = reverse_holes(boundary);
            boundary = reverse_holes(cull_small(crate::ops::simplify::simplify_preserve_topology(&hexer_holes, tolerance)?, cull));
        }
        let density = if area > 0.0 { self.points as f64 / area } else { 0.0 };
        let avg_pt_spacing = if density > 0.0 { (1.0 / density).sqrt() } else { 0.0 };
        Ok(Hexbin {
            boundary,
            hexagons,
            point_count: self.points,
            edge_length: if o.edge_length > 0.0 { o.edge_length } else { h / 3f64.sqrt() },
            estimated: o.edge_length == 0.0,
            threshold: o.threshold,
            origin,
            area,
            density,
            avg_pt_spacing,
            crs: self.crs.unwrap_or_default(),
        })
    }
}

/// Bins points into hexagons, as PDAL's `filters.hexbin`: the boundary of the hexagons with at
/// least `threshold` points, and those hexagons with their point counts.
pub fn hexbin(cloud: &PointCloud, options: &HexbinOptions) -> Result<Hexbin> {
    let mut binner = HexBinner::new(options)?;
    binner.add(cloud)?;
    binner.finish()
}

/// A corner of a hexagon, `CORNERS[corner]`, computed as hexer computes it: the hexagon's
/// lower-left corner (`column × width`, `row × height`, half a height higher in odd columns),
/// plus the corner's offset from it, plus the origin.
fn vertex(origin: [f64; 2], height: f64, (column, row): (i64, i64), corner: usize) -> Coord {
    let width = 3.0 / (2.0 * HEXER_SQRT_3) * height;
    // hexer's offsets from the lower-left corner, in the order of CORNERS: lower-left,
    // lower-right, right, upper-right, upper-left, left.
    let offsets = [
        (0.0, 0.0),
        (2.0 * width / 3.0, 0.0),
        (width, height / 2.0),
        (2.0 * width / 3.0, height),
        (0.0, height),
        (-width / 3.0, height / 2.0),
    ];
    let mut y = row as f64 * height;
    if column.rem_euclid(2) == 1 {
        y += height / 2.0;
    }
    let (dx, dy) = offsets[corner];
    Coord::xy((column as f64 * width + dx) + origin[0], (y + dy) + origin[1])
}

impl Hexbin {
    /// The hexagons' height (between their flat sides), `√3 × edge_length` (PDAL's
    /// `estimated_edge`).
    pub fn height(&self) -> f64 {
        self.edge_length * 3f64.sqrt()
    }

    /// A hexagon's polygon, counter-clockwise from its lower-left corner.
    pub fn hexagon_polygon(&self, column: i64, row: i64) -> Geometry {
        let ring = (0..6).map(|k| vertex(self.origin, self.height(), (column, row), k));
        Geometry::Polygon { exterior: Ring(ring.collect()), interiors: Vec::new() }
    }

    /// The boundary as well-known text, its coordinates with `precision` significant digits, as
    /// PDAL writes its `boundary` (8 by default), trailing zeros dropped: `MULTIPOLYGON EMPTY`
    /// without a polygon.
    pub fn boundary_wkt(&self, precision: usize) -> String {
        let polygons = match &self.boundary {
            Geometry::MultiPolygon(ps) if !ps.is_empty() => ps,
            _ => return "MULTIPOLYGON EMPTY".into(),
        };
        let precision = precision.max(1);
        let num = |v: f64| {
            let magnitude = if v == 0.0 { 0 } else { v.abs().log10().floor() as i64 };
            let decimals = (precision as i64 - 1 - magnitude).clamp(0, 17) as usize;
            let s = format!("{v:.decimals$}");
            let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
            if s == "-0" { "0".to_string() } else { s }
        };
        let ring = |r: &Ring| {
            let mut cs: Vec<String> = r.0.iter().map(|c| format!("{} {}", num(c.x), num(c.y))).collect();
            if let Some(first) = cs.first().cloned() {
                cs.push(first);
            }
            format!("({})", cs.join(","))
        };
        let parts: Vec<String> =
            polygons.iter().map(|(e, hs)| format!("({})", std::iter::once(e).chain(hs).map(ring).collect::<Vec<_>>().join(","))).collect();
        format!("MULTIPOLYGON ({})", parts.join(","))
    }

    /// The boundary as a layer of one multipolygon feature, with the `area`, `density`,
    /// `avg_pt_spacing`, `point_count`, `hex_count`, `edge_length` and `threshold` as attributes.
    pub fn boundary_layer(&self, name: Option<&str>) -> Layer {
        let mut layer = Layer::new(name.unwrap_or("boundary")).with_geom_type(GeometryType::MultiPolygon);
        let fields: [(&str, FieldType, FieldValue); 7] = [
            ("area", FieldType::Float, FieldValue::Float(self.area)),
            ("density", FieldType::Float, FieldValue::Float(self.density)),
            ("avg_pt_spacing", FieldType::Float, FieldValue::Float(self.avg_pt_spacing)),
            ("point_count", FieldType::Integer, FieldValue::Integer(self.point_count as i64)),
            ("hex_count", FieldType::Integer, FieldValue::Integer(self.hexagons.len() as i64)),
            ("edge_length", FieldType::Float, FieldValue::Float(self.edge_length)),
            ("threshold", FieldType::Integer, FieldValue::Integer(self.threshold as i64)),
        ];
        let mut attributes = Vec::new();
        for (n, t, v) in fields {
            layer.add_field(FieldDef::new(n, t));
            attributes.push(v);
        }
        self.crs.apply_to_layer(&mut layer);
        layer.push(Feature { fid: 0, geometry: Some(self.boundary.clone()), attributes });
        layer
    }

    /// The dense hexagons as a layer of polygons with their `column`, `row` and point `count`
    /// (PDAL's `density` output, whose `ID` packs the column and row and whose `COUNT` is the
    /// count).
    pub fn density_layer(&self, name: Option<&str>) -> Layer {
        let mut layer = Layer::new(name.unwrap_or("hexagons")).with_geom_type(GeometryType::Polygon);
        for n in ["column", "row", "count"] {
            layer.add_field(FieldDef::new(n, FieldType::Integer));
        }
        self.crs.apply_to_layer(&mut layer);
        for (i, h) in self.hexagons.iter().enumerate() {
            let attributes = vec![FieldValue::Integer(h.column), FieldValue::Integer(h.row), FieldValue::Integer(h.count as i64)];
            layer.push(Feature { fid: i as u64, geometry: Some(self.hexagon_polygon(h.column, h.row)), attributes });
        }
        layer
    }
}

/// The corners of a hexagon around its centre, in units of half the edge length (x) and half
/// the height (y), counter-clockwise from the lower left.
const CORNERS: [(i64, i64); 6] = [(-1, -1), (1, -1), (2, 0), (1, 1), (-1, 1), (-2, 0)];
/// The centre offsets of the neighbours across the edges from each corner to the next.
const NEIGHBOURS: [(i64, i64); 6] = [(0, -2), (3, -1), (3, 1), (0, 2), (-3, 1), (-3, -1)];

/// A hexagon's centre on the lattice of corners: x in half edges, y in half heights.
fn center(column: i64, row: i64) -> (i64, i64) {
    (1 + 3 * column, 1 + 2 * row + column.rem_euclid(2))
}

/// The column and row of the hexagon of a point `(u, v)` from the origin, as hexer's
/// `HexGrid::findHexagon` finds them: by columns of offset rectangles, then, in the strip where
/// two columns interlock, by the sloping edge of the half row, whose index hexer truncates
/// (rather than floors) below the origin.
fn locate(u: f64, v: f64, height: f64) -> (i64, i64) {
    let width = 3.0 / (2.0 * HEXER_SQRT_3) * height;
    let col = u / width;
    let mut x = col.floor() as i64;
    let x_odd = x.rem_euclid(2) == 1;
    let mut y = if x_odd { ((v - height / 2.0) / height).floor() } else { (v / height).floor() } as i64;
    let mut offset = col - col.floor();
    if offset > 2.0 / 3.0 {
        offset = (offset - 2.0 / 3.0) * 3.0;
        let halfrow = v / (height / 2.0);
        // hexer's (int) cast, toward zero.
        let halfy = halfrow.trunc() as i64;
        let row_offset = halfrow - halfrow.floor();
        let half_odd = halfy % 2 != 0;
        if half_odd == x_odd {
            // The edge slopes down to the right.
            if offset > row_offset {
                if !x_odd {
                    y -= 1;
                }
                x += 1;
            }
        } else if row_offset > offset {
            if x_odd {
                y += 1;
            }
            x += 1;
        }
    }
    (x, y)
}

type Lattice = (i64, i64);
/// A vertex of a ring: its lattice point, and the column, row and corner (in `CORNERS`) of the
/// hexagon hexer computes it from.
type Corner = (Lattice, i64, i64, usize);

/// hexer's order of hexagons (`HexId`'s `operator<`): by row, then even columns before odd
/// ones, then by column.
fn hexer_key(column: i64, row: i64) -> (i64, bool, i64) {
    (row, column.rem_euclid(2) == 1, column)
}

/// The polygons (exterior, holes) of the union of hexagons given by their centres, on the corner
/// lattice: exteriors counter-clockwise, holes clockwise, rings open, in hexer's order.
///
/// As hexer's, each ring starts at the lower-right corner of its first hexagon (in hexer's order)
/// with its bottom edge on the ring, the rings come in the order of those hexagons, polygons with
/// their holes in that order, each top-level polygon followed by the islands in its holes (and
/// the islands in their holes, level after level). Smoothing depends on the rings' starts.
fn trace(dense: &HashSet<Lattice>) -> Vec<(Vec<Corner>, Vec<Vec<Corner>>)> {
    // Every edge of a dense hexagon whose neighbour is not dense, in the hexagon's
    // counter-clockwise order: the boundary with the dense side on the left; bottom edges keep
    // their hexagon.
    let mut next: BTreeMap<Lattice, (Corner, bool)> = BTreeMap::new();
    for &(u, v) in dense {
        for k in 0..6 {
            let (nu, nv) = NEIGHBOURS[k];
            if dense.contains(&(u + nu, v + nv)) {
                continue;
            }
            let (a, b) = (CORNERS[k], CORNERS[(k + 1) % 6]);
            let column = (u - 1).div_euclid(3);
            let row = (v - 1 - column.rem_euclid(2)).div_euclid(2);
            next.insert((u + a.0, v + a.1), (((u + b.0, v + b.1), column, row, (k + 1) % 6), k == 0));
        }
    }
    // The rings, each with its first hexagon in hexer's order.
    // hexer records the end of each edge, computed from the edge's hexagon.
    let mut rings: Vec<((i64, bool, i64), Vec<Corner>)> = Vec::new();
    while let Some((&start, _)) = next.iter().next() {
        let mut ring: Vec<Corner> = Vec::new();
        let mut root: Option<((i64, bool, i64), Lattice)> = None;
        let mut at = start;
        while let Some((end, bottom)) = next.remove(&at) {
            ring.push(end);
            if bottom {
                let key = hexer_key(end.1, end.2);
                if root.is_none_or(|(k, _)| key < k) {
                    root = Some((key, end.0));
                }
            }
            at = end.0;
        }
        let (key, corner) = root.unwrap_or(((i64::MAX, true, i64::MAX), start));
        let first = ring.iter().position(|p| p.0 == corner).unwrap_or(0);
        ring.rotate_left(first);
        rings.push((key, ring));
    }
    rings.sort_by_key(|(k, _)| *k);
    let rings: Vec<Vec<Corner>> = rings.into_iter().map(|(_, r)| r).collect();
    let lattice: Vec<Vec<Lattice>> = rings.iter().map(|r| r.iter().map(|c| c.0).collect()).collect();
    // Each ring's parent: the smallest ring around it (exteriors' parents are holes, islands' are
    // their holes).
    // The rings never touch, so the rings around a ring are nested, of different areas, and the
    // smallest is its parent. Their candidates are the rings whose bounding box holds its first
    // vertex, found in an R-tree of the boxes (the lattice coordinates, within 2⁵⁰, are exact
    // in f64).
    let areas: Vec<i128> = lattice.iter().map(|r| lattice_area2(r).abs()).collect();
    let boxes = rstar::RTree::bulk_load(
        lattice
            .iter()
            .enumerate()
            .map(|(k, r)| {
                let b = lattice_bounds(r);
                rstar::primitives::GeomWithData::new(
                    rstar::primitives::Rectangle::from_corners([b[0] as f64, b[1] as f64], [b[2] as f64, b[3] as f64]),
                    k,
                )
            })
            .collect(),
    );
    let parent: Vec<Option<usize>> = (0..rings.len())
        .map(|i| {
            let p = lattice[i][0];
            boxes
                .locate_all_at_point([p.0 as f64, p.1 as f64])
                .map(|b| b.data)
                .filter(|&k| k != i && areas[k] > areas[i] && inside(p, &lattice[k]))
                .min_by_key(|&k| areas[k])
        })
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rings.len()];
    for (i, p) in parent.iter().enumerate() {
        if let Some(p) = p {
            children[*p].push(i);
        }
    }
    let polygon = |i: usize| (rings[i].clone(), children[i].iter().map(|&h| rings[h].clone()).collect::<Vec<_>>());
    let mut out = Vec::new();
    for top in (0..rings.len()).filter(|&i| parent[i].is_none()) {
        out.push(polygon(top));
        let mut islands: Vec<usize> = children[top].iter().flat_map(|&h| children[h].iter().copied()).collect();
        while !islands.is_empty() {
            let mut deeper = Vec::new();
            for island in islands {
                out.push(polygon(island));
                deeper.extend(children[island].iter().flat_map(|&h| children[h].iter().copied()));
            }
            islands = deeper;
        }
    }
    out
}

/// Twice the signed area of a lattice ring (positive counter-clockwise), about its first vertex
/// and in `i128`, so that neither the products nor the sum overflow far from the origin.
fn lattice_area2(r: &[Lattice]) -> i128 {
    let Some(&(x0, y0)) = r.first() else { return 0 };
    let d = |p: Lattice| ((p.0 - x0) as i128, (p.1 - y0) as i128);
    (0..r.len())
        .map(|i| {
            let (a, b) = (d(r[i]), d(r[(i + 1) % r.len()]));
            a.0 * b.1 - b.0 * a.1
        })
        .sum()
}

fn lattice_bounds(r: &[Lattice]) -> [i64; 4] {
    r.iter().fold([i64::MAX, i64::MAX, i64::MIN, i64::MIN], |b, p| [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)])
}

/// Whether a lattice point not on a ring is inside it (even-odd rule, exact on integers).
fn inside(p: Lattice, r: &[Lattice]) -> bool {
    let mut c = false;
    for i in 0..r.len() {
        let (a, b) = (r[i], r[(i + 1) % r.len()]);
        if (a.1 > p.1) != (b.1 > p.1) {
            // The crossing's x, compared without division: p.0 < a.0 + (p.1 - a.1)(b.0 - a.0)/(b.1 - a.1).
            let lhs = (p.0 - a.0) as i128 * (b.1 - a.1) as i128;
            let rhs = (p.1 - a.1) as i128 * (b.0 - a.0) as i128;
            if (b.1 > a.1 && lhs < rhs) || (b.1 < a.1 && lhs > rhs) {
                c = !c;
            }
        }
    }
    c
}

/// The unsigned area of a ring.
fn ring_area(r: &Ring) -> f64 {
    let cs = &r.0;
    let n = cs.len();
    if n < 3 {
        return 0.0;
    }
    // About the first vertex, for precision with large coordinates.
    let (x0, y0) = (cs[0].x, cs[0].y);
    let mut s = 0.0;
    for i in 1..n - 1 {
        let (ax, ay) = (cs[i].x - x0, cs[i].y - y0);
        let (bx, by) = (cs[i + 1].x - x0, cs[i + 1].y - y0);
        s += ax * by - bx * ay;
    }
    (s / 2.0).abs()
}

/// The polygons with their coordinates rounded to `decimals` decimals, as hexer writes them for
/// PDAL to read back.
fn round_coords(g: Geometry, decimals: usize) -> Geometry {
    let decimals = decimals.min(MAX_DECIMALS);
    let fixed = |v: f64| format!("{v:.decimals$}").parse().unwrap_or(v);
    let round = |r: Ring| Ring(r.0.into_iter().map(|c| Coord::xy(fixed(c.x), fixed(c.y))).collect());
    match g {
        Geometry::MultiPolygon(ps) => {
            Geometry::MultiPolygon(ps.into_iter().map(|(e, hs)| (round(e), hs.into_iter().map(round).collect())).collect())
        }
        g => g,
    }
}

/// The geometry with the direction of its holes reversed, from the same first vertex.
fn reverse_holes(g: Geometry) -> Geometry {
    match g {
        Geometry::MultiPolygon(ps) => Geometry::MultiPolygon(
            ps.into_iter()
                .map(|(e, hs)| {
                    let hs = hs
                        .into_iter()
                        .map(|Ring(mut cs)| {
                            if cs.len() > 1 {
                                cs[1..].reverse();
                            }
                            Ring(cs)
                        })
                        .collect();
                    (e, hs)
                })
                .collect(),
        ),
        g => g,
    }
}

/// Drops the polygons (holes subtracted) and then the holes smaller than `cull`, as PDAL's
/// `Polygon::simplify` does after simplifying.
fn cull_small(g: Geometry, cull: f64) -> Geometry {
    match g {
        Geometry::MultiPolygon(ps) => Geometry::MultiPolygon(
            ps.into_iter()
                .filter(|(e, hs)| ring_area(e) - hs.iter().map(ring_area).sum::<f64>() >= cull)
                .map(|(e, hs)| (e, hs.into_iter().filter(|h| ring_area(h) >= cull).collect()))
                .collect(),
        ),
        g => g,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pointcloud::{Dimension, Values};

    fn cloud(points: &[(f64, f64)]) -> PointCloud {
        let dims = vec![
            Dimension::new("x", Values::F64(points.iter().map(|p| p.0).collect())),
            Dimension::new("y", Values::F64(points.iter().map(|p| p.1).collect())),
            Dimension::new("z", Values::F64(vec![0.0; points.len()])),
        ];
        PointCloud::new(dims, SpatialRef::default())
    }

    /// The centre of a hexagon of edge `e` from an origin at (0, 0).
    fn centre_xy(column: i64, row: i64, e: f64) -> (f64, f64) {
        let (u, v) = center(column, row);
        (u as f64 * e / 2.0, v as f64 * e * 3f64.sqrt() / 2.0)
    }

    fn raw(edge: f64, threshold: u64) -> HexbinOptions {
        HexbinOptions { edge_length: edge, threshold, smooth: false, ..Default::default() }
    }

    fn polygons(g: &Geometry) -> &[(Ring, Vec<Ring>)] {
        match g {
            Geometry::MultiPolygon(ps) => ps,
            _ => panic!("not a multipolygon"),
        }
    }

    const HEX_AREA: f64 = 2.598_076_211_353_316; // 3√3/2 for an edge of 1

    #[test]
    fn anchors_the_grid_at_the_first_point_as_pdal() {
        // PDAL 2.10: one point at (10.3, 20.7), edge 1 -> the hexagon whose lower-left corner it is,
        // written as PDAL writes it.
        let r = hexbin(&cloud(&[(10.3, 20.7)]), &raw(1.0, 1)).unwrap();
        assert_eq!(r.hexagons, vec![Hexagon { column: 0, row: 0, count: 1 }]);
        assert_eq!(
            r.boundary_wkt(8),
            "MULTIPOLYGON (((11.3 20.7,11.8 21.566025,11.3 22.432051,10.3 22.432051,9.8 21.566025,10.3 20.7,11.3 20.7)))"
        );
        // PDAL's second hexagon for (0, 0) after (10.3, 20.7): lower-left corner (-0.2, -0.95063509).
        let r = hexbin(&cloud(&[(10.3, 20.7), (0.0, 0.0)]), &raw(1.0, 1)).unwrap();
        assert_eq!(r.hexagons[0], Hexagon { column: -7, row: -13, count: 1 });
        let Geometry::Polygon { exterior, .. } = r.hexagon_polygon(-7, -13) else { panic!() };
        assert!((exterior.0[0].x - -0.2).abs() < 1e-8 && (exterior.0[0].y - -0.950_635_09).abs() < 1e-8);
        assert!((r.area - 2.0 * HEX_AREA).abs() < 1e-7);
    }

    #[test]
    fn locates_points_in_the_hexagons_around_them() {
        // Above the origin, hexer's assignment is the hexagons' tessellation.
        let e = 2.5;
        let h = e * 3f64.sqrt();
        for column in -5..5 {
            for row in 0..5 {
                let (x, y) = centre_xy(column, row, e);
                assert_eq!(locate(x, y, h), (column, row));
                for (du, dv) in CORNERS {
                    let (px, py) = (x + 0.95 * du as f64 * e / 2.0, y + 0.95 * dv as f64 * h / 2.0);
                    if py >= 0.0 {
                        assert_eq!(locate(px, py, h), (column, row), "corner {du},{dv} of {column},{row}");
                    }
                }
            }
        }
        // Below it, in the strips where columns interlock, hexer takes one half row for the
        // other. PDAL 2.10 puts (1.3, -0.5) from the origin in column 1, row -2 (with an edge of
        // 1), though it is in column 0, row -1; and (1.3, 0.5), above, in column 1, row -1.
        let h = 3f64.sqrt();
        assert_eq!(locate(1.3, -0.5, h), (1, -2));
        assert_eq!(locate(1.3, 0.5, h), (1, -1));
        assert_eq!(locate(0.5, -0.5, h), (0, -1));
    }

    #[test]
    fn estimates_the_edge_length_as_pdal() {
        // 100 points 0.5 apart in a line: hexer's height is threshold × the sum of the 99
        // distances / 100 points.
        let pts: Vec<(f64, f64)> = (0..100).map(|i| (i as f64 * 0.5, 0.0)).collect();
        let o = HexbinOptions { threshold: 2, smooth: false, ..Default::default() };
        let r = hexbin(&cloud(&pts), &o).unwrap();
        assert!(r.estimated);
        assert!((r.height() - 2.0 * 49.5 / 100.0).abs() < 1e-12);
        assert_eq!(r.point_count, 100);
        assert!(r.hexagons.iter().all(|h| h.count >= 2));
        // A sample smaller than the points: the same height from the first 10 points, and every
        // point counted once.
        let r = hexbin(&cloud(&pts), &HexbinOptions { sample_size: 10, ..o }).unwrap();
        assert!((r.height() - 2.0 * 4.5 / 10.0).abs() < 1e-12);
        assert_eq!(r.point_count, 100);
        let counted: u64 = HexBinner::new(&HexbinOptions { sample_size: 10, threshold: 1, ..o })
            .and_then(|mut b| {
                b.add(&cloud(&pts))?;
                b.finish()
            })
            .unwrap()
            .hexagons
            .iter()
            .map(|h| h.count)
            .sum();
        assert_eq!(counted, 100);
    }

    #[test]
    fn traces_holes_and_separate_parts() {
        // A ring of the six neighbours of (2, 2), and the origin's hexagon apart.
        let neighbours = [(1, 1), (1, 2), (2, 1), (2, 3), (3, 1), (3, 2)];
        let mut pts = vec![(0.0, 0.0)];
        pts.extend(neighbours.iter().map(|&(c, r)| centre_xy(c, r, 1.0)));
        let r = hexbin(&cloud(&pts), &raw(1.0, 1)).unwrap();
        let ps = polygons(&r.boundary);
        assert_eq!(ps.len(), 2);
        let holes: Vec<usize> = ps.iter().map(|p| p.1.len()).collect();
        assert_eq!(holes.iter().sum::<usize>(), 1);
        let ring = ps.iter().find(|p| p.1.len() == 1).unwrap();
        assert_eq!(ring.0.0.len(), 18);
        assert_eq!(ring.1[0].0.len(), 6);
        // Exteriors counter-clockwise, holes clockwise; the hole's area is subtracted.
        assert!(signed(&ring.0) > 0.0 && signed(&ring.1[0]) < 0.0);
        assert!((r.area - 7.0 * HEX_AREA).abs() < 1e-7, "{}", r.area);
        assert!((r.density - 7.0 / r.area).abs() < 1e-12);
        // PDAL 2.10's area for the same points: 18.1865335.
        assert!((r.area - 18.186_533_5).abs() < 1e-6);
        // Filling the hole leaves one polygon of seven hexagons with no hole.
        pts.push(centre_xy(2, 2, 1.0));
        let r = hexbin(&cloud(&pts), &raw(1.0, 1)).unwrap();
        let ps = polygons(&r.boundary);
        assert_eq!(ps.iter().map(|p| p.1.len()).sum::<usize>(), 0);
        assert_eq!(ps.len(), 2);
    }

    #[test]
    fn nests_an_island_in_a_hole() {
        // A ring of hexagons at distance 2 from (5, 5), and its centre: the centre is a part of
        // its own, inside the ring's hole.
        let mut pts = vec![(0.0, 0.0)];
        let (cx, cy) = centre_xy(5, 5, 1.0);
        for column in 0..12 {
            for row in 0..12 {
                let (x, y) = centre_xy(column, row, 1.0);
                let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                if (2.9..3.6).contains(&d) || d < 0.1 {
                    pts.push((x, y));
                }
            }
        }
        let r = hexbin(&cloud(&pts), &raw(1.0, 1)).unwrap();
        let ps = polygons(&r.boundary);
        let with_hole: Vec<_> = ps.iter().filter(|p| !p.1.is_empty()).collect();
        assert_eq!(with_hole.len(), 1);
        assert!(ps.iter().filter(|p| p.1.is_empty()).count() >= 2);
        let hexes = r.hexagons.len() as f64;
        assert!((r.area - hexes * HEX_AREA).abs() < 1e-7 * hexes);
    }

    #[test]
    fn counts_points_against_the_threshold() {
        let (x, y) = centre_xy(3, 0, 1.0);
        let pts = [(0.0, 0.0), (0.5, 0.8), (0.4, 0.2), (x, y), (x + 0.1, y)];
        let r = hexbin(&cloud(&pts), &raw(1.0, 3)).unwrap();
        assert_eq!(r.hexagons, vec![Hexagon { column: 0, row: 0, count: 3 }]);
        assert_eq!(polygons(&r.boundary).len(), 1);
        // Every point counts towards the density, those of sparse hexagons too (as PDAL's).
        assert!((r.density - 5.0 / r.area).abs() < 1e-12 && (r.area - HEX_AREA).abs() < 1e-7);
        let r = hexbin(&cloud(&pts), &raw(1.0, 2)).unwrap();
        assert_eq!(r.hexagons.len(), 2);
        let r = hexbin(&cloud(&pts), &raw(1.0, 4)).unwrap();
        assert!(r.hexagons.is_empty() && polygons(&r.boundary).is_empty());
        assert_eq!(r.boundary_wkt(8), "MULTIPOLYGON EMPTY");
        assert_eq!((r.area, r.density), (0.0, 0.0));
    }

    #[test]
    fn handles_no_points_and_bad_options() {
        let r = hexbin(&cloud(&[]), &raw(1.0, 1)).unwrap();
        assert_eq!(r.point_count, 0);
        assert_eq!(r.boundary_wkt(8), "MULTIPOLYGON EMPTY");
        assert_eq!(r.boundary_layer(None).features.len(), 1);
        assert!(r.density_layer(None).features.is_empty());
        assert!(hexbin(&cloud(&[(0.0, 0.0)]), &HexbinOptions::default()).is_err());
        assert!(hexbin(&cloud(&[(0.0, 0.0)]), &HexbinOptions { edge_length: -1.0, ..Default::default() }).is_err());
        let o = HexbinOptions { edge_length: 1.0, preserve_topology: false, ..Default::default() };
        assert!(hexbin(&cloud(&[(0.0, 0.0)]), &o).is_err());
        assert!(hexbin(&cloud(&[(0.0, 0.0)]), &HexbinOptions { smooth: false, ..o }).is_ok());
    }

    #[test]
    fn bins_in_chunks_as_whole() {
        let pts: Vec<(f64, f64)> = (0..2000).map(|i| ((i as f64 * 0.37) % 23.0, (i as f64 * 0.91) % 17.0)).collect();
        let whole = hexbin(&cloud(&pts), &raw(1.4, 3)).unwrap();
        let mut b = HexBinner::new(&raw(1.4, 3)).unwrap();
        for chunk in pts.chunks(333) {
            b.add(&cloud(chunk)).unwrap();
        }
        assert_eq!(b.finish().unwrap(), whole);
        // A given origin moves the grid.
        let moved = hexbin(&cloud(&pts), &HexbinOptions { origin: Some([0.3, 0.3]), ..raw(1.4, 3) }).unwrap();
        assert_eq!(moved.origin, [0.3, 0.3]);
        assert_ne!(moved.hexagons, whole.hexagons);
    }

    #[test]
    fn smooths_and_culls_as_pdal() {
        // A long strip of hexagons: smoothing straightens its zigzag edges and keeps it one
        // polygon; a single far hexagon is smaller than 6 × tolerance² and dropped.
        let mut pts: Vec<(f64, f64)> = (0..40).flat_map(|c| (0..3).map(move |r| centre_xy(c, r, 1.0))).collect();
        pts.insert(0, (0.0, 0.0));
        pts.push(centre_xy(60, 0, 1.0));
        let raw_r = hexbin(&cloud(&pts), &raw(1.0, 1)).unwrap();
        assert_eq!(polygons(&raw_r.boundary).len(), 2);
        let smooth = hexbin(&cloud(&pts), &HexbinOptions { edge_length: 1.0, threshold: 1, ..Default::default() }).unwrap();
        let ps = polygons(&smooth.boundary);
        assert_eq!(ps.len(), 1);
        assert!(ps[0].0.0.len() < polygons(&raw_r.boundary)[0].0.0.len());
        // The cull tolerance can be set: 0 keeps the single hexagon.
        let keep = HexbinOptions { edge_length: 1.0, threshold: 1, hole_cull_area_tolerance: Some(0.0), ..Default::default() };
        assert_eq!(polygons(&hexbin(&cloud(&pts), &keep).unwrap().boundary).len(), 2);
    }

    #[test]
    fn writes_layers() {
        let r = hexbin(&cloud(&[(0.0, 0.0), (5.0, 5.0)]), &raw(1.0, 1)).unwrap();
        let b = r.boundary_layer(None);
        assert_eq!(b.features.len(), 1);
        assert_eq!(b.schema.fields().len(), 7);
        let d = r.density_layer(Some("density"));
        assert_eq!(d.name, "density");
        assert_eq!(d.features.len(), 2);
    }

    #[test]
    fn nests_many_isolated_rings() {
        // An annulus of hexagons (columns and rows -20..=20, 3 thick) around a hole with 121
        // isolated hexagons, and 273 isolated hexagons outside it: 396 rings.
        let origin = HexbinOptions { origin: Some([0.0, 0.0]), ..raw(1.0, 1) };
        let mut cells: Vec<(i64, i64)> = Vec::new();
        for c in -20..=20i64 {
            for r in -20..=20i64 {
                if c.abs() >= 18 || r.abs() >= 18 || (c % 3 == 0 && r % 3 == 0 && c.abs() <= 15 && r.abs() <= 15) {
                    cells.push((c, r));
                }
            }
        }
        cells.extend((24..=60).step_by(3).flat_map(|c| (-30..=30).step_by(3).map(move |r| (c, r))));
        let pts: Vec<(f64, f64)> = cells.iter().map(|&(c, r)| centre_xy(c, r, 1.0)).collect();
        let r = hexbin(&cloud(&pts), &origin).unwrap();
        assert_eq!(r.hexagons.len(), cells.len());
        let ps = polygons(&r.boundary);
        assert_eq!(ps.len(), 1 + 121 + 273);
        let annulus = ps.iter().position(|(_, hs)| !hs.is_empty()).unwrap();
        assert_eq!(ps.iter().map(|(_, hs)| hs.len()).sum::<usize>(), 1);
        // The islands in the hole follow the annulus, as hexer orders them.
        let hole = &ps[annulus].1[0];
        let (lo, hi) = hole.0.iter().fold((f64::MAX, f64::MIN), |(lo, hi), c| (lo.min(c.x), hi.max(c.x)));
        for (e, _) in &ps[annulus + 1..annulus + 122] {
            assert!(e.0.iter().all(|c| lo < c.x && c.x < hi));
        }
        assert!(ps[annulus + 122..].iter().chain(&ps[..annulus]).all(|(e, _)| e.0.iter().all(|c| c.x > 30.0)));
        assert!((r.area - cells.len() as f64 * HEX_AREA).abs() < 1e-6);
    }

    #[test]
    fn nests_rings_far_from_the_origin() {
        // Six hexagons around an empty one about 1e8 from the origin, with an edge of 0.02: the
        // corners' lattice coordinates (about 9e9 and 6e9) overflow an i64 area.
        let e = 0.02;
        let (c, r) = (3_000_000_000i64, 3_000_000_000i64);
        let (u, v) = center(c, r);
        // The neighbours' centres, with hexer's arithmetic (its √3 is not exact: 3e9 columns out,
        // the true centres are a third of a column off).
        let pts: Vec<(f64, f64)> = NEIGHBOURS
            .iter()
            .map(|&(du, dv)| {
                let column = (u + du - 1).div_euclid(3);
                let row = (v + dv - 1 - column.rem_euclid(2)).div_euclid(2);
                let cs: Vec<Coord> = (0..6).map(|k| vertex([0.0, 0.0], e * 3f64.sqrt(), (column, row), k)).collect();
                (cs.iter().map(|c| c.x).sum::<f64>() / 6.0, cs.iter().map(|c| c.y).sum::<f64>() / 6.0)
            })
            .collect();
        let far: Lattice = (u, v);
        let ring: Vec<Lattice> = CORNERS.iter().map(|&(a, b)| (far.0 + a, far.1 + b)).collect();
        assert_eq!(lattice_area2(&ring), lattice_area2(&CORNERS));
        let o = HexbinOptions { origin: Some([0.0, 0.0]), ..raw(e, 1) };
        let r = hexbin(&cloud(&pts), &o).unwrap();
        assert_eq!(r.hexagons.len(), 6);
        let ps = polygons(&r.boundary);
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].1.len(), 1);
        assert!((r.area - 6.0 * HEX_AREA * e * e).abs() < 1e-3 * HEX_AREA * e * e, "{}", r.area);
    }

    #[test]
    fn rejects_points_too_far_from_the_origin() {
        for far in [(1e300, 0.0), (0.0, -1e300), (f64::MAX, 0.0)] {
            let err = hexbin(&cloud(&[(-f64::MAX, 0.0), far]), &raw(1.0, 1)).unwrap_err();
            assert!(err.to_string().contains("too far"), "{err}");
        }
    }

    #[test]
    fn rounds_to_at_most_the_decimals_of_an_f64() {
        // Rounding to more decimals than an f64 has is a no-op, and does not allocate them.
        let ring = Ring(vec![Coord::xy(1e-300, 0.1), Coord::xy(123_456.789, 5e-324), Coord::xy(-0.3, 7.0)]);
        let g = Geometry::MultiPolygon(vec![(ring, Vec::new())]);
        assert_eq!(round_coords(g.clone(), usize::MAX), g);
        assert_eq!(round_coords(g.clone(), MAX_DECIMALS), g);
        let pts = [(10.3, 20.7), (0.0, 0.0)];
        let huge = hexbin(&cloud(&pts), &HexbinOptions { precision: usize::MAX, ..raw(1.0, 1) }).unwrap();
        let exact = hexbin(&cloud(&pts), &HexbinOptions { precision: MAX_DECIMALS, ..raw(1.0, 1) }).unwrap();
        assert_eq!(huge.boundary, exact.boundary);
    }

    fn signed(r: &Ring) -> f64 {
        let cs = &r.0;
        (0..cs.len()).map(|i| cs[i].x * cs[(i + 1) % cs.len()].y - cs[(i + 1) % cs.len()].x * cs[i].y).sum::<f64>() / 2.0
    }
}
