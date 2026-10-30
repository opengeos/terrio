//! Original native tests. Sections are labelled:
//! [O] compare with GEOS-observed fixture summaries (no GEOS at test time);
//! [P] defensive/project policy, not a GEOS observation;
//! [Inv] properties implied by the model (determinism, invariance, idempotence).

#![allow(clippy::expect_used)]

use super::{IPt, P, Polygon, try_make_valid};
use serde_json::Value;

type Out = Vec<Polygon<P>>;

/// Shoelace relative to the first vertex (keeps precision at large offsets).
fn signed_area(ring: &[P]) -> f64 {
    let o = ring[0];
    ring.windows(2).map(|w| (w[0].0 - o.0) * (w[1].1 - o.1) - (w[1].0 - o.0) * (w[0].1 - o.1)).sum::<f64>() / 2.0
}

/// Structural checks independent of any oracle: closed, finite, >= 3 distinct
/// vertices, shells clockwise and holes counter-clockwise, no empty polygon.
fn check_structure(out: &Out) {
    for polygon in out {
        assert!(!polygon.is_empty(), "empty polygon");
        for (i, ring) in polygon.iter().enumerate() {
            assert!(ring.len() >= 4, "short ring");
            assert_eq!(ring.first(), ring.last(), "unclosed ring");
            assert!(ring.iter().all(|p| p.0.is_finite() && p.1.is_finite()));
            let mut distinct: Vec<(u64, u64)> = ring.iter().map(|p| (p.0.to_bits(), p.1.to_bits())).collect();
            distinct.sort_unstable();
            distinct.dedup();
            assert!(distinct.len() >= 3, "collapsed ring");
            for w in ring.windows(2) {
                assert_ne!(w[0], w[1], "consecutive duplicate");
            }
            let a = signed_area(ring);
            if i == 0 {
                assert!(a < 0.0, "shell must be clockwise, area {a}");
            } else {
                assert!(a > 0.0, "hole must be counter-clockwise, area {a}");
            }
        }
    }
}

fn area(out: &Out) -> f64 {
    out.iter().flat_map(|p| p.iter()).map(|r| -signed_area(r)).sum()
}

fn holes(out: &Out) -> usize {
    out.iter().map(|p| p.len() - 1).sum()
}

fn repair(input: &[Polygon<IPt>]) -> Out {
    let out = try_make_valid(input).expect("independent repair must succeed");
    check_structure(&out);
    out
}

fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<IPt> {
    vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]
}

// ---------------------------------------------------------------- [O]

fn fixtures() -> Vec<Value> {
    let text = include_str!("../../../tests/data/make-valid.json");
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    value["observations"].as_array().cloned().unwrap_or_default()
}

fn input_of(case: &Value) -> Vec<Polygon<IPt>> {
    serde_json::from_value(case["input_polygons"].clone()).unwrap_or_default()
}

#[test]
fn fixture_summaries_match() {
    let cases = fixtures();
    assert_eq!(cases.len(), 51, "fixture file must be readable");
    for case in &cases {
        let id = case["id"].as_str().unwrap_or("?");
        let out = repair(&input_of(case));
        assert_eq!(out.len() as u64, case["polygon_count"].as_u64().unwrap_or(u64::MAX), "{id}: polygon count");
        assert_eq!(holes(&out) as u64, case["hole_count"].as_u64().unwrap_or(u64::MAX), "{id}: hole count");
        let expected = case["polygonal_area"].as_f64().unwrap_or(f64::NAN);
        let got = area(&out);
        assert!((got - expected).abs() <= 1e-9 * expected.abs().max(1.0), "{id}: area {got} vs {expected}");
    }
}

#[test]
fn fixture_exact_coordinates_for_fractional_crossings() {
    // Every output vertex must be a binary64 value that occurs in the oracle output.
    for case in fixtures() {
        let id = case["id"].as_str().unwrap_or("?").to_string();
        let expected: Vec<Polygon<P>> = serde_json::from_value(case["polygonal_coordinates"].clone()).unwrap_or_default();
        let mut wanted: Vec<(u64, u64)> = expected.iter().flatten().flatten().map(|p| (p.0.to_bits(), p.1.to_bits())).collect();
        wanted.sort_unstable();
        let out = repair(&input_of(&case));
        for p in out.iter().flatten().flatten() {
            if case["input_valid"].as_bool() == Some(true) {
                continue; // the oracle returns valid input unchanged; extra contact nodes are allowed
            }
            assert!(wanted.binary_search(&(p.0.to_bits(), p.1.to_bits())).is_ok(), "{id}: unexpected vertex {p:?}");
        }
    }
}

// ---------------------------------------------------------------- [P]

#[test]
fn empty_inputs() {
    assert!(repair(&[]).is_empty());
    assert!(repair(&[vec![]]).is_empty());
    assert!(repair(&[vec![vec![]]]).is_empty());
    assert!(repair(&[vec![], vec![vec![]], vec![]]).is_empty());
}

