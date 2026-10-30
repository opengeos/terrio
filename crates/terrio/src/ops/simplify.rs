// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions.
// SPDX-FileCopyrightText: Copyright (c) 2023 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions, and others.
// SPDX-FileCopyrightText: Copyright (c) 1999, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2008-2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   JTS 1.20.0 (commit 6e95fe82feb986a7aa657f4ffa406d8c290af509):
//     modules/core/src/main/java/org/locationtech/jts/simplify/TaggedLineStringSimplifier.java
//     modules/core/src/main/java/org/locationtech/jts/simplify/TaggedLineString.java
//     modules/core/src/main/java/org/locationtech/jts/simplify/ComponentJumpChecker.java
//     modules/core/src/main/java/org/locationtech/jts/simplify/TopologyPreservingSimplifier.java
//     modules/core/src/main/java/org/locationtech/jts/algorithm/RayCrossingCounter.java
//     modules/core/src/main/java/org/locationtech/jts/algorithm/Distance.java
//     modules/core/src/main/java/org/locationtech/jts/algorithm/RobustLineIntersector.java
//   GDAL v3.13.3:
//     ogr/ogrlinestring.cpp

//! Topology-preserving simplification, derived from JTS 1.20.0, and OGR segmentization.
//!
//! The JTS sources are `simplify/TaggedLineStringSimplifier`, `TaggedLineString`,
//! `ComponentJumpChecker` and `algorithm/RayCrossingCounter`. Copyright (c) 2016
//! Vivid Solutions, and others; Copyright (c) 2023 Martin Davis. EDL-1.0;
//! see NOTICE and docs/jts-provenance.md. GEOS is only a behavioral oracle.
//!
//! Segments retain their input coordinate indices, preserving Z while dropping M.
//! An explicit work stack replaces JTS's recursion; R-trees replace its quadtree.

use crate::{Error, Geometry, Result};
use geo::algorithm::kernels::{Kernel, Orientation, RobustKernel};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use wbvector::{Coord, Ring};

type P = (f64, f64);

/// Robust orientation, positive for a counterclockwise turn.
pub(crate) fn orient(a: P, b: P, c: P) -> i32 {
    let xy = |p: P| geo::coord! { x: p.0, y: p.1 };
    match RobustKernel::orient2d(xy(a), xy(b), xy(c)) {
        Orientation::CounterClockwise => 1,
        Orientation::Clockwise => -1,
        Orientation::Collinear => 0,
    }
}

/// Euclidean distance, using the arithmetic of JTS Coordinate.distance.
pub(crate) fn dist(a: P, b: P) -> f64 {
    ((a.0 - b.0) * (a.0 - b.0) + (a.1 - b.1) * (a.1 - b.1)).sqrt()
}

/// JTS Distance.pointToSegment, with endpoint clamping.
fn distance_to_segment(p: P, a: P, b: P) -> f64 {
    if a == b {
        return dist(p, a);
    }
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let squared = dx * dx + dy * dy;
    let projection = ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / squared;
    if projection <= 0.0 {
        return dist(p, a);
    }
    if projection >= 1.0 {
        return dist(p, b);
    }
    (((a.1 - p.1) * dx - (a.0 - p.0) * dy) / squared).abs() * squared.sqrt()
}

fn bounds(a: P, b: P) -> AABB<[f64; 2]> {
    AABB::from_corners([a.0.min(b.0), a.1.min(b.1)], [a.0.max(b.0), a.1.max(b.1)])
}

