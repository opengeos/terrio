//! Topology-preserving simplification and segmentization of single geometries.
//!
//! [`simplify_preserve_topology`] is a port of GEOS 3.14's `TopologyPreservingSimplifier`
//! (itself a port of the JTS Topology Suite's), which OGR's `SimplifyPreserveTopology`, `gdal
//! vector simplify` and `ogr2ogr -simplify` run: Douglas-Peucker simplification of every linear
//! component, where a section is only flattened when the flattened segment crosses no input or
//! output segment of the same geometry and does not move another component (a hole, say) to
//! its other side, rings keep at least four points and ring endpoints may move. Every output
//! vertex is an input vertex; Z is kept and M is dropped, as GDAL's GEOS conversion does.
//!
//! [`segmentize`] is OGR's `OGRSimpleCurve::segmentize`, which `gdal vector segmentize` and
//! `ogr2ogr -segmentize` run.

use crate::{Error, Geometry, Result};
use geo::algorithm::kernels::{Kernel, Orientation, RobustKernel};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use wbvector::{Coord, Ring};

type P = (f64, f64);

/// The orientation of `c` relative to the directed line `a`→`b`: 1 to the left, -1 to the
/// right, 0 collinear, with robust predicates (as GEOS's `Orientation::index`).
pub(crate) fn orient(a: P, b: P, c: P) -> i32 {
    let k = |p: P| geo::coord! { x: p.0, y: p.1 };
    match RobustKernel::orient2d(k(a), k(b), k(c)) {
        Orientation::CounterClockwise => 1,
        Orientation::Clockwise => -1,
        Orientation::Collinear => 0,
    }
}

/// Euclidean distance, as GEOS computes it (`sqrt(dx² + dy²)`).
pub(crate) fn dist(a: P, b: P) -> f64 {
    let (dx, dy) = (a.0 - b.0, a.1 - b.1);
    (dx * dx + dy * dy).sqrt()
}

/// The distance from `p` to the segment `a`-`b` (GEOS's `Distance::pointToSegment`).
fn point_segment_distance(p: P, a: P, b: P) -> f64 {
    if a == b {
        return dist(p, a);
    }
    let len2 = (b.0 - a.0) * (b.0 - a.0) + (b.1 - a.1) * (b.1 - a.1);
    let r = ((p.0 - a.0) * (b.0 - a.0) + (p.1 - a.1) * (b.1 - a.1)) / len2;
    if r <= 0.0 {
        return dist(p, a);
    }
    if r >= 1.0 {
        return dist(p, b);
    }
    let s = ((a.1 - p.1) * (b.0 - a.0) - (a.0 - p.0) * (b.1 - a.1)) / len2;
    s.abs() * len2.sqrt()
}

/// Whether the envelopes of two segments intersect.
fn envelopes_intersect(p1: P, p2: P, q1: P, q2: P) -> bool {
    !(p1.0.min(p2.0) > q1.0.max(q2.0)
        || p1.0.max(p2.0) < q1.0.min(q2.0)
        || p1.1.min(p2.1) > q1.1.max(q2.1)
        || p1.1.max(p2.1) < q1.1.min(q2.1))
}

/// Whether `q` is in the envelope of the segment `p1`-`p2`.
fn in_envelope(p1: P, p2: P, q: P) -> bool {
    q.0 >= p1.0.min(p2.0) && q.0 <= p1.0.max(p2.0) && q.1 >= p1.1.min(p2.1) && q.1 <= p1.1.max(p2.1)
}

