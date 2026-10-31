// SPDX-FileCopyrightText: Copyright (c) 2022 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2021 Martin Davis.
// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   JTS (commit 3460207216f3091cd7f72d48d6ddd1b561b2b8fd):
//     modules/core/src/main/java/org/locationtech/jts/coverage/CoverageValidator.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/CoveragePolygonValidator.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/CoverageRing.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/InvalidSegmentDetector.java
//     modules/core/src/main/java/org/locationtech/jts/coverage/CoveragePolygon.java
//     modules/core/src/main/java/org/locationtech/jts/algorithm/PolygonNodeTopology.java
//     modules/core/src/main/java/org/locationtech/jts/geom/LineSegment.java

//! Polygon coverage validation. Predicates use planar XY coordinates and gap
//! distances use the layer's coordinate units. Each input polygon must itself be
//! valid. Indexed segment interactions detect overlaps, unmatched shared nodes
//! and narrow gaps; matching edges are classified before intersection tests.
use crate::{Error, Feature, Geometry, GeometryType, Layer, Result};
use geo::algorithm::kernels::{Kernel, Orientation, RobustKernel};
use geo::algorithm::line_intersection::{LineIntersection, line_intersection};
use geo::algorithm::winding_order::WindingOrder;
use geo::{BoundingRect, Winding};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use std::collections::HashMap;
use wbvector::{Coord, Ring};

type C = geo::Coord<f64>;
type Index = RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>;
type Key = [u64; 4];

/// Options for [`check_coverage`].
#[derive(Debug, Clone, Default)]
pub struct CoverageCheckOptions {
    /// Largest distance between sufficiently long, nearly parallel edges to flag.
    /// Zero disables narrow-gap detection. Units are those of the input CRS.
    pub maximum_gap_width: f64,
    /// Keep valid inputs as features with empty MultiLineString geometry.
    pub include_valid: bool,
}

