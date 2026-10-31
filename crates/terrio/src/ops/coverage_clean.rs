// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions.
// SPDX-FileCopyrightText: Copyright (c) 2025 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2020 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2016 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions, and others.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   JTS commit 3460207216f3091cd7f72d48d6ddd1b561b2b8fd:
//     coverage/CoverageCleaner.java, coverage/CleanCoverage.java
//     noding/snap/SnappingNoder.java, noding/snap/SnappingIntersectionAdder.java
//     noding/snap/SnappingPointIndex.java
//     algorithm/construct/MaximumInscribedCircle.java, ExactMaxInscribedCircle.java
//     operation/polygonize/Polygonizer.java
//     dissolve/LineDissolver.java, edgegraph/EdgeGraph.java, edgegraph/HalfEdge.java
//     noding/NodedSegmentString.java, noding/SegmentNodeList.java
//     noding/SegmentPointComparator.java
//     algorithm/RobustLineIntersector.java, algorithm/Angle.java
//     geom/Triangle.java, math/MathUtil.java
// Paths are relative to modules/core/src/main/java/org/locationtech/jts/.
//
// Written to match the behavior of:
//   GDAL v3.13.3:
//     apps/gdalalg_vector_clean_coverage.cpp
// No code is translated from these sources.

//! Polygon coverage cleaning: snap and node linework, classify bounded faces,
//! assign overlaps, and merge enclosed narrow gaps. Reference: GDAL 3.13.3 /
//! GEOS 3.14.1. Distances use planar XY coordinates in the input CRS units.
use crate::{Error, Feature, Geometry, Layer, Result};
use geo::algorithm::line_intersection::{LineIntersection, line_intersection};
use geo::{Area, BoundingRect, Contains, InteriorPoint};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use wbvector::{Coord, Ring};

type C = geo::Coord<f64>;
type P = geo::Polygon<f64>;
type Key = [u64; 2];
type Edge = [usize; 2];

/// How an overlap face is assigned to one of its source features. Area and
/// border length are measured on the already assigned non-overlap faces;
/// subsequent overlap assignments contribute to later decisions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CoverageMergeStrategy {
    /// Merge into the parent with the longest shared boundary.
    #[default]
    LongestBorder,
    /// Merge into the parent with the greatest already assigned area.
    MaxArea,
    /// Merge into the parent with the smallest already assigned area.
    MinArea,
    /// Merge into the earliest input feature.
    MinIndex,
}

impl std::str::FromStr for CoverageMergeStrategy {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "longest-border" => Ok(Self::LongestBorder),
            "max-area" => Ok(Self::MaxArea),
            "min-area" => Ok(Self::MinArea),
            "min-index" => Ok(Self::MinIndex),
            _ => Err(Error::InvalidArgument("merge_strategy must be longest-border, max-area, min-area or min-index".into())),
        }
    }
}

/// Options for [`clean_coverage`].
#[derive(Debug, Clone)]
pub struct CoverageCleanOptions {
    /// Negative selects extent diameter / 1e8. Zero disables proximity snapping.
    pub snapping_distance: f64,
    /// Strategy used to choose the owner of each overlap face.
    pub merge_strategy: CoverageMergeStrategy,
    /// Enclosed gaps with inscribed diameter at most this width are merged into
    /// the adjacent feature with longest common border. Zero disables filling.
    pub maximum_gap_width: f64,
}
impl Default for CoverageCleanOptions {
    fn default() -> Self {
        Self { snapping_distance: -1., merge_strategy: CoverageMergeStrategy::default(), maximum_gap_width: 0. }
    }
}