/// Whether two segments intersect at a point interior to either of them: GEOS's
/// `RobustLineIntersector::computeIntersection` followed by `isInteriorIntersection`.
fn interior_intersection(p1: P, p2: P, q1: P, q2: P) -> bool {
    if !envelopes_intersect(p1, p2, q1, q2) {
        return false;
    }
    let (pq1, pq2) = (orient(p1, p2, q1), orient(p1, p2, q2));
    if (pq1 > 0 && pq2 > 0) || (pq1 < 0 && pq2 < 0) {
        return false;
    }
    let (qp1, qp2) = (orient(q1, q2, p1), orient(q1, q2, p2));
    if (qp1 > 0 && qp2 > 0) || (qp1 < 0 && qp2 < 0) {
        return false;
    }
    let interior = |points: &[P]| points.iter().any(|&i| (i != p1 && i != p2) || (i != q1 && i != q2));
    if pq1 == 0 && pq2 == 0 && qp1 == 0 && qp2 == 0 {
        // Collinear: the intersection is a point or a segment.
        let (q1p, q2p, p1q, p2q) = (in_envelope(p1, p2, q1), in_envelope(p1, p2, q2), in_envelope(q1, q2, p1), in_envelope(q1, q2, p2));
        let points: &[P] = if q1p && q2p {
            &[q1, q2]
        } else if p1q && p2q {
            &[p1, p2]
        } else if q1p && p1q {
            if q1 == p1 && !q2p && !p2q { &[q1] } else { &[q1, p1] }
        } else if q1p && p2q {
            if q1 == p2 && !q2p && !p1q { &[q1] } else { &[q1, p2] }
        } else if q2p && p1q {
            if q2 == p1 && !q1p && !p2q { &[q2] } else { &[q2, p1] }
        } else if q2p && p2q {
            if q2 == p2 && !q1p && !p1q { &[q2] } else { &[q2, p2] }
        } else {
            return false;
        };
        return interior(points);
    }
    if pq1 == 0 || pq2 == 0 || qp1 == 0 || qp2 == 0 {
        // An endpoint of one segment lies on the other.
        let i = if p1 == q1 || p1 == q2 {
            p1
        } else if p2 == q1 || p2 == q2 {
            p2
        } else if pq1 == 0 {
            q1
        } else if pq2 == 0 {
            q2
        } else if qp1 == 0 {
            p1
        } else {
            p2
        };
        return interior(&[i]);
    }
    // A proper intersection, interior to both segments.
    true
}

/// GEOS's `RayCrossingCounter`: the crossings of a ray from `p` towards +X by a segment.
fn crossings(p: P, segments: impl Iterator<Item = (P, P)>) -> usize {
    let mut count = 0;
    for (p1, p2) in segments {
        if p1.0 < p.0 && p2.0 < p.0 {
            continue;
        }
        if p == p2 {
            continue;
        }
        if p1.1 == p.1 && p2.1 == p.1 {
            continue;
        }
        if (p1.1 > p.1 && p2.1 <= p.1) || (p2.1 > p.1 && p1.1 <= p.1) {
            let mut o = orient(p1, p2, p);
            if o == 0 {
                continue;
            }
            if p2.1 < p1.1 {
                o = -o;
            }
            if o > 0 {
                count += 1;
            }
        }
    }
    count
}

/// A segment of a simplified line: the indices of its vertices, and its entry in the output
/// index when it is one.
#[derive(Clone, Copy)]
struct ResultSegment {
    a: usize,
    b: usize,
    output: Option<usize>,
}

/// A linear component being simplified (GEOS's `TaggedLineString`).
struct Line {
    pts: Vec<P>,
    min_size: usize,
    is_ring: bool,
    result: Vec<ResultSegment>,
}

impl Line {
    fn result_size(&self) -> usize {
        if self.result.is_empty() { 0 } else { self.result.len() + 1 }
    }

    fn component_point(&self) -> P {
        match self.result.first() {
            Some(s) => self.pts[s.a],
            None => self.pts[1.min(self.pts.len() - 1)],
        }
    }
}

type SegmentIndex<T> = RTree<GeomWithData<Rectangle<[f64; 2]>, T>>;

fn segment_box(a: P, b: P) -> Rectangle<[f64; 2]> {
    Rectangle::from_corners([a.0.min(b.0), a.1.min(b.1)], [a.0.max(b.0), a.1.max(b.1)])
}

fn query_box(a: P, b: P) -> AABB<[f64; 2]> {
    AABB::from_corners([a.0.min(b.0), a.1.min(b.1)], [a.0.max(b.0), a.1.max(b.1)])
}

/// GEOS's `TaggedLinesSimplifier`, with its input and output segment indices.
struct Simplifier {
    lines: Vec<Line>,
    tolerance: f64,
    input: SegmentIndex<(usize, usize)>,
    input_alive: Vec<Vec<bool>>,
    output: SegmentIndex<usize>,
    output_segments: Vec<(P, P, bool)>,
}

impl Simplifier {
    fn new(lines: Vec<Line>, tolerance: f64) -> Self {
        let mut items = Vec::new();
        let mut input_alive = Vec::with_capacity(lines.len());
        for (l, line) in lines.iter().enumerate() {
            let n = line.pts.len().saturating_sub(1);
            for k in 0..n {
                items.push(GeomWithData::new(segment_box(line.pts[k], line.pts[k + 1]), (l, k)));
            }
            input_alive.push(vec![true; n]);
        }
        Self { lines, tolerance, input: RTree::bulk_load(items), input_alive, output: RTree::new(), output_segments: Vec::new() }
    }

