//! Polygon validity and repair as GEOS decides and does them for GDAL's MVT writer.
//!
//! GDAL quantizes each polygon to the tile grid, checks the result with GEOS's `IsValid` and,
//! when it is invalid, replaces it with GEOS's `MakeValid` (the "linework" method): the rings'
//! segments are noded, faces are built from the noded linework (keeping those nested an even
//! number of times), and the result is toggled with the faces built from the edges left over
//! until none can be built. [`is_valid`] and [`make_valid`] do the same on the integer tile
//! coordinates.
//!
//! The faces of the noded linework are computed once (a planar graph of the noded edges), and
//! every later step of `MakeValid` (building areas from a subset of the edges, unions, symmetric
//! differences, boundaries and line differences) is done on sets of those faces and edges, which
//! is what GEOS computes with its overlay operations on that same linework.

use std::collections::HashMap;

/// A point of the tile grid.
pub(crate) type IPt = (i32, i32);

/// A point of the noded linework.
pub(crate) type P = (f64, f64);

/// A polygon: rings (the exterior first), each closed (its last point repeats the first).
pub(crate) type Polygon<T> = Vec<Vec<T>>;

/// An envelope `(min_x, min_y, max_x, max_y)`.
type Env = (f64, f64, f64, f64);

fn orient(a: IPt, b: IPt, c: IPt) -> i128 {
    (b.0 as i128 - a.0 as i128) * (c.1 as i128 - a.1 as i128) - (b.1 as i128 - a.1 as i128) * (c.0 as i128 - a.0 as i128)
}

fn sign(v: i128) -> i32 {
    v.signum() as i32
}

/// Whether `p` lies within the bounding box of `a`-`b` (for a point collinear with them: on the
/// segment).
fn in_box(a: IPt, b: IPt, p: IPt) -> bool {
    p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1)
}

/// The pairs of boxes `(min_x, min_y, max_x, max_y)` that overlap or touch, each pair once,
/// found through a grid.
pub(crate) fn box_pairs(boxes: &[(f64, f64, f64, f64)]) -> Vec<(usize, usize)> {
    let n = boxes.len();
    if n < 2 {
        return Vec::new();
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for b in boxes {
        x0 = x0.min(b.0);
        y0 = y0.min(b.1);
        x1 = x1.max(b.2);
        y1 = y1.max(b.3);
    }
    let cells = ((n as f64).sqrt().ceil() as usize).clamp(1, 512);
    let cw = ((x1 - x0) / cells as f64).max(1e-9);
    let ch = ((y1 - y0) / cells as f64).max(1e-9);
    let cell = |x: f64, y: f64| (((x - x0) / cw) as usize).min(cells - 1) + (((y - y0) / ch) as usize).min(cells - 1) * cells;
    let mut grid: Vec<Vec<usize>> = vec![Vec::new(); cells * cells];
    for (i, b) in boxes.iter().enumerate() {
        let (c0, c1) = (cell(b.0, b.1), cell(b.2, b.3));
        let (cx0, cy0, cx1, cy1) = (c0 % cells, c0 / cells, c1 % cells, c1 / cells);
        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                grid[cy * cells + cx].push(i);
            }
        }
    }
    let mut out = Vec::new();
    for (c, items) in grid.iter().enumerate() {
        for (k, &i) in items.iter().enumerate() {
            for &j in &items[k + 1..] {
                let (a, b) = (boxes[i], boxes[j]);
                if a.0 > b.2 || b.0 > a.2 || a.1 > b.3 || b.1 > a.3 {
                    continue;
                }
                // Reported by the cell holding the corner of the boxes' intersection.
                if cell(a.0.max(b.0), a.1.max(b.1)) == c {
                    out.push((i.min(j), i.max(j)));
                }
            }
        }
    }
    out
}

/// Where a point lies relative to a ring or polygon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
    Interior,
    Boundary,
    Exterior,
}

/// Where a point given in doubled coordinates lies relative to a ring (closed, in tile
/// coordinates).
fn locate2(p: (i64, i64), ring: &[IPt]) -> Loc {
    let mut inside = false;
    for w in ring.windows(2) {
        let (a, b) = ((2 * w[0].0 as i64, 2 * w[0].1 as i64), (2 * w[1].0 as i64, 2 * w[1].1 as i64));
        let cross = (b.0 - a.0) as i128 * (p.1 - a.1) as i128 - (b.1 - a.1) as i128 * (p.0 - a.0) as i128;
        if cross == 0 && p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1) {
            return Loc::Boundary;
        }
        if (a.1 > p.1) != (b.1 > p.1) {
            // The crossing's x against p's: (p.0 - a.0) * (b.1 - a.1) < (b.0 - a.0) * (p.1 - a.1), by
            // the sign of (b.1 - a.1).
            let lhs = (p.0 - a.0) as i128 * (b.1 - a.1) as i128;
            let rhs = (b.0 - a.0) as i128 * (p.1 - a.1) as i128;
            if (b.1 > a.1 && lhs < rhs) || (b.1 < a.1 && lhs > rhs) {
                inside = !inside;
            }
        }
    }
    if inside { Loc::Interior } else { Loc::Exterior }
}

