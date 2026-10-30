// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions.
// SPDX-FileCopyrightText: Copyright (c) 2016 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   JTS 1.20.0 (commit 6e95fe82feb986a7aa657f4ffa406d8c290af509):
//     modules/core/src/main/java/org/locationtech/jts/triangulate/DelaunayTriangulationBuilder.java
//     modules/core/src/main/java/org/locationtech/jts/triangulate/IncrementalDelaunayTriangulator.java
//     modules/core/src/main/java/org/locationtech/jts/triangulate/quadedge/QuadEdge.java
//     modules/core/src/main/java/org/locationtech/jts/triangulate/quadedge/QuadEdgeSubdivision.java
//     modules/core/src/main/java/org/locationtech/jts/triangulate/quadedge/LastFoundQuadEdgeLocator.java
//     modules/core/src/main/java/org/locationtech/jts/triangulate/quadedge/Vertex.java
//     modules/core/src/main/java/org/locationtech/jts/triangulate/quadedge/TrianglePredicate.java

//! Zero-tolerance Delaunay subdivision derived from JTS 1.20.0 under EDL-1.0.
//! Copyright (c) 2016 Vivid Solutions; Copyright (c) 2016 Martin Davis.
//! Sources: DelaunayTriangulationBuilder,
//! IncrementalDelaunayTriangulator, QuadEdge, QuadEdgeSubdivision,
//! LastFoundQuadEdgeLocator, Vertex and TrianglePredicate. See NOTICE.
//!
//! Arena indices replace object links. Every topology walk is bounded and returns
//! an error if it fails to converge. Face traversal retains the JTS order, which
//! matters when hull erosion has equal-size, equal-area candidates.
use super::simplify::orient;
use crate::{Error, Result};
use wbvector::Coord;