/// Cleans polygon and multipolygon features into a coverage, preserving one
/// output feature and its attributes per input feature. Geometry is constructed
/// in XY topology, retaining vertex Z and interpolating Z at intersections. M is
/// dropped on cleaned layers. Empty polygons have empty Polygon output. Active
/// null/non-polygonal features are rejected, as in GDAL's public operation.
/// Invalid polygons are rejected: repair individual polygons before cleaning.
/// The complete selected layer is materialized; this is not a streaming operation.
pub fn clean_coverage(layer: &Layer, options: &CoverageCleanOptions) -> Result<Layer> {
    if !options.snapping_distance.is_finite() || !options.maximum_gap_width.is_finite() || options.maximum_gap_width < 0. {
        return Err(Error::InvalidArgument(
            "coverage cleaning needs a finite snapping distance and non-negative finite maximum gap width".into(),
        ));
    }
    let mut sources = Vec::with_capacity(layer.len());
    let mut rings = Vec::new();
    for f in &layer.features {
        let geometry = f
            .geometry
            .as_ref()
            .ok_or_else(|| Error::InvalidArgument(format!("feature {}: coverage cleaning requires polygonal geometry, not null", f.fid)))?;
        if !matches!(geometry, Geometry::Polygon { .. } | Geometry::MultiPolygon(_)) {
            return Err(Error::InvalidArgument(format!("feature {}: coverage cleaning requires polygonal geometry", f.fid)));
        }
        super::topology::require_valid(geometry, f.fid)?;
        let polygons = super::topology::polygons(geometry, f.fid)?;
        for p in &polygons.0 {
            rings.push(p.exterior().0.clone());
            rings.extend(p.interiors().iter().map(|r| r.0.clone()));
        }
        sources.push(polygons);
    }
    let extent = geo::MultiLineString(rings.iter().cloned().map(geo::LineString).collect()).bounding_rect();
    let distance =
        if options.snapping_distance < 0. { extent.map_or(0., |e| e.width().hypot(e.height()) / 1e8) } else { options.snapping_distance };
    let original_rings: Vec<Vec<Coord>> = layer
        .features
        .iter()
        .filter_map(|f| f.geometry.as_ref())
        .flat_map(|g| match g {
            Geometry::Polygon { exterior, interiors } => {
                std::iter::once(exterior.0.clone()).chain(interiors.iter().map(|r| r.0.clone())).collect()
            }
            Geometry::MultiPolygon(ps) => {
                ps.iter().flat_map(|(e, hs)| std::iter::once(e.0.clone()).chain(hs.iter().map(|r| r.0.clone()))).collect::<Vec<_>>()
            }
            _ => vec![],
        })
        .collect();
    let (points, edges, elevations) = node(original_rings, distance);
    let faces = polygonize(&points, &edges);
    let mut assigned: Vec<Vec<usize>> = vec![vec![]; sources.len()];
    let mut overlaps = Vec::new();
    let mut gaps = Vec::new();
    for (i, face) in faces.iter().enumerate() {
        let Some(p) = face.interior_point() else { continue };
        let parents: Vec<_> = sources.iter().enumerate().filter_map(|(i, g)| g.contains(&p).then_some(i)).collect();
        match parents.as_slice() {
            [] => gaps.push(i),
            [parent] => assigned[*parent].push(i),
            _ => overlaps.push((i, parents)),
        }
    }
    for (i, parents) in overlaps {
        let target = parents
            .into_iter()
            .reduce(|a, b| {
                let score = |j: usize| match options.merge_strategy {
                    CoverageMergeStrategy::LongestBorder => assigned[j].iter().map(|&f| common_border(&faces[i], &faces[f])).sum(),
                    CoverageMergeStrategy::MaxArea => assigned[j].iter().map(|&f| faces[f].unsigned_area()).sum(),
                    CoverageMergeStrategy::MinArea => -assigned[j].iter().map(|&f| faces[f].unsigned_area()).sum::<f64>(),
                    CoverageMergeStrategy::MinIndex => -(j as f64),
                };
                if score(b) > score(a) { b } else { a }
            })
            .expect("overlap has parents");
        assigned[target].push(i);
    }
    if options.maximum_gap_width > 0. {
        for i in gaps {
            if !radius_within(&faces[i], options.maximum_gap_width / 2.) {
                continue;
            }
            let target = assigned
                .iter()
                .enumerate()
                .filter_map(|(j, fs)| {
                    let border = fs.iter().map(|&f| common_border(&faces[i], &faces[f])).sum::<f64>();
                    (border > 0.).then_some((j, border))
                })
                .reduce(|a, b| if b.1 > a.1 { b } else { a });
            if let Some((target, _)) = target {
                assigned[target].push(i);
            }
        }
    }
    let mut out = super::geometry::header(layer, None);
    for (f, fs) in layer.features.iter().zip(assigned) {
        let polygons = dissolve_faces(fs.iter().map(|&i| &faces[i]));
        let ring = |r: geo::LineString<f64>| {
            Ring(r.0.into_iter().map(|p| Coord { x: p.x, y: p.y, z: elevations.get(&key(p)).copied().flatten(), m: None }).collect())
        };
        let mut g = Geometry::MultiPolygon(
            polygons
                .into_iter()
                .map(|p| {
                    let (e, hs) = p.into_inner();
                    (ring(e), hs.into_iter().map(ring).collect())
                })
                .collect(),
        );
        // Like GEOS, a one-part result is Polygon, including a typed empty.
        if let Geometry::MultiPolygon(ps) = &mut g
            && ps.len() <= 1
        {
            let (exterior, interiors) = ps.pop().unwrap_or_else(|| (Ring(vec![]), vec![]));
            g = Geometry::Polygon { exterior, interiors };
        }
        let mut attributes = f.attributes.clone();
        attributes.resize(layer.schema.len(), crate::FieldValue::Null);
        out.features.push(Feature { fid: out.features.len() as u64 + 1, geometry: Some(g), attributes });
    }
    Ok(out)
}