/// Where a point (doubled coordinates) lies relative to a polygon: inside its exterior and
/// outside its holes.
fn locate_poly2(p: (i64, i64), poly: &[Vec<IPt>]) -> Loc {
    match locate2(p, &poly[0]) {
        Loc::Interior => {
            for h in &poly[1..] {
                match locate2(p, h) {
                    Loc::Interior => return Loc::Exterior,
                    Loc::Boundary => return Loc::Boundary,
                    Loc::Exterior => {}
                }
            }
            Loc::Interior
        }
        l => l,
    }
}

/// Whether ring `test` lies inside ring `target` (as GEOS's `isRingNested`): its first point, or
/// else the first point (or segment midpoint) of it that is not on `target`.
fn ring_nested(test: &[IPt], target: &[IPt]) -> bool {
    let candidates = test
        .iter()
        .map(|p| (2 * p.0 as i64, 2 * p.1 as i64))
        .chain(test.windows(2).map(|w| (w[0].0 as i64 + w[1].0 as i64, w[0].1 as i64 + w[1].1 as i64)));
    for p in candidates {
        match locate2(p, target) {
            Loc::Interior => return true,
            Loc::Exterior => return false,
            Loc::Boundary => {}
        }
    }
    false
}

/// Whether polygon `shell`'s exterior lies within polygon `poly` (GEOS's nested shells test).
fn shell_nested(shell: &[IPt], poly: &[Vec<IPt>]) -> bool {
    let candidates = shell
        .iter()
        .map(|p| (2 * p.0 as i64, 2 * p.1 as i64))
        .chain(shell.windows(2).map(|w| (w[0].0 as i64 + w[1].0 as i64, w[0].1 as i64 + w[1].1 as i64)));
    for p in candidates {
        match locate_poly2(p, poly) {
            Loc::Interior => return true,
            Loc::Exterior => return false,
            Loc::Boundary => {}
        }
    }
    false
}

/// The quadrant of a direction, as GEOS numbers them (NE 0, NW 1, SW 2, SE 3).
fn quadrant(dx: i64, dy: i64) -> i32 {
    match (dx >= 0, dy >= 0) {
        (true, true) => 0,
        (false, true) => 1,
        (false, false) => 2,
        (true, false) => 3,
    }
}

/// Compares the angles of `p` and `q` around `origin` (GEOS's `PolygonNodeTopology::compareAngle`).
fn compare_angle(origin: IPt, p: IPt, q: IPt) -> i32 {
    let (dpx, dpy) = (p.0 as i64 - origin.0 as i64, p.1 as i64 - origin.1 as i64);
    let (dqx, dqy) = (q.0 as i64 - origin.0 as i64, q.1 as i64 - origin.1 as i64);
    let (qp, qq) = (quadrant(dpx, dpy), quadrant(dqx, dqy));
    if qp > qq {
        return 1;
    }
    if qp < qq {
        return -1;
    }
    match sign(orient(origin, q, p)) {
        1 => 1,
        -1 => -1,
        _ => 0,
    }
}

fn compare_between(origin: IPt, p: IPt, e0: IPt, e1: IPt) -> i32 {
    let c0 = compare_angle(origin, p, e0);
    if c0 == 0 {
        return 0;
    }
    let c1 = compare_angle(origin, p, e1);
    if c1 == 0 {
        return 0;
    }
    if c0 > 0 && c1 < 0 { 1 } else { -1 }
}

/// Whether two rings meeting at `node` (ring A's neighbours `a0` and `a1`, ring B's `b0` and `b1`)
/// cross there (GEOS's `PolygonNodeTopology::isCrossing`).
fn is_crossing(node: IPt, a0: IPt, a1: IPt, b0: IPt, b1: IPt) -> bool {
    let (lo, hi) = if compare_angle(node, a0, a1) > 0 { (a1, a0) } else { (a0, a1) };
    let c0 = compare_between(node, b0, lo, hi);
    if c0 == 0 {
        return false;
    }
    let c1 = compare_between(node, b1, lo, hi);
    if c1 == 0 {
        return false;
    }
    c0 != c1
}

/// A ring of the polygons being checked: its polygon, and its points without the closing one
/// nor repeated points.
struct VRing {
    poly: usize,
    pts: Vec<IPt>,
}

impl VRing {
    /// The ring's neighbours of a point on it: the points before and after it when it is a
    /// vertex, else the ends of the segment holding it.
    fn around(&self, p: IPt) -> (IPt, IPt) {
        let n = self.pts.len();
        if let Some(k) = self.pts.iter().position(|&q| q == p) {
            return (self.pts[(k + n - 1) % n], self.pts[(k + 1) % n]);
        }
        for i in 0..n {
            let (a, b) = (self.pts[i], self.pts[(i + 1) % n]);
            if orient(a, b, p) == 0 && in_box(a, b, p) {
                return (a, b);
            }
        }
        (p, p)
    }
}

