//! Convex and concave hulls of single geometries, after GEOS 3.14 (ports of the JTS Topology
//! Suite's algorithms), which OGR's `ConvexHull`, `ConcaveHull` and `ConcaveHullOfPolygons` run.
//!
//! - [`convex_hull`] is GEOS's Graham scan: the smallest convex polygon covering the vertices, a
//!   line string for collinear input and a point for a single distinct vertex. The ring starts at
//!   the lowest (then leftmost) vertex and runs clockwise, as GEOS's; collinear hull vertices
//!   are dropped.
//! - [`concave_hull`] is GEOS's `ConcaveHull` by edge length ratio: the Delaunay triangulation
//!   of the distinct vertices, eroded from its border (longest border edges first) while border
//!   edges are longer than `ratio * (max - min) + min` of the triangulation's edge lengths, never
//!   splitting the hull; with holes allowed, interior triangles with long edges are eroded too.
//! - [`concave_hull_of_polygons`] is GEOS's `ConcaveHullOfPolygons`: the constrained Delaunay
//!   triangulation of the space between the polygons (inside a frame), eroded from the frame
//!   while border edges are longer than the threshold (and, with `tight`, while triangles
//!   touch a single polygon), united with the polygons.
//!
//! Triangulations come from the `spade` crate rather than GEOS's quad-edge structure: for
//! inputs with co-circular vertices (regular grids) the Delaunay triangulation is not unique, and
//! the hulls may differ from GEOS's there. Ring start vertices and orientation of concave hulls
//! may differ from GEOS's; their coverage does not. Output vertices are input vertices: Z is kept,
//! M dropped.

use super::simplify::{dist, open_ring, orient};
use crate::{Error, Geometry, Result};
use spade::handles::{FixedFaceHandle, InnerTag};
use spade::{ConstrainedDelaunayTriangulation, DelaunayTriangulation, HasPosition, Point2, Triangulation};
use std::collections::{BinaryHeap, HashMap, VecDeque};
use wbvector::{Coord, Ring};

type P = (f64, f64);

/// The distinct XY vertices of a geometry, in first-seen order, with their coordinates.
fn unique_vertices(g: &Geometry) -> Vec<Coord> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in g.all_coords() {
        if seen.insert((key(c.x), key(c.y))) {
            out.push(Coord { x: c.x, y: c.y, z: c.z, m: None });
        }
    }
    out
}

/// A hashable key of a coordinate value (0 and -0 are the same).
fn key(v: f64) -> u64 {
    if v == 0.0 { 0 } else { v.to_bits() }
}

fn finite(g: &Geometry) -> Result<()> {
    if g.all_coords().iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
        return Err(Error::InvalidArgument("non-finite XY coordinates".into()));
    }
    Ok(())
}

/// GEOS's `ConvexHull::isBetween`.
fn is_between(c1: P, c2: P, c3: P) -> bool {
    if orient(c1, c2, c3) != 0 {
        return false;
    }
    if c1.0 != c3.0 && ((c1.0 <= c2.0 && c2.0 <= c3.0) || (c3.0 <= c2.0 && c2.0 <= c1.0)) {
        return true;
    }
    c1.1 != c3.1 && ((c1.1 <= c2.1 && c2.1 <= c3.1) || (c3.1 <= c2.1 && c2.1 <= c1.1))
}

/// The convex hull of a geometry, as GEOS computes it (see the module documentation). Empty
/// geometries give an empty geometry collection.
pub fn convex_hull(g: &Geometry) -> Result<Geometry> {
    finite(g)?;
    let pts = unique_vertices(g);
    match pts.len() {
        0 => return Ok(Geometry::GeometryCollection(Vec::new())),
        1 => return Ok(Geometry::Point(pts[0].clone())),
        2 => return Ok(Geometry::LineString(pts)),
        _ => {}
    }
    let xy = |i: usize| (pts[i].x, pts[i].y);
    let mut order: Vec<usize> = (0..pts.len()).collect();
    // The lowest, then leftmost, point first; the others sorted radially around it.
    for i in 1..order.len() {
        let (p0, pi) = (xy(order[0]), xy(order[i]));
        if pi.1 < p0.1 || (pi.1 == p0.1 && pi.0 < p0.0) {
            order.swap(0, i);
        }
    }
    let origin = xy(order[0]);
    let polar = |p: P, q: P| -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match orient(origin, p, q) {
            1 => return Greater,
            -1 => return Less,
            _ => {}
        }
        p.1.partial_cmp(&q.1).unwrap_or(Equal).then(p.0.partial_cmp(&q.0).unwrap_or(Equal))
    };
    order[..].sort_by(|&a, &b| polar(xy(a), xy(b)));
    // Graham scan, as GEOS: left turns are popped, giving a clockwise ring.
    let mut stack = vec![order[0], order[1], order[2]];
    for &c in &order[3..] {
        let mut p = stack.pop().expect("three points");
        while let Some(&top) = stack.last() {
            if orient(xy(top), xy(p), xy(c)) > 0 {
                p = stack.pop().expect("non-empty");
            } else {
                break;
            }
        }
        stack.push(p);
        stack.push(c);
    }
    stack.push(order[0]);
    // GEOS's cleanRing: drop repeated and collinear vertices.
    let mut cleaned: Vec<usize> = Vec::new();
    let mut prev: Option<usize> = None;
    for w in stack.windows(2) {
        let (curr, next) = (w[0], w[1]);
        if xy(curr) == xy(next) {
            continue;
        }
        if let Some(p) = prev
            && is_between(xy(p), xy(curr), xy(next))
        {
            continue;
        }
        cleaned.push(curr);
        prev = Some(curr);
    }
    cleaned.push(*stack.last().expect("closed"));
    let coords: Vec<Coord> = cleaned.iter().map(|&i| pts[i].clone()).collect();
    if coords.len() == 3 {
        return Ok(Geometry::LineString(coords[..2].to_vec()));
    }
    Ok(Geometry::Polygon { exterior: open_ring(coords), interiors: Vec::new() })
}

