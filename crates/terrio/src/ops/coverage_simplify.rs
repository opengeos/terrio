// SPDX-FileCopyrightText: Copyright (c) 2022 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   JTS (commit 3460207216f3091cd7f72d48d6ddd1b561b2b8fd):
//     modules/core/src/main/java/org/locationtech/jts/coverage/CoverageSimplifier.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/CoverageRingEdges.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/CoverageEdge.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/CoverageBoundarySegmentFinder.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/VertexRingCounter.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/TPVWSimplifier.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/Corner.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/CornerArea.java
//     modules/core/src/main/java/org/locationtech/jts/simplify/LinkedLine.java
//     modules/core/src/main/java/org/locationtech/jts/geom/Triangle.java

//! Topology-preserving Visvalingam-Whyatt simplification of shared coverage edges.
//! Reference: GDAL 3.13.3 with GEOS 3.14.1.
use crate::{Error, Feature, Geometry, Layer, Result};
use geo::algorithm::kernels::{Kernel, Orientation, RobustKernel};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use wbvector::{Coord, Ring};

type PointKey = [u64; 2];
type EdgeKey = [PointKey; 2];
const ABSENT: usize = usize::MAX;

/// Coverage simplification options; tolerances are in the input CRS units.
#[derive(Debug, Clone, Default)]
pub struct CoverageSimplifyOptions {
    /// Square root of the triangle-area threshold used by Visvalingam-Whyatt.
    pub tolerance: f64,
    /// Simplify shared internal edges only, retaining exterior/hole boundaries.
    pub preserve_boundary: bool,
}

/// Simplifies a polygon coverage while preserving shared-edge topology and nodes.
/// Inputs must be individually valid Polygon/MultiPolygon geometries forming a
/// valid coverage; use coverage validation first. Attributes, layer name and CRS
/// are retained, with output IDs sequential from 1. Polygon parts and holes are
/// retained, matching GDAL/GEOS; rings can simplify to three distinct vertices.
/// Retained vertices keep Z; measures are dropped, matching GEOS.
/// This operation materializes the coverage and does not repair invalid coverages.
pub fn simplify_coverage(layer: &Layer, options: &CoverageSimplifyOptions) -> Result<Layer> {
    if !options.tolerance.is_finite() || options.tolerance < 0.0 {
        return Err(Error::InvalidArgument("coverage tolerance must be finite and non-negative".into()));
    }
    let mut rings = Vec::new();
    let mut features = Vec::new();
    for feature in &layer.features {
        let geometry = feature.geometry.as_ref().ok_or_else(|| {
            Error::InvalidArgument(format!("feature {}: coverage simplification requires polygonal geometry, not null", feature.fid))
        })?;
        if !matches!(geometry, Geometry::Polygon { .. } | Geometry::MultiPolygon(_)) {
            return Err(Error::InvalidArgument(format!("feature {}: coverage simplification requires polygonal geometry", feature.fid)));
        }
        super::topology::require_valid(geometry, feature.fid)?;
        let parts = match geometry {
            Geometry::Polygon { exterior, interiors } => vec![(exterior.clone(), interiors.clone())],
            Geometry::MultiPolygon(parts) => parts.clone(),
            _ => unreachable!(),
        };
        let mut feature_rings = Vec::new();
        for (shell, holes) in parts {
            let shell = add_ring(shell, &mut rings);
            let holes: Vec<usize> = holes.into_iter().map(|r| add_ring(r, &mut rings)).collect();
            feature_rings.push((shell, holes));
        }
        features.push((matches!(geometry, Geometry::MultiPolygon(_)), feature_rings));
    }
    let mut graph = Graph::new(rings);
    graph.simplify(options);
    let mut out = super::geometry::header(layer, layer.geom_type);
    for (feature, (multi, parts)) in layer.features.iter().zip(features) {
        let mut result = Vec::new();
        for (shell, holes) in parts {
            let exterior = graph.ring(shell);
            let interiors = holes.into_iter().map(|h| graph.ring(h)).collect();
            result.push((exterior, interiors));
        }
        let mut geometry = if !multi {
            let (exterior, interiors) = result.into_iter().next().unwrap_or_default();
            Geometry::Polygon { exterior, interiors }
        } else {
            Geometry::MultiPolygon(result)
        };
        // GEOS coverage simplification carries elevation but does not propagate measures.
        crate::vector::map_coords(&mut geometry, &mut |c| {
            c.m = None;
            Ok(())
        })?;
        let mut attributes = feature.attributes.clone();
        attributes.resize(layer.schema.len(), crate::FieldValue::Null);
        out.features.push(Feature { fid: out.features.len() as u64 + 1, geometry: Some(geometry), attributes });
    }
    Ok(out)
}

