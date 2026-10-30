// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions.
// SPDX-FileCopyrightText: Copyright (c) 2021 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2022 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   JTS 1.20.0 (commit 6e95fe82feb986a7aa657f4ffa406d8c290af509):
//     modules/core/src/main/java/org/locationtech/jts/algorithm/ConvexHull.java
//     modules/core/src/main/java/org/locationtech/jts/algorithm/hull/ConcaveHull.java
//     modules/core/src/main/java/org/locationtech/jts/algorithm/hull/ConcaveHullOfPolygons.java
//     modules/core/src/main/java/org/locationtech/jts/algorithm/hull/HullTri.java
//     modules/core/src/main/java/org/locationtech/jts/triangulate/tri/Tri.java

//! Convex and concave hulls derived from JTS 1.20.0 under EDL-1.0.
//!
//! Sources: `algorithm/ConvexHull`, `algorithm/hull/{ConcaveHull,
//! ConcaveHullOfPolygons,HullTri}` and `triangulate/tri/Tri`.
//! Copyright (c) 2016 Vivid Solutions; Copyright (c) 2021-2022 Martin Davis.
//! See NOTICE and docs/jts-provenance.md for source revisions and compatibility.
//!
//! Point and constrained polygon triangulation retain JTS construction order.
//! The polygon mesh includes JTS's finite frame. Rings are assembled from
//! the surviving directed boundary edges without rounding or overlay snapping.

use super::simplify::{dist, open_ring, orient};
use crate::{Error, Geometry, Result};
use rstar::{
    AABB, Envelope, RTree,
    primitives::{GeomWithData, Rectangle},
};
use std::collections::{BinaryHeap, HashMap, HashSet};
use wbvector::{Coord, Ring};

type P = (f64, f64);
fn xy(c: &Coord) -> P {
    (c.x, c.y)
}
fn bits(x: f64) -> u64 {
    if x == 0.0 { 0 } else { x.to_bits() }
}
fn coord_key(c: &Coord) -> (u64, u64) {
    (bits(c.x), bits(c.y))
}

fn vertices(g: &Geometry) -> Result<Vec<Coord>> {
    let mut seen = HashSet::new();
    let mut coords = Vec::new();
    for c in g.all_coords() {
        if !c.x.is_finite() || !c.y.is_finite() {
            return Err(Error::InvalidArgument("non-finite XY coordinates".into()));
        }
        if seen.insert(coord_key(c)) {
            coords.push(Coord { x: c.x, y: c.y, z: c.z, m: None });
        }
    }
    Ok(coords)
}

/// The smallest convex geometry covering the input. Z is retained, M dropped.
/// Polygon output starts at the lowest, then leftmost, site and runs clockwise.
pub fn convex_hull(g: &Geometry) -> Result<Geometry> {
    let mut sites = vertices(g)?;
    if sites.is_empty() {
        return Ok(Geometry::GeometryCollection(Vec::new()));
    }
    if sites.len() == 1 {
        return Ok(Geometry::Point(sites.remove(0)));
    }
    // Observed GEOS behavior: the two-site result retains first-occurrence
    // order. Sorting starts only when there are at least three unique sites.
    if sites.len() == 2 {
        return Ok(Geometry::LineString(sites));
    }
    sites.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    let origin = xy(&sites[0]);
    sites[1..].sort_by(|a, b| orient(origin, xy(a), xy(b)).cmp(&0).then(a.y.total_cmp(&b.y)).then(a.x.total_cmp(&b.x)));
    let mut scan: Vec<Coord> = Vec::new();
    for p in sites {
        while scan.len() >= 2 && orient(xy(&scan[scan.len() - 2]), xy(&scan[scan.len() - 1]), xy(&p)) > 0 {
            scan.pop();
        }
        scan.push(p);
    }
    // JTS cleanRing removes intermediate collinear sites after the Graham scan.
    let mut clean: Vec<Coord> = Vec::new();
    for i in 0..scan.len() {
        let p = &scan[i];
        let q = &scan[(i + 1) % scan.len()];
        if let Some(a) = clean.last()
            && orient(xy(a), xy(p), xy(q)) == 0
            && p.x >= a.x.min(q.x)
            && p.x <= a.x.max(q.x)
            && p.y >= a.y.min(q.y)
            && p.y <= a.y.max(q.y)
        {
            continue;
        }
        clean.push(p.clone());
    }
    Ok(if clean.len() <= 2 { Geometry::LineString(clean) } else { Geometry::Polygon { exterior: Ring(clean), interiors: Vec::new() } })
}

#[derive(Clone)]
struct Triangle {
    vertices: [usize; 3],
    adjacent: [Option<usize>; 3],
    alive: bool,
}

struct Mesh {
    coords: Vec<Coord>,
    triangles: Vec<Triangle>,
}

