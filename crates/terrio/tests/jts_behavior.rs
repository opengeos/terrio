//! Replay observations captured from executable GEOS APIs, without native deps.
use geo::{Area, BooleanOps, HausdorffDistance, Validation};
use serde_json::Value;
use std::collections::BTreeSet;
use terrio::{
    Feature, Geometry, Layer,
    ops::{hull, simplify, vector_geom},
};
use wbvector::{Coord, Ring};

fn decode(hex: &str) -> Geometry {
    let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
    Geometry::from_wkb(&bytes).unwrap()
}
fn ordinate(x: f64) -> u64 {
    if x == 0.0 { 0 } else { x.to_bits() }
}
type Point = (u64, u64);
fn point(c: &Coord) -> Point {
    (ordinate(c.x), ordinate(c.y))
}
fn edges(g: &Geometry) -> BTreeSet<(Point, Point)> {
    let mut result = BTreeSet::new();
    let mut line = |cs: &[Coord], close: bool| {
        if cs.len() == 1 {
            result.insert((point(&cs[0]), point(&cs[0])));
        }
        let end = cs.first().filter(|_| close && cs.len() > 1);
        let pts = cs.iter().chain(end).collect::<Vec<_>>();
        for pair in pts.windows(2) {
            let (a, b) = (point(pair[0]), point(pair[1]));
            if a != b {
                result.insert(if a < b { (a, b) } else { (b, a) });
            }
        }
    };
    match g {
        Geometry::Point(c) => line(std::slice::from_ref(c), false),
        Geometry::MultiPoint(cs) => {
            for c in cs {
                line(std::slice::from_ref(c), false);
            }
        }
        Geometry::LineString(cs) => line(cs, false),
        Geometry::MultiLineString(ls) => {
            for cs in ls {
                line(cs, false);
            }
        }
        Geometry::Polygon { exterior, interiors } => {
            for r in std::iter::once(exterior).chain(interiors) {
                line(&r.0, true);
            }
        }
        Geometry::MultiPolygon(ps) => {
            for (e, h) in ps {
                for r in std::iter::once(e).chain(h) {
                    line(&r.0, true);
                }
            }
        }
        Geometry::GeometryCollection(gs) => {
            for g in gs {
                result.extend(edges(g));
            }
        }
    }
    result
}
fn polygons(g: &Geometry) -> geo::MultiPolygon<f64> {
    let ring = |r: &Ring| geo::LineString(r.0.iter().map(|c| geo::Coord { x: c.x, y: c.y }).collect());
    let polygon = |e: &Ring, h: &[Ring]| geo::Polygon::new(ring(e), h.iter().map(ring).collect());
    match g {
        Geometry::Polygon { exterior, interiors } => geo::MultiPolygon(vec![polygon(exterior, interiors)]),
        Geometry::MultiPolygon(ps) => geo::MultiPolygon(ps.iter().map(|(e, h)| polygon(e, h)).collect()),
        _ => panic!("expected a polygon for tolerance comparison: {g:?}"),
    }
}
fn canonical_components(g: &Geometry) -> Vec<Vec<Vec<Point>>> {
    let ring = |r: &Ring| {
        let mut points = r.0.iter().map(point).collect::<Vec<_>>();
        if points.len() > 1 && points.first() == points.last() {
            points.pop();
        }
        let n = points.len();
        (0..n)
            .flat_map(|start| {
                let points = &points;
                [false, true].map(move |reverse| {
                    (0..n).map(|i| points[if reverse { (start + n - i) % n } else { (start + i) % n }]).collect::<Vec<_>>()
                })
            })
            .min()
            .unwrap_or_default()
    };
    let polygon = |shell: &Ring, holes: &[Ring]| {
        let mut holes = holes.iter().map(&ring).collect::<Vec<_>>();
        holes.sort();
        std::iter::once(ring(shell)).chain(holes).collect::<Vec<_>>()
    };
    let mut result = match g {
        Geometry::Polygon { exterior, interiors } => vec![polygon(exterior, interiors)],
        Geometry::MultiPolygon(ps) => ps.iter().map(|(e, h)| polygon(e, h)).collect(),
        _ => panic!("expected reference-invalid polygon: {g:?}"),
    };
    result.sort();
    result
}
fn corpus(text: &str) {
    let fixture: Value = serde_json::from_str(text).unwrap();
    for c in fixture["cases"].as_array().unwrap() {
        let g = decode(c["input"].as_str().unwrap());
        let ratio = c["ratio"].as_f64().unwrap();
        let holes = c["holes"].as_bool().unwrap();
        let op = c["operation"].as_str().unwrap();
        if op == "strtree" {
            let Geometry::GeometryCollection(gs) = g else { panic!() };
            let mut layer = Layer::new("test");
            layer.features = gs.into_iter().enumerate().map(|(i, g)| Feature::with_geometry(i as u64, g, 0)).collect();
            let result = vector_geom::sort(&layer, vector_geom::SortMethod::StrTree);
            let expected = c["expected"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect::<Vec<_>>();
            let actual = result.features.iter().map(|f| f.fid).collect::<Vec<_>>();
            if actual != expected {
                // GEOS leaves equal-key order unspecified; every feature must
                // remain in a legal X slice and each slice must be sorted by Y.
                let mut permutation = actual.clone();
                permutation.sort_unstable();
                assert_eq!(permutation, (0..layer.features.len() as u64).collect::<Vec<_>>());
                let centre = |id: u64, x: bool| {
                    let b = layer.features[id as usize].geometry.as_ref().unwrap().bbox().unwrap();
                    if x { b.min_x + b.max_x } else { b.min_y + b.max_y }
                };
                let slices = (actual.len().div_ceil(10) as f64).sqrt().ceil() as usize;
                let chunks = actual.chunks(actual.len().div_ceil(slices)).collect::<Vec<_>>();
                for pair in chunks.windows(2) {
                    let max = pair[0].iter().map(|&i| centre(i, true)).fold(f64::NEG_INFINITY, f64::max);
                    let min = pair[1].iter().map(|&i| centre(i, true)).fold(f64::INFINITY, f64::min);
                    assert!(max <= min, "{}: invalid X slice", c["name"]);
                }
                for chunk in chunks {
                    assert!(chunk.windows(2).all(|p| centre(p[0], false) <= centre(p[1], false)), "{}: invalid Y order", c["name"]);
                }
            }
            continue;
        }
        let actual = match op {
            "simplify" => simplify::simplify_preserve_topology(&g, ratio),
            "convex" => hull::convex_hull(&g),
            "concave" => hull::concave_hull(&g, ratio, holes),
            "polygon_hull" => hull::concave_hull_of_polygons(&g, ratio, c["tight"].as_bool().unwrap(), holes),
            _ => panic!("unknown operation"),
        }
        .unwrap_or_else(|e| panic!("{}: {e}", c["name"]));
        let expected = decode(c["expected"].as_str().unwrap());
        if op == "simplify" {
            assert_eq!(actual.to_wkb(), expected.to_wkb(), "{}", c["name"]);
            continue;
        }
        if c["reference_invalid"].as_bool() == Some(true) {
            assert!(!polygons(&expected).is_valid(), "{}: expected recorded invalidity", c["name"]);
            assert_eq!(canonical_components(&actual), canonical_components(&expected), "{}: reference-invalid exact rings", c["name"]);
            continue;
        }
        if matches!(actual, Geometry::Polygon { .. } | Geometry::MultiPolygon(_)) {
            assert!(polygons(&actual).is_valid(), "{}: invalid result", c["name"]);
        }
        if edges(&actual) == edges(&expected) {
            continue;
        }
        let a = polygons(&actual);
        let b = polygons(&expected);
        assert!(a.is_valid() && b.is_valid(), "{}: invalid result", c["name"]);
        let error = a.xor(&b).unsigned_area() / b.unsigned_area().max(f64::MIN_POSITIVE);
        let scale = expected.all_coords().iter().flat_map(|c| [c.x.abs(), c.y.abs()]).fold(1.0, f64::max);
        assert!(error <= 1e-10 && a.hausdorff_distance(&b) <= 1e-10 * scale, "{}: area error {error}", c["name"]);
    }
}
#[test]
fn simplification_observations() {
    corpus(include_str!("data/jts/simplify.json"));
}
#[test]
fn convex_hull_observations() {
    corpus(include_str!("data/jts/convex.json"));
}
#[test]
fn concave_hull_observations() {
    corpus(include_str!("data/jts/concave.json"));
}
#[test]
fn polygon_hull_observations() {
    corpus(include_str!("data/jts/polygon_hull.json"));
}
#[test]
fn str_order_observations() {
    corpus(include_str!("data/jts/strtree.json"));
}