/// Cleans selected layers and copies inactive layers unchanged.
pub fn clean_coverage_layers(layers: &[Layer], active_layer: Option<&str>, options: &CoverageCleanOptions) -> Result<Vec<Layer>> {
    if let Some(name) = active_layer
        && !layers.iter().any(|l| l.name == name)
    {
        return Err(Error::InvalidArgument(format!("active layer {name:?} not found")));
    }
    layers.iter().map(|l| if active_layer.is_none_or(|name| l.name == name) { clean_coverage(l, options) } else { Ok(l.clone()) }).collect()
}

fn key(p: C) -> Key {
    [if p.x == 0. { 0 } else { p.x.to_bits() }, if p.y == 0. { 0 } else { p.y.to_bits() }]
}
fn dist(a: C, b: C) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}
fn segment_distance(p: C, a: C, b: C) -> f64 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / (dx * dx + dy * dy)).clamp(0., 1.);
    dist(p, C { x: a.x + t * dx, y: a.y + t * dy })
}
struct Snap {
    points: RTree<[f64; 2]>,
    tolerance: f64,
    elevations: HashMap<Key, Option<f64>>,
}
impl Snap {
    fn point(&mut self, p: C) -> C {
        let nearest = self.points.locate_within_distance([p.x, p.y], self.tolerance * self.tolerance).min_by(|a, b| {
            dist(p, C::from(**a)).total_cmp(&dist(p, C::from(**b))).then_with(|| a[0].total_cmp(&b[0])).then_with(|| a[1].total_cmp(&b[1]))
        });
        if let Some(&p) = nearest {
            return C::from(p);
        }
        self.points.insert([p.x, p.y]);
        p
    }
}