impl Mesh {
    fn new(coords: Vec<Coord>, faces: Vec<[usize; 3]>) -> Result<Self> {
        let mut triangles: Vec<Triangle> =
            faces.into_iter().map(|vertices| Triangle { vertices, adjacent: [None; 3], alive: true }).collect();
        // Retain the first face after pairing an undirected edge, so a third
        // incident face or repeated direction cannot overwrite its adjacency.
        let mut edges: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
        for t in 0..triangles.len() {
            for i in 0..3 {
                let a = triangles[t].vertices[i];
                let b = triangles[t].vertices[(i + 1) % 3];
                let key = (a.min(b), a.max(b));
                if let Some(&(s, j)) = edges.get(&key) {
                    if triangles[s].vertices[j] == a || triangles[s].adjacent[j].is_some() {
                        return Err(Error::InvalidArgument("cannot construct a consistent hull triangulation".into()));
                    }
                    triangles[t].adjacent[i] = Some(s);
                    triangles[s].adjacent[j] = Some(t);
                } else {
                    edges.insert(key, (t, i));
                }
            }
        }
        Ok(Self { coords, triangles })
    }
    fn length(&self, t: usize, i: usize) -> f64 {
        let v = self.triangles[t].vertices;
        dist(xy(&self.coords[v[i]]), xy(&self.coords[v[(i + 1) % 3]]))
    }
    fn longest(&self, t: usize) -> f64 {
        (0..3).map(|i| self.length(t, i)).fold(0.0, f64::max)
    }
    fn area(&self, t: usize) -> f64 {
        let v = self.triangles[t].vertices.map(|i| xy(&self.coords[i]));
        ((v[1].0 - v[0].0) * (v[2].1 - v[0].1) - (v[2].0 - v[0].0) * (v[1].1 - v[0].1)).abs() / 2.0
    }
    fn degree(&self, t: usize) -> usize {
        self.triangles[t].adjacent.iter().flatten().count()
    }
    fn boundary_length(&self, t: usize) -> f64 {
        (0..3).filter(|&i| self.triangles[t].adjacent[i].is_none()).map(|i| self.length(t, i)).sum()
    }
    fn remove(&mut self, t: usize) -> [Option<usize>; 3] {
        let neighbors = self.triangles[t].adjacent;
        for n in neighbors.iter().flatten() {
            for edge in &mut self.triangles[*n].adjacent {
                if *edge == Some(t) {
                    *edge = None;
                }
            }
        }
        self.triangles[t].alive = false;
        self.triangles[t].adjacent = [None; 3];
        neighbors
    }
    /// JTS Tri.isInteriorVertex: walk around the vertex until back at the start,
    /// or until the fan reaches the boundary. Iteration is bounded by mesh size.
    fn interior_vertex(&self, t: usize, corner: usize) -> bool {
        let (mut current, mut edge) = (t, corner);
        for _ in 0..=self.triangles.len() {
            let Some(n) = self.triangles[current].adjacent[edge] else {
                return false;
            };
            let Some(reverse) = self.triangles[n].adjacent.iter().position(|v| *v == Some(current)) else {
                return false;
            };
            if n == t {
                return true;
            }
            (current, edge) = (n, (reverse + 1) % 3);
        }
        false
    }
    fn connecting(&self, t: usize) -> bool {
        (0..3)
            .find(|&i| self.triangles[t].adjacent[i].is_some() && self.triangles[t].adjacent[(i + 2) % 3].is_some())
            .is_some_and(|i| !self.interior_vertex(t, i))
    }
    fn boundary_touch(&self, t: usize) -> bool {
        (0..3).any(|i| {
            self.triangles[t].adjacent[i].is_some() && self.triangles[t].adjacent[(i + 2) % 3].is_some() && !self.interior_vertex(t, i)
        })
    }
    fn queue_border(&self, t: usize, queue: &mut BinaryHeap<Candidate>) {
        if self.triangles[t].alive && self.degree(t) == 2 {
            queue.push(Candidate { size: self.boundary_length(t), area: self.area(t), id: t });
        }
    }
    fn erode_points(&mut self, queue: &mut BinaryHeap<Candidate>, target: f64, hole: bool, seed: Option<usize>) {
        while let Some(c) = queue.pop() {
            let t = c.id;
            if !self.triangles[t].alive {
                continue;
            }
            if Some(t) != seed && c.size < target {
                break;
            }
            if Some(t) == seed || (self.degree(t) == 2 && if hole { !self.boundary_touch(t) } else { !self.connecting(t) }) {
                let neighbors = self.remove(t);
                for n in neighbors.into_iter().flatten() {
                    self.queue_border(n, queue);
                }
            }
        }
    }
    fn geometry(&self, selected: &[bool]) -> Result<Geometry> {
        let faces = self.triangles.iter().zip(selected).filter(|(_, keep)| **keep).map(|(t, _)| t.vertices).collect::<Vec<_>>();
        boundary_geometry(&self.coords, &faces)
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    size: f64,
    area: f64,
    id: usize,
}
impl PartialEq for Candidate {
    fn eq(&self, b: &Self) -> bool {
        self.cmp(b).is_eq()
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, b: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(b))
    }
}
impl Ord for Candidate {
    fn cmp(&self, b: &Self) -> std::cmp::Ordering {
        self.size.total_cmp(&b.size).then(self.area.total_cmp(&b.area))
    }
}
/// Compatibility introsort reproducing observed Linux/libstdc++ std::sort
/// tie ordering for equal-size hole seeds. That order changes erosion results;
/// a stable tie-break is not behaviorally interchangeable. This helper was
/// independently written from executable observations, without GEOS or
/// libstdc++ implementation source. See docs/jts-provenance.md.
fn sort_hole_candidates(items: &mut [Candidate]) {
    fn less(a: Candidate, b: Candidate) -> bool {
        a.size < b.size
    }
    fn heap_adjust(a: &mut [Candidate], root: usize) {
        let value = a[root];
        let mut hole = root;
        loop {
            let left = 2 * hole + 1;
            if left >= a.len() {
                break;
            }
            let right = left + 1;
            let child = if right < a.len() && !less(a[right], a[left]) { right } else { left };
            a[hole] = a[child];
            hole = child;
        }
        while hole > root {
            let parent = (hole - 1) / 2;
            if !less(a[parent], value) {
                break;
            }
            a[hole] = a[parent];
            hole = parent;
        }
        a[hole] = value;
    }
    fn heap_sort(a: &mut [Candidate]) {
        for root in (0..a.len() / 2).rev() {
            heap_adjust(a, root);
        }
        for end in (1..a.len()).rev() {
            a.swap(0, end);
            heap_adjust(&mut a[..end], 0);
        }
    }
    fn partition(a: &mut [Candidate]) -> usize {
        let (lo, mid, hi) = (1, a.len() / 2, a.len() - 1);
        let pivot = if less(a[lo], a[mid]) {
            if less(a[mid], a[hi]) {
                mid
            } else if less(a[lo], a[hi]) {
                hi
            } else {
                lo
            }
        } else if less(a[lo], a[hi]) {
            lo
        } else if less(a[mid], a[hi]) {
            hi
        } else {
            mid
        };
        a.swap(0, pivot);
        let pivot = a[0];
        let (mut i, mut j) = (1, a.len() - 1);
        loop {
            while i < a.len() && less(a[i], pivot) {
                i += 1;
            }
            while j > 0 && less(pivot, a[j]) {
                j -= 1;
            }
            if i >= j {
                return i;
            }
            a.swap(i, j);
            i += 1;
            j -= 1;
        }
    }
    fn intro(mut a: &mut [Candidate], mut depth: usize) {
        while a.len() > 16 {
            if depth == 0 {
                heap_sort(a);
                return;
            }
            depth -= 1;
            let cut = partition(a);
            if cut == 0 || cut == a.len() {
                heap_sort(a);
                return;
            }
            let (left, right) = a.split_at_mut(cut);
            intro(right, depth);
            a = left;
        }
    }
    if items.len() < 2 {
        return;
    }
    intro(items, 2 * items.len().ilog2() as usize);
    for i in 1..items.len() {
        let value = items[i];
        let mut j = i;
        while j > 0 && less(value, items[j - 1]) {
            items[j] = items[j - 1];
            j -= 1;
        }
        items[j] = value;
    }
}