/// JTS's invalid-intersection rule: equality, overlap, crossing or an interior touch.
pub(crate) fn conflicts(a: P, b: P, c: P, d: P) -> bool {
    if (a == c && b == d) || (a == d && b == c) {
        return true;
    }
    if !rstar::Envelope::intersects(&bounds(a, b), &bounds(c, d)) {
        return false;
    }
    let ab = [orient(a, b, c), orient(a, b, d)];
    let cd = [orient(c, d, a), orient(c, d, b)];
    if ab[0] * ab[1] < 0 && cd[0] * cd[1] < 0 {
        return true;
    }
    // Collinear overlap also has an endpoint interior to at least one segment.
    [(c, a, b, ab[0]), (d, a, b, ab[1]), (a, c, d, cd[0]), (b, c, d, cd[1])]
        .iter()
        .any(|&(p, u, v, o)| o == 0 && p != u && p != v && rstar::Envelope::contains_point(&bounds(u, v), &[p.0, p.1]))
}

/// The crossing count (not boundary classification) from JTS RayCrossingCounter.
fn ray_count(p: P, segments: impl Iterator<Item = (P, P)>) -> usize {
    segments
        .filter(|&(a, b)| {
            if (a.0 < p.0 && b.0 < p.0) || p == b || (a.1 == p.1 && b.1 == p.1) {
                return false;
            }
            if (a.1 > p.1) == (b.1 > p.1) {
                return false;
            }
            let turn = orient(a, b, p);
            if b.1 < a.1 { turn < 0 } else { turn > 0 }
        })
        .count()
}

#[derive(Clone, Copy)]
struct IndexedSegment {
    a: P,
    b: P,
    owner: usize,
    start: usize,
    active: bool,
}

type Tree = RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>;

/// JTS's separate input and output segment indexes. The bulk-loaded input index
/// stays immutable; the output index is grown from empty. Inactive segment flags
/// support removals without relying on geometric identity.
struct TaggedSimplifier {
    points: Vec<Vec<P>>,
    rings: Vec<bool>,
    component_points: Vec<P>,
    original: Vec<Vec<usize>>,
    segments: Vec<IndexedSegment>,
    input_tree: Tree,
    output_tree: Tree,
}

impl TaggedSimplifier {
    fn new(parts: &[(Vec<Coord>, bool)]) -> Self {
        let points: Vec<Vec<P>> = parts.iter().map(|(cs, _)| cs.iter().map(|c| (c.x, c.y)).collect()).collect();
        let mut segments = Vec::new();
        let mut original = Vec::new();
        for (owner, cs) in points.iter().enumerate() {
            let mut ids = Vec::new();
            for (start, pair) in cs.windows(2).enumerate() {
                ids.push(segments.len());
                segments.push(IndexedSegment { a: pair[0], b: pair[1], owner, start, active: true });
            }
            original.push(ids);
        }
        let entries = segments
            .iter()
            .enumerate()
            .map(|(id, s)| {
                GeomWithData::new(Rectangle::from_corners([s.a.0.min(s.b.0), s.a.1.min(s.b.1)], [s.a.0.max(s.b.0), s.a.1.max(s.b.1)]), id)
            })
            .collect();
        let component_points = points.iter().map(|cs| cs.get(1).copied().unwrap_or(cs[0])).collect();
        Self {
            points,
            component_points,
            rings: parts.iter().map(|p| p.1).collect(),
            original,
            segments,
            input_tree: RTree::bulk_load(entries),
            output_tree: RTree::new(),
        }
    }

    fn add(&mut self, owner: usize, a: usize, b: usize) -> usize {
        let (p, q) = (self.points[owner][a], self.points[owner][b]);
        let id = self.segments.len();
        self.segments.push(IndexedSegment { a: p, b: q, owner, start: usize::MAX, active: true });
        self.output_tree.insert(GeomWithData::new(Rectangle::from_corners([p.0.min(q.0), p.1.min(q.1)], [p.0.max(q.0), p.1.max(q.1)]), id));
        id
    }

    fn intersects(&self, a: P, b: P, excluded: impl Fn(&IndexedSegment) -> bool) -> bool {
        self.input_tree
            .locate_in_envelope_intersecting(bounds(a, b))
            .chain(self.output_tree.locate_in_envelope_intersecting(bounds(a, b)))
            .any(|entry| {
                let s = &self.segments[entry.data];
                s.active && !excluded(s) && conflicts(a, b, s.a, s.b)
            })
    }