/// A triangle of a hull triangulation, as GEOS's `HullTri`: vertex `i` and edge `i` (from vertex
/// `i` to `i + 1`), across which `adj[i]` lies.
struct Tri {
    v: [usize; 3],
    adj: [Option<usize>; 3],
    size: f64,
    removed: bool,
}

/// A triangulation being eroded into a hull.
struct Tris {
    pts: Vec<P>,
    tris: Vec<Tri>,
}

fn next(i: usize) -> usize {
    (i + 1) % 3
}

fn prev(i: usize) -> usize {
    (i + 2) % 3
}

impl Tris {
    fn coord(&self, t: usize, i: usize) -> P {
        self.pts[self.tris[t].v[i]]
    }

    fn length(&self, t: usize, i: usize) -> f64 {
        dist(self.coord(t, i), self.coord(t, next(i)))
    }

    fn longest_edge(&self, t: usize) -> f64 {
        (0..3).map(|i| self.length(t, i)).fold(f64::NEG_INFINITY, f64::max)
    }

    fn area(&self, t: usize) -> f64 {
        let (a, b, c) = (self.coord(t, 0), self.coord(t, 1), self.coord(t, 2));
        ((b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1)).abs() / 2.0
    }

    fn num_adjacent(&self, t: usize) -> usize {
        self.tris[t].adj.iter().filter(|a| a.is_some()).count()
    }

    fn is_boundary(&self, t: usize, i: usize) -> bool {
        self.tris[t].adj[i].is_none()
    }

    fn is_border(&self, t: usize) -> bool {
        (0..3).any(|i| self.is_boundary(t, i))
    }

    /// The edge index of `t` across which `other` lies.
    fn index_of(&self, t: usize, other: usize) -> Option<usize> {
        (0..3).find(|&i| self.tris[t].adj[i] == Some(other))
    }

    /// Whether every triangle around vertex `index` of `t` is adjacent to the next one.
    fn is_interior_vertex(&self, t: usize, index: usize) -> bool {
        let (mut curr, mut i) = (t, index);
        loop {
            let Some(adj) = self.tris[curr].adj[i] else { return false };
            let Some(adj_index) = self.index_of(adj, curr) else { return false };
            curr = adj;
            i = next(adj_index);
            if curr == t {
                return true;
            }
        }
    }

    fn is_boundary_touch(&self, t: usize, i: usize) -> bool {
        !self.is_boundary(t, i) && !self.is_boundary(t, prev(i)) && !self.is_interior_vertex(t, i)
    }

    fn has_boundary_touch(&self, t: usize) -> bool {
        (0..3).any(|i| self.is_boundary_touch(t, i))
    }

    /// GEOS's `isConnecting`: whether removing a border triangle would split the hull.
    fn is_connecting(&self, t: usize) -> bool {
        let a = &self.tris[t].adj;
        let index = if a[0].is_some() && a[1].is_some() {
            1
        } else if a[1].is_some() && a[2].is_some() {
            2
        } else if a[2].is_some() && a[0].is_some() {
            0
        } else {
            return true;
        };
        !self.is_interior_vertex(t, index)
    }

    fn boundary_length(&self, t: usize) -> f64 {
        (0..3).filter(|&i| self.is_boundary(t, i)).map(|i| self.length(t, i)).sum()
    }

    fn remove(&mut self, t: usize) {
        for i in 0..3 {
            if let Some(adj) = self.tris[t].adj[i].take()
                && let Some(j) = self.index_of(adj, t)
            {
                self.tris[adj].adj[j] = None;
            }
        }
        self.tris[t].removed = true;
    }

    /// The region covered by the triangles not removed, with the vertices' coordinates.
    fn to_geometry(&self, coords: &[Coord], keep: &[bool]) -> Geometry {
        let faces: Vec<[usize; 3]> = self.tris.iter().zip(keep).filter(|(_, k)| **k).map(|(t, _)| t.v).collect();
        region_boundary(&self.pts, coords, &faces)
    }
}