    fn add_output(&mut self, a: P, b: P) -> usize {
        let id = self.output_segments.len();
        self.output_segments.push((a, b, true));
        self.output.insert(GeomWithData::new(segment_box(a, b), id));
        id
    }

    fn invalid_intersection(s0: (P, P), s1: (P, P)) -> bool {
        if (s0.0 == s1.0 && s0.1 == s1.1) || (s0.0 == s1.1 && s0.1 == s1.0) {
            return true;
        }
        interior_intersection(s0.0, s0.1, s1.0, s1.1)
    }

    fn has_output_intersection(&self, flat: (P, P)) -> bool {
        self.output.locate_in_envelope_intersecting(&query_box(flat.0, flat.1)).any(|e| {
            let (a, b, alive) = self.output_segments[e.data];
            alive && Self::invalid_intersection((a, b), flat)
        })
    }

    /// Whether the flattening segment crosses an input segment, other than those of the section
    /// `excluded` (line, start, end) it replaces.
    fn has_input_intersection(&self, excluded: Option<(usize, usize, usize)>, flat: (P, P)) -> bool {
        self.input.locate_in_envelope_intersecting(&query_box(flat.0, flat.1)).any(|e| {
            let (l, k) = e.data;
            if !self.input_alive[l][k] {
                return false;
            }
            let pts = &self.lines[l].pts;
            if !Self::invalid_intersection((pts[k], pts[k + 1]), flat) {
                return false;
            }
            match excluded {
                Some((line, start, end)) if line == l => {
                    let inside = if start <= end { k >= start && k < end } else { k >= start || k <= end };
                    !inside
                }
                _ => true,
            }
        })
    }

    /// GEOS's `ComponentJumpChecker`: whether replacing `section` (segments) by `flat` moves a
    /// point of another component to the other side of the line.
    fn has_jump(&self, line: usize, section: &[(P, P)], flat: (P, P)) -> bool {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (a, b) in section {
            for p in [a, b] {
                x0 = x0.min(p.0);
                y0 = y0.min(p.1);
                x1 = x1.max(p.0);
                y1 = y1.max(p.1);
            }
        }
        self.lines.iter().enumerate().any(|(i, comp)| {
            if i == line || comp.pts.is_empty() {
                return false;
            }
            let p = comp.component_point();
            if p.0 < x0 || p.0 > x1 || p.1 < y0 || p.1 > y1 {
                return false;
            }
            crossings(p, section.iter().copied()) % 2 != crossings(p, std::iter::once(flat)) % 2
        })
    }

    fn section_valid(&self, line: usize, i: usize, j: usize, flat: (P, P)) -> bool {
        if self.has_output_intersection(flat) || self.has_input_intersection(Some((line, i, j)), flat) {
            return false;
        }
        let pts = &self.lines[line].pts;
        let section: Vec<(P, P)> = (i..j).map(|k| (pts[k], pts[k + 1])).collect();
        !self.has_jump(line, &section, flat)
    }

    fn simplify_line(&mut self, l: usize) {
        let n = self.lines[l].pts.len();
        if n == 0 {
            return;
        }
        // GEOS recurses, left section first; a stack keeps that order without deep recursion.
        let mut stack = vec![(0usize, n - 1, 0usize)];
        while let Some((i, j, depth)) = stack.pop() {
            let depth = depth + 1;
            let line = &self.lines[l];
            if i + 1 == j {
                self.lines[l].result.push(ResultSegment { a: i, b: j, output: None });
                continue;
            }
            let mut valid = true;
            if line.result_size() < line.min_size && depth + 1 < line.min_size {
                valid = false;
            }
            let (seg_a, seg_b) = (line.pts[i], line.pts[j]);
            let (mut furthest, mut max) = (i, -1.0);
            for k in i + 1..j {
                let d = point_segment_distance(line.pts[k], seg_a, seg_b);
                if d > max {
                    max = d;
                    furthest = k;
                }
            }
            // Extreme finite coordinates can overflow distance arithmetic to NaN;
            // keep the original segments when no comparable distance was found.
            if max < 0.0 {
                for k in i..j {
                    self.lines[l].result.push(ResultSegment { a: k, b: k + 1, output: None });
                }
                continue;
            }
            if max > self.tolerance {
                valid = false;
            }
            if valid && self.section_valid(l, i, j, (seg_a, seg_b)) {
                let id = self.add_output(seg_a, seg_b);
                for k in i..j {
                    self.input_alive[l][k] = false;
                }
                self.lines[l].result.push(ResultSegment { a: i, b: j, output: Some(id) });
                continue;
            }
            stack.push((furthest, j, depth));
            stack.push((i, furthest, depth));
        }
        let line = &self.lines[l];
        let pts = &line.pts;
        if line.is_ring && pts.len() >= 4 && pts[0] == pts[pts.len() - 1] {
            self.simplify_ring_endpoint(l);
        }
    }