/// Whether polygons (in tile coordinates, rings closed) are valid as GEOS's `IsValid` decides:
/// rings of 4 points or more that neither cross nor touch themselves, rings of a polygon touching
/// each other at single points without disconnecting its interior, holes inside their exterior
/// and not nested, polygons not nested and not overlapping. Polygons without rings are ignored.
pub(crate) fn is_valid(polys: &[Polygon<IPt>]) -> bool {
    let mut rings: Vec<VRing> = Vec::new();
    for (pi, poly) in polys.iter().enumerate() {
        for r in poly {
            let mut pts: Vec<IPt> = Vec::with_capacity(r.len());
            for &p in r {
                if pts.last() != Some(&p) {
                    pts.push(p);
                }
            }
            if pts.len() > 1 && pts.first() == pts.last() {
                pts.pop();
            }
            if pts.len() < 3 {
                return false;
            }
            rings.push(VRing { poly: pi, pts });
        }
    }
    if rings.is_empty() {
        return true;
    }
    // Segments: (ring, index).
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut boxes = Vec::new();
    for (ri, r) in rings.iter().enumerate() {
        let n = r.pts.len();
        for i in 0..n {
            let (a, b) = (r.pts[i], r.pts[(i + 1) % n]);
            segs.push((ri, i));
            boxes.push((a.0.min(b.0) as f64, a.1.min(b.1) as f64, a.0.max(b.0) as f64, a.1.max(b.1) as f64));
        }
    }
    let seg = |s: usize| {
        let (ri, i) = segs[s];
        let r = &rings[ri].pts;
        (r[i], r[(i + 1) % r.len()])
    };
    let mut touches: Vec<(usize, usize, IPt)> = Vec::new();
    for (s, t) in box_pairs(&boxes) {
        let ((p0, p1), (q0, q1)) = (seg(s), seg(t));
        let (o1, o2, o3, o4) = (sign(orient(p0, p1, q0)), sign(orient(p0, p1, q1)), sign(orient(q0, q1, p0)), sign(orient(q0, q1, p1)));
        let (rs, is) = segs[s];
        let (rt, it) = segs[t];
        let n = rings[rs].pts.len();
        let adjacent = rs == rt && (is.abs_diff(it) == 1 || is.abs_diff(it) == n - 1);
        let point = if o1 == 0 && o2 == 0 {
            // Collinear: overlapping along a stretch is invalid.
            let key = |p: IPt| if p0.0 != p1.0 { p.0 } else { p.1 };
            let (pa, pb) = (key(p0).min(key(p1)), key(p0).max(key(p1)));
            let (qa, qb) = (key(q0).min(key(q1)), key(q0).max(key(q1)));
            let (lo, hi) = (pa.max(qa), pb.min(qb));
            if lo < hi {
                return false;
            }
            if lo > hi {
                continue;
            }
            [p0, p1].into_iter().find(|&p| key(p) == lo)
        } else if o1 * o2 < 0 && o3 * o4 < 0 {
            return false;
        } else {
            let mut found = None;
            for (o, p, a, b) in [(o1, q0, p0, p1), (o2, q1, p0, p1), (o3, p0, q0, q1), (o4, p1, q0, q1)] {
                if o == 0 && in_box(a, b, p) {
                    found = Some(p);
                    break;
                }
            }
            found
        };
        let Some(p) = point else { continue };
        if adjacent {
            continue;
        }
        if rs == rt {
            // A ring touching itself.
            return false;
        }
        touches.push((rs.min(rt), rs.max(rt), p));
    }
    touches.sort();
    touches.dedup();
    // Rings crossing at a vertex; then rings of a polygon touching in a cycle (the interior
    // disconnected).
    let mut point_ids: HashMap<IPt, usize> = HashMap::new();
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for &(ra, rb, p) in &touches {
        let (a0, a1) = rings[ra].around(p);
        let (b0, b1) = rings[rb].around(p);
        if is_crossing(p, a0, a1, b0, b1) {
            return false;
        }
        if rings[ra].poly != rings[rb].poly {
            continue;
        }
        let next = rings.len() + point_ids.len();
        let id = *point_ids.entry(p).or_insert(next);
        edges.push((ra, id));
        edges.push((rb, id));
    }
    edges.sort();
    edges.dedup();
    let mut parent: Vec<usize> = (0..rings.len() + point_ids.len()).collect();
    for (a, b) in edges {
        let (x, y) = (find(&mut parent, a), find(&mut parent, b));
        if x == y {
            return false;
        }
        parent[x] = y;
    }
    // Holes inside their exterior and not nested; polygons not nested.
    let closed = |r: &VRing| -> Vec<IPt> {
        let mut v = r.pts.clone();
        v.push(r.pts[0]);
        v
    };
    let bbox = |r: &[IPt]| {
        r.iter().fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |b, p| (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1)))
    };
    let covers = |a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)| b.0 >= a.0 && b.1 >= a.1 && b.2 <= a.2 && b.3 <= a.3;
    let mut by_poly: Vec<Vec<Vec<IPt>>> = vec![Vec::new(); polys.len()];
    for r in &rings {
        by_poly[r.poly].push(closed(r));
    }
    for poly in by_poly.iter().filter(|p| !p.is_empty()) {
        let shell_box = bbox(&poly[0]);
        for (i, h) in poly.iter().enumerate().skip(1) {
            if !covers(shell_box, bbox(h)) || !ring_nested(h, &poly[0]) {
                return false;
            }
            for (j, other) in poly.iter().enumerate().skip(1) {
                if i != j && covers(bbox(other), bbox(h)) && ring_nested(h, other) {
                    return false;
                }
            }
        }
    }
    for (i, p) in by_poly.iter().enumerate().filter(|(_, p)| !p.is_empty()) {
        for (j, q) in by_poly.iter().enumerate().filter(|(_, q)| !q.is_empty()) {
            if i != j && covers(bbox(&q[0]), bbox(&p[0])) && shell_nested(&p[0], q) {
                return false;
            }
        }
    }
    true
}

