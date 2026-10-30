//! Exact noding of the joint input linework into a planar straight-line graph.

use super::exact::RPoint;
use super::{IPt, Polygon, RepairError};
use std::cmp::Ordering;
use std::collections::HashMap;

pub type V2 = (i64, i64);

/// An input segment with lexicographically ordered integer endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Segment {
    pub a: V2,
    pub b: V2,
}

impl Segment {
    pub fn direction(&self) -> V2 {
        (self.b.0 - self.a.0, self.b.1 - self.a.1)
    }
}

/// A dissolved edge `u -> v` (u lexicographically before v) lying on `segment`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub u: usize,
    pub v: usize,
    pub segment: usize,
}

pub struct Arrangement {
    pub segments: Vec<Segment>,
    pub points: Vec<RPoint>,
    pub edges: Vec<Edge>,
}

pub fn orient(a: V2, b: V2, c: V2) -> i128 {
    let abx = i128::from(b.0 - a.0);
    let aby = i128::from(b.1 - a.1);
    let acx = i128::from(c.0 - a.0);
    let acy = i128::from(c.1 - a.1);
    abx * acy - aby * acx
}

pub fn cross(a: V2, b: V2) -> i128 {
    i128::from(a.0) * i128::from(b.1) - i128::from(a.1) * i128::from(b.0)
}

/// Collect the linework of every ring of every polygon with a nonempty exterior.
pub fn collect_segments(polys: &[Polygon<IPt>]) -> Vec<Segment> {
    let mut segments = Vec::new();
    for polygon in polys {
        let Some(exterior) = polygon.first() else { continue };
        if exterior.is_empty() {
            // MV-05 policy: an absent exterior removes the polygon; holes are not promoted.
            continue;
        }
        for ring in polygon {
            let mut points: Vec<V2> = Vec::with_capacity(ring.len());
            for &(x, y) in ring {
                let p = (i64::from(x), i64::from(y));
                if points.last() != Some(&p) {
                    points.push(p);
                }
            }
            while points.len() > 1 && points.first() == points.last() {
                points.pop();
            }
            if points.len() < 2 {
                continue;
            }
            for i in 0..points.len() {
                let p = points[i];
                let q = points[(i + 1) % points.len()];
                if p != q {
                    segments.push(if p < q { Segment { a: p, b: q } } else { Segment { a: q, b: p } });
                }
            }
        }
    }
    segments.sort_unstable();
    segments.dedup();
    segments
}

type Bbox = [i64; 4];

fn bbox(s: &Segment) -> Bbox {
    [s.a.0.min(s.b.0), s.a.0.max(s.b.0), s.a.1.min(s.b.1), s.a.1.max(s.b.1)]
}

fn overlaps(p: &Bbox, q: &Bbox) -> bool {
    p[0] <= q[1] && q[0] <= p[1] && p[2] <= q[3] && q[2] <= p[3]
}

struct BvhNode {
    bbox: Bbox,
    start: usize,
    end: usize,
    children: Option<(usize, usize)>,
}

/// Static bounding-volume hierarchy over segment boxes (median splits).
struct Bvh {
    nodes: Vec<BvhNode>,
    order: Vec<usize>,
}

const LEAF: usize = 8;

impl Bvh {
    fn new(boxes: &[Bbox]) -> Self {
        let mut order: Vec<usize> = (0..boxes.len()).collect();
        let mut nodes = Vec::new();
        if boxes.is_empty() {
            return Bvh { nodes, order };
        }
        let mut pending = vec![(0usize, boxes.len(), usize::MAX, false)];
        while let Some((start, end, parent, right)) = pending.pop() {
            let mut b = [i64::MAX, i64::MIN, i64::MAX, i64::MIN];
            for &i in &order[start..end] {
                let c = &boxes[i];
                b = [b[0].min(c[0]), b[1].max(c[1]), b[2].min(c[2]), b[3].max(c[3])];
            }
            let id = nodes.len();
            nodes.push(BvhNode { bbox: b, start, end, children: None });
            if parent != usize::MAX {
                let slot = &mut nodes[parent].children;
                *slot = Some(match (*slot, right) {
                    (Some((l, _)), true) => (l, id),
                    (Some((_, r)), false) => (id, r),
                    (None, true) => (usize::MAX, id),
                    (None, false) => (id, usize::MAX),
                });
            }
            if end - start > LEAF {
                let mid = start + (end - start) / 2;
                let wide = (i128::from(b[1]) - i128::from(b[0])) >= (i128::from(b[3]) - i128::from(b[2]));
                let key = |i: &usize| {
                    let c = &boxes[*i];
                    if wide { i128::from(c[0]) + i128::from(c[1]) } else { i128::from(c[2]) + i128::from(c[3]) }
                };
                order[start..end].select_nth_unstable_by_key(mid - start, key);
                pending.push((mid, end, id, true));
                pending.push((start, mid, id, false));
            }
        }
        Bvh { nodes, order }
    }