fn validate_ratio(ratio: f64) -> Result<()> {
    if ratio.is_finite() && (0.0..=1.0).contains(&ratio) {
        Ok(())
    } else {
        Err(Error::InvalidArgument("the concave hull ratio must be between 0 and 1".into()))
    }
}
fn threshold(mesh: &Mesh, ratio: f64, eligible: impl Fn(usize, usize) -> bool) -> f64 {
    if ratio == 0.0 {
        return 0.0;
    }
    let (mut lo, mut hi) = (f64::INFINITY, 0.0_f64);
    for t in 0..mesh.triangles.len() {
        for i in 0..3 {
            if eligible(t, i) {
                let d = mesh.length(t, i);
                lo = lo.min(d);
                hi = hi.max(d);
            }
        }
    }
    if ratio == 1.0 { 2.0 * hi } else { lo + ratio * (hi - lo) }
}

/// A connected concave hull of the input sites, using JTS's edge-length ratio.
/// Holes may be eroded when `allow_holes` is true. Empty input gives POLYGON EMPTY.
pub fn concave_hull(g: &Geometry, ratio: f64, allow_holes: bool) -> Result<Geometry> {
    validate_ratio(ratio)?;
    let coords = vertices(g)?;
    if coords.is_empty() {
        return Ok(Geometry::Polygon { exterior: Ring(Vec::new()), interiors: Vec::new() });
    }
    let faces = super::jts_delaunay::triangles(&coords)?;
    if faces.is_empty() {
        return convex_hull(g);
    }
    let mut mesh = Mesh::new(coords, faces)?;
    let target = threshold(&mesh, ratio, |_, _| true);
    let mut queue = BinaryHeap::new();
    for t in 0..mesh.triangles.len() {
        mesh.queue_border(t, &mut queue);
    }
    mesh.erode_points(&mut queue, target, false, None);
    if allow_holes {
        let mut seeds: Vec<Candidate> = (0..mesh.triangles.len())
            .filter(|&t| mesh.triangles[t].alive && mesh.degree(t) == 3 && !mesh.boundary_touch(t))
            .map(|t| Candidate { size: mesh.longest(t), area: mesh.area(t), id: t })
            .filter(|c| c.size >= target)
            .collect();
        sort_hole_candidates(&mut seeds);
        for c in seeds {
            if !mesh.triangles[c.id].alive || mesh.degree(c.id) != 3 || mesh.boundary_touch(c.id) {
                continue;
            }
            queue.push(c);
            mesh.erode_points(&mut queue, target, true, Some(c.id));
        }
    }
    mesh.geometry(&mesh.triangles.iter().map(|t| t.alive).collect::<Vec<_>>())
}

