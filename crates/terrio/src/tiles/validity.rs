//! Integer-grid adaptation of JTS 1.20.0 IsValidOp, PolygonIntersectionAnalyzer,
//! PolygonTopologyAnalyzer, PolygonNodeTopology and PolygonRing. Copyright (c)
//! 2016 Vivid Solutions; Copyright (c) 2021 Martin Davis. EDL-1.0; see NOTICE.
//!
//! This module checks validity only. It contains no MakeValid implementation.
//! Tile ordinates are i32, so i128 determinants and doubled-coordinate midpoint
//! probes are exact, including at the extremes of the tile coordinate type.

use super::repair::{IPt, Polygon};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use std::collections::{HashMap, HashSet};

type Wide = (i64, i64);
fn wide(p: IPt) -> Wide {
    (p.0 as i64, p.1 as i64)
}
fn turn(a: Wide, b: Wide, c: Wide) -> i128 {
    (b.0 - a.0) as i128 * (c.1 - a.1) as i128 - (b.1 - a.1) as i128 * (c.0 - a.0) as i128
}
fn on(p: IPt, a: IPt, b: IPt) -> bool {
    turn(wide(a), wide(b), wide(p)) == 0 && p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1)
}
fn quadrant(o: IPt, p: IPt) -> i32 {
    match (p.0 >= o.0, p.1 >= o.1) {
        (true, true) => 0,
        (false, true) => 1,
        (false, false) => 2,
        (true, false) => 3,
    }
}
fn angle(o: IPt, p: IPt, q: IPt) -> i32 {
    let a = quadrant(o, p);
    let b = quadrant(o, q);
    if a != b { (a - b).signum() } else { turn(wide(o), wide(q), wide(p)).signum() as i32 }
}
/// JTS PolygonNodeTopology.isCrossing; collinear rays were checked for overlap earlier.
fn crossing(o: IPt, a: IPt, b: IPt, c: IPt, d: IPt) -> bool {
    let (lo, hi) = if angle(o, a, b) > 0 { (b, a) } else { (a, b) };
    let between = |p| {
        let x = angle(o, p, lo);
        let y = angle(o, p, hi);
        if x == 0 || y == 0 {
            0
        } else if x > 0 && y < 0 {
            1
        } else {
            -1
        }
    };
    let (x, y) = (between(c), between(d));
    x != 0 && y != 0 && x != y
}

struct Ring {
    points: Vec<IPt>,
    polygon: usize,
    hole: bool,
}
#[derive(Clone, Copy)]
struct Segment {
    ring: usize,
    index: usize,
    a: IPt,
    b: IPt,
}
fn arms(r: &Ring, s: &Segment, p: IPt) -> (IPt, IPt) {
    let n = r.points.len() - 1;
    if p == s.a {
        (r.points[(s.index + n - 1) % n], s.b)
    } else if p == s.b {
        (s.a, r.points[(s.index + 2) % n])
    } else {
        (s.a, s.b)
    }
}

#[derive(PartialEq, Eq)]
enum Location {
    Inside,
    Outside,
    Boundary,
}
/// JTS ray crossing, on doubled integer coordinates for exact segment midpoints.
fn locate(p: Wide, ring: &[IPt]) -> Location {
    let mut parity = false;
    for edge in ring.windows(2) {
        let a = (2 * edge[0].0 as i64, 2 * edge[0].1 as i64);
        let b = (2 * edge[1].0 as i64, 2 * edge[1].1 as i64);
        let side = turn(a, b, p);
        if side == 0 && p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1) {
            return Location::Boundary;
        }
        if (a.1 > p.1) != (b.1 > p.1) && side.signum() == if b.1 > a.1 { 1 } else { -1 } {
            parity = !parity;
        }
    }
    if parity { Location::Inside } else { Location::Outside }
}
/// After intersection checks, every non-boundary probe has the same location.
/// Midpoints replace JTS's incident-segment corner test without precision loss.
fn nested(test: &Ring, target: &Ring) -> bool {
    for edge in test.points.windows(2) {
        for p in [(2 * edge[0].0 as i64, 2 * edge[0].1 as i64), (edge[0].0 as i64 + edge[1].0 as i64, edge[0].1 as i64 + edge[1].1 as i64)]
        {
            match locate(p, &target.points) {
                Location::Inside => return true,
                Location::Outside => return false,
                Location::Boundary => {}
            }
        }
    }
    false
}
fn root(parents: &mut [usize], mut v: usize) -> usize {
    while parents[v] != v {
        parents[v] = parents[parents[v]];
        v = parents[v];
    }
    v
}