/// Applies coverage simplification to selected layers, copying inactive layers.
/// An unknown active-layer name is an error. Output IDs are sequential in every
/// layer, matching GDAL's default writer behavior, including inactive layers.
pub fn simplify_coverage_layers(layers: &[Layer], options: &CoverageSimplifyOptions, active_layer: Option<&str>) -> Result<Vec<Layer>> {
    if let Some(name) = active_layer
        && !layers.iter().any(|layer| layer.name == name)
    {
        return Err(Error::InvalidArgument(format!("no active layer named '{name}'")));
    }
    layers
        .iter()
        .map(|layer| {
            if active_layer.is_none_or(|name| name == layer.name) {
                simplify_coverage(layer, options)
            } else {
                let mut out = layer.clone();
                for (i, feature) in out.features.iter_mut().enumerate() {
                    feature.fid = i as u64 + 1;
                }
                Ok(out)
            }
        })
        .collect()
}

fn point(c: &Coord) -> geo::Coord<f64> {
    geo::coord! {x:c.x,y:c.y}
}
fn cmp(a: &Coord, b: &Coord) -> Ordering {
    a.x.partial_cmp(&b.x).unwrap().then(a.y.partial_cmp(&b.y).unwrap())
}
fn same(a: &Coord, b: &Coord) -> bool {
    a.x == b.x && a.y == b.y
}
fn key(c: &Coord) -> PointKey {
    [if c.x == 0.0 { 0 } else { c.x.to_bits() }, if c.y == 0.0 { 0 } else { c.y.to_bits() }]
}
fn segment(a: &Coord, b: &Coord) -> EdgeKey {
    if cmp(a, b).is_gt() { [key(b), key(a)] } else { [key(a), key(b)] }
}
struct CovRing {
    original: Ring,
    points: Vec<Coord>,
    edges: Vec<usize>,
}
impl CovRing {
    fn source_points(&self) -> &[Coord] {
        let points = &self.original.0;
        if points.len() > 1 && same(&points[0], points.last().unwrap()) { &points[..points.len() - 1] } else { points }
    }
}
fn add_ring(original: Ring, rings: &mut Vec<CovRing>) -> usize {
    let mut points = original.0.clone();
    points.dedup_by(|a, b| same(a, b));
    if !points.is_empty() && !same(&points[0], points.last().unwrap()) {
        points.push(points[0].clone());
    }
    let index = rings.len();
    rings.push(CovRing { original, points, edges: vec![] });
    index
}
struct Edge {
    points: Vec<Coord>,
    count: usize,
    free: bool,
    ring: bool,
    alive: Vec<bool>,
    prev: Vec<usize>,
    next: Vec<usize>,
    size: usize,
    tolerance: f64,
}
impl Edge {
    fn new(points: Vec<Coord>, free: bool) -> Self {
        let ring = same(&points[0], points.last().unwrap());
        let size = points.len() - usize::from(ring);
        let mut prev: Vec<_> = (0..size).map(|i| if i == 0 { if ring { size - 1 } else { ABSENT } } else { i - 1 }).collect();
        let mut next: Vec<_> = (0..size).map(|i| if i + 1 == size { if ring { 0 } else { ABSENT } } else { i + 1 }).collect();
        // Keep endpoint sentinels explicit even for a two-point open edge.
        if !ring {
            prev[0] = ABSENT;
            next[size - 1] = ABSENT;
        }
        Self { points, count: 1, free, ring, alive: vec![true; size], prev, next, size, tolerance: 0.0 }
    }
    fn coords(&self) -> Vec<Coord> {
        let mut out: Vec<_> =
            self.points.iter().take(self.alive.len()).zip(&self.alive).filter_map(|(p, &a)| a.then_some(p.clone())).collect();
        if self.ring && !out.is_empty() {
            out.push(out[0].clone());
        }
        out
    }
    fn corner(&self, i: usize) -> Option<Corner> {
        if !self.alive[i] || (!self.free && (i == 0 || i == self.points.len() - 1)) {
            return None;
        }
        let (prev, next) = (self.prev[i], self.next[i]);
        if prev == ABSENT || next == ABSENT {
            return None;
        }
        let (a, b, c) = (&self.points[prev], &self.points[i], &self.points[next]);
        let area = ((c.x - a.x) * (b.y - a.y) - (b.x - a.x) * (c.y - a.y)).abs() / 2.;
        (area <= self.tolerance * self.tolerance).then_some(Corner { index: i, prev, next, area })
    }
}
#[derive(Clone, Copy)]
struct Corner {
    index: usize,
    prev: usize,
    next: usize,
    area: f64,
}
impl PartialEq for Corner {
    fn eq(&self, b: &Self) -> bool {
        self.cmp(b) == Ordering::Equal
    }
}
impl Eq for Corner {}
impl PartialOrd for Corner {
    fn partial_cmp(&self, b: &Self) -> Option<Ordering> {
        Some(self.cmp(b))
    }
}
impl Ord for Corner {
    fn cmp(&self, b: &Self) -> Ordering {
        self.area.total_cmp(&b.area).then(self.index.cmp(&b.index))
    }
}
struct Graph {
    rings: Vec<CovRing>,
    edges: Vec<Edge>,
}
impl Graph {
    fn new(mut rings: Vec<CovRing>) -> Self {
        let mut counts = HashMap::<PointKey, usize>::new();
        let mut boundary = HashSet::new();
        for ring in &rings {
            let points = ring.source_points();
            for p in points {
                *counts.entry(key(p)).or_default() += 1;
            }
            for i in 0..points.len() {
                let k = segment(&points[i], &points[(i + 1) % points.len()]);
                if !boundary.insert(k) {
                    boundary.remove(&k);
                }
            }
        }
        let mut nodes: HashSet<_> = counts.into_iter().filter_map(|(p, n)| (n >= 3).then_some(p)).collect();
        let mut incident = HashMap::<PointKey, usize>::new();
        for edge in &boundary {
            for p in edge {
                *incident.entry(*p).or_default() += 1;
            }
        }
        nodes.extend(incident.into_iter().filter_map(|(p, n)| (n > 2).then_some(p)));
        for ring in &rings {
            let points = ring.source_points();
            let n = points.len();
            for i in 0..n {
                let prev = (i + n - 1) % n;
                if boundary.contains(&segment(&points[i], &points[(i + 1) % n])) != boundary.contains(&segment(&points[prev], &points[i])) {
                    nodes.insert(key(&points[i]));
                }
            }
        }
        let mut edges: Vec<Edge> = vec![];
        let mut unique = HashMap::<EdgeKey, usize>::new();
        for ring in &mut rings {
            if ring.points.len() < 3 {
                continue;
            }
            let n = ring.points.len() - 1;
            let anchors: Vec<_> = (0..n).filter(|&i| nodes.contains(&key(&ring.points[i]))).collect();
            let free = anchors.is_empty();
            let spans =
                if free { vec![(0, 0)] } else { anchors.iter().enumerate().map(|(i, &a)| (a, anchors[(i + 1) % anchors.len()])).collect() };
            for (start, end) in spans {
                let closed = start == end;
                let edge_key = if closed {
                    let i = (0..n).max_by(|&a, &b| cmp(&ring.points[a], &ring.points[b])).unwrap();
                    let (a, b) = (&ring.points[(i + n - 1) % n], &ring.points[(i + 1) % n]);
                    [key(&ring.points[i]), key(if cmp(a, b).is_lt() { a } else { b })]
                } else if cmp(&ring.points[start], &ring.points[end]).is_lt() {
                    [key(&ring.points[start]), key(&ring.points[(start + 1) % n])]
                } else {
                    [key(&ring.points[end]), key(&ring.points[(end + n - 1) % n])]
                };
                let index = if let Some(&index) = unique.get(&edge_key) {
                    edges[index].count += 1;
                    index
                } else {
                    let length = if closed { n } else { (end + n - start) % n };
                    let points = (0..=length).map(|i| ring.points[(start + i) % n].clone()).collect();
                    let index = edges.len();
                    edges.push(Edge::new(points, free));
                    unique.insert(edge_key, index);
                    index
                };
                ring.edges.push(index);
            }
        }
        Self { rings, edges }
    }
    fn simplify(&mut self, options: &CoverageSimplifyOptions) {
        for edge in &mut self.edges {
            edge.tolerance = options.tolerance;
        }
        if options.preserve_boundary && self.edges.iter().all(|e| e.count != 2) {
            return;
        }
        let vertices = RTree::bulk_load(
            self.edges
                .iter()
                .enumerate()
                .flat_map(|(e, edge)| {
                    edge.points.iter().take(edge.alive.len()).enumerate().map(move |(i, p)| GeomWithData::new([p.x, p.y], (e, i)))
                })
                .collect(),
        );
        let bounds = RTree::bulk_load(
            self.edges.iter().enumerate().map(|(i, e)| GeomWithData::new(Rectangle::from_aabb(envelope(e.points.iter())), i)).collect(),
        );
        for ei in 0..self.edges.len() {
            if options.preserve_boundary && self.edges[ei].count != 2 {
                continue;
            }
            let mut queue: BinaryHeap<_> = (0..self.edges[ei].alive.len()).filter_map(|i| self.edges[ei].corner(i).map(Reverse)).collect();
            let min = if self.edges[ei].ring { 3 } else { 2 };
            while self.edges[ei].size > min {
                let Some(Reverse(corner)) = queue.pop() else {
                    break;
                };
                let edge = &self.edges[ei];
                if !edge.alive[corner.index] || edge.prev[corner.index] != corner.prev || edge.next[corner.index] != corner.next {
                    continue;
                }
                let triangle = [&edge.points[corner.prev], &edge.points[corner.index], &edge.points[corner.next]];
                let query = envelope(triangle.into_iter());
                let blocked = vertices.locate_in_envelope_intersecting(query).any(|entry| {
                    let (e, i) = entry.data;
                    let p = &self.edges[e].points[i];
                    self.edges[e].alive[i] && !triangle.iter().any(|v| same(v, p)) && in_triangle(triangle, p)
                }) || bounds.locate_in_envelope_intersecting(query).any(|entry| {
                    let e = &self.edges[entry.data];
                    if entry.data == ei || e.size != 2 {
                        return false;
                    }
                    let ends = [&e.points[0], e.points.last().expect("two-point edge")];
                    (same(ends[0], triangle[0]) && same(ends[1], triangle[2])) || (same(ends[1], triangle[0]) && same(ends[0], triangle[2]))
                });
                if blocked {
                    continue;
                }
                let edge = &mut self.edges[ei];
                edge.next[corner.prev] = corner.next;
                edge.prev[corner.next] = corner.prev;
                edge.alive[corner.index] = false;
                edge.prev[corner.index] = ABSENT;
                edge.next[corner.index] = ABSENT;
                edge.size -= 1;
                for i in [corner.prev, corner.next] {
                    if let Some(c) = edge.corner(i) {
                        queue.push(Reverse(c));
                    }
                }
            }
        }
    }
    fn ring(&self, index: usize) -> Ring {
        let ring = &self.rings[index];
        if ring.edges.is_empty() {
            return ring.original.clone();
        }
        let mut points: Vec<Coord> = vec![];
        for (i, &ei) in ring.edges.iter().enumerate() {
            let edge = &self.edges[ei];
            let mut coords = edge.coords();
            let forward = if i == 0 {
                if ring.edges.len() <= 2 {
                    true
                } else {
                    let next = &self.edges[ring.edges[1]].points;
                    same(edge.points.last().unwrap(), &next[0]) || same(edge.points.last().unwrap(), next.last().unwrap())
                }
            } else {
                same(points.last().unwrap(), &coords[0])
            };
            if !forward {
                coords.reverse();
            }
            for p in coords {
                if points.last().is_none_or(|last| !same(last, &p)) {
                    points.push(p);
                }
            }
        }
        // The shared-edge graph uses closed rings; wbvector stores their closure
        // implicitly. Its GeoJSON/WKT serializers append the first coordinate.
        if points.len() > 1 && same(&points[0], points.last().unwrap()) {
            points.pop();
        }
        Ring::new(points)
    }
}
fn envelope<'a>(points: impl Iterator<Item = &'a Coord>) -> AABB<[f64; 2]> {
    let mut lo = [f64::INFINITY; 2];
    let mut hi = [f64::NEG_INFINITY; 2];
    for p in points {
        lo[0] = lo[0].min(p.x);
        lo[1] = lo[1].min(p.y);
        hi[0] = hi[0].max(p.x);
        hi[1] = hi[1].max(p.y);
    }
    AABB::from_corners(lo, hi)
}
fn in_triangle(t: [&Coord; 3], p: &Coord) -> bool {
    let orientation = |a: &Coord, b: &Coord| RobustKernel::orient2d(point(a), point(b), point(p));
    let signs = [orientation(t[0], t[1]), orientation(t[1], t[2]), orientation(t[2], t[0])];
    !signs.contains(&Orientation::Clockwise) || !signs.contains(&Orientation::CounterClockwise)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;
    fn part_area(part: &(Ring, Vec<Ring>)) -> f64 {
        geo::Polygon::new(
            geo::LineString(part.0.0.iter().map(point).collect()),
            part.1.iter().map(|r| geo::LineString(r.0.iter().map(point).collect())).collect(),
        )
        .unsigned_area()
    }

    fn layer(wkts: &[&str]) -> Layer {
        let mut out = Layer::new("coverage").with_epsg(3857);
        out.schema.add_field(crate::FieldDef::new("name", crate::FieldType::Text));
        for (i, wkt) in wkts.iter().enumerate() {
            let text = format!("WKT\n\"{wkt}\"\n");
            let geometry = crate::memory::open_vector_bytes(text.into_bytes(), "ring.csv", &Default::default()).unwrap().layers[0].features
                [0]
            .geometry
            .clone();
            out.features.push(Feature { fid: 100 + i as u64, geometry, attributes: vec![crate::FieldValue::Text(format!("p{i}"))] });
        }
        out
    }
    fn area(geometry: &Geometry) -> f64 {
        match geometry {
            Geometry::Polygon { exterior, interiors } => part_area(&(exterior.clone(), interiors.clone())),
            Geometry::MultiPolygon(parts) => parts.iter().map(part_area).sum(),
            _ => 0.,
        }
    }
    fn vertices(layer: &Layer) -> usize {
        layer.features.iter().filter_map(|f| f.geometry.as_ref()).map(|g| g.all_coords().len()).sum()
    }
    #[test]
    fn zero_tolerance_removes_collinear_vertices_unless_boundary_is_preserved() {
        let source = layer(&["POLYGON ((0 0,2 0,4 0,4 4,0 4,0 0))"]);
        let simplified = simplify_coverage(&source, &Default::default()).unwrap();
        assert_eq!(vertices(&simplified), 4); // closure is implicit in the Layer model
        let preserved = simplify_coverage(&source, &CoverageSimplifyOptions { preserve_boundary: true, ..Default::default() }).unwrap();
        assert_eq!(vertices(&preserved), 5);
    }

    #[test]
    fn retained_elevations_survive_but_measures_are_dropped_only_in_active_layers() {
        let source = layer(&["POLYGON ZM ((0 0 10 100,2 0 20 200,4 0 30 300,4 4 40 400,0 4 50 500,0 0 10 100))"]);
        let out = simplify_coverage(&source, &CoverageSimplifyOptions { tolerance: 0., preserve_boundary: true }).unwrap();
        let input = source.features[0].geometry.as_ref().unwrap().all_coords();
        for c in out.features[0].geometry.as_ref().unwrap().all_coords() {
            assert!(c.m.is_none());
            assert_eq!(c.z, input.iter().find(|p| same(p, c)).unwrap().z);
        }
        let mut inactive = source.clone();
        inactive.name = "inactive".into();
        let copied = simplify_coverage_layers(&[source, inactive.clone()], &Default::default(), Some("coverage")).unwrap();
        assert_eq!(copied[1].features[0].geometry, inactive.features[0].geometry);
    }

    #[test]
    fn shared_edges_are_simplified_once_and_exterior_is_preserved() {
        let source = layer(&["POLYGON ((0 0,2 0,1.8 1,2.2 2,1.8 3,2 4,0 4,0 0))", "POLYGON ((2 0,4 0,4 4,2 4,1.8 3,2.2 2,1.8 1,2 0))"]);
        let out = simplify_coverage(&source, &CoverageSimplifyOptions { tolerance: 1., preserve_boundary: true }).unwrap();
        assert_eq!(out.features.len(), 2);
        assert!(vertices(&out) < vertices(&source));
        assert!(super::super::coverage::check_coverage(&out, &Default::default()).unwrap().features.is_empty());
        let area: f64 = out.features.iter().map(|f| area(f.geometry.as_ref().unwrap())).sum();
        assert_eq!(area, 16.);
        for f in &out.features {
            for c in f.geometry.as_ref().unwrap().all_coords() {
                assert!([0., 2., 4.].contains(&c.x) && [0., 4.].contains(&c.y));
            }
        }
        assert_eq!(out.name, source.name);
        assert_eq!(out.schema.len(), source.schema.len());
        assert_eq!(out.schema.fields()[0].name, source.schema.fields()[0].name);
        assert_eq!(out.schema.fields()[0].field_type, source.schema.fields()[0].field_type);
        assert_eq!(out.features[0].attributes, source.features[0].attributes);
        assert_eq!(out.features[0].fid, 1);
        assert!(crate::crs::SpatialRef::from_layer(&out).same_as(&crate::crs::SpatialRef::from_layer(&source)));
    }
    #[test]
    fn isolated_rings_keep_three_distinct_vertices_and_can_preserve_all_boundaries() {
        let source = layer(&["POLYGON ((0 0,2 0,4 0,6 0,6 2,6 4,4 4,2 4,0 4,0 2,0 0))"]);
        let out = simplify_coverage(&source, &CoverageSimplifyOptions { tolerance: 100., preserve_boundary: false }).unwrap();
        assert_eq!(vertices(&out), 3);
        let ring = out.features[0].geometry.as_ref().unwrap().all_coords();
        assert!(!same(ring[0], ring.last().unwrap()), "ring closure must be implicit for GeoJSON/WKT serializers");
        let fixed = simplify_coverage(&source, &CoverageSimplifyOptions { tolerance: 100., preserve_boundary: true }).unwrap();
        let mut coords = fixed.features[0].geometry.as_ref().unwrap().all_coords();
        if coords.len() > 1 && same(coords[0], coords.last().unwrap()) {
            coords.pop();
        }
        assert_eq!(coords, source.features[0].geometry.as_ref().unwrap().all_coords());
    }
    #[test]
    fn small_holes_and_polygon_parts_are_retained_like_gdal() {
        let source = layer(&[
            "MULTIPOLYGON (((0 0,100 0,100 100,0 100,0 0),(10 10,10 11,11 11,11 10,10 10)),((200 200,201 200,201 201,200 201,200 200)))",
        ]);
        let out = simplify_coverage(&source, &CoverageSimplifyOptions { tolerance: 5., ..Default::default() }).unwrap();
        assert!(matches!(&out.features[0].geometry,Some(Geometry::MultiPolygon(parts)) if parts.len()==2 && parts[0].1.len()==1));
        assert_eq!(area(out.features[0].geometry.as_ref().unwrap()), 10000.);
    }
    #[test]
    fn all_polygon_parts_survive_large_tolerances() {
        let source = layer(&["MULTIPOLYGON (((0 0,1 0,1 1,0 1,0 0)),((10 10,11 10,11 11,10 11,10 10)))"]);
        let out = simplify_coverage(&source, &CoverageSimplifyOptions { tolerance: 10., ..Default::default() }).unwrap();
        assert!(matches!(&out.features[0].geometry,Some(Geometry::MultiPolygon(parts)) if parts.len()==2));
        assert_eq!(vertices(&out), 6);
    }
    #[test]
    fn inactive_nonpolygon_layers_are_copied_and_unknown_active_names_fail() {
        let source = layer(&["POLYGON ((0 0,1 0,1 1,0 1,0 0))"]);
        let mut points = layer(&["POINT (2 3)"]);
        points.name = "points".into();
        let layers = vec![source, points.clone()];
        let out = simplify_coverage_layers(&layers, &Default::default(), Some("coverage")).unwrap();
        assert_eq!(out[1].features[0].geometry, points.features[0].geometry);
        assert_eq!(out[1].features[0].fid, 1);
        assert!(simplify_coverage_layers(&layers, &Default::default(), Some("missing")).is_err());
        assert!(simplify_coverage_layers(&layers, &Default::default(), None).is_err());
    }

    #[test]
    fn invalid_inputs_and_tolerances_are_rejected_and_empty_polygons_survive() {
        for wkt in ["POINT (0 0)", "POLYGON ((0 0,1 1,1 0,0 1,0 0))"] {
            assert!(simplify_coverage(&layer(&[wkt]), &Default::default()).is_err());
        }
        let mut source = layer(&["POLYGON ((0 0,1 0,1 1,0 1,0 0))"]);
        for tolerance in [-1., f64::NAN, f64::INFINITY] {
            assert!(simplify_coverage(&source, &CoverageSimplifyOptions { tolerance, ..Default::default() }).is_err());
        }
        source.features[0].geometry = None;
        assert!(simplify_coverage(&source, &Default::default()).is_err());
        source.features[0].geometry = Some(Geometry::Polygon { exterior: Ring::default(), interiors: vec![] });
        assert_eq!(simplify_coverage(&source, &Default::default()).unwrap().features[0].geometry, source.features[0].geometry);
    }
}