// ─── MakeValid ──────────────────────────────────────────────────────────────────────────────────

/// The noded linework as a planar graph: nodes, edges, and the faces on each side of each edge.
struct Graph {
    nodes: Vec<P>,
    /// Edges `(from, to)`; half-edge `2e` runs from `from` to `to`, `2e + 1` back.
    edges: Vec<(usize, usize)>,
    /// The outgoing half-edges of each node, by increasing angle.
    out: Vec<Vec<usize>>,
    /// The position of each half-edge in its origin's list.
    pos: Vec<usize>,
    /// The face on the left of each half-edge (0 is the unbounded face).
    face: Vec<usize>,
    faces: usize,
}

impl Graph {
    fn origin(&self, h: usize) -> usize {
        let e = self.edges[h / 2];
        if h.is_multiple_of(2) { e.0 } else { e.1 }
    }

    fn dest(&self, h: usize) -> usize {
        self.origin(h ^ 1)
    }

    /// The half-edge after `h` around the face on its left, among the edges `active` keeps:
    /// at `h`'s end, the first active edge clockwise from `h` reversed.
    fn next(&self, h: usize, active: &dyn Fn(usize) -> bool) -> usize {
        let twin = h ^ 1;
        let v = self.origin(twin);
        let list = &self.out[v];
        let k = self.pos[twin];
        let n = list.len();
        for step in 1..=n {
            let c = list[(k + n - step) % n];
            if active(c / 2) {
                return c;
            }
        }
        twin
    }

    fn area(&self, cycle: &[usize]) -> f64 {
        let mut a = 0.0;
        for &h in cycle {
            let (p, q) = (self.nodes[self.origin(h)], self.nodes[self.dest(h)]);
            a += p.0 * q.1 - q.0 * p.1;
        }
        a / 2.0
    }

    /// The cycles of half-edges (among those `start` lists) around faces, the edges `active`
    /// keeps bounding them, each split into simple loops at the nodes it passes twice.
    fn loops(&self, start: &[usize], active: &dyn Fn(usize) -> bool) -> Vec<Vec<usize>> {
        let mut seen = vec![false; self.edges.len() * 2];
        let mut out = Vec::new();
        for &h0 in start {
            if seen[h0] {
                continue;
            }
            let mut cycle = Vec::new();
            let mut h = h0;
            while !seen[h] {
                seen[h] = true;
                cycle.push(h);
                h = self.next(h, active);
            }
            // Split at repeated nodes.
            let mut stack: Vec<usize> = Vec::new();
            let mut at: HashMap<usize, usize> = HashMap::new();
            at.insert(self.origin(cycle[0]), 0);
            for &h in &cycle {
                stack.push(h);
                let v = self.dest(h);
                if let Some(&j) = at.get(&v) {
                    let lp = stack.split_off(j);
                    for &x in &lp[1..] {
                        at.remove(&self.origin(x));
                    }
                    out.push(lp);
                } else {
                    at.insert(v, stack.len());
                }
            }
            if !stack.is_empty() {
                out.push(stack);
            }
        }
        out
    }

    fn ring(&self, lp: &[usize]) -> Vec<P> {
        let mut r: Vec<P> = lp.iter().map(|&h| self.nodes[self.origin(h)]).collect();
        if let Some(&f) = r.first() {
            r.push(f);
        }
        r
    }
}

/// Whether a point is inside a ring (even-odd; `None` on its boundary).
fn inside(p: P, ring: &[P]) -> Option<bool> {
    let mut c = false;
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        if cross == 0.0 && p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1) {
            return None;
        }
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            c = !c;
        }
    }
    Some(c)
}

fn ring_area(r: &[P]) -> f64 {
    r.windows(2).map(|w| w[0].0 * w[1].1 - w[1].0 * w[0].1).sum::<f64>() / 2.0
}