/// A triangle in the erosion queue: the largest size first, then the largest area (GEOS's
/// `HullTriCompare` in a max-heap).
struct Queued {
    size: f64,
    area: f64,
    tri: usize,
}

impl PartialEq for Queued {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == std::cmp::Ordering::Equal
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Queued {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.size.total_cmp(&o.size).then(self.area.total_cmp(&o.area))
    }
}

/// A vertex of a triangulation, with its index in the input.
#[derive(Clone, Copy)]
struct Vertex {
    p: Point2<f64>,
    index: usize,
}

impl HasPosition for Vertex {
    type Scalar = f64;
    fn position(&self) -> Point2<f64> {
        self.p
    }
}

fn spade_error(e: spade::InsertionError) -> Error {
    Error::InvalidArgument(format!("the coordinates cannot be triangulated: {e:?}"))
}

/// The concave hull of the vertices of a geometry, as GEOS's `ConcaveHull` computes it by
/// edge length ratio, from 0 (most concave) to 1 (the convex hull). Fewer than three
/// non-collinear vertices give the convex hull.
pub fn concave_hull(g: &Geometry, ratio: f64, allow_holes: bool) -> Result<Geometry> {
    if !(0.0..=1.0).contains(&ratio) {
        return Err(Error::InvalidArgument("the concave hull ratio must be between 0 and 1".into()));
    }
    finite(g)?;
    let coords = unique_vertices(g);
    // GEOS returns POLYGON EMPTY here, unlike ConvexHull's empty collection.
    if coords.is_empty() {
        return Ok(Geometry::Polygon { exterior: Ring(Vec::new()), interiors: Vec::new() });
    }
    let vertices: Vec<Vertex> = coords.iter().enumerate().map(|(index, c)| Vertex { p: Point2::new(c.x, c.y), index }).collect();
    let dt = DelaunayTriangulation::<Vertex>::bulk_load_stable(vertices).map_err(spade_error)?;
    let pts: Vec<P> = coords.iter().map(|c| (c.x, c.y)).collect();
    let mut t = Tris { pts, tris: Vec::new() };
    let faces: Vec<FixedFaceHandle<InnerTag>> = dt.fixed_inner_faces().collect();
    let slot: HashMap<FixedFaceHandle<InnerTag>, usize> = faces.iter().enumerate().map(|(i, f)| (*f, i)).collect();
    for f in &faces {
        let face = dt.face(*f);
        let v = face.vertices().map(|v| v.data().index);
        let adj = face.adjacent_edges().map(|e| e.rev().face().as_inner().map(|f| slot[&f.fix()]));
        t.tris.push(Tri { v, adj, size: 0.0, removed: false });
    }
    if t.tris.is_empty() {
        return convex_hull(g);
    }
    for i in 0..t.tris.len() {
        t.tris[i].size = t.longest_edge(i);
    }
    let max_size = target_length(&t, ratio, |_| true, |_, _| true);
    let in_hull = |t: &Tris, i: usize| t.tris[i].size < max_size;
    let push = |t: &mut Tris, heap: &mut BinaryHeap<Queued>, i: Option<usize>| {
        let Some(i) = i else { return };
        if t.num_adjacent(i) != 2 {
            return;
        }
        t.tris[i].size = t.boundary_length(i);
        heap.push(Queued { size: t.tris[i].size, area: t.area(i), tri: i });
    };
    // Erode the border.
    let mut heap = BinaryHeap::new();
    for i in 0..t.tris.len() {
        push(&mut t, &mut heap, Some(i));
    }
    while let Some(Queued { tri, .. }) = heap.pop() {
        if in_hull(&t, tri) {
            break;
        }
        if t.num_adjacent(tri) == 2 && !t.is_connecting(tri) {
            let adj = t.tris[tri].adj;
            t.remove(tri);
            adj.into_iter().for_each(|a| push(&mut t, &mut heap, a));
        }
    }
    if allow_holes {
        // GEOS 3.14 sorts the candidate holes by increasing size.
        let mut candidates: Vec<usize> = (0..t.tris.len())
            .filter(|&i| !t.tris[i].removed && t.tris[i].size >= max_size && !t.is_border(i) && !t.has_boundary_touch(i))
            .collect();
        candidates.sort_by(|&a, &b| t.tris[a].size.total_cmp(&t.tris[b].size).then(t.area(a).total_cmp(&t.area(b))));
        for hole in candidates {
            if t.tris[hole].removed || t.num_adjacent(hole) == 0 || t.is_border(hole) || t.has_boundary_touch(hole) {
                continue;
            }
            let mut heap = BinaryHeap::new();
            heap.push(Queued { size: t.tris[hole].size, area: t.area(hole), tri: hole });
            while let Some(Queued { tri, .. }) = heap.pop() {
                if tri != hole && in_hull(&t, tri) {
                    break;
                }
                if tri == hole || (t.num_adjacent(tri) == 2 && !t.has_boundary_touch(tri)) {
                    let adj = t.tris[tri].adj;
                    t.remove(tri);
                    adj.into_iter().for_each(|a| push(&mut t, &mut heap, a));
                }
            }
        }
    }
    let keep: Vec<bool> = t.tris.iter().map(|x| !x.removed).collect();
    Ok(t.to_geometry(&coords, &keep))
}