fn node(rings: Vec<Vec<Coord>>, tolerance: f64) -> (Vec<C>, Vec<Edge>, HashMap<Key, Option<f64>>) {
    let mut snap = Snap { points: RTree::new(), tolerance, elevations: HashMap::new() };
    // JTS seeds long monotone rings with a golden-ratio low-discrepancy sequence.
    for ring in &rings {
        let mut r = 0.;
        for _ in 0..ring.len() / 100 {
            r = (r + 0.6180339887498949) % 1.;
            let p = &ring[(ring.len() as f64 * r) as usize];
            let xy = snap.point(C { x: p.x, y: p.y });
            snap.elevations.entry(key(xy)).or_insert(p.z);
        }
    }
    let mut segments = Vec::new();
    for ring in rings {
        let mut ring: Vec<_> = ring
            .into_iter()
            .map(|p| {
                let xy = snap.point(C { x: p.x, y: p.y });
                snap.elevations.entry(key(xy)).or_insert(p.z);
                xy
            })
            .collect();
        ring.dedup();
        if ring.first() != ring.last()
            && let Some(&p) = ring.first()
        {
            ring.push(p);
        }
        segments.extend(ring.windows(2).filter(|p| p[0] != p[1]).map(|p| [p[0], p[1]]));
    }
    let index = RTree::bulk_load(
        segments
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let env = AABB::from_corners([s[0].x.min(s[1].x), s[0].y.min(s[1].y)], [s[0].x.max(s[1].x), s[0].y.max(s[1].y)]);
                GeomWithData::new(Rectangle::from_aabb(env), i)
            })
            .collect(),
    );
    let mut nodes = segments.clone().into_iter().map(Vec::from).collect::<Vec<_>>();
    for (i, s) in segments.iter().enumerate() {
        let e = 2. * tolerance;
        let env = AABB::from_corners([s[0].x.min(s[1].x) - e, s[0].y.min(s[1].y) - e], [s[0].x.max(s[1].x) + e, s[0].y.max(s[1].y) + e]);
        let mut candidates: Vec<_> = index.locate_in_envelope_intersecting(env).map(|g| g.data).filter(|&j| j > i).collect();
        candidates.sort_unstable();
        for j in candidates {
            let t = segments[j];
            if let Some(intersection) = line_intersection(geo::Line::new(s[0], s[1]), geo::Line::new(t[0], t[1])) {
                match intersection {
                    LineIntersection::SinglePoint { intersection, .. } => {
                        let z = |s: [C; 2]| {
                            let a = snap.elevations.get(&key(s[0])).copied().flatten();
                            let b = snap.elevations.get(&key(s[1])).copied().flatten();
                            match (a, b) {
                                (Some(a), Some(b)) => Some(a + (b - a) * dist(s[0], intersection) / dist(s[0], s[1])),
                                (Some(z), None) | (None, Some(z)) => Some(z),
                                _ => None,
                            }
                        };
                        let elevation = match (z(*s), z(t)) {
                            (Some(a), Some(b)) => Some((a + b) / 2.),
                            (a, b) => a.or(b),
                        };
                        let p = snap.point(intersection);
                        snap.elevations.entry(key(p)).or_insert(elevation);
                        nodes[i].push(p);
                        nodes[j].push(p);
                    }
                    LineIntersection::Collinear { intersection } => {
                        for p in [intersection.start, intersection.end] {
                            nodes[i].push(p);
                            nodes[j].push(p);
                        }
                    }
                }
            }
            for (src, dst, a, b) in [(i, j, *s, t), (j, i, t, *s)] {
                for p in a {
                    if dist(p, b[0]) >= tolerance && dist(p, b[1]) >= tolerance && segment_distance(p, b[0], b[1]) < tolerance {
                        nodes[src].push(p);
                        nodes[dst].push(p);
                    }
                }
            }
        }
    }
    let mut points = Vec::new();
    let mut ids = HashMap::new();
    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    for (s, mut ns) in segments.into_iter().zip(nodes) {
        // NodedSegmentString orders nodes in the segment's dominant axis.
        let xaxis = (s[1].x - s[0].x).abs() >= (s[1].y - s[0].y).abs();
        ns.sort_by(|a, b| {
            if xaxis {
                a.x.total_cmp(&b.x).then_with(|| a.y.total_cmp(&b.y))
            } else {
                a.y.total_cmp(&b.y).then_with(|| a.x.total_cmp(&b.x))
            }
        });
        ns.dedup();
        if if xaxis { s[1].x < s[0].x } else { s[1].y < s[0].y } {
            ns.reverse();
        }
        let ns: Vec<_> = ns
            .into_iter()
            .map(|p| {
                *ids.entry(key(p)).or_insert_with(|| {
                    points.push(p);
                    points.len() - 1
                })
            })
            .collect();
        for pair in ns.windows(2) {
            let mut e = [pair[0], pair[1]];
            e.sort_unstable();
            if seen.insert(e) {
                edges.push([pair[0], pair[1]]);
            }
        }
    }
    (points, edges, snap.elevations)
}