/// Nodes the rings' segments (every intersection becomes a node) and builds the planar graph of
/// the noded edges, each edge once, with its faces.
fn build_graph(polys: &[Polygon<IPt>]) -> Graph {
    let mut segs: Vec<(IPt, IPt)> = Vec::new();
    for r in polys.iter().flatten() {
        for w in r.windows(2) {
            if w[0] != w[1] {
                segs.push((w[0], w[1]));
            }
        }
    }
    let boxes: Vec<(f64, f64, f64, f64)> =
        segs.iter().map(|(a, b)| (a.0.min(b.0) as f64, a.1.min(b.1) as f64, a.0.max(b.0) as f64, a.1.max(b.1) as f64)).collect();
    // The points splitting each segment, by their position along it.
    let mut splits: Vec<Vec<(f64, P)>> = segs.iter().map(|&(a, b)| vec![(0.0, ip(a)), (1.0, ip(b))]).collect();
    let param = |s: (IPt, IPt), p: IPt| -> f64 {
        let (a, b) = s;
        if a.0 != b.0 { (p.0 - a.0) as f64 / (b.0 - a.0) as f64 } else { (p.1 - a.1) as f64 / (b.1 - a.1) as f64 }
    };
    for (s, t) in box_pairs(&boxes) {
        let ((p0, p1), (q0, q1)) = (segs[s], segs[t]);
        let (o1, o2, o3, o4) = (orient(p0, p1, q0), orient(p0, p1, q1), orient(q0, q1, p0), orient(q0, q1, p1));
        if o1 == 0 && o2 == 0 {
            // Collinear: the ends of each inside the other split it.
            for (p, seg, idx) in [(q0, segs[s], s), (q1, segs[s], s), (p0, segs[t], t), (p1, segs[t], t)] {
                if in_box(seg.0, seg.1, p) {
                    splits[idx].push((param(seg, p), ip(p)));
                }
            }
            continue;
        }
        if o1.signum() * o2.signum() > 0 || o3.signum() * o4.signum() > 0 {
            continue;
        }
        // They meet at one point: an end of one, or a crossing.
        let mut end = None;
        for (o, p) in [(o1, q0), (o2, q1)] {
            if o == 0 && in_box(p0, p1, p) {
                end = Some(p);
            }
        }
        for (o, p) in [(o3, p0), (o4, p1)] {
            if o == 0 && in_box(q0, q1, p) {
                end = Some(p);
            }
        }
        if let Some(p) = end {
            splits[s].push((param(segs[s], p), ip(p)));
            splits[t].push((param(segs[t], p), ip(p)));
            continue;
        }
        // A proper crossing: p0 + t (p1 - p0), t = cross(q0 - p0, q1 - q0) / cross(p1 - p0, q1 - q0).
        let (dx, dy) = (p1.0 as i128 - p0.0 as i128, p1.1 as i128 - p0.1 as i128);
        let (ex, ey) = (q1.0 as i128 - q0.0 as i128, q1.1 as i128 - q0.1 as i128);
        let d = dx * ey - dy * ex;
        let tn = (q0.0 as i128 - p0.0 as i128) * ey - (q0.1 as i128 - p0.1 as i128) * ex;
        let x = (p0.0 as i128 * d + tn * dx) as f64 / d as f64;
        let y = (p0.1 as i128 * d + tn * dy) as f64 / d as f64;
        let un = (q0.0 as i128 - p0.0 as i128) * dy - (q0.1 as i128 - p0.1 as i128) * dx;
        splits[s].push((tn as f64 / d as f64, (x, y)));
        splits[t].push((un as f64 / d as f64, (x, y)));
    }
    let mut ids: HashMap<(u64, u64), usize> = HashMap::new();
    let mut nodes: Vec<P> = Vec::new();
    let mut id = |p: P| -> usize {
        let key = ((p.0 + 0.0).to_bits(), (p.1 + 0.0).to_bits());
        *ids.entry(key).or_insert_with(|| {
            nodes.push(p);
            nodes.len() - 1
        })
    };
    let mut edge_set: HashMap<(usize, usize), ()> = HashMap::new();
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for mut sp in splits {
        sp.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut prev: Option<usize> = None;
        for (_, p) in sp {
            let n = id(p);
            if let Some(m) = prev
                && m != n
            {
                let key = (m.min(n), m.max(n));
                if edge_set.insert(key, ()).is_none() {
                    edges.push((m, n));
                }
            }
            prev = Some(n);
        }
    }
    // Outgoing half-edges by angle.
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (e, &(a, b)) in edges.iter().enumerate() {
        out[a].push(2 * e);
        out[b].push(2 * e + 1);
    }
    let mut g = Graph { nodes, edges, out, pos: Vec::new(), face: Vec::new(), faces: 1 };
    for v in 0..g.out.len() {
        let mut list = std::mem::take(&mut g.out[v]);
        let o = g.nodes[v];
        list.sort_by(|&h1, &h2| {
            let (p, q) = (g.nodes[g.dest(h1)], g.nodes[g.dest(h2)]);
            (p.1 - o.1).atan2(p.0 - o.0).total_cmp(&(q.1 - o.1).atan2(q.0 - o.0))
        });
        g.out[v] = list;
    }
    g.pos = vec![0; g.edges.len() * 2];
    for list in &g.out {
        for (k, &h) in list.iter().enumerate() {
            g.pos[h] = k;
        }
    }
    // Faces: counter-clockwise cycles bound faces; clockwise ones are the outer boundaries of
    // connected parts, in the smallest face of another part that contains them.
    let all: Vec<usize> = (0..g.edges.len() * 2).collect();
    let mut seen = vec![false; all.len()];
    let mut ccw: Vec<(Vec<usize>, f64)> = Vec::new();
    let mut cw: Vec<Vec<usize>> = Vec::new();
    for &h0 in &all {
        if seen[h0] {
            continue;
        }
        let mut cycle = Vec::new();
        let mut h = h0;
        while !seen[h] {
            seen[h] = true;
            cycle.push(h);
            h = g.next(h, &|_| true);
        }
        let a = g.area(&cycle);
        if a > 0.0 { ccw.push((cycle, a)) } else { cw.push(cycle) }
    }
    // Connected parts.
    let mut part: Vec<usize> = (0..g.nodes.len()).collect();
    for &(a, b) in &g.edges {
        let (x, y) = (find(&mut part, a), find(&mut part, b));
        part[x] = y;
    }
    g.face = vec![0; all.len()];
    let rings: Vec<Vec<P>> = ccw.iter().map(|(c, _)| g.ring(c)).collect();
    for (i, (c, _)) in ccw.iter().enumerate() {
        for &h in c {
            g.face[h] = i + 1;
        }
    }
    g.faces = ccw.len() + 1;
    for c in &cw {
        let v = g.origin(c[0]);
        let pv = find(&mut part, v);
        let q = g.nodes[v];
        let mut best: Option<(usize, f64)> = None;
        for (i, (cc, a)) in ccw.iter().enumerate() {
            if find(&mut part, g.origin(cc[0])) == pv || best.is_some_and(|(_, b)| *a >= b) {
                continue;
            }
            if inside(q, &rings[i]) == Some(true) {
                best = Some((i + 1, *a));
            }
        }
        let f = best.map_or(0, |b| b.0);
        for &h in c {
            g.face[h] = f;
        }
    }
    g
}