/// JTS ConcaveHullOfPolygons: erode the triangulated space between polygon shells,
/// then join the retained fill with the original polygons. Original holes remain holes.
/// Input should contain valid, non-overlapping polygons. Mixed degenerate shells
/// can fail triangulation; inconsistent mesh edges return an argument error.
/// Invalid inputs have no equivalence or repair guarantee.
pub fn concave_hull_of_polygons(g: &Geometry, ratio: f64, tight: bool, allow_holes: bool) -> Result<Geometry> {
    validate_ratio(ratio)?;
    let mut coords = vertices(g)?;
    let polygons: Vec<(&Ring, &[Ring])> = match g {
        Geometry::Polygon { exterior, interiors } => vec![(exterior, interiors.as_slice())],
        Geometry::MultiPolygon(ps) => ps.iter().map(|(r, h)| (r, h.as_slice())).collect(),
        _ => return Err(Error::InvalidArgument("the polygon hull requires polygonal input".into())),
    };
    let ring_area = |r: &Ring| signed_area(&r.0.iter().map(xy).collect::<Vec<_>>()).abs();
    let area = polygons.iter().map(|(shell, holes)| ring_area(shell) - holes.iter().map(ring_area).sum::<f64>()).sum::<f64>();
    if coords.is_empty() || area == 0.0 {
        return Ok(Geometry::Polygon { exterior: Ring(Vec::new()), interiors: Vec::new() });
    }
    let input_count = coords.len();
    let bounds = coords.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |b, c| {
        (b.0.min(c.x), b.1.min(c.y), b.2.max(c.x), b.3.max(c.y))
    });
    let pad = 4.0 * dist((bounds.0, bounds.1), (bounds.2, bounds.3));
    if pad == 0.0 {
        return Err(Error::InvalidArgument("polygon hull requires a non-degenerate extent".into()));
    }
    let frame = [
        (bounds.0 - pad, bounds.1 - pad),
        (bounds.0 - pad, bounds.3 + pad),
        (bounds.2 + pad, bounds.3 + pad),
        (bounds.2 + pad, bounds.1 - pad),
    ];
    if frame.iter().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
        return Err(Error::InvalidArgument("polygon hull frame exceeds the coordinate range".into()));
    }
    coords.extend(frame.iter().map(|&(x, y)| Coord::xy(x, y)));
    let ids: HashMap<_, _> = coords.iter().enumerate().map(|(i, c)| (coord_key(c), i)).collect();
    let ring_ids = |r: &Ring| r.0.iter().map(|c| ids[&coord_key(c)]).collect::<Vec<_>>();
    // A contact vertex may belong to several shells. Keep sorted memberships
    // rather than scanning every shell for each tight-erosion candidate.
    let mut shell_membership = if tight { vec![Vec::new(); coords.len()] } else { Vec::new() };
    if tight {
        for (shell, (ring, _)) in polygons.iter().enumerate() {
            for vertex in ring_ids(ring) {
                if shell_membership[vertex].last() != Some(&shell) {
                    shell_membership[vertex].push(shell);
                }
            }
        }
    }
    let shell_bounds = polygons.iter().map(|(r, _)| ring_bounds(r)).collect::<Vec<_>>();
    let shell_index = RTree::bulk_load(
        shell_bounds
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b.map(|b| GeomWithData::new(Rectangle::from_corners(b.lower(), b.upper()), i)))
            .collect(),
    );
    // Nested islands are already excluded by the enclosing shell in the frame.
    // Retain them in the original geometry, without adding a nested frame hole.
    let frame_holes = polygons
        .iter()
        .enumerate()
        .filter(|(i, (ring, _))| {
            let Some(bounds) = shell_bounds[*i] else { return false };
            !shell_index.locate_in_envelope_intersecting(bounds).any(|entry| {
                entry.data != *i
                    && shell_bounds[entry.data].is_some_and(|outer| outer.contains_envelope(&bounds))
                    && ring_nested(ring, polygons[entry.data].0)
            })
        })
        .map(|(_, (r, _))| ring_ids(r))
        .filter(|r| !r.is_empty())
        .collect::<Vec<_>>();
    let triangulator = super::jts_cdt::Triangulator::new(&coords);
    let mut faces = triangulator.triangulate((input_count..input_count + 4).collect(), frame_holes)?;
    let frame_face_count = faces.len();
    for (shell, holes) in &polygons {
        if !shell.0.is_empty() {
            faces.extend(triangulator.triangulate(ring_ids(shell), holes.iter().map(ring_ids).collect())?);
        }
    }
    let mut mesh = Mesh::new(coords, faces)?;
    let input = (0..mesh.triangles.len()).map(|t| t >= frame_face_count).collect::<Vec<_>>();
    let fill = mesh
        .triangles
        .iter()
        .enumerate()
        .map(|(t, tri)| t < frame_face_count && tri.vertices.iter().all(|&v| v < input_count))
        .collect::<Vec<_>>();
    for (t, original) in input.iter().enumerate() {
        if *original {
            mesh.remove(t);
        }
    }
    let target = threshold(&mesh, ratio, |t, i| fill[t] && mesh.triangles[t].adjacent[i].is_some());
    let mut queue = Vec::new();
    let mut border = vec![None; mesh.triangles.len()];
    // Preserve the JTS construction order of the frame triangles. Observed
    // GEOS erosion processes the resulting border as a stack and retains the
    // first exposed edge of each triangle.
    for t in 0..frame_face_count {
        let Some(corner) = mesh.triangles[t].vertices.iter().position(|&v| v >= input_count) else {
            continue;
        };
        let opposite = (corner + 1) % 3;
        if let Some(n) = mesh.triangles[t].adjacent[opposite]
            && fill[n]
            && let Some(i) = mesh.triangles[n].adjacent.iter().position(|v| *v == Some(t))
        {
            border[n].get_or_insert(i);
            queue.push(n);
        }
        mesh.remove(t);
    }
    let mut seed_from = 0;
    loop {
        while let Some(t) = queue.pop() {
            if !mesh.triangles[t].alive {
                continue;
            }
            let vertices = mesh.triangles[t].vertices;
            let same_shell = tight
                && shell_membership[vertices[0]]
                    .iter()
                    .any(|shell| vertices[1..].iter().all(|&v| shell_membership[v].binary_search(shell).is_ok()));
            if !same_shell && !border[t].is_some_and(|i| mesh.length(t, i) > target) {
                continue;
            }
            for n in mesh.triangles[t].adjacent.into_iter().flatten() {
                if let Some(i) = mesh.triangles[n].adjacent.iter().position(|v| *v == Some(t)) {
                    border[n].get_or_insert(i);
                    queue.push(n);
                }
            }
            mesh.remove(t);
        }
        if !allow_holes {
            break;
        }
        // Degrees only decrease and edge lengths are fixed, so a rejected
        // seed cannot become eligible later. Never rescan a discarded prefix.
        let seed = (seed_from..mesh.triangles.len())
            .find(|&t| mesh.triangles[t].alive && mesh.degree(t) == 3 && (0..3).any(|i| mesh.length(t, i) > target));
        let Some(t) = seed else {
            break;
        };
        seed_from = t + 1;
        for n in mesh.triangles[t].adjacent.into_iter().flatten() {
            if let Some(i) = mesh.triangles[n].adjacent.iter().position(|v| *v == Some(t)) {
                border[n].get_or_insert(i);
                queue.push(n);
            }
        }
        mesh.remove(t);
    }
    let keep = input.iter().zip(&mesh.triangles).map(|(a, t)| *a || t.alive).collect::<Vec<_>>();
    mesh.geometry(&keep)
}