// Trace the left face of each directed edge. At a vertex the next edge is the
// clockwise predecessor of the incoming edge's reverse. Splitting repeated
// vertices produces minimal rings for components touching only at a point.
fn trace(points: &[C], edges: impl IntoIterator<Item = Edge>) -> Vec<geo::LineString<f64>> {
    let starts: Vec<_> = edges.into_iter().collect();
    let edges: HashSet<_> = starts.iter().copied().collect();
    let mut neighbors = vec![vec![]; points.len()];
    for &[a, b] in &edges {
        neighbors[a].push(b);
    }
    for (i, ns) in neighbors.iter_mut().enumerate() {
        ns.sort_by(|&a, &b| {
            let angle = |j: usize| (points[j].y - points[i].y).atan2(points[j].x - points[i].x);
            angle(a).total_cmp(&angle(b))
        });
    }
    let mut used = HashSet::new();
    let mut rings = Vec::new();
    for start in starts {
        if used.contains(&start) {
            continue;
        }
        let mut path = Vec::new();
        let mut positions = HashMap::new();
        let mut edge = start;
        while used.insert(edge) {
            let [a, b] = edge;
            if let Some(&pos) = positions.get(&a) {
                let mut ring = path.split_off(pos);
                for v in &ring {
                    positions.remove(v);
                }
                ring.push(a);
                if ring.len() >= 4 {
                    rings.push(geo::LineString(ring.into_iter().map(|i| points[i]).collect()));
                }
            }
            positions.insert(a, path.len());
            path.push(a);
            let ns = &neighbors[b];
            let reverse_angle = (points[a].y - points[b].y).atan2(points[a].x - points[b].x);
            let pos = ns.partition_point(|&j| (points[j].y - points[b].y).atan2(points[j].x - points[b].x) < reverse_angle);
            if ns.is_empty() {
                break;
            }
            edge = [b, ns[(pos + ns.len() - 1) % ns.len()]];
        }
        if edge == start && path.len() >= 3 {
            path.push(start[0]);
            rings.push(geo::LineString(path.into_iter().map(|i| points[i]).collect()));
        }
    }
    rings
}
fn polygons_from_rings(rings: Vec<geo::LineString<f64>>) -> Vec<P> {
    let mut shells = Vec::new();
    let mut holes = Vec::new();
    for r in rings {
        let p = P::new(r, vec![]);
        match p.signed_area().partial_cmp(&0.) {
            Some(Ordering::Greater) => shells.push(p),
            Some(Ordering::Less) => holes.push(p),
            _ => {}
        }
    }
    let boundary = |p: &P| -> BTreeSet<_> {
        p.exterior()
            .0
            .windows(2)
            .map(|pair| {
                let mut e = [key(pair[0]), key(pair[1])];
                e.sort_unstable();
                e
            })
            .collect()
    };
    let boundaries: Vec<_> = shells.iter().map(boundary).collect();
    let mut assigned: Vec<Vec<geo::LineString<f64>>> = vec![vec![]; shells.len()];
    for h in holes {
        let Some(point) = h.interior_point() else { continue };
        let edges = boundary(&h);
        // A reversed cycle bounds the same face. Area equality is insufficient:
        // cyclic starts change floating summation, especially on large coordinates.
        let target = shells
            .iter()
            .enumerate()
            .filter(|(i, p)| boundaries[*i] != edges && p.unsigned_area() > h.unsigned_area() && p.contains(&point))
            .min_by(|(_, a), (_, b)| a.unsigned_area().total_cmp(&b.unsigned_area()))
            .map(|(i, _)| i);
        if let Some(i) = target {
            assigned[i].push(h.exterior().clone());
        }
    }
    for (p, holes) in shells.iter_mut().zip(assigned) {
        for h in holes {
            p.interiors_push(h);
        }
    }
    shells
}
// Adapted from JTS LineDissolver: mark undirected edges, merge degree-two
// chains and use an origin-edge stack to traverse nodes. Keeping this stage
// matters because polygonization order affects progressive overlap assignment.
fn dissolve_lines(points: &[C], edges: &[Edge]) -> Vec<Vec<usize>> {
    let mut neighbors = vec![vec![]; points.len()];
    let mut first = vec![None; points.len()];
    for &[a, b] in edges {
        first[a].get_or_insert([a, b]);
        first[b].get_or_insert([b, a]);
        neighbors[a].push(b);
        neighbors[b].push(a);
    }
    for (i, ns) in neighbors.iter_mut().enumerate() {
        ns.sort_by(|&a, &b| {
            let angle = |j: usize| (points[j].y - points[i].y).atan2(points[j].x - points[i].x);
            angle(a).total_cmp(&angle(b))
        });
    }
    let key = |mut e: Edge| {
        e.sort_unstable();
        e
    };
    let stack_edges = |e: Edge, stack: &mut Vec<Edge>, marked: &HashSet<Edge>| {
        let [a, b] = e;
        let ns = &neighbors[a];
        let pos = ns.iter().position(|&i| i == b).expect("edge in origin star");
        for i in 0..ns.len() {
            let edge = [a, ns[(pos + i) % ns.len()]];
            if !marked.contains(&key(edge)) {
                stack.push(edge);
            }
        }
    };
    let mut marked = HashSet::new();
    let mut lines = Vec::new();
    let mut stack = Vec::new();
    // Fix the vertex iteration order rather than depending on Rust's randomized
    // HashMap. The reference's own hash iteration is not an ownership contract.
    for e in first.into_iter().rev().flatten() {
        if marked.contains(&key(e)) {
            continue;
        }
        let mut node = e;
        while neighbors[node[0]].len() == 2 {
            let previous = *neighbors[node[0]].iter().find(|&&i| i != node[1]).unwrap();
            node = [previous, node[0]];
            if node == e {
                break;
            }
        }
        stack_edges(node, &mut stack, &marked);
        while let Some(start) = stack.pop() {
            if marked.contains(&key(start)) {
                continue;
            }
            let mut e = start;
            let mut line = vec![e[0]];
            loop {
                marked.insert(key(e));
                line.push(e[1]);
                if neighbors[e[1]].len() != 2 {
                    stack_edges([e[1], e[0]], &mut stack, &marked);
                    break;
                }
                let next = *neighbors[e[1]].iter().find(|&&i| i != e[0]).unwrap();
                e = [e[1], next];
                if e == start {
                    break;
                }
            }
            lines.push(line);
        }
    }
    lines
}
fn polygonize(points: &[C], edges: &[Edge]) -> Vec<P> {
    let lines = dissolve_lines(points, edges);
    polygons_from_rings(trace(points, lines.iter().flat_map(|line| line.windows(2).flat_map(|p| [[p[1], p[0]], [p[0], p[1]]]))))
}
fn dissolve_faces<'a>(faces: impl IntoIterator<Item = &'a P>) -> Vec<P> {
    let mut ids = HashMap::new();
    let mut points = Vec::new();
    let mut edges = HashSet::new();
    for p in faces {
        // Face shells are CCW; holes are CW.
        for r in std::iter::once(p.exterior()).chain(p.interiors()) {
            for pair in r.0.windows(2) {
                let mut id = |p: C| {
                    *ids.entry(key(p)).or_insert_with(|| {
                        points.push(p);
                        points.len() - 1
                    })
                };
                let a = id(pair[0]);
                let b = id(pair[1]);
                if !edges.remove(&[b, a]) {
                    edges.insert([a, b]);
                }
            }
        }
    }
    let mut edges: Vec<_> = edges.into_iter().collect();
    edges.sort_unstable();
    polygons_from_rings(trace(&points, edges))
}
fn common_border(a: &P, b: &P) -> f64 {
    let mut es = HashSet::new();
    for r in std::iter::once(a.exterior()).chain(a.interiors()) {
        for p in r.0.windows(2) {
            let mut e = [key(p[0]), key(p[1])];
            e.sort_unstable();
            es.insert(e);
        }
    }
    let mut length = 0.;
    for r in std::iter::once(b.exterior()).chain(b.interiors()) {
        for p in r.0.windows(2) {
            let mut e = [key(p[0]), key(p[1])];
            e.sort_unstable();
            if es.contains(&e) {
                length += dist(p[0], p[1]);
            }
        }
    }
    length
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    p: C,
    h: f64,
    d: f64,
    max: f64,
}
impl PartialEq for Cell {
    fn eq(&self, b: &Self) -> bool {
        self.max == b.max
    }
}
impl Eq for Cell {}
impl PartialOrd for Cell {
    fn partial_cmp(&self, b: &Self) -> Option<Ordering> {
        Some(self.cmp(b))
    }
}
impl Ord for Cell {
    fn cmp(&self, b: &Self) -> Ordering {
        self.max.total_cmp(&b.max)
    }
}
fn radius_within(poly: &P, radius: f64) -> bool {
    if radius <= 0. {
        return false;
    }
    let Some(env) = poly.bounding_rect() else { return true };
    if env.width() < 2. * radius || env.height() < 2. * radius {
        return true;
    }
    if let Some(exact) = exact_radius(poly) {
        return exact <= radius;
    }
    let distance = |p: C| {
        let d = std::iter::once(poly.exterior())
            .chain(poly.interiors())
            .flat_map(|r| r.0.windows(2))
            .map(|s| segment_distance(p, s[0], s[1]))
            .fold(f64::INFINITY, f64::min);
        if poly.contains(&geo::Point(p)) { d } else { -d }
    };
    let cell = |p: C, h: f64| {
        let d = distance(p);
        Cell { p, h, d, max: d + h * std::f64::consts::SQRT_2 }
    };
    let mut heap = BinaryHeap::from([cell(env.center(), env.width().max(env.height()) / 2.)]);
    let mut best = poly.interior_point().map_or(0., |p| distance(p.0));
    let tolerance = radius * 0.0001;
    let factor = (env.width().hypot(env.height()) / tolerance).ln().max(1.) as usize;
    for _ in 0..2000 + 2000 * factor {
        let Some(c) = heap.pop() else { break };
        best = best.max(c.d);
        if best > radius {
            return false;
        }
        if c.max < radius || c.max - best < tolerance {
            break;
        }
        let h = c.h / 2.;
        for dx in [-h, h] {
            for dy in [-h, h] {
                heap.push(cell(C { x: c.p.x + dx, y: c.p.y + dy }, h));
            }
        }
    }
    best <= radius
}