    fn jumps(&self, owner: usize, replaced: &[(P, P)], flat: (P, P)) -> bool {
        let env = replaced
            .iter()
            .flat_map(|&(a, b)| [a, b])
            .fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |e, p| {
                (e.0.min(p.0), e.1.min(p.1), e.2.max(p.0), e.3.max(p.1))
            });
        self.component_points.iter().enumerate().any(|(id, &p)| {
            if id == owner {
                return false;
            }
            p.0 >= env.0
                && p.0 <= env.2
                && p.1 >= env.1
                && p.1 <= env.3
                && ray_count(p, replaced.iter().copied()) % 2 != ray_count(p, std::iter::once(flat)) % 2
        })
    }

    fn line(&mut self, owner: usize, tolerance: f64) -> Vec<usize> {
        let n = self.points[owner].len();
        if n < 2 {
            return Vec::new();
        }
        let minimum = if self.points[owner].first() == self.points[owner].last() { 4 } else { 2 };
        let mut output: Vec<(usize, usize, usize)> = Vec::new();
        let mut stack = vec![(0, n - 1, 1)];
        while let Some((a, b, depth)) = stack.pop() {
            if b == a + 1 {
                output.push((a, b, self.original[owner][a]));
                continue;
            }
            let (p, q) = (self.points[owner][a], self.points[owner][b]);
            let (mut farthest, mut distance) = (a + 1, -1.0);
            for k in a + 1..b {
                let d = distance_to_segment(self.points[owner][k], p, q);
                if d > distance {
                    (farthest, distance) = (k, d);
                }
            }
            let size = if output.is_empty() { 0 } else { output.len() + 1 };
            let enough = size >= minimum || depth + 1 >= minimum;
            let valid = enough
                && distance >= 0.0
                && distance <= tolerance
                && !self.intersects(p, q, |s| s.owner == owner && s.start >= a && s.start < b)
                && !self.jumps(owner, &self.points[owner][a..=b].windows(2).map(|s| (s[0], s[1])).collect::<Vec<_>>(), (p, q));
            if valid {
                for &id in &self.original[owner][a..b] {
                    self.segments[id].active = false;
                }
                let id = self.add(owner, a, b);
                output.push((a, b, id));
            } else {
                stack.push((farthest, b, depth + 1));
                stack.push((a, farthest, depth + 1));
            }
        }
        // JTS only moves endpoints of LinearRings, not closed LineStrings.
        if self.rings[owner] && output.len() + 1 > minimum {
            let first = output[0];
            let last = output[output.len() - 1];
            let (a, mid, b) = (self.points[owner][last.0], self.points[owner][first.0], self.points[owner][first.1]);
            let valid = orient(a, b, mid) == 0 || (!self.intersects(a, b, |_| false) && !self.jumps(owner, &[(a, mid), (mid, b)], (a, b)));
            if distance_to_segment(mid, a, b) <= tolerance && valid {
                self.segments[first.2].active = false;
                self.segments[last.2].active = false;
                let id = self.add(owner, last.0, first.1);
                output[0] = (last.0, first.1, id);
                output.pop();
            }
        }
        let mut indices: Vec<usize> = output.iter().map(|s| s.0).collect();
        if let Some(s) = output.last() {
            indices.push(s.1);
        }
        // Executable GEOS observations distinguish completed components from
        // untouched ones: their jump probe is the first retained coordinate,
        // rather than JTS 1.20's original second coordinate.
        if let Some(&first) = indices.first() {
            self.component_points[owner] = self.points[owner][first];
        }
        indices
    }
}