fn signed_area(r: &[P]) -> f64 {
    if r.len() < 3 {
        return 0.0;
    }
    let origin = r[0];
    (0..r.len())
        .map(|i| {
            let a = r[i];
            let b = r[(i + 1) % r.len()];
            (a.0 - origin.0) * (b.1 - origin.1) - (b.0 - origin.0) * (a.1 - origin.1)
        })
        .sum::<f64>()
        / 2.0
}
fn ring_bounds(ring: &Ring) -> Option<AABB<[f64; 2]>> {
    let first = ring.0.first()?;
    let (lo, hi) = ring
        .0
        .iter()
        .fold(([first.x, first.y], [first.x, first.y]), |(lo, hi), c| ([lo[0].min(c.x), lo[1].min(c.y)], [hi[0].max(c.x), hi[1].max(c.y)]));
    Some(AABB::from_corners(lo, hi))
}

/// Executable GEOS frame selection includes a covered first vertex, even at
/// a point contact. Preserve this ring-start-sensitive compatibility behavior.
fn ring_nested(ring: &Ring, outer: &Ring) -> bool {
    let Some(c) = ring.0.first() else { return false };
    let p = xy(c);
    in_ring(p, &outer.0)
        || (0..outer.0.len()).any(|i| {
            let (a, b) = (xy(&outer.0[i]), xy(&outer.0[(i + 1) % outer.0.len()]));
            orient(a, b, p) == 0 && p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1)
        })
}

fn in_ring(p: P, ring: &[Coord]) -> bool {
    let mut inside = false;
    for i in 0..ring.len() {
        let (a, b) = (xy(&ring[i]), xy(&ring[(i + 1) % ring.len()]));
        if (a.1 > p.1) != (b.1 > p.1) && orient(a, b, p) * if b.1 > a.1 { 1 } else { -1 } > 0 {
            inside = !inside;
        }
    }
    inside
}