#[test]
fn empty_exterior_does_not_promote_holes() {
    let out = repair(&[vec![vec![], rect(0, 0, 4, 4)]]);
    assert!(out.is_empty());
    // but a separate valid polygon survives
    let out = repair(&[vec![vec![], rect(0, 0, 4, 4)], vec![rect(10, 10, 12, 12)]]);
    assert_eq!(out.len(), 1);
    assert_eq!(area(&out), 4.0);
}

#[test]
fn unclosed_rings_are_closed_logically() {
    let closed = repair(&[vec![rect(0, 0, 6, 6)]]);
    let open = repair(&[vec![vec![(0, 0), (6, 0), (6, 6), (0, 6)]]]);
    assert_eq!(closed, open);
    let bow = repair(&[vec![vec![(0, 0), (6, 6), (0, 6), (6, 0)]]]);
    assert_eq!(bow.len(), 2);
    assert_eq!(area(&bow), 18.0);
}

#[test]
fn degenerate_rings_never_fill() {
    for ring in
        [vec![(1, 1)], vec![(1, 1), (1, 1)], vec![(0, 0), (5, 5)], vec![(0, 0), (5, 5), (0, 0)], vec![(0, 0), (2, 0), (4, 0), (0, 0)]]
    {
        assert!(repair(&[vec![ring.clone()]]).is_empty(), "{ring:?}");
    }
    // a collapsed ring touching a square only adds a contact node, not area
    let out = repair(&[vec![rect(0, 0, 6, 6)], vec![vec![(3, 6), (3, 9), (3, 6)]]]);
    assert_eq!(area(&out), 36.0);
    assert_eq!(out[0][0].len(), 6); // includes contact node (3, 6)
}

#[test]
fn consecutive_duplicates_are_ignored() {
    let a = repair(&[vec![vec![(0, 0), (0, 0), (6, 0), (6, 0), (6, 6), (0, 6), (0, 6), (0, 0), (0, 0)]]]);
    assert_eq!(a, repair(&[vec![rect(0, 0, 6, 6)]]));
}

#[test]
fn integer_extremes() {
    let (lo, hi) = (i32::MIN, i32::MAX);
    let out = repair(&[vec![rect(lo, lo, hi, hi)]]);
    assert_eq!(out.len(), 1);
    let span = f64::from(hi) - f64::from(lo);
    assert_eq!(area(&out), span * span);
    let bow = repair(&[vec![vec![(lo, lo), (hi, hi), (lo, hi), (hi, lo), (lo, lo)]]]);
    assert_eq!(bow.len(), 2);
    assert!(bow.iter().flatten().flatten().any(|p| *p == (-0.5, -0.5)));
    // nearly parallel full-range edges crossing near the origin
    let thin = repair(&[vec![vec![(lo, lo), (hi, hi - 1), (hi, hi), (lo, lo + 1), (lo, lo)]]]);
    check_structure(&thin);
    assert!(!thin.is_empty());
}

#[test]
fn deep_nesting_alternates() {
    let rings: Vec<Vec<IPt>> = (0..60).map(|i| rect(i, i, 200 - i, 200 - i)).collect();
    let as_components: Vec<Polygon<IPt>> = rings.iter().map(|r| vec![r.clone()]).collect();
    let out = repair(&as_components);
    // 30 annuli, each one polygon with one hole
    assert_eq!(out.len(), 30);
    assert_eq!(holes(&out), 30);
    let expected: i64 = (0..60).map(|i: i64| if i % 2 == 0 { 1 } else { -1 } * (200 - 2 * i).pow(2)).sum();
    assert_eq!(area(&out), expected as f64);
    // the same rings declared as holes of one polygon give the same area
    assert_eq!(area(&repair(&[rings])), expected as f64);
}

#[test]
fn many_disconnected_parts() {
    let parts: Vec<Polygon<IPt>> = (0..900).map(|k| vec![rect(3 * (k % 30), 3 * (k / 30), 3 * (k % 30) + 2, 3 * (k / 30) + 2)]).collect();
    let mut with_overlap = parts.clone();
    with_overlap.push(vec![rect(1, 1, 4, 4)]);
    let out = repair(&parts);
    assert_eq!(out.len(), 900);
    assert_eq!(area(&out), 3600.0);
    check_structure(&repair(&with_overlap));
}

#[test]
fn high_degree_contacts() {
    // 64 triangles sharing apex (0, 0), alternating directions, all point contacts
    let mut polys = Vec::new();
    for k in 0..64 {
        let a = (100 * k - 3200, 1000);
        let b = (100 * k - 3150, 1000);
        polys.push(vec![vec![(0, 0), a, b, (0, 0)]]);
    }
    let out = repair(&polys);
    assert_eq!(out.len(), 64);
    // a fan where neighbours share edges collapses into one polygon
    let fan: Vec<Polygon<IPt>> = (0..32).map(|k| vec![vec![(0, 0), (10 * k, 100), (10 * k + 10, 100), (0, 0)]]).collect();
    let out = repair(&fan);
    assert_eq!(out.len(), 1);
    assert_eq!(area(&out), 32.0 * 500.0);
}

#[test]
fn partially_coincident_segments() {
    let out = repair(&[vec![rect(0, 0, 6, 6)], vec![rect(2, 6, 9, 8)], vec![rect(6, -2, 8, 3)]]);
    assert_eq!(out.len(), 1);
    assert_eq!(area(&out), 36.0 + 14.0 + 10.0);
}