fn ip(p: IPt) -> P {
    (p.0 as f64, p.1 as f64)
}

fn find(p: &mut [usize], mut i: usize) -> usize {
    while p[i] != i {
        p[i] = p[p[i]];
        i = p[i];
    }
    i
}

/// The order GEOS's STR tree (node capacity 10) lists items in, by their envelopes: in vertical
/// slices by the centres' x, each slice by the centres' y. `CascadedPolygonUnion` unions
/// polygons in that order, and the union lists its polygons in the order of their first input.
fn str_order(envs: &[(f64, f64, f64, f64)]) -> Vec<usize> {
    const CAPACITY: usize = 10;
    let n = envs.len();
    let mut idx: Vec<usize> = (0..n).collect();
    let cy = |i: &usize| envs[*i].1 + envs[*i].3;
    let cx = |i: &usize| envs[*i].0 + envs[*i].2;
    if n <= CAPACITY {
        idx.sort_by(|a, b| cy(a).total_cmp(&cy(b)));
        return idx;
    }
    let slices = (n.div_ceil(CAPACITY) as f64).sqrt().ceil() as usize;
    let per_slice = n.div_ceil(slices);
    idx.sort_by(|a, b| cx(a).total_cmp(&cx(b)));
    for chunk in idx.chunks_mut(per_slice) {
        chunk.sort_by(|a, b| cy(a).total_cmp(&cy(b)));
    }
    idx
}