/// The edge length threshold of a ratio: `ratio * (max - min) + min` over the edges counted,
/// 0 for a ratio of 0 and twice the longest edge for 1.
fn target_length(t: &Tris, ratio: f64, tri: impl Fn(usize) -> bool, edge: impl Fn(usize, usize) -> bool) -> f64 {
    if ratio == 0.0 {
        return 0.0;
    }
    let (mut max, mut min) = (-1.0f64, -1.0f64);
    for i in (0..t.tris.len()).filter(|&i| tri(i)) {
        for e in (0..3).filter(|&e| edge(i, e)) {
            let len = t.length(i, e);
            if len > max {
                max = len;
            }
            if min < 0.0 || len < min {
                min = len;
            }
        }
    }
    if ratio == 1.0 {
        return 2.0 * max;
    }
    ratio * (max - min) + min
}

/// Whether a point is inside a set of rings, by the parity of the crossings (inside a polygon
/// of a valid multipolygon, holes excluded).
fn inside_rings(p: P, rings: &[Vec<P>]) -> bool {
    let mut inside = false;
    for r in rings {
        let n = r.len();
        for i in 0..n {
            let (a, b) = (r[i], r[(i + 1) % n]);
            if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
                inside = !inside;
            }
        }
    }
    inside
}

/// The concave hull of polygons, as GEOS's `ConcaveHullOfPolygons` computes it by edge length
/// ratio. The polygons must be valid: crossing rings are rejected.
pub fn concave_hull_of_polygons(g: &Geometry, ratio: f64, tight: bool, allow_holes: bool) -> Result<Geometry> {
    if !(0.0..=1.0).contains(&ratio) {
        return Err(Error::InvalidArgument("the concave hull ratio must be between 0 and 1".into()));
    }
    finite(g)?;
    let polygons: Vec<(&Ring, &[Ring])> = match g {
        Geometry::Polygon { exterior, interiors } => vec![(exterior, interiors.as_slice())],
        Geometry::MultiPolygon(ps) => ps.iter().map(|(e, h)| (e, h.as_slice())).collect(),
        _ => return Err(Error::InvalidArgument("the concave hull of polygons needs polygonal input".into())),
    };
    let ring_pts = |r: &Ring| -> Vec<P> {
        let mut v: Vec<P> = r.0.iter().map(|c| (c.x, c.y)).collect();
        if v.len() > 1 && v.first() == v.last() {
            v.pop();
        }
        v
    };
    let shells: Vec<Vec<P>> = polygons.iter().map(|(e, _)| ring_pts(e)).filter(|r| r.len() >= 3).collect();
    let all_rings: Vec<Vec<P>> =
        polygons.iter().flat_map(|(e, h)| std::iter::once(*e).chain(h.iter())).map(ring_pts).filter(|r| r.len() >= 3).collect();
    let area: f64 = all_rings.iter().map(|r| signed_area(r).abs()).sum();
    if shells.is_empty() || area == 0.0 {
        return Ok(Geometry::Polygon { exterior: Ring(Vec::new()), interiors: Vec::new() });
    }
    // The frame: the polygons' envelope expanded by 4 times its diagonal.
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in shells.iter().flatten() {
        x0 = x0.min(p.0);
        y0 = y0.min(p.1);
        x1 = x1.max(p.0);
        y1 = y1.max(p.1);
    }
    let d = 4.0 * (x1 - x0).hypot(y1 - y0);
    let frame = [(x0 - d, y0 - d), (x1 + d, y0 - d), (x1 + d, y1 + d), (x0 - d, y1 + d)];
    // The vertices (distinct), with their coordinates; the frame corners come last.
    let mut coords: Vec<Coord> = Vec::new();
    let mut index: HashMap<(u64, u64), usize> = HashMap::new();
    let mut vertex = |c: Coord, coords: &mut Vec<Coord>| -> usize {
        *index.entry((key(c.x), key(c.y))).or_insert_with(|| {
            coords.push(Coord { m: None, ..c });
            coords.len() - 1
        })
    };
    let mut edges: Vec<[usize; 2]> = Vec::new();
    for (e, h) in &polygons {
        for r in std::iter::once(*e).chain(h.iter()) {
            let ids: Vec<usize> = r.0.iter().map(|c| vertex(c.clone(), &mut coords)).collect();
            for w in ids.windows(2) {
                if w[0] != w[1] {
                    edges.push([w[0], w[1]]);
                }
            }
            if let (Some(&a), Some(&b)) = (ids.first(), ids.last())
                && a != b
            {
                edges.push([b, a]);
            }
        }
    }
    let first_frame = coords.len();
    let frame_ids: Vec<usize> = frame.iter().map(|&(x, y)| vertex(Coord::xy(x, y), &mut coords)).collect();
    for i in 0..4 {
        edges.push([frame_ids[i], frame_ids[(i + 1) % 4]]);
    }
    let mut seen = std::collections::HashSet::new();
    edges.retain(|&[a, b]| seen.insert((a.min(b), a.max(b))));
    let vertices: Vec<Vertex> = coords.iter().enumerate().map(|(index, c)| Vertex { p: Point2::new(c.x, c.y), index }).collect();
    let mut conflict = false;
    let cdt = ConstrainedDelaunayTriangulation::<Vertex>::try_bulk_load_cdt(vertices, edges, |_| conflict = true).map_err(spade_error)?;
    if conflict {
        return Err(Error::InvalidArgument("the polygons are invalid: their rings cross; run make-valid first".into()));
    }
    let pts: Vec<P> = coords.iter().map(|c| (c.x, c.y)).collect();
    // Classify the faces: in the polygons (kept), or in the frame's space between them.
    let faces: Vec<FixedFaceHandle<InnerTag>> = cdt.fixed_inner_faces().collect();
    let slot: HashMap<FixedFaceHandle<InnerTag>, usize> = faces.iter().enumerate().map(|(i, f)| (*f, i)).collect();
    let mut t = Tris { pts, tris: Vec::new() };
    // A 0-1 flood from the frame counts nested ring boundaries. Valid polygonal
    // input has disjoint shells and holes, so odd depth is polygon interior and
    // any positive depth lies within a shell. Each face/edge is visited O(1)
    // times, without testing every face against every input ring.
    let mut neighbors = Vec::with_capacity(faces.len());
    let mut depth = vec![usize::MAX; faces.len()];
    let mut pending = VecDeque::new();
    for (i, f) in faces.iter().enumerate() {
        let face = cdt.face(*f);
        let v = face.vertices().map(|v| v.data().index);
        let raw = face
            .adjacent_edges()
            .map(|e| (e.rev().face().as_inner().map(|f| slot[&f.fix()]), cdt.is_constraint_edge(e.as_undirected().fix())));
        let adj = raw.map(|(other, boundary)| if boundary { None } else { other });
        if v.iter().any(|&v| v >= first_frame) {
            depth[i] = 0;
            pending.push_back(i);
        }
        neighbors.push(raw);
        t.tris.push(Tri { v, adj, size: 0.0, removed: false });
    }
    while let Some(i) = pending.pop_front() {
        for (other, boundary) in neighbors[i] {
            if let Some(j) = other {
                let candidate = depth[i] + usize::from(boundary);
                if candidate < depth[j] {
                    depth[j] = candidate;
                    if boundary {
                        pending.push_back(j);
                    } else {
                        pending.push_front(j);
                    }
                }
            }
        }
    }
    let in_polygons: Vec<bool> = depth.iter().map(|d| d % 2 == 1).collect();
    let in_shells: Vec<bool> = depth.iter().map(|&d| d > 0).collect();
    // Cache the shell membership of each vertex for the tight erosion test.
    let mut memberships = vec![Vec::new(); coords.len()];
    for (shell, ring) in shells.iter().enumerate() {
        for &(x, y) in ring {
            let v = index[&(key(x), key(y))];
            if memberships[v].last() != Some(&shell) {
                memberships[v].push(shell);
            }
        }
    }
    let is_frame_tri = |t: &Tris, i: usize| t.tris[i].v.iter().any(|&v| v >= first_frame);
    let max_length = target_length(&t, ratio, |i| !in_shells[i] && !is_frame_tri(&t, i), |i, e| t.tris[i].adj[e].is_some());
    // Remove the frame corner triangles; the triangles opposite their corners are the border.
    let mut hull: Vec<bool> = in_shells.iter().map(|s| !s).collect();
    let mut queue: Vec<usize> = Vec::new();
    let mut border_edge: HashMap<usize, usize> = HashMap::new();
    let add_border = |t: &Tris, tri: usize, e: usize, queue: &mut Vec<usize>, border_edge: &mut HashMap<usize, usize>| {
        if let Some(adj) = t.tris[tri].adj[e] {
            queue.push(adj);
            if let Some(i) = t.index_of(adj, tri) {
                // GEOS ConcaveHullOfPolygons::addBorderTri uses map::insert: retain
                // the first exposed edge even if another neighbour is removed later.
                border_edge.entry(adj).or_insert(i);
            }
        }
    };
    #[allow(clippy::needless_range_loop)]
    for i in 0..t.tris.len() {
        if !hull[i] {
            continue;
        }
        if let Some(corner) = (0..3).find(|&k| t.tris[i].v[k] >= first_frame) {
            // The edge opposite vertex `corner` is the one from `corner + 1` to `corner + 2`.
            let opp = next(corner);
            if let Some(adj) = t.tris[i].adj[opp]
                && !is_frame_tri(&t, adj)
            {
                add_border(&t, i, opp, &mut queue, &mut border_edge);
            }
            t.remove(i);
            hull[i] = false;
        }
    }
    let touches_single_polygon = |t: &Tris, i: usize| -> bool {
        let [a, b, c] = t.tris[i].v;
        memberships[a].iter().any(|s| memberships[b].binary_search(s).is_ok() && memberships[c].binary_search(s).is_ok())
    };
    let erode = |t: &mut Tris, hull: &mut Vec<bool>, queue: &mut Vec<usize>, border_edge: &mut HashMap<usize, usize>| {
        while let Some(tri) = queue.pop() {
            if !hull[tri] {
                continue;
            }
            let removable =
                (tight && touches_single_polygon(t, tri)) || border_edge.get(&tri).is_some_and(|&e| t.length(tri, e) > max_length);
            if removable {
                for e in 0..3 {
                    add_border(t, tri, e, queue, border_edge);
                }
                t.remove(tri);
                hull[tri] = false;
                border_edge.remove(&tri);
            }
        }
    };
    erode(&mut t, &mut hull, &mut queue, &mut border_edge);
    if allow_holes {
        let mut from = 0;
        loop {
            let seed = (from..t.tris.len())
                .find(|&i| hull[i] && !t.is_border(i) && (0..3).any(|e| t.tris[i].adj[e].is_some() && t.length(i, e) > max_length));
            let Some(seed) = seed else { break };
            from = seed + 1;
            for e in 0..3 {
                add_border(&t, seed, e, &mut queue, &mut border_edge);
            }
            t.remove(seed);
            hull[seed] = false;
            border_edge.remove(&seed);
            erode(&mut t, &mut hull, &mut queue, &mut border_edge);
        }
    }
    let keep: Vec<bool> = (0..t.tris.len()).map(|i| hull[i] || in_polygons[i]).collect();
    Ok(t.to_geometry(&coords, &keep))
}