    /// Call `visit(j)` for every indexed box overlapping `query`.
    fn query(&self, boxes: &[Bbox], query: &Bbox, stack: &mut Vec<usize>, mut visit: impl FnMut(usize)) {
        if self.nodes.is_empty() {
            return;
        }
        stack.clear();
        stack.push(0);
        while let Some(id) = stack.pop() {
            let node = &self.nodes[id];
            if !overlaps(&node.bbox, query) {
                continue;
            }
            match node.children {
                Some((l, r)) => {
                    stack.push(l);
                    stack.push(r);
                }
                None => {
                    for &j in &self.order[node.start..node.end] {
                        if overlaps(&boxes[j], query) {
                            visit(j);
                        }
                    }
                }
            }
        }
    }
}

/// Record split points that the pair `(i, j)` induces on either segment.
fn intersect(segments: &[Segment], i: usize, j: usize, splits: &mut Vec<(usize, RPoint)>) {
    let s = segments[i];
    let t = segments[j];
    let (a, b, c, d) = (s.a, s.b, t.a, t.b);
    let d1 = orient(a, b, c);
    let d2 = orient(a, b, d);
    if d1 == 0 && d2 == 0 {
        // Collinear: endpoints strictly inside the other segment split it.
        for p in [c, d] {
            if a < p && p < b {
                splits.push((i, RPoint::integer(p.0, p.1)));
            }
        }
        for p in [a, b] {
            if c < p && p < d {
                splits.push((j, RPoint::integer(p.0, p.1)));
            }
        }
        return;
    }
    let d3 = orient(c, d, a);
    let d4 = orient(c, d, b);
    if (d1 > 0 && d2 > 0) || (d1 < 0 && d2 < 0) || (d3 > 0 && d4 > 0) || (d3 < 0 && d4 < 0) {
        return;
    }
    if d1 != 0 && d2 != 0 && d3 != 0 && d4 != 0 {
        // Proper crossing: P = a + (b - a) * num / den.
        let den = cross((b.0 - a.0, b.1 - a.1), (d.0 - c.0, d.1 - c.1));
        let num = cross((c.0 - a.0, c.1 - a.1), (d.0 - c.0, d.1 - c.1));
        let x = i128::from(a.0) * den + i128::from(b.0 - a.0) * num;
        let y = i128::from(a.1) * den + i128::from(b.1 - a.1) * num;
        let p = RPoint::new(x, y, den);
        splits.push((i, p));
        splits.push((j, p));
        return;
    }
    // Touching: an endpoint of one segment lies on the other.
    for (zero, p, target, ends) in [(d1, c, i, (a, b)), (d2, d, i, (a, b)), (d3, a, j, (c, d)), (d4, b, j, (c, d))] {
        if zero == 0 && p != ends.0 && p != ends.1 {
            splits.push((target, RPoint::integer(p.0, p.1)));
        }
    }
}

/// Node and dissolve the segments.
pub fn build(segments: Vec<Segment>) -> Result<Arrangement, RepairError> {
    let boxes: Vec<Bbox> = segments.iter().map(bbox).collect();
    let bvh = Bvh::new(&boxes);
    let mut splits: Vec<(usize, RPoint)> = Vec::new();
    let mut stack = Vec::new();
    for i in 0..segments.len() {
        bvh.query(&boxes, &boxes[i], &mut stack, |j| {
            if j > i {
                intersect(&segments, i, j, &mut splits);
            }
        });
    }
    // Points on one segment are ordered along it by lexicographic order.
    splits.sort_unstable_by(|p, q| p.0.cmp(&q.0).then_with(|| p.1.cmp_lex(&q.1)));
    splits.dedup();

    let mut ids: HashMap<RPoint, usize> = HashMap::new();
    let mut points: Vec<RPoint> = Vec::new();
    let mut id_of = |p: RPoint, points: &mut Vec<RPoint>| -> usize {
        *ids.entry(p).or_insert_with(|| {
            points.push(p);
            points.len() - 1
        })
    };
    let mut edges = Vec::with_capacity(segments.len() + splits.len());
    let mut cursor = 0;
    for (index, s) in segments.iter().enumerate() {
        let mut previous = id_of(RPoint::integer(s.a.0, s.a.1), &mut points);
        while cursor < splits.len() && splits[cursor].0 == index {
            let next = id_of(splits[cursor].1, &mut points);
            edges.push(Edge { u: previous, v: next, segment: index });
            previous = next;
            cursor += 1;
        }
        let last = id_of(RPoint::integer(s.b.0, s.b.1), &mut points);
        edges.push(Edge { u: previous, v: last, segment: index });
    }
    edges.sort_unstable_by_key(|e| (e.u, e.v));
    edges.dedup_by_key(|e| (e.u, e.v));
    for e in &edges {
        if e.u == e.v || points[e.u].cmp_lex(&points[e.v]) != Ordering::Less {
            return Err(RepairError::new("noding produced a non-forward edge"));
        }
    }
    Ok(Arrangement { segments, points, edges })
}