/// Cancels interior triangle edges and traces the remaining half-edges with the
/// selected region on their left. At point contacts, take the next clockwise edge
/// from the incoming edge's reverse; this separates rings without recursion.
fn boundary_geometry(coords: &[Coord], faces: &[[usize; 3]]) -> Result<Geometry> {
    let mut edges = HashSet::new();
    for face in faces {
        let mut v = *face;
        if orient(xy(&coords[v[0]]), xy(&coords[v[1]]), xy(&coords[v[2]])) < 0 {
            v.swap(1, 2);
        }
        for i in 0..3 {
            let edge = (v[i], v[(i + 1) % 3]);
            if !edges.remove(&(edge.1, edge.0)) {
                edges.insert(edge);
            }
        }
    }
    let mut outgoing: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(a, b) in &edges {
        outgoing.entry(a).or_default().push(b);
    }
    // Preserve absolute-angle ordering when turn keys compare equal below.
    for (&a, bs) in &mut outgoing {
        bs.sort_by(|&b, &c| {
            let p = xy(&coords[a]);
            let q = xy(&coords[b]);
            let r = xy(&coords[c]);
            (q.1 - p.1).atan2(q.0 - p.0).total_cmp(&(r.1 - p.1).atan2(r.0 - p.0))
        });
    }
    let mut starts = edges.iter().copied().collect::<Vec<_>>();
    starts.sort_unstable();
    let mut loops = Vec::new();
    for start in starts {
        if !edges.contains(&start) {
            continue;
        }
        let mut ring = vec![start.0];
        let mut edge = start;
        while edges.remove(&edge) {
            ring.push(edge.1);
            if edge.1 == start.0 {
                break;
            }
            let a = xy(&coords[edge.0]);
            let b = xy(&coords[edge.1]);
            let reverse = (a.1 - b.1).atan2(a.0 - b.0);
            let Some(options) = outgoing.get(&edge.1) else {
                break;
            };
            let next = options.iter().copied().filter(|&c| edges.contains(&(edge.1, c))).min_by(|&c, &d| {
                let turn = |i: usize| {
                    let p = xy(&coords[i]);
                    (reverse - (p.1 - b.1).atan2(p.0 - b.0)).rem_euclid(std::f64::consts::TAU)
                };
                turn(c).total_cmp(&turn(d))
            });
            let Some(c) = next else {
                break;
            };
            edge = (edge.1, c);
        }
        if ring.first() != ring.last() {
            return Err(Error::InvalidData("hull boundary walk did not close".into()));
        }
        if ring.len() >= 4 {
            // A region may meet its own hole at a point away from the walk's
            // start. Split every closed subcycle at repeated vertex IDs so
            // the shell and hole remain separate simple rings.
            let mut path = Vec::new();
            let mut at = HashMap::new();
            for vertex in ring {
                if let Some(&start) = at.get(&vertex) {
                    let mut cycle = path[start..].to_vec();
                    cycle.push(vertex);
                    if cycle.len() >= 4 {
                        loops.push(cycle);
                    }
                    for removed in path.drain(start + 1..) {
                        at.remove(&removed);
                    }
                } else {
                    at.insert(vertex, path.len());
                    path.push(vertex);
                }
            }
        }
    }
    let mut shells: Vec<(Ring, Vec<Ring>)> = Vec::new();
    let mut shell_areas = Vec::new();
    let mut holes = Vec::new();
    for ids in loops {
        let r = ids.iter().map(|&i| xy(&coords[i])).collect::<Vec<_>>();
        let ring = open_ring(ids.iter().map(|&i| coords[i].clone()).collect());
        let area = signed_area(&r);
        if area > 0.0 {
            shells.push((ring, Vec::new()));
            shell_areas.push(area);
        } else {
            holes.push(ring);
        }
    }
    if !holes.is_empty() {
        let shell_bounds = shells.iter().map(|(ring, _)| ring_bounds(ring)).collect::<Vec<_>>();
        let shell_index = RTree::bulk_load(
            shell_bounds
                .iter()
                .enumerate()
                .filter_map(|(i, bounds)| bounds.map(|b| GeomWithData::new(Rectangle::from_corners(b.lower(), b.upper()), i)))
                .collect(),
        );
        for hole in holes {
            let Some(bounds) = ring_bounds(&hole) else { continue };
            // A point contact can make the parity test accept a vertex on a
            // smaller island's boundary. An owner must contain the whole hole
            // envelope, rather than merely intersecting it at that contact.
            let mut candidates = shell_index
                .locate_in_envelope_intersecting(bounds)
                .filter(|entry| shell_bounds[entry.data].is_some_and(|outer| outer.contains_envelope(&bounds)))
                .map(|entry| entry.data)
                .collect::<Vec<_>>();
            // Restore shell order so equal-area owners keep the original tie.
            candidates.sort_unstable();
            let owner = candidates
                .into_iter()
                .filter(|&i| hole.0.iter().any(|c| in_ring(xy(c), &shells[i].0.0)))
                .min_by(|&a, &b| shell_areas[a].total_cmp(&shell_areas[b]));
            if let Some(i) = owner {
                shells[i].1.push(hole);
            }
        }
    }
    Ok(if shells.len() == 1 {
        let (exterior, interiors) = shells.remove(0);
        Geometry::Polygon { exterior, interiors }
    } else if shells.is_empty() {
        Geometry::Polygon { exterior: Ring(Vec::new()), interiors: Vec::new() }
    } else {
        Geometry::MultiPolygon(shells)
    })
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
    fn empty_shell_with_a_nonempty_hole_has_an_empty_hull() {
        let input = Geometry::Polygon {
            exterior: Ring(Vec::new()),
            interiors: vec![Ring([(0., 0.), (4., 0.), (4., 4.), (0., 4.)].map(|(x, y)| Coord::xy(x, y)).to_vec())],
        };
        for ratio in [0., 0.3, 1.] {
            for tight in [false, true] {
                for holes in [false, true] {
                    let result = concave_hull_of_polygons(&input, ratio, tight, holes).unwrap();
                    assert!(matches!(result, Geometry::Polygon { exterior, interiors } if exterior.0.is_empty() && interiors.is_empty()));
                }
            }
        }
    }

    #[test]
    fn zero_area_polygon_hulls_are_empty() {
        // GEOS 3.14.1 returns POLYGON EMPTY at both zero and nonzero extent.
        for pts in [
            vec![(1., 1.); 4],
            vec![(0., 0.), (1., 1.), (2., 2.), (0., 0.)],
            vec![(0., 0.), (2., 2.), (0., 2.), (2., 0.), (0., 0.)],
            vec![(0., 0.), (1e-200, 0.), (0., 1e-200)],
        ] {
            let input = Geometry::polygon(pts.iter().map(|&(x, y)| Coord::xy(x, y)).collect(), vec![]);
            for tight in [false, true] {
                for holes in [false, true] {
                    let actual = concave_hull_of_polygons(&input, 0.3, tight, holes).unwrap();
                    assert!(matches!(actual, Geometry::Polygon { exterior, interiors } if exterior.0.is_empty() && interiors.is_empty()));
                }
            }
        }
    }

    #[test]
    fn polygon_hull_preserves_a_hole_pinched_at_a_nonstart_vertex() {
        let coords = |pts: &[(f64, f64)]| pts.iter().map(|&(x, y)| Coord::xy(x, y)).collect();
        let input = Geometry::polygon(
            coords(&[(0., 0.), (10., 0.), (10., 10.), (0., 10.), (0., 5.)]),
            vec![coords(&[(0., 5.), (3., 3.), (3., 7.)])],
        );
        let result = concave_hull_of_polygons(&input, 0.3, true, true).unwrap();
        let Geometry::Polygon { exterior, interiors } = result else { panic!("{result:?}") };
        assert_eq!(exterior.0.len(), 5);
        assert_eq!(interiors.len(), 1);
        assert_eq!(interiors[0].0.len(), 3);
        assert_eq!(signed_area(&exterior.0.iter().map(xy).collect::<Vec<_>>()).abs(), 100.);
        assert_eq!(signed_area(&interiors[0].0.iter().map(xy).collect::<Vec<_>>()).abs(), 6.);
    }

    #[test]
    fn polygon_hull_keeps_components_touching_at_a_concave_vertex() {
        let coords = |ps: &[(f64, f64)]| Ring(ps.iter().map(|&(x, y)| Coord::xy(x, y)).collect());
        let l_shape = coords(&[(0., 0.), (6., 0.), (6., 2.), (2., 2.), (2., 6.), (0., 6.)]);
        let triangle = coords(&[(2., 2.), (4., 3.), (3., 4.)]);
        for reverse in [false, true] {
            let mut input = vec![(l_shape.clone(), vec![]), (triangle.clone(), vec![])];
            if reverse {
                input.reverse();
            }
            for holes in [false, true] {
                let result = concave_hull_of_polygons(&Geometry::MultiPolygon(input.clone()), 0.3, true, holes).unwrap();
                let Geometry::MultiPolygon(polygons) = result else { panic!("{result:?}") };
                assert_eq!(polygons.len(), 2);
                for (shell, holes) in &input {
                    let wanted = shell.0.iter().map(coord_key).collect::<HashSet<_>>();
                    assert!(polygons.iter().any(|(r, h)| h == holes && r.0.iter().map(coord_key).collect::<HashSet<_>>() == wanted));
                }
            }
        }
    }

    #[test]
    fn hole_seed_ties_match_the_recorded_sort_observation() {
        // Linux std::sort executable observation, Debian g++ 14.2.0-19.
        // Equal sizes tie even when areas differ. The seed sort compares size
        // only; the erosion heap separately uses size and area.
        for unequal_areas in [false, true] {
            let mut candidates =
                (0..20).map(|id| Candidate { id, size: 1., area: if unequal_areas { (20 - id) as f64 } else { 1. } }).collect::<Vec<_>>();
            sort_hole_candidates(&mut candidates);
            assert_eq!(
                candidates.iter().map(|c| c.id).collect::<Vec<_>>(),
                [10, 19, 18, 17, 16, 15, 14, 13, 12, 11, 0, 9, 8, 7, 6, 5, 4, 3, 2, 1]
            );
        }
    }

    #[test]
    fn hole_seed_sort_preserves_candidates_and_size_order() {
        let mut state = 0x4A54_5353_u64;
        for len in [0, 1, 2, 15, 16, 17, 20, 31, 64, 127, 200, 513] {
            for pattern in 0..5 {
                let mut candidates = (0..len)
                    .map(|id| {
                        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                        let size = match pattern {
                            0 => id as f64,
                            1 => (len - id) as f64,
                            2 => 1.,
                            3 => (state % 7) as f64,
                            _ => (state >> 11) as f64,
                        };
                        Candidate { id, size, area: (id % 11) as f64 }
                    })
                    .collect::<Vec<_>>();
                let records = |items: &[Candidate]| {
                    let mut keys = items.iter().map(|c| (c.id, c.size.to_bits(), c.area.to_bits())).collect::<Vec<_>>();
                    keys.sort_unstable();
                    keys
                };
                let expected = records(&candidates);
                sort_hole_candidates(&mut candidates);
                assert_eq!(records(&candidates), expected, "length {len}, pattern {pattern}: permutation");
                assert!(candidates.windows(2).all(|pair| pair[0].size <= pair[1].size), "length {len}, pattern {pattern}: size order");
            }
        }
    }

    #[test]
    fn unsupported_polygon_hull_inputs_fail_explicitly() {
        let ring = |p: &[(f64, f64)]| Ring(p.iter().map(|&(x, y)| Coord::xy(x, y)).collect());
        let square = ring(&[(4., 0.), (4., 4.), (0., 4.), (0., 0.), (4., 0.)]);
        // Captured GEOS 3.14.1 observations retain invalid parts for the first
        // two cases and reject crossing squares. Native invalid-input behavior
        // is explicitly outside parity guarantees; these cases fail boundedly.
        for part in
            [ring(&[(0., 0.), (1., 1.), (0., 0.), (0., 0.)]), square.clone(), ring(&[(6., 2.), (6., 6.), (2., 6.), (2., 2.), (6., 2.)])]
        {
            for tight in [false, true] {
                for holes in [false, true] {
                    let input = Geometry::MultiPolygon(vec![(square.clone(), vec![]), (part.clone(), vec![])]);
                    assert!(matches!(concave_hull_of_polygons(&input, 0.3, tight, holes), Err(Error::InvalidArgument(_))));
                }
            }
        }
    }

    #[test]
    fn boundary_holes_stay_with_their_disconnected_shells() {
        let mut coords = Vec::new();
        let mut faces = Vec::new();
        for i in 0..1024 {
            let (x, y) = ((i % 32) as f64 * 8., (i / 32) as f64 * 8.);
            let base = coords.len();
            coords.extend(
                [(0., 0.), (4., 0.), (4., 4.), (0., 4.), (1., 1.), (3., 1.), (3., 3.), (1., 3.)].map(|(a, b)| Coord::xy(x + a, y + b)),
            );
            faces.extend(
                [[0, 1, 5], [0, 5, 4], [1, 2, 6], [1, 6, 5], [2, 3, 7], [2, 7, 6], [3, 0, 4], [3, 4, 7]].map(|f| f.map(|v| v + base)),
            );
        }
        let Geometry::MultiPolygon(parts) = boundary_geometry(&coords, &faces).unwrap() else { panic!("expected disconnected polygons") };
        assert_eq!(parts.len(), 1024);
        for (shell, holes) in parts {
            assert_eq!(holes.len(), 1);
            let bounds = ring_bounds(&shell).unwrap();
            let inner = ring_bounds(&holes[0]).unwrap();
            assert_eq!(inner.lower(), bounds.lower().map(|v| v + 1.));
            assert_eq!(inner.upper(), bounds.upper().map(|v| v - 1.));
        }
    }

    #[test]
    fn repeated_mesh_edges_are_rejected() {
        let coords = [(0., 0.), (4., 0.), (0., 4.), (1., 2.), (0., -4.)].iter().map(|&(x, y)| Coord::xy(x, y)).collect::<Vec<_>>();
        assert!(Mesh::new(coords.clone(), vec![[0, 1, 2], [1, 0, 4]]).is_ok());
        for faces in [vec![[0, 1, 2], [0, 1, 3]], vec![[0, 1, 2], [1, 0, 4], [0, 1, 3]], vec![[0, 1, 2], [1, 0, 4], [1, 0, 3]]] {
            assert!(matches!(Mesh::new(coords.clone(), faces), Err(Error::InvalidArgument(_))));
        }
    }

    #[test]
    fn malformed_mesh_boundary_returns_an_error() {
        // Overlapping faces share a directed edge, leaving an unbalanced
        // boundary after duplicate edges collapse. No component may vanish.
        let coords = [(0., 0.), (4., 0.), (0., 4.), (1., 2.)].iter().map(|&(x, y)| Coord::xy(x, y)).collect::<Vec<_>>();
        assert!(matches!(boundary_geometry(&coords, &[[0, 1, 2], [0, 1, 3]]), Err(Error::InvalidData(_))));
    }

    #[test]
    fn traces_regions_touching_at_one_vertex_separately() {
        let coords = [(0., 0.), (2., 0.), (1., 2.), (4., 0.), (3., 2.)].iter().map(|&(x, y)| Coord::xy(x, y)).collect::<Vec<_>>();
        let g = boundary_geometry(&coords, &[[0, 1, 2], [1, 3, 4]]).unwrap();
        let Geometry::MultiPolygon(ps) = g else { panic!("{g:?}") };
        assert_eq!(ps.len(), 2);
        assert!(ps.iter().all(|(e, h)| e.0.len() == 3 && h.is_empty()));
    }

    #[test]
    fn two_site_convex_hulls_retain_input_order() {
        let input = Geometry::LineString(vec![Coord::xy(0., 10.), Coord::xy(10., 0.)]);
        assert_eq!(convex_hull(&input).unwrap(), input);
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
    fn polygon_hull_keeps_a_touching_islands_hole_on_the_outer_shell() {
        let ring = |points: &[(f64, f64)]| Ring(points.iter().map(|&(x, y)| Coord::xy(x, y)).collect());
        let outer = ring(&[(0., 0.), (10., 0.), (10., 10.), (0., 10.)]);
        let hole = ring(&[(2., 2.), (8., 1.), (9., 9.), (1., 8.)]);
        let island = ring(&[(2., 2.), (4., 2.), (4., 4.), (2., 4.)]);
        let input = Geometry::MultiPolygon(vec![(outer, vec![hole]), (island, vec![])]);
        // Executable GEOS 3.14.1 preserves the outer hole and the island.
        // The island touches the hole at (2,2), without sharing an edge.
        for tight in [false, true] {
            let result = concave_hull_of_polygons(&input, 0., tight, true).unwrap();
            let Geometry::MultiPolygon(polygons) = &result else { panic!("{result:?}") };
            assert_eq!(polygons.len(), 2);
            let (shell, holes) = polygons.iter().find(|(_, holes)| !holes.is_empty()).unwrap();
            assert_eq!(ring_bounds(shell), Some(AABB::from_corners([0., 0.], [10., 10.])));
            assert_eq!(holes.len(), 1);
            assert_eq!(ring_bounds(&holes[0]), Some(AABB::from_corners([1., 1.], [9., 9.])));
            let mut coordinates = holes[0].0.iter().map(xy).collect::<Vec<_>>();
            assert_eq!(coordinates.len(), 4);
            let start = coordinates.iter().position(|&p| p == (2., 2.)).unwrap();
            coordinates.rotate_left(start);
            if coordinates[1] != (8., 1.) {
                coordinates[1..].reverse();
            }
            assert_eq!(coordinates, [(2., 2.), (8., 1.), (9., 9.), (1., 8.)]);
        }
    }

    #[test]
    fn polygon_hull_fills_gap_enclosed_by_touching_shells() {
        let square = |x, y| (Ring(vec![Coord::xy(x, y), Coord::xy(x + 4., y), Coord::xy(x + 4., y + 4.), Coord::xy(x, y + 4.)]), vec![]);
        let input = Geometry::MultiPolygon(vec![square(0., 0.), square(4., 4.), square(8., 0.), square(4., -4.)]);
        // GEOS 3.14 fills the central gap for all four option combinations at ratio 1.
        for tight in [false, true] {
            for holes in [false, true] {
                let hull = concave_hull_of_polygons(&input, 1., tight, holes).unwrap();
                let Geometry::Polygon { exterior, interiors } = hull else { panic!("{hull:?}") };
                assert!(interiors.is_empty());
                let ring: Vec<P> = exterior.0.iter().map(|c| (c.x, c.y)).collect();
                assert_eq!(signed_area(&ring).abs(), 112.);
            }
        }
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