// JTS evaluates triangles and convex quadrilaterals without an approximate
// search. This matters for gap widths close to the actual inscribed diameter.
fn exact_radius(poly: &P) -> Option<f64> {
    if !poly.interiors().is_empty() {
        return None;
    }
    let r = &poly.exterior().0;
    if r.len() == 4 {
        let lengths = [dist(r[1], r[2]), dist(r[0], r[2]), dist(r[0], r[1])];
        let perimeter = lengths.iter().sum::<f64>();
        let p = C {
            x: r[0].x + ((r[1].x - r[0].x) * lengths[1] + (r[2].x - r[0].x) * lengths[2]) / perimeter,
            y: r[0].y + ((r[1].y - r[0].y) * lengths[1] + (r[2].y - r[0].y) * lengths[2]) / perimeter,
        };
        return Some(segment_distance(p, r[0], r[1]));
    }
    if r.len() != 5 {
        return None;
    }
    use geo::algorithm::kernels::{Kernel, Orientation, RobustKernel};
    let mut orientation = None;
    for i in 0..4 {
        let o = RobustKernel::orient2d(r[i], r[(i + 1) % 4], r[(i + 2) % 4]);
        if o == Orientation::Collinear {
            continue;
        }
        if orientation.is_some_and(|previous| previous != o) {
            return None;
        }
        orientation = Some(o);
    }
    let mut r = r[..4].to_vec();
    if poly.signed_area() > 0. {
        r.reverse();
    }
    let e = poly.bounding_rect()?;
    let length = 2. * e.width().hypot(e.height());
    let bisectors: Vec<_> = (0..4)
        .map(|i| {
            let p = r[i];
            let prev = r[(i + 3) % 4];
            let next = r[(i + 1) % 4];
            let a = (prev.y - p.y).atan2(prev.x - p.x);
            let b = (next.y - p.y).atan2(next.x - p.x);
            let mut delta = b - a;
            if delta > std::f64::consts::PI {
                delta -= std::f64::consts::TAU;
            }
            if delta <= -std::f64::consts::PI {
                delta += std::f64::consts::TAU;
            }
            let angle = a + delta / 2.;
            geo::Line::new(p, C { x: p.x + length * angle.cos(), y: p.y + length * angle.sin() })
        })
        .collect();
    (0..4)
        .filter_map(|i| {
            let LineIntersection::SinglePoint { intersection: p, .. } = line_intersection(bisectors[i], bisectors[(i + 1) % 4])? else {
                return None;
            };
            poly.contains(&geo::Point(p)).then(|| (0..4).map(|j| segment_distance(p, r[j], r[(j + 1) % 4])).fold(f64::INFINITY, f64::min))
        })
        .max_by(f64::total_cmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rectangle(x: f64, y: f64, w: f64, h: f64) -> Geometry {
        Geometry::Polygon {
            exterior: Ring(vec![Coord::xy(x, y), Coord::xy(x + w, y), Coord::xy(x + w, y + h), Coord::xy(x, y + h), Coord::xy(x, y)]),
            interiors: vec![],
        }
    }
    fn layer(gs: Vec<Geometry>) -> Layer {
        let mut l = Layer::new("coverage");
        for (i, g) in gs.into_iter().enumerate() {
            l.features.push(Feature { fid: 10 + i as u64, geometry: Some(g), attributes: vec![] });
        }
        l
    }
    fn areas(l: &Layer) -> Vec<f64> {
        l.features.iter().map(|f| super::super::topology::polygons(f.geometry.as_ref().unwrap(), f.fid).unwrap().unsigned_area()).collect()
    }
    #[test]
    fn overlap_strategies_and_contained_empty() {
        for (strategy, expected) in [
            (CoverageMergeStrategy::LongestBorder, vec![6., 2.]),
            (CoverageMergeStrategy::MaxArea, vec![6., 2.]),
            (CoverageMergeStrategy::MinArea, vec![4., 4.]),
            (CoverageMergeStrategy::MinIndex, vec![6., 2.]),
        ] {
            let out = clean_coverage(
                &layer(vec![rectangle(0., 0., 3., 2.), rectangle(2., 0., 2., 2.)]),
                &CoverageCleanOptions { merge_strategy: strategy, ..Default::default() },
            )
            .unwrap();
            assert_eq!(areas(&out), expected, "{strategy:?}");
            assert!(super::super::coverage::check_coverage(&out, &Default::default()).unwrap().is_empty());
        }
        let out = clean_coverage(&layer(vec![rectangle(0., 0., 4., 4.), rectangle(1., 1., 1., 1.)]), &Default::default()).unwrap();
        assert_eq!(areas(&out), vec![16., 0.]);
    }
    #[test]
    fn enclosed_gap_width_and_inlet() {
        let l = layer(vec![
            rectangle(0., 0., 1.9, 4.),
            rectangle(2.1, 0., 1.9, 4.),
            rectangle(1.9, 0., 0.2, 0.2),
            rectangle(1.9, 3.8, 0.2, 0.2),
        ]);
        for (width, expected) in [(0.15, 15.28), (0.25, 16.)] {
            let out = clean_coverage(&l, &CoverageCleanOptions { maximum_gap_width: width, ..Default::default() }).unwrap();
            assert!((areas(&out).iter().sum::<f64>() - expected).abs() < 1e-12);
            assert!(super::super::coverage::check_coverage(&out, &Default::default()).unwrap().is_empty());
        }
    }
    #[test]
    fn snapping_nodes_and_partial_shared_edge() {
        let l = layer(vec![rectangle(0., 0., 2., 4.), rectangle(2.001, 1., 2., 2.)]);
        let out = clean_coverage(&l, &CoverageCleanOptions { snapping_distance: 0.01, ..Default::default() }).unwrap();
        assert!(super::super::coverage::check_coverage(&out, &Default::default()).unwrap().is_empty());
    }
    #[test]
    fn z_nodes_and_copied_layers_keep_their_ordinates() {
        let mut west = rectangle(0., 0., 3., 2.);
        let mut east = rectangle(2., 0., 2., 2.);
        for (g, base) in [(&mut west, 1.), (&mut east, 10.)] {
            if let Geometry::Polygon { exterior, .. } = g {
                for (i, c) in exterior.0.iter_mut().enumerate() {
                    c.z = Some(base + i as f64);
                    c.m = Some(100. + i as f64);
                }
            }
        }
        let l = layer(vec![west, east]);
        let out = clean_coverage(&l, &Default::default()).unwrap();
        let nodes: Vec<_> = out.features.iter().flat_map(|f| f.geometry.as_ref().unwrap().all_coords()).collect();
        assert!(nodes.iter().all(|p| p.z.is_some() && p.m.is_none()));
        assert!(nodes.iter().any(|p| p.x == 2. && p.y == 0. && p.z == Some(10.)));
        assert!(nodes.iter().any(|p| p.x == 3. && p.y == 0. && p.z == Some(2.)));
        let mut point = layer(vec![Geometry::Point(Coord { x: 4., y: 5., z: Some(6.), m: Some(7.) })]);
        point.name = "point".into();
        let copied = clean_coverage_layers(&[l, point.clone()], Some("coverage"), &Default::default()).unwrap();
        assert_eq!(copied[1].features[0].geometry, point.features[0].geometry);
        assert!(clean_coverage_layers(&copied, Some("missing"), &Default::default()).is_err());
    }
    #[test]
    fn empty_inputs_and_invalid_options() {
        let empty = Layer::new("empty");
        assert!(clean_coverage(&empty, &Default::default()).unwrap().is_empty());
        let l = layer(vec![Geometry::MultiPolygon(vec![]), Geometry::Polygon { exterior: Ring(vec![]), interiors: vec![] }]);
        assert_eq!(areas(&clean_coverage(&l, &Default::default()).unwrap()), vec![0., 0.]);
        for width in [-1., f64::NAN, f64::INFINITY] {
            assert!(clean_coverage(&l, &CoverageCleanOptions { maximum_gap_width: width, ..Default::default() }).is_err());
        }
        assert!(!radius_within(&geo::Polygon::new(geo::LineString::new(vec![]), vec![]), 0.));
    }
    #[test]
    fn active_nulls_and_non_polygonal_features_are_rejected() {
        let mut point = layer(vec![Geometry::Point(Coord::xy(1., 2.))]);
        assert!(clean_coverage(&point, &Default::default()).unwrap_err().to_string().contains("polygonal geometry"));
        point.features[0].geometry = None;
        assert!(clean_coverage(&point, &Default::default()).unwrap_err().to_string().contains("not null"));
    }
    #[test]
    fn reversed_cycles_do_not_become_holes_of_their_own_face() {
        for shift in 0..3 {
            let mut points = vec![C { x: 0.1, y: 0.1 }, C { x: 3.7, y: 0.2 }, C { x: 1.4, y: 4.9 }];
            let mut other = points.clone();
            other.reverse();
            other.rotate_left(shift);
            points.push(points[0]);
            other.push(other[0]);
            let result = polygons_from_rings(vec![geo::LineString(points), geo::LineString(other)]);
            assert_eq!(result.len(), 1);
            assert!(result[0].interiors().is_empty());
            assert!(result[0].unsigned_area() > 8.);
        }
    }
}