pub(crate) fn closed_ring(r: &[Coord]) -> Vec<Coord> {
    let mut cs = r.to_vec();
    if let (Some(a), Some(b)) = (cs.first(), cs.last())
        && (a.x != b.x || a.y != b.y)
    {
        cs.push(a.clone());
    }
    cs
}

/// A ring as terrio's layers hold them: without the closing point, which writers add.
pub(crate) fn open_ring(mut cs: Vec<Coord>) -> Ring {
    if cs.len() > 1 && cs.first().zip(cs.last()).is_some_and(|(a, b)| a.x == b.x && a.y == b.y) {
        cs.pop();
    }
    Ring(cs)
}

/// Visits linear components in geometry order (polygon shells before their
/// holes), as `(coordinates, is_ring)`.
fn components(g: &Geometry, out: &mut Vec<(Vec<Coord>, bool)>) {
    match g {
        Geometry::Point(_) | Geometry::MultiPoint(_) => {}
        Geometry::LineString(cs) => {
            if !cs.is_empty() {
                out.push((cs.clone(), false));
            }
        }
        Geometry::MultiLineString(ls) => ls.iter().filter(|l| !l.is_empty()).for_each(|l| out.push((l.clone(), false))),
        Geometry::Polygon { exterior, interiors } => {
            for r in std::iter::once(exterior).chain(interiors) {
                if !r.0.is_empty() {
                    out.push((closed_ring(&r.0), true));
                }
            }
        }
        Geometry::MultiPolygon(ps) => {
            for (e, h) in ps {
                for r in std::iter::once(e).chain(h) {
                    if !r.0.is_empty() {
                        out.push((closed_ring(&r.0), true));
                    }
                }
            }
        }
        Geometry::GeometryCollection(gs) => gs.iter().for_each(|g| components(g, out)),
    }
}

/// Rebuilds a geometry from the simplified coordinates of its components, taken in order.
fn rebuild(g: &Geometry, results: &mut std::vec::IntoIter<Vec<Coord>>) -> Geometry {
    let xyz = |c: &Coord| Coord { x: c.x, y: c.y, z: c.z, m: None };
    let mut ring = |r: &Ring| if r.0.is_empty() { Ring(Vec::new()) } else { open_ring(results.next().unwrap_or_default()) };
    match g {
        Geometry::Point(c) => Geometry::Point(xyz(c)),
        Geometry::MultiPoint(cs) => Geometry::MultiPoint(cs.iter().map(xyz).collect()),
        Geometry::LineString(cs) => Geometry::LineString(if cs.is_empty() { Vec::new() } else { results.next().unwrap_or_default() }),
        Geometry::MultiLineString(ls) => Geometry::MultiLineString(
            ls.iter().map(|l| if l.is_empty() { Vec::new() } else { results.next().unwrap_or_default() }).collect(),
        ),
        Geometry::Polygon { exterior, interiors } => {
            let exterior = ring(exterior);
            Geometry::Polygon { exterior, interiors: interiors.iter().map(&mut ring).collect() }
        }
        Geometry::MultiPolygon(ps) => Geometry::MultiPolygon(
            ps.iter()
                .map(|(e, h)| {
                    let e = ring(e);
                    (e, h.iter().map(&mut ring).collect())
                })
                .collect(),
        ),
        Geometry::GeometryCollection(gs) => Geometry::GeometryCollection(gs.iter().map(|g| rebuild(g, results)).collect()),
    }
}