/// Checks closed rings under JTS's default OGC validity rules. Adjacent repeated
/// vertices are ignored; self-touches, shared edges, nested holes/shells and
/// chains of holes disconnecting an interior are invalid.
pub(crate) fn is_valid(polygons: &[Polygon<IPt>]) -> bool {
    let mut rings = Vec::new();
    let mut by_polygon = vec![Vec::new(); polygons.len()];
    for (polygon, poly) in polygons.iter().enumerate() {
        for (index, ring) in poly.iter().enumerate() {
            if ring.is_empty() {
                continue;
            }
            if ring.first() != ring.last() {
                return false;
            }
            let mut points = ring.clone();
            points.dedup();
            if points.len() < 4 {
                return false;
            }
            if index > 0 && poly.first().is_none_or(|r| r.is_empty()) {
                return false;
            }
            by_polygon[polygon].push(rings.len());
            rings.push(Ring { points, polygon, hole: index != 0 });
        }
    }
    let mut segments = Vec::new();
    for (ring, r) in rings.iter().enumerate() {
        for (index, edge) in r.points.windows(2).enumerate() {
            segments.push(Segment { ring, index, a: edge[0], b: edge[1] });
        }
    }
    // Use floating envelopes: every i32 ordinate is exactly representable, and
    // envelope area/centre calculations cannot overflow integer arithmetic.
    let tree = RTree::bulk_load(
        segments
            .iter()
            .enumerate()
            .map(|(id, s)| {
                GeomWithData::new(
                    Rectangle::from_corners(
                        [s.a.0.min(s.b.0) as f64, s.a.1.min(s.b.1) as f64],
                        [s.a.0.max(s.b.0) as f64, s.a.1.max(s.b.1) as f64],
                    ),
                    id,
                )
            })
            .collect(),
    );
    // Touch nodes make a bipartite graph with ring nodes. Collapsing all contacts
    // at one coordinate into one node excludes JTS's trivial same-point cycles.
    let mut touches: HashMap<(usize, IPt), HashSet<usize>> = HashMap::new();
    for (id, a) in segments.iter().enumerate() {
        let env =
            AABB::from_corners([a.a.0.min(a.b.0) as f64, a.a.1.min(a.b.1) as f64], [a.a.0.max(a.b.0) as f64, a.a.1.max(a.b.1) as f64]);
        for entry in tree.locate_in_envelope_intersecting(env) {
            if entry.data <= id {
                continue;
            }
            let b = &segments[entry.data];
            let ab = [turn(wide(a.a), wide(a.b), wide(b.a)), turn(wide(a.a), wide(a.b), wide(b.b))];
            let ba = [turn(wide(b.a), wide(b.b), wide(a.a)), turn(wide(b.a), wide(b.b), wide(a.b))];
            if ab[0].signum() * ab[1].signum() < 0 && ba[0].signum() * ba[1].signum() < 0 {
                return false;
            }
            let mut common = Vec::new();
            for p in [a.a, a.b, b.a, b.b] {
                if on(p, a.a, a.b) && on(p, b.a, b.b) && !common.contains(&p) {
                    common.push(p);
                }
            }
            if common.len() > 1 {
                return false;
            }
            let Some(&p) = common.first() else {
                continue;
            };
            if a.ring == b.ring {
                let delta = a.index.abs_diff(b.index);
                let n = rings[a.ring].points.len() - 1;
                if delta != 1 && delta != n - 1 {
                    return false;
                }
                continue;
            }
            let (a0, a1) = arms(&rings[a.ring], a, p);
            let (b0, b1) = arms(&rings[b.ring], b, p);
            if crossing(p, a0, a1, b0, b1) {
                return false;
            }
            if rings[a.ring].polygon == rings[b.ring].polygon {
                let set = touches.entry((rings[a.ring].polygon, p)).or_default();
                set.insert(a.ring);
                set.insert(b.ring);
            }
        }
    }
    for ids in &by_polygon {
        let Some(&shell) = ids.first() else {
            continue;
        };
        for &hole in ids.iter().skip(1) {
            if !nested(&rings[hole], &rings[shell]) {
                return false;
            }
            for &other in ids.iter().skip(1) {
                if hole != other && nested(&rings[hole], &rings[other]) {
                    return false;
                }
            }
        }
    }
    for (id, shell) in rings.iter().enumerate().filter(|(_, r)| !r.hole) {
        for (other, target) in rings.iter().enumerate().filter(|(_, r)| !r.hole) {
            if id == other || !nested(shell, target) {
                continue;
            }
            if !by_polygon[target.polygon].iter().skip(1).any(|&h| nested(shell, &rings[h])) {
                return false;
            }
        }
    }
    let mut parents: Vec<usize> = (0..rings.len() + touches.len()).collect();
    for (node, set) in touches.values().enumerate() {
        for &ring in set {
            let a = root(&mut parents, ring);
            let b = root(&mut parents, rings.len() + node);
            if a == b {
                return false;
            }
            parents[a] = b;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Geometry;
    #[test]
    fn full_range_index_arithmetic_is_bounded() {
        let (lo, hi) = (i32::MIN, i32::MAX);
        let mut ring = Vec::new();
        for i in 0..32 {
            let v = (lo as i64 + (hi as i64 - lo as i64) * i / 32) as i32;
            ring.push((v, lo));
        }
        for i in 0..32 {
            let v = (lo as i64 + (hi as i64 - lo as i64) * i / 32) as i32;
            ring.push((hi, v));
        }
        for i in (1..=32).rev() {
            let v = (lo as i64 + (hi as i64 - lo as i64) * i / 32) as i32;
            ring.push((v, hi));
        }
        for i in (1..=32).rev() {
            let v = (lo as i64 + (hi as i64 - lo as i64) * i / 32) as i32;
            ring.push((lo, v));
        }
        ring.push(ring[0]);
        assert!(is_valid(&[vec![ring]]));
    }

    #[test]
    fn observed_validity() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!("../../tests/data/jts/valid.json")).unwrap();
        for c in fixture["cases"].as_array().unwrap() {
            let hex = c["input"].as_str().unwrap();
            let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
            let g = Geometry::from_wkb(&bytes).unwrap();
            let ps = match g {
                Geometry::Polygon { exterior, interiors } => vec![(exterior, interiors)],
                Geometry::MultiPolygon(ps) => ps,
                _ => panic!("polygon input required"),
            };
            let polygons = ps
                .iter()
                .map(|(e, h)| {
                    std::iter::once(e)
                        .chain(h)
                        .map(|r| {
                            let mut pts = r.0.iter().map(|c| (c.x as i32, c.y as i32)).collect::<Vec<_>>();
                            if !pts.is_empty() && pts.first() != pts.last() {
                                pts.push(pts[0]);
                            }
                            pts
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(is_valid(&polygons), c["expected"].as_bool().unwrap(), "{}", c["name"]);
        }
    }
}