/// Returns each coverage-invalid feature with its invalid boundary segments.
/// CRS is retained. Attributes are omitted and output feature IDs are sequential,
/// matching GDAL. With `include_valid`, empty line geometries maintain the
/// correspondence with valid inputs. The output layer is named
/// `invalid_edge`; its geometry type is MultiLineString. Original edge coordinates
/// (including Z/M) are retained. Overlapping interiors,
/// equal polygons and non-matching shared-edge nodes are invalid.
///
/// ```no_run
/// let ds = terrio::open_vector("counties.gpkg", &Default::default())?;
/// let errors = terrio::ops::coverage::check_coverage(&ds.layers[0], &Default::default())?;
/// terrio::write_vector(&[errors], "invalid_edges.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn check_coverage(layer: &Layer, options: &CoverageCheckOptions) -> Result<Layer> {
    let gap = options.maximum_gap_width;
    if !gap.is_finite() || gap < 0.0 {
        return Err(Error::InvalidArgument("maximum_gap_width must be finite and non-negative".into()));
    }
    let mut rings = Vec::new();
    let mut polygons = Vec::new();
    for (feature, f) in layer.features.iter().enumerate() {
        let g = f.geometry.as_ref().ok_or_else(|| {
            Error::InvalidArgument(format!("feature {}: coverage validation requires polygonal geometry, not null", f.fid))
        })?;
        if !matches!(g, Geometry::Polygon { .. } | Geometry::MultiPolygon(_)) {
            return Err(Error::InvalidArgument(format!("feature {}: coverage validation requires polygonal geometry", f.fid)));
        }
        super::topology::require_valid(g, f.fid)?;
        collect(g, feature, &mut rings, &mut polygons)?;
    }
    let mut edges = Vec::new();
    for (ri, ring) in rings.iter_mut().enumerate() {
        ring.start = edges.len();
        for (segment, pair) in ring.points.windows(2).enumerate() {
            edges.push(Edge { ring: ri, segment, feature: ring.feature, line: geo::Line::new(xy(&pair[0]), xy(&pair[1])) });
        }
    }
    let mut states = vec![State::default(); edges.len()];
    let mut equal = HashMap::<Key, Vec<usize>>::new();
    for (i, edge) in edges.iter().enumerate() {
        equal.entry(key(edge.line)).or_default().push(i);
    }
    for group in equal.values().filter(|g| g.len() > 1) {
        let mut sides = [0usize; 2];
        let mut features = HashMap::<usize, [usize; 2]>::new();
        for &i in group {
            let edge = &edges[i];
            let side = usize::from(forward(edge.line) == rings[edge.ring].interior_right);
            sides[side] += 1;
            features.entry(edge.feature).or_default()[side] += 1;
        }
        for &i in group {
            let edge = &edges[i];
            let side = usize::from(forward(edge.line) == rings[edge.ring].interior_right);
            let own = features[&edge.feature];
            states[i].invalid = sides[side] > own[side];
            states[i].matched = sides[1 - side] > own[1 - side];
        }
    }
    drop(equal);
    let edge_index = make_index(edges.iter().enumerate().map(|(i, e)| (envelope(e.line), i)));
    for (i, target) in edges.iter().enumerate() {
        if states[i].known() {
            continue;
        }
        let query = expanded(envelope(target.line), gap)?;
        for other in edge_index.locate_in_envelope_intersecting(query) {
            let adj = &edges[other.data];
            if target.feature == adj.feature || key(target.line) == key(adj.line) {
                continue;
            }
            if invalid_interaction(target.line, adj, &rings, gap) {
                states[i].invalid = true;
                break;
            }
        }
    }
    let polygon_index = make_index(polygons.iter().enumerate().map(|(i, p)| (p.locator.bounds, i)));
    for (i, edge) in edges.iter().enumerate() {
        if states[i].known() {
            continue;
        }
        let p = edge.line.start;
        for other in polygon_index.locate_in_envelope_intersecting(AABB::from_point([p.x, p.y])) {
            let polygon = &polygons[other.data];
            if polygon.feature != edge.feature && polygon.locator.position(p) == Position::Inside {
                states[i].invalid = true;
                let ring = &rings[edge.ring];
                let n = ring.points.len() - 1;
                let previous = ring.start + (i - ring.start + n - 1) % n;
                if !states[previous].known() {
                    states[previous].invalid = true;
                }
            }
        }
    }
    let mut errors: Vec<Vec<Vec<Coord>>> = vec![vec![]; layer.features.len()];
    for ring in &rings {
        invalid_lines(ring, &states, &mut errors[ring.feature]);
    }
    let mut out = super::geometry::header(layer, Some(GeometryType::MultiLineString));
    out.name = "invalid_edge".into();
    out.schema = Default::default();
    for lines in errors {
        if !lines.is_empty() || options.include_valid {
            out.features.push(Feature {
                fid: out.features.len() as u64 + 1,
                geometry: Some(Geometry::MultiLineString(lines)),
                attributes: vec![],
            });
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Default)]
struct State {
    matched: bool,
    invalid: bool,
}
impl State {
    fn known(self) -> bool {
        self.matched || self.invalid
    }
}
struct CovRing {
    points: Vec<Coord>,
    feature: usize,
    interior_right: bool,
    start: usize,
}
struct Edge {
    ring: usize,
    segment: usize,
    feature: usize,
    line: geo::Line<f64>,
}
struct Polygon {
    feature: usize,
    locator: Locator,
}