/// GEOS's `BuildArea` on the edges `cut` keeps, as a set of faces: the regions those edges bound
/// (the polygonizer's faces), those nested an even number of times kept. With each face kept,
/// the rank of its region in the order GEOS unions them.
fn build_area(g: &Graph, cut: &[bool]) -> Option<(Vec<bool>, Vec<usize>)> {
    let mut region: Vec<usize> = (0..g.faces).collect();
    for (e, &c) in cut.iter().enumerate() {
        if !c {
            let (a, b) = (find(&mut region, g.face[2 * e]), find(&mut region, g.face[2 * e + 1]));
            region[a] = b;
        }
    }
    let outer = find(&mut region, 0);
    let roots: Vec<usize> = (0..g.faces).map(|f| find(&mut region, f)).collect();
    let mut regions: Vec<usize> = roots.iter().copied().filter(|&r| r != outer).collect();
    regions.sort_unstable();
    regions.dedup();
    if regions.is_empty() {
        return None;
    }
    if regions.len() == 1 {
        return Some((roots.iter().map(|&r| r == regions[0]).collect(), vec![0; g.faces]));
    }
    // Each region's exterior loop and hole loops, as sets of edges.
    let active = |e: usize| cut[e] && roots[g.face[2 * e]] != roots[g.face[2 * e + 1]];
    struct Face {
        root: usize,
        env_area: f64,
        env: (f64, f64, f64, f64),
        exterior: Vec<usize>,
        holes: Vec<Vec<usize>>,
        parent: Option<usize>,
    }
    let mut faces: Vec<Face> = Vec::new();
    for &r in &regions {
        let start: Vec<usize> =
            (0..g.edges.len()).filter(|&e| active(e)).flat_map(|e| [2 * e, 2 * e + 1]).filter(|&h| roots[g.face[h]] == r).collect();
        let mut exterior: Option<(Vec<usize>, f64, f64, Env)> = None;
        let mut holes = Vec::new();
        for lp in g.loops(&start, &active) {
            let a = g.area(&lp);
            let mut edges: Vec<usize> = lp.iter().map(|h| h / 2).collect();
            edges.sort_unstable();
            if a > 0.0 {
                let ring = g.ring(&lp);
                let b = ring
                    .iter()
                    .fold((f64::MAX, f64::MAX, f64::MIN, f64::MIN), |b, p| (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1)));
                let env = (b.2 - b.0) * (b.3 - b.1);
                if exterior.as_ref().is_none_or(|x| a > x.1) {
                    exterior = Some((edges, a, env, b));
                }
            } else {
                holes.push(edges);
            }
        }
        let (exterior, _, env_area, env) = exterior.unwrap_or_default();
        faces.push(Face { root: r, env_area, env, exterior, holes, parent: None });
    }
    faces.sort_by(|a, b| b.env_area.total_cmp(&a.env_area));
    for i in 0..faces.len() {
        for h in 0..faces[i].holes.len() {
            for j in i + 1..faces.len() {
                if faces[j].parent.is_some() {
                    continue;
                }
                if faces[j].exterior == faces[i].holes[h] {
                    faces[j].parent = Some(i);
                    break;
                }
            }
        }
    }
    let mut keep: Vec<usize> = Vec::new();
    for i in 0..faces.len() {
        let mut depth = 0;
        let mut p = faces[i].parent;
        while let Some(k) = p {
            depth += 1;
            p = faces[k].parent;
        }
        if depth % 2 == 0 {
            keep.push(i);
        }
    }
    let envs: Vec<(f64, f64, f64, f64)> = keep.iter().map(|&i| faces[i].env).collect();
    let mut rank_of_root: HashMap<usize, usize> = HashMap::new();
    for (rank, k) in str_order(&envs).into_iter().enumerate() {
        rank_of_root.insert(faces[keep[k]].root, rank);
    }
    let member = roots.iter().map(|r| rank_of_root.contains_key(r)).collect();
    let ranks = roots.iter().map(|r| rank_of_root.get(r).copied().unwrap_or(usize::MAX)).collect();
    Some((member, ranks))
}