/// Simplifies linear components using JTS's topology-preserving algorithm.
/// Points and Z values are preserved; M values are dropped. XY must be finite,
/// and tolerance must be non-negative (positive infinity is accepted).
pub fn simplify_preserve_topology(g: &Geometry, tolerance: f64) -> Result<Geometry> {
    if tolerance.is_nan() || tolerance < 0.0 {
        return Err(Error::InvalidArgument("the simplification tolerance must be a non-negative number".into()));
    }
    if g.all_coords().iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
        return Err(Error::InvalidArgument("non-finite XY coordinates".into()));
    }
    let mut parts = Vec::new();
    components(g, &mut parts);
    if parts.iter().any(|(cs, _)| cs.len() == 1) {
        return Err(Error::InvalidArgument("a line must contain zero or at least two points".into()));
    }
    let mut simplifier = TaggedSimplifier::new(&parts);
    let results = parts
        .iter()
        .enumerate()
        .map(|(id, (cs, _))| {
            simplifier
                .line(id, tolerance)
                .into_iter()
                .map(|i| {
                    let c = &cs[i];
                    Coord { x: c.x, y: c.y, z: c.z, m: None }
                })
                .collect()
        })
        .collect::<Vec<_>>();
    Ok(rebuild(g, &mut results.into_iter()))
}

/// Adds vertices so that no segment is longer than `max_length`, as OGR's
/// `OGRSimpleCurve::segmentize` does: a segment of length `L` gets `floor(L / max_length - 0.01)`
/// evenly spaced intermediate points (none when `L² - max² <= 1e-5 max²`). Lines are processed
/// from the end with the larger X (then Y), so that a line and its reverse get the same points.
/// Intermediate points take the Z and M of the segment's start in that processing direction
/// (OGR does not interpolate them). Points are unchanged.
pub fn segmentize(g: &Geometry, max_length: f64) -> Result<Geometry> {
    if max_length.is_nan() || max_length <= 0.0 || !max_length.is_finite() {
        return Err(Error::InvalidArgument("the maximum segment length must be a positive number".into()));
    }
    segmentize_budget(g, max_length, &mut { MAX_SEGMENTIZED_POINTS })
}

fn segmentize_ring(r: &Ring, max_length: f64, remaining: &mut usize) -> Result<Ring> {
    if r.0.len() > *remaining {
        return Err(Error::InvalidArgument("too many points in a segmentized feature (limit: 10,000,000 vertices)".into()));
    }
    Ok(open_ring(segmentize_line(&closed_ring(&r.0), max_length, remaining)?))
}

fn segmentize_budget(g: &Geometry, max_length: f64, remaining: &mut usize) -> Result<Geometry> {
    Ok(match g {
        Geometry::Point(_) | Geometry::MultiPoint(_) => {
            let n = match g {
                Geometry::MultiPoint(cs) => cs.len(),
                _ => 1,
            };
            *remaining = remaining
                .checked_sub(n)
                .ok_or_else(|| Error::InvalidArgument("too many points in a segmentized feature (limit: 10,000,000 vertices)".into()))?;
            g.clone()
        }
        Geometry::LineString(cs) => Geometry::LineString(segmentize_line(cs, max_length, remaining)?),
        Geometry::MultiLineString(ls) => {
            Geometry::MultiLineString(ls.iter().map(|l| segmentize_line(l, max_length, remaining)).collect::<Result<_>>()?)
        }
        Geometry::Polygon { exterior, interiors } => Geometry::Polygon {
            exterior: segmentize_ring(exterior, max_length, remaining)?,
            interiors: interiors.iter().map(|r| segmentize_ring(r, max_length, remaining)).collect::<Result<_>>()?,
        },
        Geometry::MultiPolygon(ps) => Geometry::MultiPolygon(
            ps.iter()
                .map(|(e, hs)| {
                    Ok((
                        segmentize_ring(e, max_length, remaining)?,
                        hs.iter().map(|h| segmentize_ring(h, max_length, remaining)).collect::<Result<_>>()?,
                    ))
                })
                .collect::<Result<_>>()?,
        ),
        Geometry::GeometryCollection(gs) => {
            Geometry::GeometryCollection(gs.iter().map(|g| segmentize_budget(g, max_length, remaining)).collect::<Result<_>>()?)
        }
    })
}

/// The most vertices a segmentized feature may have, including all components.
const MAX_SEGMENTIZED_POINTS: usize = 10_000_000;