fn signed_area(r: &[P]) -> f64 {
    let n = r.len();
    (0..n).map(|i| r[i].0 * r[(i + 1) % n].1 - r[(i + 1) % n].0 * r[i].1).sum::<f64>() / 2.0
}

/// The polygons covered by a set of triangles (`faces`, vertex indices into `pts` in either
/// orientation): their boundary edges chained into rings, split where rings touch themselves,
/// shells counter-clockwise and holes clockwise. One polygon gives a Polygon, several a
/// MultiPolygon, none an empty Polygon.
fn region_boundary(pts: &[P], coords: &[Coord], faces: &[[usize; 3]]) -> Geometry {
    // Directed edges of counter-clockwise faces; boundary edges have no twin.
    let mut directed: HashMap<(usize, usize), usize> = HashMap::new();
    let ccw: Vec<[usize; 3]> =
        faces.iter().map(|f| if orient(pts[f[0]], pts[f[1]], pts[f[2]]) < 0 { [f[0], f[2], f[1]] } else { *f }).collect();
    for (k, f) in ccw.iter().enumerate() {
        for i in 0..3 {
            directed.insert((f[i], f[(i + 1) % 3]), k);
        }
    }
    let mut boundary: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut count = 0;
    for f in &ccw {
        for i in 0..3 {
            let (a, b) = (f[i], f[(i + 1) % 3]);
            if !directed.contains_key(&(b, a)) {
                boundary.entry(a).or_default().push(b);
                count += 1;
            }
        }
    }
    // Chain the edges: leaving a vertex, take the outgoing edge first clockwise from the edge
    // arrived along, which keeps to the faces of the sector on the left.
    let angle = |a: usize, b: usize| (pts[b].1 - pts[a].1).atan2(pts[b].0 - pts[a].0);
    let mut used: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut rings: Vec<Vec<usize>> = Vec::new();
    let mut starts: Vec<(usize, usize)> = boundary.iter().flat_map(|(&a, bs)| bs.iter().map(move |&b| (a, b))).collect();
    starts.sort_unstable();
    for (a0, b0) in starts {
        if used.contains(&(a0, b0)) {
            continue;
        }
        let mut ring = vec![a0];
        let (mut a, mut b) = (a0, b0);
        loop {
            used.insert((a, b));
            ring.push(b);
            if b == a0 && ring.len() > 1 {
                // Close here; unused edges at a pinch vertex form separate loops later.
                break;
            }
            let back = angle(b, a);
            let candidates = boundary.get(&b).map(|v| v.as_slice()).unwrap_or(&[]);
            let mut best: Option<(f64, usize)> = None;
            for &c in candidates {
                if used.contains(&(b, c)) {
                    continue;
                }
                // Clockwise turn from the backward direction, in (0, 2π].
                let mut turn = back - angle(b, c);
                while turn <= 0.0 {
                    turn += std::f64::consts::TAU;
                }
                while turn > std::f64::consts::TAU {
                    turn -= std::f64::consts::TAU;
                }
                if best.is_none_or(|(t, _)| turn < t) {
                    best = Some((turn, c));
                }
            }
            let Some((_, c)) = best else { break };
            a = b;
            b = c;
            if ring.len() > count + 1 {
                break;
            }
        }
        rings.extend(split_ring(ring));
    }
    // Shells (counter-clockwise) and holes; each hole goes in the smallest shell containing it.
    let ring_xy = |r: &[usize]| r.iter().map(|&i| pts[i]).collect::<Vec<P>>();
    let mut shells: Vec<(Vec<usize>, f64, Vec<Vec<usize>>)> = Vec::new();
    let mut holes: Vec<Vec<usize>> = Vec::new();
    for r in rings {
        let a = signed_area(&ring_xy(&r[..r.len() - 1]));
        if a > 0.0 {
            shells.push((r, a, Vec::new()));
        } else if a < 0.0 {
            holes.push(r);
        }
    }
    let shell_xy: Vec<Vec<P>> = shells.iter().map(|(s, _, _)| ring_xy(&s[..s.len() - 1])).collect();
    for h in holes {
        // A point just inside the hole: the midpoint of an edge, moved to its right side.
        let (p, q) = (pts[h[0]], pts[h[1]]);
        let probe = hole_probe(p, q, &ring_xy(&h));
        let owner = shells
            .iter()
            .enumerate()
            .filter(|(i, _)| inside_rings(probe, std::slice::from_ref(&shell_xy[*i])))
            .min_by(|a, b| a.1.1.total_cmp(&b.1.1))
            .map(|(i, _)| i);
        if let Some(i) = owner {
            shells[i].2.push(h);
        }
    }
    let ring = |r: &[usize]| open_ring(r.iter().map(|&i| coords[i].clone()).collect());
    let mut polygons: Vec<(Ring, Vec<Ring>)> = shells.iter().map(|(s, _, hs)| (ring(s), hs.iter().map(|h| ring(h)).collect())).collect();
    match polygons.len() {
        0 => Geometry::Polygon { exterior: Ring(Vec::new()), interiors: Vec::new() },
        1 => {
            let (exterior, interiors) = polygons.remove(0);
            Geometry::Polygon { exterior, interiors }
        }
        _ => Geometry::MultiPolygon(polygons),
    }
}