/// GEOS's `MakeValid` (the linework method) of polygons in tile coordinates (rings closed): the
/// polygons of the result, rings closed, in the coordinates of the noded linework.
pub(crate) fn make_valid(polys: &[Polygon<IPt>]) -> Vec<Polygon<P>> {
    let g = build_graph(polys);
    let ne = g.edges.len();
    let mut cut = vec![true; ne];
    let mut area = vec![false; g.faces];
    // When each face of the area joined it: (round, rank), which orders the polygons.
    let mut order: Vec<(usize, usize)> = vec![(usize::MAX, usize::MAX); g.faces];
    let mut round = 0;
    while cut.iter().any(|&c| c) {
        let Some((new_area, ranks)) = build_area(&g, &cut) else { break };
        for (f, a) in area.iter_mut().enumerate() {
            if new_area[f] {
                *a = !*a;
                order[f] = if *a { (round, ranks[f]) } else { (usize::MAX, usize::MAX) };
            }
        }
        round += 1;
        let mut removed = false;
        for (e, c) in cut.iter_mut().enumerate() {
            if *c && new_area[g.face[2 * e]] != new_area[g.face[2 * e + 1]] {
                *c = false;
                removed = true;
            }
        }
        if !removed {
            break;
        }
    }
    // The area's boundary as polygons: loops with the area on their left, counter-clockwise ones
    // exteriors and clockwise ones holes, each hole in the smallest exterior around it.
    let active = |e: usize| area[g.face[2 * e]] != area[g.face[2 * e + 1]];
    let start: Vec<usize> = (0..ne).filter(|&e| active(e)).map(|e| if area[g.face[2 * e]] { 2 * e } else { 2 * e + 1 }).collect();
    let mut shells: Vec<(Vec<P>, f64, (usize, usize))> = Vec::new();
    let mut holes: Vec<(Vec<P>, (usize, usize))> = Vec::new();
    for lp in g.loops(&start, &active) {
        let ring = g.ring(&lp);
        let a = ring_area(&ring);
        let key = lp.iter().map(|&h| order[g.face[h]]).min().unwrap_or_default();
        // GEOS's overlays give exteriors clockwise and holes counter-clockwise.
        let mut ring = ring;
        ring.reverse();
        if a > 0.0 {
            shells.push((ring, a, key));
        } else if a < 0.0 {
            holes.push((ring, key));
        }
    }
    shells.sort_by_key(|s| s.2);
    let mut out: Vec<Polygon<P>> = shells.iter().map(|(r, ..)| vec![r.clone()]).collect();
    for (h, _) in holes {
        let mut best: Option<(usize, f64)> = None;
        for (i, (s, a, _)) in shells.iter().enumerate() {
            if best.is_some_and(|(_, b)| *a >= b) {
                continue;
            }
            let probe = h
                .iter()
                .copied()
                .chain(h.windows(2).map(|w| ((w[0].0 + w[1].0) / 2.0, (w[0].1 + w[1].1) / 2.0)))
                .find_map(|p| inside(p, s));
            if probe == Some(true) {
                best = Some((i, *a));
            }
        }
        if let Some((i, _)) = best {
            out[i].push(h);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(pts: &[(i32, i32)]) -> Vec<IPt> {
        let mut r = pts.to_vec();
        r.push(pts[0]);
        r
    }

    fn area(polys: &[Polygon<P>]) -> f64 {
        polys.iter().map(|p| ring_area(&p[0]).abs() - p[1..].iter().map(|h| ring_area(h).abs()).sum::<f64>()).sum()
    }

    #[test]
    fn checks_validity_as_geos() {
        let square = ring(&[(0, 0), (10, 0), (10, 10), (0, 10)]);
        assert!(is_valid(&[vec![square.clone()]]));
        // A bow tie.
        assert!(!is_valid(&[vec![ring(&[(0, 0), (10, 10), (10, 0), (0, 10)])]]));
        // A ring touching itself at a vertex.
        assert!(!is_valid(&[vec![ring(&[(0, 0), (10, 0), (5, 5), (10, 10), (0, 10), (5, 5)])]]));
        // A hole touching the exterior at one point is valid; at two points it cuts the interior.
        assert!(is_valid(&[vec![square.clone(), ring(&[(0, 5), (5, 2), (5, 8)])]]));
        assert!(!is_valid(&[vec![square.clone(), ring(&[(0, 5), (5, 0), (10, 5), (5, 8)])]]));
        // A hole outside, nested holes, nested polygons.
        assert!(!is_valid(&[vec![square.clone(), ring(&[(20, 0), (30, 0), (30, 10)])]]));
        assert!(!is_valid(&[vec![square.clone(), ring(&[(1, 1), (9, 1), (9, 9), (1, 9)]), ring(&[(2, 2), (3, 2), (3, 3)])]]));
        assert!(!is_valid(&[vec![square.clone()], vec![ring(&[(2, 2), (3, 2), (3, 3)])]]));
        // An island in a lake is valid; polygons sharing an edge are not.
        assert!(is_valid(&[vec![square.clone(), ring(&[(1, 1), (9, 1), (9, 9), (1, 9)])], vec![ring(&[(2, 2), (3, 2), (3, 3)])]]));
        assert!(!is_valid(&[vec![square.clone()], vec![ring(&[(10, 0), (20, 0), (20, 10), (10, 10)])]]));
        assert!(is_valid(&[vec![square], vec![ring(&[(10, 10), (20, 10), (20, 20)])]]));
        // Too few points, and a spike.
        assert!(!is_valid(&[vec![ring(&[(0, 0), (10, 0)])]]));
        assert!(!is_valid(&[vec![ring(&[(0, 0), (10, 0), (10, 10), (10, 20), (10, 10), (0, 10)])]]));
    }

    #[test]
    fn repairs_as_geos() {
        // A bow tie: two triangles.
        let v = make_valid(&[vec![ring(&[(0, 0), (10, 10), (10, 0), (0, 10)])]]);
        assert_eq!(v.len(), 2);
        assert_eq!(area(&v), 50.0);
        // A ring going around twice over part of itself: the even-odd area.
        let v = make_valid(&[vec![ring(&[(0, 0), (10, 0), (10, 10), (0, 10), (0, 2), (8, 2), (8, 8), (2, 8), (2, 0)])]]);
        assert!(area(&v) > 0.0);
        // Overlapping polygons are merged; a hole outside its exterior becomes a polygon.
        let a = ring(&[(0, 0), (10, 0), (10, 10), (0, 10)]);
        let b = ring(&[(5, 0), (15, 0), (15, 10), (5, 10)]);
        let v = make_valid(&[vec![a.clone()], vec![b]]);
        assert_eq!(area(&v), 150.0);
        let v = make_valid(&[vec![a.clone(), ring(&[(20, 0), (30, 0), (30, 10)])]]);
        assert_eq!(area(&v), 150.0);
        // A valid polygon with a hole stays as it is.
        let v = make_valid(&[vec![a, ring(&[(2, 2), (2, 8), (8, 8), (8, 2)])]]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].len(), 2);
        assert_eq!(area(&v), 64.0);
    }
}