#[test]
fn near_integer_intersection_is_correctly_rounded() {
    // Edge (0,0)-(k,1) crosses x = x0 at y = x0/k; choose a crossing with a steep edge.
    let k = 1_000_003;
    let x0 = 999_999;
    let out = repair(&[vec![vec![(0, 0), (k, 1), (k, 3), (0, 2), (0, 0)]], vec![vec![(x0, -2), (x0, 5), (x0 + 1, 5), (x0 + 1, -2)]]]);
    let expected = (x0 as f64, (x0 as f64) / (k as f64));
    assert!(out.iter().flatten().flatten().any(|p| *p == expected), "exact rounded crossing missing");
}

// ---------------------------------------------------------------- [Inv]

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i32
    }
}

fn random_case(rng: &mut Rng) -> Vec<Polygon<IPt>> {
    let span = [6, 10, 40][rng.range(0, 2) as usize];
    (0..rng.range(1, 3))
        .map(|_| {
            (0..rng.range(1, 3))
                .map(|_| {
                    let mut ring: Vec<IPt> = (0..rng.range(3, 8)).map(|_| (rng.range(-span, span), rng.range(-span, span))).collect();
                    ring.push(ring[0]);
                    ring
                })
                .collect()
        })
        .collect()
}

#[test]
fn determinism_and_non_mutation() {
    let mut rng = Rng(0x5eed_2026_1030);
    for _ in 0..300 {
        let input = random_case(&mut rng);
        let copy = input.clone();
        let a = repair(&input);
        let b = repair(&input);
        assert_eq!(input, copy);
        let bits = |o: &Out| o.iter().flatten().flatten().map(|p| (p.0.to_bits(), p.1.to_bits())).collect::<Vec<_>>();
        assert_eq!(bits(&a), bits(&b));
    }
}

#[test]
fn invariance_under_rotation_reversal_and_permutation() {
    let mut rng = Rng(0xfeed_beef_1234);
    for _ in 0..400 {
        let input = random_case(&mut rng);
        let reference = repair(&input);
        let mut transformed: Vec<Polygon<IPt>> = input
            .iter()
            .map(|p| {
                p.iter()
                    .map(|ring| {
                        let mut open = ring[..ring.len() - 1].to_vec();
                        let k = rng.range(0, open.len() as i32 - 1) as usize;
                        open.rotate_left(k);
                        if rng.next() & 1 == 1 {
                            open.reverse();
                        }
                        open.push(open[0]);
                        open
                    })
                    .collect()
            })
            .collect();
        transformed.reverse();
        assert_eq!(repair(&transformed), reference, "input {input:?}");
    }
}

#[test]
fn integral_outputs_are_idempotent() {
    let mut rng = Rng(0x1d3e_0070_7e57);
    let mut checked = 0;
    for i in 0..600 {
        let input = if i % 2 == 0 {
            random_case(&mut rng)
        } else {
            (0..rng.range(1, 5))
                .map(|_| {
                    let (x, y) = (rng.range(-8, 8), rng.range(-8, 8));
                    (0..rng.range(1, 3))
                        .map(|_| rect(x + rng.range(-4, 4), y + rng.range(-4, 4), x + rng.range(5, 9), y + rng.range(5, 9)))
                        .collect()
                })
                .collect()
        };
        let out = repair(&input);
        if out.is_empty() || !out.iter().flatten().flatten().all(|p| p.0.fract() == 0.0 && p.1.fract() == 0.0) {
            continue;
        }
        let as_int: Vec<Polygon<IPt>> =
            out.iter().map(|poly| poly.iter().map(|r| r.iter().map(|p| (p.0 as i32, p.1 as i32)).collect()).collect()).collect();
        assert_eq!(repair(&as_int), out);
        checked += 1;
    }
    assert!(checked > 50, "too few integral outputs: {checked}");
}

#[test]
fn translation_moves_nodes_exactly_up_to_rounding() {
    let mut rng = Rng(0x7a45_1a7e);
    for _ in 0..200 {
        let input = random_case(&mut rng);
        let (dx, dy) = (rng.range(-1_000_000, 1_000_000), rng.range(-1_000_000, 1_000_000));
        let moved: Vec<Polygon<IPt>> =
            input.iter().map(|p| p.iter().map(|r| r.iter().map(|&(x, y)| (x + dx, y + dy)).collect()).collect()).collect();
        let a = repair(&input);
        let b = repair(&moved);
        assert_eq!(a.len(), b.len());
        let pa: Vec<P> = a.iter().flatten().flatten().copied().collect();
        let pb: Vec<P> = b.iter().flatten().flatten().copied().collect();
        assert_eq!(pa.len(), pb.len());
        for (p, q) in pa.iter().zip(&pb) {
            let tol = 4.0 * f64::EPSILON * 2.0e6;
            assert!((p.0 + f64::from(dx) - q.0).abs() <= tol && (p.1 + f64::from(dy) - q.1).abs() <= tol, "{p:?} {q:?}");
        }
    }
}