type P = (f64, f64);
#[derive(Clone, Copy)]
struct Edge {
    next: usize,
    origin: usize,
}
struct Subdivision {
    points: Vec<P>,
    input_count: usize,
    edges: Vec<Edge>,
    last: usize,
}
fn rot(e: usize) -> usize {
    (e & !3) | ((e + 1) & 3)
}
fn inv(e: usize) -> usize {
    (e & !3) | ((e + 3) & 3)
}
fn sym(e: usize) -> usize {
    e ^ 2
}
fn failure() -> Error {
    Error::InvalidArgument("Delaunay subdivision failed to converge".into())
}
impl Subdivision {
    fn origin(&self, e: usize) -> usize {
        self.edges[e].origin
    }
    fn dest(&self, e: usize) -> usize {
        self.origin(sym(e))
    }
    fn next(&self, e: usize) -> usize {
        self.edges[e].next
    }
    fn previous(&self, e: usize) -> usize {
        rot(self.next(rot(e)))
    }
    fn left_next(&self, e: usize) -> usize {
        rot(self.next(inv(e)))
    }
    fn left_previous(&self, e: usize) -> usize {
        sym(self.next(e))
    }
    fn dest_previous(&self, e: usize) -> usize {
        inv(self.next(inv(e)))
    }
    fn make(&mut self, a: usize, b: usize) -> usize {
        let e = self.edges.len();
        self.edges.extend([
            Edge { next: e, origin: a },
            Edge { next: e + 3, origin: 0 },
            Edge { next: e + 2, origin: b },
            Edge { next: e + 1, origin: 0 },
        ]);
        e
    }
    fn splice(&mut self, a: usize, b: usize) {
        let alpha = rot(self.next(a));
        let beta = rot(self.next(b));
        let (t1, t2, t3, t4) = (self.next(b), self.next(a), self.next(beta), self.next(alpha));
        self.edges[a].next = t1;
        self.edges[b].next = t2;
        self.edges[alpha].next = t3;
        self.edges[beta].next = t4;
    }
    fn connect(&mut self, a: usize, b: usize) -> usize {
        let e = self.make(self.dest(a), self.origin(b));
        self.splice(e, self.left_next(a));
        self.splice(sym(e), b);
        e
    }
    fn swap(&mut self, e: usize) {
        let a = self.previous(e);
        let b = self.previous(sym(e));
        self.splice(e, a);
        self.splice(sym(e), b);
        self.splice(e, self.left_next(a));
        self.splice(sym(e), self.left_next(b));
        self.edges[e].origin = self.dest(a);
        self.edges[sym(e)].origin = self.dest(b);
    }
    /// JTS Vertex.rightOf uses its unrolled determinant here. Robust orientation
    /// is used for the boundary-convexity test, as in the JTS triangulator.
    fn right_of(&self, p: usize, e: usize) -> bool {
        let p = self.points[p];
        let b = self.points[self.dest(e)];
        let c = self.points[self.origin(e)];
        (b.0 - p.0) * (c.1 - p.1) - (b.1 - p.1) * (c.0 - p.0) > 0.0
    }
    fn locate(&mut self, p: usize) -> Result<usize> {
        let mut e = self.last;
        for _ in 0..=self.edges.len() {
            if self.points[p] == self.points[self.origin(e)] || self.points[p] == self.points[self.dest(e)] {
                self.last = e;
                return Ok(e);
            }
            if self.right_of(p, e) {
                e = sym(e);
            } else if !self.right_of(p, self.next(e)) {
                e = self.next(e);
            } else if !self.right_of(p, self.dest_previous(e)) {
                e = self.dest_previous(e);
            } else {
                self.last = e;
                return Ok(e);
            }
        }
        Err(failure())
    }
    fn frame(&self, v: usize) -> bool {
        v >= self.input_count
    }
    fn concave_boundary(&self, e: usize) -> bool {
        let edge = if self.frame(self.dest(e)) {
            e
        } else if self.frame(self.origin(e)) {
            sym(e)
        } else {
            return false;
        };
        orient(self.points[self.dest(self.previous(edge))], self.points[self.dest(self.next(edge))], self.points[self.origin(edge)]) > 0
    }
    fn between_frame(&self, e: usize, p: usize) -> bool {
        let a = self.dest(self.next(e));
        let b = self.dest(self.previous(e));
        (a == p && self.frame(b)) || (b == p && self.frame(a))
    }
    fn in_circle(&self, p: usize, a: usize, b: usize, c: usize) -> bool {
        let p = self.points[p];
        let a = self.points[a];
        let b = self.points[b];
        let c = self.points[c];
        let (ax, ay, bx, by, cx, cy) = (a.0 - p.0, a.1 - p.1, b.0 - p.0, b.1 - p.1, c.0 - p.0, c.1 - p.1);
        let ab = ax * by - bx * ay;
        let bc = bx * cy - cx * by;
        let ca = cx * ay - ax * cy;
        let al = ax * ax + ay * ay;
        let bl = bx * bx + by * by;
        let cl = cx * cx + cy * cy;
        al * bc + bl * ca + cl * ab > 0.0
    }
    fn insert(&mut self, p: usize) -> Result<()> {
        let mut e = self.locate(p)?;
        if self.points[p] == self.points[self.origin(e)] || self.points[p] == self.points[self.dest(e)] {
            return Ok(());
        }
        // At zero tolerance JTS's distance < edgeCoincidenceTolerance check
        // does not delete on-edge segments before constructing the fan.
        let mut base = self.make(self.origin(e), p);
        self.splice(base, e);
        let start = base;
        let bound = self.edges.len().saturating_mul(2);
        let mut closed = false;
        for _ in 0..bound {
            base = self.connect(e, sym(base));
            e = self.previous(base);
            if self.left_next(e) == start {
                closed = true;
                break;
            }
        }
        if !closed {
            return Err(failure());
        }
        for _ in 0..self.edges.len().saturating_mul(4) {
            let t = self.previous(e);
            let mut flip = self.right_of(self.dest(t), e) && self.in_circle(p, self.origin(e), self.dest(t), self.dest(e));
            if self.concave_boundary(e) {
                flip = true;
            } else if self.between_frame(e, p) {
                flip = false;
            }
            if flip {
                self.swap(e);
                e = self.previous(e);
                continue;
            }
            if self.next(e) == start {
                return Ok(());
            }
            e = self.left_previous(self.next(e));
        }
        Err(failure())
    }
    fn triangles(&self) -> Result<Vec<[usize; 3]>> {
        let mut visited = vec![false; self.edges.len()];
        let mut stack = vec![0];
        let mut result = Vec::new();
        while let Some(e) = stack.pop() {
            if visited[e] {
                continue;
            }
            let mut edge = e;
            let mut face = [0; 3];
            let mut closed = false;
            for vertex in &mut face {
                *vertex = self.origin(edge);
                if !visited[sym(edge)] {
                    stack.push(sym(edge));
                }
                visited[edge] = true;
                edge = self.left_next(edge);
                if edge == e {
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err(failure());
            }
            if face.iter().any(|&v| self.frame(v)) {
                continue;
            }
            let orientation = orient(self.points[face[0]], self.points[face[1]], self.points[face[2]]);
            if orientation == 0 {
                continue;
            }
            if orientation > 0 {
                face.swap(1, 2);
            }
            result.push(face);
        }
        Ok(result)
    }
}

/// Returns clockwise triangles in subdivision traversal order. Input IDs are
/// preserved; insertion order is sorted by XY, as JTS's builder specifies.
pub(crate) fn triangles(coords: &[Coord]) -> Result<Vec<[usize; 3]>> {
    if coords.len() < 3 {
        return Ok(Vec::new());
    }
    let mut points = coords.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>();
    let env = points.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |b, p| {
        (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1))
    });
    let size = (env.2 - env.0).max(env.3 - env.1) * 10.0;
    let frame = [((env.2 + env.0) / 2.0, env.3 + size), (env.0 - size, env.1 - size), (env.2 + size, env.1 - size)];
    if frame.iter().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
        return Err(Error::InvalidArgument("Delaunay frame exceeds the coordinate range".into()));
    }
    let n = points.len();
    points.extend(frame);
    let mut s = Subdivision { points, input_count: n, edges: Vec::new(), last: 0 };
    let a = s.make(n, n + 1);
    let b = s.make(n + 1, n + 2);
    s.splice(sym(a), b);
    let c = s.make(n + 2, n);
    s.splice(sym(b), c);
    s.splice(sym(c), a);
    let mut order = (0..n).collect::<Vec<_>>();
    order.sort_by(|&a, &b| s.points[a].0.total_cmp(&s.points[b].0).then(s.points[a].1.total_cmp(&s.points[b].1)));
    for p in order {
        s.insert(p)?;
    }
    s.triangles()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserting_a_site_on_an_existing_edge_keeps_the_covered_area() {
        let mut s = Subdivision {
            points: vec![(0., 0.), (8., 0.), (0., 8.), (4., 0.), (4., 88.), (-80., -80.), (88., -80.)],
            input_count: 4,
            edges: Vec::new(),
            last: 0,
        };
        let a = s.make(4, 5);
        let b = s.make(5, 6);
        s.splice(sym(a), b);
        let c = s.make(6, 4);
        s.splice(sym(b), c);
        s.splice(sym(c), a);
        for i in 0..3 {
            s.insert(i).unwrap();
        }
        let edge = s.locate(3).unwrap();
        let (a, b) = (s.points[s.origin(edge)], s.points[s.dest(edge)]);
        assert_ne!(s.points[3], a);
        assert_ne!(s.points[3], b);
        assert_eq!(orient(a, b, s.points[3]), 0);
        s.insert(3).unwrap();
        let faces = s.triangles().unwrap();
        // JTS 1.20's executable returns the same two non-degenerate triangles.
        let mut expected = vec![vec![0, 2, 3], vec![1, 2, 3]];
        expected.sort();
        let mut actual = faces
            .iter()
            .map(|f| {
                let mut f = f.to_vec();
                f.sort();
                f
            })
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, expected);
        let area = faces
            .iter()
            .map(|f| {
                let (a, b, c) = (s.points[f[0]], s.points[f[1]], s.points[f[2]]);
                ((b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1)).abs() / 2.
            })
            .sum::<f64>();
        assert_eq!(area, 32.);
    }

    #[test]
    fn underflowing_sites_return_a_bounded_reference_failure() {
        // Executable GEOS 3.14.1 also reports a locate failure here. This
        // source-compatible port does not claim exact predicates at every scale.
        let coords = [Coord::xy(0., 0.), Coord::xy(1e-200, 0.), Coord::xy(0., 1e-200)];
        assert!(matches!(triangles(&coords), Err(Error::InvalidArgument(message)) if message.contains("failed to converge")));
    }
}