fn segmentize_line(cs: &[Coord], max_length: f64, remaining: &mut usize) -> Result<Vec<Coord>> {
    let n = cs.len();
    if n > *remaining {
        return Err(Error::InvalidArgument("too many points in a segmentized feature (limit: 10,000,000 vertices)".into()));
    }
    if n < 2 {
        *remaining -= n;
        return Ok(cs.to_vec());
    }
    let (first, last) = (&cs[0], &cs[n - 1]);
    if first.x < last.x || (first.x == last.x && first.y < last.y) {
        let reversed: Vec<Coord> = cs.iter().rev().cloned().collect();
        let mut out = segmentize_line_forward(&reversed, max_length, remaining)?;
        out.reverse();
        return Ok(out);
    }
    segmentize_line_forward(cs, max_length, remaining)
}

fn segmentize_line_forward(cs: &[Coord], max_length: f64, remaining: &mut usize) -> Result<Vec<Coord>> {
    const REL_EPSILON_LENGTH_SQUARE: f64 = 1e-5;
    const REL_EPSILON_ROUND: f64 = 1e-2;
    let square_max = max_length * max_length;
    let intermediate = |a: &Coord, b: &Coord| -> (f64, f64, usize) {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let square = dx * dx + dy * dy;
        // Keep OGR's ordinary squared arithmetic, but avoid underflow/overflow
        // silently suppressing subdivisions for very small or large lengths.
        let ratio = if square_max.is_normal() && square.is_normal() { (square / square_max).sqrt() } else { dx.hypot(dy) / max_length };
        if ratio > (1.0 + REL_EPSILON_LENGTH_SQUARE).sqrt() {
            let k = (ratio - REL_EPSILON_ROUND).floor();
            (dx, dy, if k.is_nan() { 0 } else { k.clamp(0.0, i32::MAX as f64) as usize })
        } else {
            (dx, dy, 0)
        }
    };
    let mut total = cs.len();
    for w in cs.windows(2) {
        total = total.saturating_add(intermediate(&w[0], &w[1]).2);
        if total > *remaining {
            return Err(Error::InvalidArgument("too many points in a segmentized line (per-feature limit: 10,000,000 vertices)".into()));
        }
    }
    let mut out = Vec::new();
    *remaining -= total;
    out.try_reserve_exact(total).map_err(|_| Error::InvalidArgument("cannot allocate segmentized line".into()))?;
    for (i, a) in cs.iter().enumerate() {
        out.push(a.clone());
        let Some(b) = cs.get(i + 1) else { break };
        let (dx, dy, k) = intermediate(a, b);
        let (rx, ry) = (dx / (k as f64 + 1.0), dy / (k as f64 + 1.0));
        for j in 1..=k {
            out.push(Coord { x: a.x + j as f64 * rx, y: a.y + j as f64 * ry, z: a.z, m: a.m });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_endpoint_replacement_keeps_a_leaf_segment_barrier() {
        let coords = |p: &[(f64, f64)]| p.iter().map(|&(x, y)| Coord::xy(x, y)).collect::<Vec<_>>();
        let ring = coords(&[(0., 10.), (0., 0.), (20., 0.), (20., 20.), (0., 20.), (0., 10.)]);
        let line = coords(&[(-2., 1.), (-1., 3.), (1., 1.)]);
        let mut simplifier = TaggedSimplifier::new(&[(ring, true), (line, false)]);
        let first = simplifier.original[0][0];
        let last = *simplifier.original[0].last().unwrap();
        assert_eq!(simplifier.line(0, 2.), [4, 1, 2, 3, 4]);
        assert!(!simplifier.segments[first].active);
        assert!(!simplifier.segments[last].active);
        // The replacement covers both removed leaves, so a later component's
        // shortcut must still see the boundary that it would cross.
        assert!(simplifier.intersects((-2., 1.), (1., 1.), |s| s.owner == 1));
        assert_eq!(simplifier.line(1, 2.), [0, 1, 2]);
    }

    fn line(pts: &[(f64, f64)]) -> Vec<Coord> {
        pts.iter().map(|&(x, y)| Coord::xy(x, y)).collect()
    }

    fn xy(cs: &[Coord]) -> Vec<(f64, f64)> {
        cs.iter().map(|c| (c.x, c.y)).collect()
    }

    #[test]
    fn simplification_keeps_unmeasurable_sections() {
        // Executable GEOS 3.14.1 retains the middle vertex when the
        // point-to-segment arithmetic overflows, rather than ignoring it.
        let input = Geometry::LineString(line(&[(0., 0.), (1e155, 1e155), (2e155, 0.)]));
        assert_eq!(simplify_preserve_topology(&input, 0.3).unwrap(), input);
    }

    #[test]
    fn mixed_unmeasurable_distances_match_the_reference() {
        // GEOS 3.14.1 also flattens this mixed NaN/finite-distance section.
        // Preserve the observed numeric limitation of this compatibility API.
        let input = Geometry::LineString(line(&[(0., 0.), (1e155, 1e155), (1., 0.), (2e155, 0.)]));
        let expected = Geometry::LineString(line(&[(0., 0.), (2e155, 0.)]));
        assert_eq!(simplify_preserve_topology(&input, 2.).unwrap(), expected);
    }

    #[test]
    fn simplification_can_add_segments_after_index_bulk_loading() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!("../../tests/data/jts-index-panic.json")).unwrap();
        let read = |name: &str| {
            let hex = fixture[name].as_str().unwrap();
            let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
            Geometry::from_wkb(&bytes).unwrap()
        };
        let input = read("input");
        let expected = read("expected");
        let result = simplify_preserve_topology(&input, 0.01).unwrap();
        assert_eq!(result.to_wkb(), expected.to_wkb());
    }

    #[test]
    fn simplifies_rings_like_geos() {
        // GEOS 3.14: POLYGON ((0 10,10 0,10 10,0 10)) alone, but with a hole whose point would
        // jump outside, the shell stays and the hole's endpoint moves.
        let shell = Geometry::polygon(line(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]), vec![]);
        let Geometry::Polygon { exterior, .. } = simplify_preserve_topology(&shell, 100.0).unwrap() else { panic!() };
        assert_eq!(xy(&exterior.0), [(0.0, 10.0), (10.0, 0.0), (10.0, 10.0)]);
        let square = Geometry::polygon(
            line(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]),
            vec![line(&[(4.0, 4.0), (5.0, 4.0), (5.0, 5.0), (4.0, 5.0), (4.0, 4.0)])],
        );
        let Geometry::Polygon { exterior, interiors } = simplify_preserve_topology(&square, 100.0).unwrap() else { panic!() };
        assert_eq!(xy(&exterior.0), [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]);
        assert_eq!(xy(&interiors[0].0), [(4.0, 5.0), (5.0, 4.0), (5.0, 5.0)]);
        // A closed line string keeps its endpoint and four points.
        let closed = Geometry::LineString(line(&[(0.0, 0.0), (5.0, 1.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]));
        let Geometry::LineString(cs) = simplify_preserve_topology(&closed, 100.0).unwrap() else { panic!() };
        assert_eq!(xy(&cs), [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
    }

    #[test]
    fn simplification_keeps_z_and_drops_m() {
        let g = Geometry::LineString(vec![
            Coord { x: 0.0, y: 0.0, z: Some(1.0), m: Some(2.0) },
            Coord { x: 5.0, y: 0.1, z: Some(3.0), m: Some(4.0) },
            Coord { x: 10.0, y: 0.0, z: Some(5.0), m: Some(6.0) },
        ]);
        let Geometry::LineString(cs) = simplify_preserve_topology(&g, 1.0).unwrap() else { panic!() };
        assert_eq!(cs, vec![Coord { x: 0.0, y: 0.0, z: Some(1.0), m: None }, Coord { x: 10.0, y: 0.0, z: Some(5.0), m: None }]);
        assert!(simplify_preserve_topology(&g, -1.0).is_err());
    }

    #[test]
    fn simplification_does_not_cross_other_components() {
        // Flattening the shell's spike would leave the hole outside: the spike vertex stays.
        let g = Geometry::polygon(
            line(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (5.0, 10.5), (0.0, 10.0), (0.0, 0.0)]),
            vec![line(&[(4.0, 10.0), (6.0, 10.0), (5.0, 10.2), (4.0, 10.0)])],
        );
        let Geometry::Polygon { exterior, .. } = simplify_preserve_topology(&g, 1.0).unwrap() else { panic!() };
        assert!(xy(&exterior.0).contains(&(5.0, 10.5)), "{:?}", xy(&exterior.0));
    }

    #[test]
    fn segmentizes_like_ogr() {
        // OGR: LINESTRING Z (0 0 0,2.5 0 10,5 0 10,7.5 0 10,10 0 10): processed from the end.
        let g = Geometry::LineString(vec![Coord::xyz(0.0, 0.0, 0.0), Coord::xyz(10.0, 0.0, 10.0)]);
        let Geometry::LineString(cs) = segmentize(&g, 3.0).unwrap() else { panic!() };
        assert_eq!(xy(&cs), [(0.0, 0.0), (2.5, 0.0), (5.0, 0.0), (7.5, 0.0), (10.0, 0.0)]);
        assert_eq!(cs.iter().map(|c| c.z.unwrap()).collect::<Vec<_>>(), [0.0, 10.0, 10.0, 10.0, 10.0]);
        let g = Geometry::LineString(line(&[(0.0, 0.0), (3.0, 0.0), (3.00001, 0.0), (9.0, 0.0)]));
        let Geometry::LineString(cs) = segmentize(&g, 3.0).unwrap() else { panic!() };
        assert_eq!(xy(&cs), [(0.0, 0.0), (3.0, 0.0), (3.00001, 0.0), (6.000005, 0.0), (9.0, 0.0)]);
        assert!(segmentize(&g, 0.0).is_err());
        // A ring without its closing point (as some drivers read it) is closed, and its closing
        // segment divided, as OGR does.
        let open = Geometry::polygon(line(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0)]), vec![]);
        let Geometry::Polygon { exterior, .. } = segmentize(&open, 3.0).unwrap() else { panic!() };
        assert_eq!(exterior.0.len(), 6);
        assert_eq!(xy(&exterior.0)[5], (2.0, 2.0));
    }
    #[test]
    fn refuses_singleton_lines_like_geos_and_excessive_densification() {
        let many = Geometry::MultiLineString(vec![line(&[(0., 0.), (10., 0.)]), line(&[(0., 0.), (10., 0.)])]);
        assert!(segmentize_budget(&many, 3., &mut 9).unwrap_err().to_string().contains("too many points"));
        let singleton = Geometry::LineString(vec![Coord::xy(1., 2.)]);
        assert!(simplify_preserve_topology(&singleton, 1.).is_err());
        let source = Geometry::LineString(vec![Coord::xy(0., 0.), Coord::xy(10., 0.)]);
        assert!(segmentize(&source, 1e-200).unwrap_err().to_string().contains("too many points"));
        assert!(segmentize(&source, 1e-200).unwrap_err().to_string().contains("10,000,000"));
        let tiny = Geometry::LineString(vec![Coord::xy(0., 0.), Coord::xy(1e-199, 0.)]);
        let Geometry::LineString(divided) = segmentize(&tiny, 1e-200).unwrap() else { panic!() };
        assert_eq!(divided.len(), 11);
        assert!(segmentize(&source, 1e-9).unwrap_err().to_string().contains("too many points"));
    }
}