fn xy(c: &Coord) -> C {
    geo::coord! {x:c.x,y:c.y}
}
fn same(a: C, b: C) -> bool {
    a.x == b.x && a.y == b.y
}
fn forward(line: geo::Line<f64>) -> bool {
    line.start.x < line.end.x || (line.start.x == line.end.x && line.start.y < line.end.y)
}
fn key(line: geo::Line<f64>) -> Key {
    let (a, b) = if forward(line) { (line.start, line.end) } else { (line.end, line.start) };
    let bits = |v: f64| if v == 0.0 { 0 } else { v.to_bits() };
    [bits(a.x), bits(a.y), bits(b.x), bits(b.y)]
}
fn envelope(line: geo::Line<f64>) -> AABB<[f64; 2]> {
    AABB::from_corners(
        [line.start.x.min(line.end.x), line.start.y.min(line.end.y)],
        [line.start.x.max(line.end.x), line.start.y.max(line.end.y)],
    )
}
fn expanded(bounds: AABB<[f64; 2]>, d: f64) -> Result<AABB<[f64; 2]>> {
    let low = bounds.lower();
    let high = bounds.upper();
    let lo = [low[0] - d, low[1] - d];
    let hi = [high[0] + d, high[1] + d];
    if lo.iter().chain(&hi).any(|v| !v.is_finite()) {
        return Err(Error::InvalidArgument("coverage gap search exceeds finite coordinate bounds".into()));
    }
    Ok(AABB::from_corners(lo, hi))
}
fn make_index(entries: impl Iterator<Item = (AABB<[f64; 2]>, usize)>) -> Index {
    RTree::bulk_load(entries.map(|(b, i)| GeomWithData::new(Rectangle::from_aabb(b), i)).collect())
}
fn add_ring(r: &Ring, shell: bool, feature: usize, rings: &mut Vec<CovRing>) -> Option<geo::LineString<f64>> {
    if r.0.is_empty() {
        return None;
    }
    let mut points = r.0.clone();
    points.dedup_by(|a, b| same(xy(a), xy(b)));
    if !same(xy(&points[0]), xy(points.last().unwrap())) {
        points.push(points[0].clone());
    }
    let line = geo::LineString(points.iter().map(xy).collect());
    let ccw = line.winding_order() == Some(WindingOrder::CounterClockwise);
    rings.push(CovRing { points, feature, interior_right: if shell { !ccw } else { ccw }, start: 0 });
    Some(line)
}
fn collect(g: &Geometry, feature: usize, rings: &mut Vec<CovRing>, polygons: &mut Vec<Polygon>) -> Result<()> {
    match g {
        Geometry::Polygon { exterior, interiors } => {
            if let Some(exterior) = add_ring(exterior, true, feature, rings) {
                let interiors = interiors.iter().filter_map(|r| add_ring(r, false, feature, rings)).collect();
                polygons.push(Polygon { feature, locator: Locator::new(geo::Polygon::new(exterior, interiors)) });
            }
        }
        Geometry::MultiPolygon(ps) => {
            for (exterior, interiors) in ps {
                collect(&Geometry::Polygon { exterior: exterior.clone(), interiors: interiors.clone() }, feature, rings, polygons)?;
            }
        }
        Geometry::GeometryCollection(gs) => {
            for g in gs {
                collect(g, feature, rings, polygons)?;
            }
        }
        _ => return Err(Error::InvalidArgument("coverage checking requires polygonal geometries".into())),
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Position {
    Outside,
    Boundary,
    Inside,
}
struct Locator {
    bounds: AABB<[f64; 2]>,
    edges: Vec<geo::Line<f64>>,
    index: Index,
}
impl Locator {
    fn new(polygon: geo::Polygon<f64>) -> Self {
        let b = polygon.bounding_rect().expect("nonempty polygon");
        let bounds = AABB::from_corners([b.min().x, b.min().y], [b.max().x, b.max().y]);
        let edges: Vec<_> = std::iter::once(polygon.exterior()).chain(polygon.interiors()).flat_map(|r| r.lines()).collect();
        let index = make_index(edges.iter().enumerate().map(|(i, l)| (envelope(*l), i)));
        Self { bounds, edges, index }
    }
    fn position(&self, p: C) -> Position {
        let lo = self.bounds.lower();
        let hi = self.bounds.upper();
        if p.x < lo[0] || p.x > hi[0] || p.y < lo[1] || p.y > hi[1] {
            return Position::Outside;
        }
        let mut inside = false;
        for entry in self.index.locate_in_envelope_intersecting(AABB::from_corners([p.x, p.y], [hi[0], p.y])) {
            let line = self.edges[entry.data];
            let o = RobustKernel::orient2d(line.start, line.end, p);
            if o == Orientation::Collinear
                && p.x >= line.start.x.min(line.end.x)
                && p.x <= line.start.x.max(line.end.x)
                && p.y >= line.start.y.min(line.end.y)
                && p.y <= line.start.y.max(line.end.y)
            {
                return Position::Boundary;
            }
            if (line.start.y > p.y) != (line.end.y > p.y)
                && ((line.start.y < line.end.y && o == Orientation::CounterClockwise)
                    || (line.start.y > line.end.y && o == Orientation::Clockwise))
            {
                inside = !inside;
            }
        }
        if inside { Position::Inside } else { Position::Outside }
    }
}

fn invalid_interaction(target: geo::Line<f64>, adj: &Edge, rings: &[CovRing], gap: f64) -> bool {
    if let Some(hit) = line_intersection(target, adj.line) {
        let p = match hit {
            LineIntersection::Collinear { intersection } if !same(intersection.start, intersection.end) => return true,
            LineIntersection::Collinear { intersection } => intersection.start,
            LineIntersection::SinglePoint { intersection, is_proper } => {
                if is_proper {
                    return true;
                }
                intersection
            }
        };
        if (!same(p, target.start) && !same(p, target.end)) || (!same(p, adj.line.start) && !same(p, adj.line.end)) {
            return true;
        }
        let end = if same(p, target.start) { target.end } else { target.start };
        let ring = &rings[adj.ring];
        let n = ring.points.len() - 1;
        let j = adj.segment;
        let (mut prev, mut next) = if same(p, adj.line.start) {
            (xy(&ring.points[(j + n - 1) % n]), adj.line.end)
        } else {
            (adj.line.start, xy(&ring.points[(j + 2) % n]))
        };
        if !same(end, prev) && !same(end, next) {
            if !ring.interior_right {
                std::mem::swap(&mut prev, &mut next);
            }
            if interior_corner(p, prev, next, end) {
                return true;
            }
        }
    }
    gap > 0.0 && nearly_parallel(target, adj.line, gap)
}
fn quadrant(o: C, p: C) -> u8 {
    if p.x >= o.x {
        if p.y >= o.y { 0 } else { 3 }
    } else if p.y >= o.y {
        1
    } else {
        2
    }
}
fn angle_greater(o: C, p: C, q: C) -> bool {
    let (a, b) = (quadrant(o, p), quadrant(o, q));
    a > b || (a == b && RobustKernel::orient2d(o, q, p) == Orientation::CounterClockwise)
}
fn interior_corner(o: C, mut low: C, mut high: C, p: C) -> bool {
    let between_interior = !angle_greater(o, low, high);
    if !between_interior {
        std::mem::swap(&mut low, &mut high);
    }
    (angle_greater(o, p, low) && !angle_greater(o, p, high)) == between_interior
}
fn project(a: geo::Line<f64>, b: geo::Line<f64>) -> Option<geo::Line<f64>> {
    let dx = a.end.x - a.start.x;
    let dy = a.end.y - a.start.y;
    let factor = |p: C| ((p.x - a.start.x) * dx + (p.y - a.start.y) * dy) / (dx * dx + dy * dy);
    let (f0, f1) = (factor(b.start), factor(b.end));
    if (f0 > 1.0 && f1 > 1.0) || (f0 < 0.0 && f1 < 0.0) {
        return None;
    }
    let p = |f: f64| {
        if f < 0.0 {
            a.start
        } else if f > 1.0 {
            a.end
        } else {
            geo::coord! {x:a.start.x+f*dx,y:a.start.y+f*dy}
        }
    };
    Some(geo::Line::new(p(f0), p(f1)))
}
fn distance(a: C, b: C) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}
fn nearly_parallel(a: geo::Line<f64>, b: geo::Line<f64>, gap: f64) -> bool {
    let (Some(pa), Some(mut pb)) = (project(a, b), project(b, a)) else {
        return false;
    };
    if distance(pa.start, pa.end) <= gap || distance(pb.start, pb.end) <= gap {
        return false;
    }
    if distance(pa.start, pb.end) < distance(pa.start, pb.start) {
        std::mem::swap(&mut pb.start, &mut pb.end);
    }
    distance(pa.start, pb.start) <= gap && distance(pa.end, pb.end) <= gap
}
fn invalid_lines(ring: &CovRing, states: &[State], out: &mut Vec<Vec<Coord>>) {
    let n = ring.points.len() - 1;
    let flags = &states[ring.start..ring.start + n];
    if flags.iter().all(|s| s.invalid) {
        out.push(ring.points.clone());
        return;
    }
    let Some(valid) = flags.iter().position(|s| !s.invalid) else {
        return;
    };
    let mut line = Vec::new();
    for step in 1..=n {
        let i = (valid + step) % n;
        if flags[i].invalid {
            if line.is_empty() {
                line.push(ring.points[i].clone());
            }
            line.push(ring.points[i + 1].clone());
        } else if !line.is_empty() {
            out.push(std::mem::take(&mut line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn layer(wkts: &[&str]) -> Layer {
        let mut out = Layer::new("source");
        out.schema.add_field(crate::FieldDef::new("label", crate::FieldType::Text));
        for (i, wkt) in wkts.iter().enumerate() {
            let csv = format!("WKT\n\"{wkt}\"\n");
            let geometry = crate::memory::open_vector_bytes(csv.into_bytes(), "polygon.csv", &Default::default()).unwrap().layers[0]
                .features[0]
                .geometry
                .clone();
            out.features.push(Feature { fid: 100 + i as u64, geometry, attributes: vec![crate::FieldValue::Text(format!("p{i}"))] });
        }
        out
    }
    const LEFT: &str = "POLYGON ((0 0,1 0,1 2,0 2,0 0))";
    const RIGHT: &str = "POLYGON ((1 0,2 0,2 2,1 2,1 0))";
    #[test]
    fn shared_edges_and_holes_form_valid_coverages() {
        for source in
            [layer(&[LEFT, RIGHT]), layer(&["POLYGON ((0 0,4 0,4 4,0 4,0 0),(1 1,1 3,3 3,3 1,1 1))", "POLYGON ((1 1,3 1,3 3,1 3,1 1))"])]
        {
            assert!(check_coverage(&source, &Default::default()).unwrap().features.is_empty());
            let out = check_coverage(&source, &CoverageCheckOptions { include_valid: true, ..Default::default() }).unwrap();
            assert_eq!(out.features.len(), 2);
            assert!(out.features.iter().all(|f| f.geometry.as_ref().unwrap().all_coords().is_empty()));
            assert_eq!(out.features[0].fid, 1);
            assert!(out.features[0].attributes.is_empty());
            assert!(out.schema.is_empty());
        }
    }
    #[test]
    fn shared_edges_with_unmatched_nodes_are_flagged_on_both_sides() {
        let source = layer(&[LEFT, "POLYGON ((1 0,2 0,2 2,1 2,1 1,1 0))"]);
        let out = check_coverage(&source, &Default::default()).unwrap();
        assert_eq!(out.features.len(), 2);
        for f in &out.features {
            assert!(f.geometry.as_ref().unwrap().all_coords().iter().all(|c| c.x == 1.0));
        }
    }
    #[test]
    fn duplicated_polygons_are_invalid_regardless_of_ring_orientation() {
        let source = layer(&[LEFT, "POLYGON ((0 0,0 2,1 2,1 0,0 0))"]);
        let out = check_coverage(&source, &Default::default()).unwrap();
        assert_eq!(out.features.len(), 2);
        assert!(out.features.iter().all(|f| f.geometry.as_ref().unwrap().all_coords().len() == 5));
    }
    #[test]
    fn crossing_and_contained_boundaries_report_original_edges() {
        let source = layer(&["POLYGON ((0 0,2 0,2 2,0 2,0 0))", "POLYGON ((1 0,3 0,3 2,1 2,1 0))"]);
        let out = check_coverage(&source, &Default::default()).unwrap();
        assert_eq!(out.features.len(), 2);
        assert!(out.features.iter().all(|f| f.geometry.as_ref().unwrap().all_coords().len() == 4));
        let source = layer(&["POLYGON ((0 0,10 0,10 10,0 10,0 0))", LEFT]);
        // LEFT touches the large polygon's outside, so its shared portions have
        // unmatched nodes and are invalid; use a strictly interior polygon too.
        let out = check_coverage(&source, &Default::default()).unwrap();
        assert_eq!(out.features.len(), 2);
        let source = layer(&["POLYGON ((0 0,10 0,10 10,0 10,0 0))", "POLYGON ((1 1,2 1,2 2,1 2,1 1))"]);
        let out = check_coverage(&source, &Default::default()).unwrap();
        assert_eq!(out.features.len(), 1);
        assert_eq!(out.features[0].fid, 1);
        assert_eq!(out.features[0].geometry.as_ref().unwrap().all_coords().len(), 5);
    }
    #[test]
    fn narrow_gaps_use_distance_and_projected_overlap_length() {
        let source = layer(&["POLYGON ((0 0,1 0,1 10,0 10,0 0))", "POLYGON ((1.1 0,2.1 0,2.1 10,1.1 10,1.1 0))"]);
        for (gap, expected) in [(0., 0), (0.05, 0), (0.2, 2)] {
            let out = check_coverage(&source, &CoverageCheckOptions { maximum_gap_width: gap, ..Default::default() }).unwrap();
            assert_eq!(out.features.len(), expected, "gap {gap}");
        }
    }
    #[test]
    fn source_edge_dimensions_are_retained_and_nulls_are_rejected() {
        let mut source = layer(&[LEFT, LEFT]);
        if let Some(Geometry::Polygon { exterior, .. }) = &mut source.features[0].geometry {
            for c in &mut exterior.0 {
                c.z = Some(10.);
                c.m = Some(20.);
            }
        }
        let out = check_coverage(&source, &CoverageCheckOptions { include_valid: true, ..Default::default() }).unwrap();
        assert_eq!(out.features.len(), 2);
        assert!(out.features[0].geometry.as_ref().unwrap().all_coords().iter().all(|c| c.z == Some(10.) && c.m == Some(20.)));
        source.features.push(Feature { fid: 999, geometry: None, attributes: vec![] });
        assert!(check_coverage(&source, &Default::default()).unwrap_err().to_string().contains("not null"));
    }
    #[test]
    fn invalid_polygons_nonpolygon_inputs_and_invalid_gap_widths_fail() {
        for wkt in ["POINT (0 0)", "GEOMETRYCOLLECTION (POLYGON ((0 0,1 0,1 1,0 0)))", "POLYGON ((0 0,2 2,0 2,2 0,0 0))"] {
            assert!(check_coverage(&layer(&[wkt]), &Default::default()).is_err());
        }
        for gap in [-1., f64::NAN, f64::INFINITY] {
            assert!(check_coverage(&layer(&[LEFT]), &CoverageCheckOptions { maximum_gap_width: gap, ..Default::default() }).is_err());
        }
    }
}