    fn simplify_ring_endpoint(&mut self, l: usize) {
        let line = &self.lines[l];
        if line.result_size() <= line.min_size {
            return;
        }
        let (first, last) = (line.result[0], line.result[line.result.len() - 1]);
        let pts = &line.pts;
        let flat = (pts[last.a], pts[first.b]);
        let end = pts[first.a];
        if point_segment_distance(end, flat.0, flat.1) > self.tolerance {
            return;
        }
        let valid = orient(flat.0, flat.1, pts[first.a]) == 0
            || !(self.has_output_intersection(flat)
                || self.has_input_intersection(None, flat)
                || self.has_jump(l, &[(pts[first.a], pts[first.b]), (pts[last.a], pts[last.b])], flat));
        if !valid {
            return;
        }
        for s in [first, last] {
            if let Some(id) = s.output {
                self.output_segments[id].2 = false;
            }
        }
        let id = self.add_output(flat.0, flat.1);
        let line = &mut self.lines[l];
        line.result.pop();
        line.result[0] = ResultSegment { a: last.a, b: first.b, output: Some(id) };
    }
}

/// The coordinates of a ring, closed: drivers may omit the closing point.
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

/// Visits the linear components of a geometry in GEOS's order (polygon shells before their
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

/// Simplifies a geometry with a distance tolerance, preserving its topology, as GEOS's
/// `TopologyPreservingSimplifier` (OGR's `SimplifyPreserveTopology`) does. See the module
/// documentation. Points are unchanged; M values are dropped.
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
    let lines = parts
        .iter()
        .map(|(cs, is_ring)| {
            let pts: Vec<P> = cs.iter().map(|c| (c.x, c.y)).collect();
            let closed = pts.len() > 1 && pts.first() == pts.last();
            Line { pts, min_size: if closed { 4 } else { 2 }, is_ring: *is_ring, result: Vec::new() }
        })
        .collect();
    let mut s = Simplifier::new(lines, tolerance);
    for l in 0..s.lines.len() {
        s.simplify_line(l);
    }
    let results: Vec<Vec<Coord>> = s
        .lines
        .iter()
        .zip(&parts)
        .map(|(line, (cs, _))| {
            let xyz = |i: usize| Coord { x: cs[i].x, y: cs[i].y, z: cs[i].z, m: None };
            let mut out: Vec<Coord> = line.result.iter().map(|r| xyz(r.a)).collect();
            if let Some(r) = line.result.last() {
                out.push(xyz(r.b));
            }
            out
        })
        .collect();
    Ok(rebuild(g, &mut results.into_iter()))
}

/// Adds vertices so that no segment is longer than `max_length`, as OGR's
/// `OGRSimpleCurve::segmentize` does: a segment of length `L` gets `floor(L / max_length - 0.01)`
/// evenly spaced intermediate points (none when `L² - max² <= 1e-5 max²`). Lines are processed
/// from the end with the smaller X (then Y), so that a line and its reverse get the same points.
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
        return Err(Error::InvalidArgument("too many points in a segmentized feature".into()));
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
            *remaining =
                remaining.checked_sub(n).ok_or_else(|| Error::InvalidArgument("too many points in a segmentized feature".into()))?;
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
        return Err(Error::InvalidArgument("too many points in a segmentized feature".into()));
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
            return Err(Error::InvalidArgument("too many points in a segmentized line".into()));
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

    fn line(pts: &[(f64, f64)]) -> Vec<Coord> {
        pts.iter().map(|&(x, y)| Coord::xy(x, y)).collect()
    }

    fn xy(cs: &[Coord]) -> Vec<(f64, f64)> {
        cs.iter().map(|c| (c.x, c.y)).collect()
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
        let tiny = Geometry::LineString(vec![Coord::xy(0., 0.), Coord::xy(1e-199, 0.)]);
        let Geometry::LineString(divided) = segmentize(&tiny, 1e-200).unwrap() else { panic!() };
        assert_eq!(divided.len(), 11);
        assert!(segmentize(&source, 1e-9).unwrap_err().to_string().contains("too many points"));
    }
}