/// A point on the right of the edge `p`→`q` of a clockwise hole ring, inside the hole near the
/// edge's midpoint (the rings of a hole are clockwise, so the hole is on the right).
fn hole_probe(p: P, q: P, ring: &[P]) -> P {
    let m = ((p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0);
    let (dx, dy) = (q.0 - p.0, q.1 - p.1);
    let len = (dx * dx + dy * dy).sqrt();
    let mut eps = len * 1e-6;
    let ring = ring.to_vec();
    for _ in 0..30 {
        let c = (m.0 + dy / len * eps, m.1 - dx / len * eps);
        if inside_rings(c, std::slice::from_ref(&ring)) {
            return c;
        }
        eps /= 4.0;
    }
    m
}

/// Splits a closed ring of vertex indices at the vertices it visits twice into simple rings.
fn split_ring(ring: Vec<usize>) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for v in ring {
        if let Some(pos) = stack.iter().position(|&s| s == v) {
            let mut loop_: Vec<usize> = stack.split_off(pos);
            loop_.push(v);
            if loop_.len() >= 4 {
                out.push(loop_);
            }
            stack.push(v);
        } else {
            stack.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wkt_xy(g: &Geometry) -> Vec<(f64, f64)> {
        g.all_coords().iter().map(|c| (c.x, c.y)).collect()
    }

    fn points(pts: &[(f64, f64)]) -> Geometry {
        Geometry::MultiPoint(pts.iter().map(|&(x, y)| Coord::xy(x, y)).collect())
    }

    #[test]
    fn convex_hulls_like_geos() {
        // GEOS 3.14 results.
        let g = convex_hull(&points(&[(5.0, 0.0), (10.0, 5.0), (5.0, 10.0), (0.0, 5.0)])).unwrap();
        assert_eq!(wkt_xy(&g), [(5.0, 0.0), (0.0, 5.0), (5.0, 10.0), (10.0, 5.0)]);
        let g = convex_hull(&points(&[(3.0, 1.0), (0.0, 1.0), (1.0, 0.0), (2.0, 0.0), (3.0, 3.0)])).unwrap();
        assert_eq!(wkt_xy(&g), [(1.0, 0.0), (0.0, 1.0), (3.0, 3.0), (3.0, 1.0), (2.0, 0.0)]);
        let g = convex_hull(&Geometry::LineString(vec![Coord::xy(0.0, 10.0), Coord::xy(5.0, 5.0), Coord::xy(10.0, 0.0)])).unwrap();
        assert_eq!(g, Geometry::LineString(vec![Coord::xy(10.0, 0.0), Coord::xy(0.0, 10.0)]));
        assert_eq!(convex_hull(&points(&[(1.0, 2.0), (1.0, 2.0)])).unwrap(), Geometry::point(1.0, 2.0));
        let g = convex_hull(&Geometry::LineString(vec![
            Coord { x: 0.0, y: 0.0, z: Some(1.0), m: Some(2.0) },
            Coord { x: 5.0, y: 5.0, z: Some(3.0), m: Some(4.0) },
            Coord { x: 10.0, y: 0.0, z: Some(5.0), m: Some(6.0) },
        ]))
        .unwrap();
        assert!(g.all_coords().iter().all(|c| c.z.is_some() && c.m.is_none()));
    }

    #[test]
    fn concave_hulls_of_points() {
        // GEOS 3.14 (through OGR's ConcaveHull): areas 61, 61 and 73 for ratios 0, 0.2 and 0.4;
        // POLYGON ((2 3,1 12,5 9,5 5,11 10,10 1,8 3,0 0,2 3)) for 0.
        let g = points(&[(0.0, 0.0), (10.0, 1.0), (11.0, 10.0), (1.0, 12.0), (5.0, 5.0), (5.0, 9.0), (2.0, 3.0), (8.0, 3.0)]);
        for (ratio, area) in [(0.0, 61.0), (0.2, 61.0), (0.4, 73.0)] {
            let hull = concave_hull(&g, ratio, false).unwrap();
            let Geometry::Polygon { exterior, interiors } = &hull else { panic!("{hull:?}") };
            assert!(interiors.is_empty());
            let ring: Vec<P> = exterior.0.iter().map(|c| (c.x, c.y)).collect();
            assert_eq!(signed_area(&ring).abs(), area, "ratio {ratio}: {ring:?}");
        }
        let hull = concave_hull(&g, 0.0, false).unwrap();
        let mut got = wkt_xy(&hull);
        got.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(got, [(0.0, 0.0), (1.0, 12.0), (2.0, 3.0), (5.0, 5.0), (5.0, 9.0), (8.0, 3.0), (10.0, 1.0), (11.0, 10.0)]);
        let convex = concave_hull(&g, 1.0, false).unwrap();
        let Geometry::Polygon { exterior, .. } = convex else { panic!() };
        assert_eq!(exterior.0.len(), 4);
        // Collinear input gives the convex hull.
        let line = Geometry::LineString(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 1.0), Coord::xy(2.0, 2.0)]);
        assert!(matches!(concave_hull(&line, 0.5, false).unwrap(), Geometry::LineString(_)));
    }

    #[test]
    fn polygon_hull_classifies_holes_and_islands() {
        let square = |lo, hi| Ring(vec![Coord::xy(lo, lo), Coord::xy(hi, lo), Coord::xy(hi, hi), Coord::xy(lo, hi)]);
        let input = Geometry::MultiPolygon(vec![(square(0., 10.), vec![square(2., 8.)]), (square(4., 6.), vec![])]);
        let result = concave_hull_of_polygons(&input, 0., true, true).unwrap();
        let area = |exterior: &Ring, holes: &[Ring]| {
            let a = |r: &Ring| signed_area(&r.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>()).abs();
            a(exterior) - holes.iter().map(a).sum::<f64>()
        };
        let Geometry::MultiPolygon(ps) = result else { panic!("{result:?}") };
        assert_eq!(ps.iter().map(|(e, h)| area(e, h)).sum::<f64>(), 68.);
        let empty = Geometry::GeometryCollection(vec![]);
        assert!(matches!(convex_hull(&empty).unwrap(), Geometry::GeometryCollection(_)));
        assert!(matches!(concave_hull(&empty, 0.5, false).unwrap(), Geometry::Polygon { exterior, .. } if exterior.0.is_empty()));
    }

    #[test]
    fn concave_hull_of_two_squares() {
        let square = |x: f64, w: f64, h: f64| {
            let r = vec![Coord::xy(x, 0.0), Coord::xy(x + w, 0.0), Coord::xy(x + w, h), Coord::xy(x, h), Coord::xy(x, 0.0)];
            (Ring(r), Vec::new())
        };
        let g = Geometry::MultiPolygon(vec![square(0.0, 4.0, 4.0), square(6.0, 4.0, 10.0)]);
        // A ratio of 0 keeps the input polygons.
        let r0 = concave_hull_of_polygons(&g, 0.0, false, false).unwrap();
        assert!(matches!(&r0, Geometry::MultiPolygon(ps) if ps.len() == 2), "{r0:?}");
        // A ratio of 1 fills the convex hull of the polygons: area 16 + 40 + the gap.
        let r1 = concave_hull_of_polygons(&g, 1.0, false, false).unwrap();
        let Geometry::Polygon { exterior, interiors } = &r1 else { panic!("{r1:?}") };
        assert!(interiors.is_empty());
        let ring: Vec<P> = exterior.0.iter().map(|c| (c.x, c.y)).collect();
        // The convex hull: (0,0) (10,0) (10,10) (6,10) (0,4).
        assert!((signed_area(&ring) - (100.0 - 0.5 * 6.0 * 6.0)).abs() < 1e-9, "{}", signed_area(&ring));
    }
}
